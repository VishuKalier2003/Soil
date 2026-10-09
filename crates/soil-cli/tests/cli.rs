//! End-to-end CLI tests: init → validate → publish → plan → run → status → inspect → recover.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn soil(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_soil"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("soil binary runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soil-cli-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(soil(&dir, &["init", "."]).status.success());
    assert_eq!(soil(&dir, &["flow", "publish", "flows/prod-remediation.yaml"]).status.code(), Some(0));
    dir
}

fn run(dir: &Path, scenario: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "transaction", "run", "--flow", "prod-remediation", "--scenario", scenario, "--approve", "all", "-q",
    ];
    args.extend_from_slice(extra);
    soil(dir, &args)
}

fn latest_run(dir: &Path) -> String {
    let mut runs: Vec<_> = std::fs::read_dir(dir.join(".soil/runs"))
        .unwrap()
        .flatten()
        .map(|e| (e.metadata().unwrap().modified().unwrap(), e.file_name().to_string_lossy().to_string()))
        .collect();
    runs.sort();
    runs.last().unwrap().1.clone()
}

#[test]
fn validate_publish_and_versioning() {
    let dir = workspace("publish");
    let v = soil(&dir, &["validate"]);
    assert_eq!(v.status.code(), Some(0), "{}", stdout(&v));
    assert!(!stdout(&v).contains("FAIL"));

    let again = soil(&dir, &["flow", "publish", "flows/prod-remediation.yaml"]);
    assert!(stdout(&again).contains("unchanged"));

    // Editing the flow creates version 2; version 1 stays.
    let path = dir.join("flows/prod-remediation.yaml");
    let text = std::fs::read_to_string(&path).unwrap().replace("maxParallelism: 4", "maxParallelism: 2");
    std::fs::write(&path, text).unwrap();
    let v2 = soil(&dir, &["flow", "publish", "flows/prod-remediation.yaml"]);
    assert!(stdout(&v2).contains("prod-remediation@2"), "{}", stdout(&v2));
    assert!(dir.join(".soil/registry/flows/prod-remediation/v1.json").is_file());
    let plan = soil(&dir, &["transaction", "plan", "--flow", "prod-remediation@1", "--scenario", "happy-path"]);
    assert!(stdout(&plan).contains("prod-remediation@1"));
}

#[test]
fn invalid_flows_are_rejected() {
    let dir = workspace("invalid");
    let path = dir.join("flows/prod-remediation.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("    - {from: rollback_deploy, to: deploy, type: COMPENSATES}\n", "");
    let broken = dir.join("broken.yaml");
    std::fs::write(&broken, text).unwrap();
    let out = soil(&dir, &["flow", "validate", "broken.yaml"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("compensation-missing"), "{}", stdout(&out));
    assert_eq!(soil(&dir, &["flow", "publish", "broken.yaml"]).status.code(), Some(1));
    assert_eq!(soil(&dir, &["policy", "validate", "policies/prod-safety.yaml"]).status.code(), Some(0));
}

#[test]
fn run_status_and_inspect() {
    let dir = workspace("run");
    let out = run(&dir, "happy-path", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("COMMITTED"));
    let id = latest_run(&dir);
    let status = soil(&dir, &["transaction", "status", &id]);
    assert!(stdout(&status).contains("state     COMMITTED"));
    let inspect = soil(&dir, &["transaction", "inspect", &id, "--node", "deploy"]);
    assert!(stdout(&inspect).contains("evidence chain verified"));
    assert!(stdout(&inspect).contains("kubernetes:deployment.rollout"));

    let out = run(&dir, "slo-violation", &[]);
    assert_eq!(out.status.code(), Some(10), "{}", stdout(&out));
    assert!(stdout(&out).contains("RECOVERED"));
}

#[test]
fn halted_runs_recover_and_manual_runs_resolve() {
    let dir = workspace("recover");
    let out = run(&dir, "slo-violation", &["--no-recover"]);
    assert_eq!(out.status.code(), Some(20));
    let id = latest_run(&dir);
    let rec = soil(&dir, &["transaction", "recover", &id, "-q"]);
    assert_eq!(rec.status.code(), Some(10), "{}{}", stdout(&rec), String::from_utf8_lossy(&rec.stderr));
    assert!(stdout(&rec).contains("RECOVERED"));

    let out = run(&dir, "unobservable-timeout", &[]);
    assert_eq!(out.status.code(), Some(20));
    let id = latest_run(&dir);
    let refuse = soil(&dir, &["transaction", "recover", &id]);
    assert_eq!(refuse.status.code(), Some(1), "manual intervention needs an explicit resolution");
    let resolved = soil(&dir, &["transaction", "recover", &id, "--resolve", "aborted", "--operator", "oncall:sam", "-q"]);
    assert_eq!(resolved.status.code(), Some(10), "{}", String::from_utf8_lossy(&resolved.stderr));
}

#[test]
fn tampered_registry_is_detected() {
    let dir = workspace("tamper");
    let file = dir.join(".soil/registry/flows/prod-remediation/v1.json");
    let text = std::fs::read_to_string(&file).unwrap().replace("agent:remediator", "agent:mallory");
    std::fs::write(&file, text).unwrap();
    let out = run(&dir, "happy-path", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("corrupt"));
}

#[test]
fn adapters_are_listed() {
    let dir = workspace("adapters");
    let out = soil(&dir, &["adapter", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for a in ["github", "kubernetes", "ci", "http"] {
        assert!(v.get(a).is_some(), "{a}");
    }
}

// ---------------------------------------------------------------------------------------------
// Restartable execution (ship-fix example).

fn ship_fix(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soil-cli-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(soil(&dir, &["init", ".", "--example", "ship-fix"]).status.success());
    assert_eq!(soil(&dir, &["flow", "publish", "flows/ship-fix.yaml"]).status.code(), Some(0));
    dir
}

fn ship(dir: &Path, nonce: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "transaction", "run", "--flow", "ship-fix", "--scenario", "happy-path", "--nonce", nonce, "-q",
    ];
    args.extend_from_slice(extra);
    soil(dir, &args)
}

fn journal(dir: &Path, id: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join(".soil/runs").join(id).join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn killed_runs_resume_from_the_journal() {
    let dir = ship_fix("crash");
    let out = ship(&dir, "a", &["--approve", "all", "--crash-after", "100"]);
    assert_eq!(out.status.code(), Some(75), "{}", String::from_utf8_lossy(&out.stderr));
    let id = latest_run(&dir);
    assert_eq!(journal(&dir, &id).len(), 101, "every record up to the crash is durable");

    let resumed = soil(&dir, &["transaction", "resume", &id, "--approve", "all", "-q"]);
    assert_eq!(resumed.status.code(), Some(0), "{}", stdout(&resumed));
    assert!(stdout(&resumed).contains("COMMITTED"));
    let records = journal(&dir, &id);
    let status = soil(&dir, &["transaction", "status", &id]);
    assert!(stdout(&status).contains("state     COMMITTED"));
    let inspect = soil(&dir, &["transaction", "inspect", &id]);
    assert!(stdout(&inspect).contains(&format!("evidence chain verified: {} records", records.len())));
}

#[test]
fn in_flight_requests_are_never_resent_after_a_crash() {
    let dir = ship_fix("inflight");
    // Find where `deploy` is journaled as EXECUTING (positions are deterministic).
    assert_eq!(ship(&dir, "probe", &["--approve", "all"]).status.code(), Some(0));
    let probe = latest_run(&dir);
    let at = journal(&dir, &probe)
        .iter()
        .find(|r| r["nodeId"] == "deploy" && r["stateAfter"] == "EXECUTING")
        .unwrap()["seq"]
        .as_u64()
        .unwrap();

    // Kill the executor after the request was journaled but before it was sent.
    let out = ship(&dir, "b", &["--approve", "all", "--crash-after", &at.to_string()]);
    assert_eq!(out.status.code(), Some(75));
    let id = latest_run(&dir);
    let resumed = soil(&dir, &["transaction", "resume", &id, "--approve", "all", "-q"]);
    // UNCERTAIN → observed absent → confirmed failure → declared recovery. Not committed.
    assert_eq!(resumed.status.code(), Some(10), "{}", stdout(&resumed));
    let status = stdout(&soil(&dir, &["transaction", "status", &id]));
    assert!(status.contains("state     RECOVERED"), "{status}");
    let deploy_line = status.lines().find(|l| l.trim_start().starts_with("deploy ")).unwrap();
    assert!(deploy_line.contains("FAILED"), "{deploy_line}");
    let records = journal(&dir, &id);
    let dispatches = records
        .iter()
        .filter(|r| r["nodeId"] == "deploy" && r["stateAfter"] == "EXECUTING")
        .count();
    assert_eq!(dispatches, 1, "deploy must not be dispatched again");
}

#[test]
fn deferred_approvals_pause_and_resume() {
    let dir = ship_fix("approval");
    let out = ship(&dir, "c", &["--defer-approvals"]);
    assert_eq!(out.status.code(), Some(30), "{}", stdout(&out));
    assert!(stdout(&out).contains("waiting for approval of [\"approve\"]"));
    let id = latest_run(&dir);

    let again = soil(&dir, &["transaction", "resume", &id, "--defer-approvals", "-q"]);
    assert_eq!(again.status.code(), Some(30), "still waiting");
    let done = soil(&dir, &["transaction", "resume", &id, "--approve", "approve", "-q"]);
    assert_eq!(done.status.code(), Some(0), "{}", stdout(&done));
    assert!(stdout(&done).contains("COMMITTED"));
}

// Recovery with a compensation flow (canary-release example).

#[test]
fn canary_release_recovers_and_journals_the_compensation_flow_separately() {
    let dir = std::env::temp_dir().join(format!("soil-cli-test-canary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(soil(&dir, &["init", ".", "--example", "canary-release"]).status.success());
    assert_eq!(soil(&dir, &["validate"]).status.code(), Some(0));
    // The compensation flow must be published before any flow that uses it can be planned.
    assert_eq!(soil(&dir, &["flow", "publish", "flows/canary-release.yaml"]).status.code(), Some(0));
    let early = soil(&dir, &["transaction", "run", "--flow", "canary-release", "--scenario", "slo-violation", "-q"]);
    assert_ne!(early.status.code(), Some(10), "{}", stdout(&early));
    assert_eq!(soil(&dir, &["flow", "publish", "flows/traffic-failback.yaml"]).status.code(), Some(0));

    let out = soil(&dir, &["transaction", "run", "--flow", "canary-release", "--scenario", "slo-violation", "-q"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(10), "{text}");
    assert!(text.contains("result    RECOVERED"), "{text}");
    let id = text.split_whitespace().find(|w| w.starts_with("tx-")).unwrap().to_string();

    let run_dir = dir.join(".soil/runs").join(&id);
    assert!(journal(&dir, &id).iter().all(|r| r["transactionId"] == id.as_str()));
    let nested: Vec<_> = std::fs::read_dir(&run_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("subtx-"))
        .collect();
    assert_eq!(nested.len(), 1, "one nested journal for the traffic-failback flow");

    let status = stdout(&soil(&dir, &["transaction", "status", &id]));
    assert!(status.contains("restored (verified)"), "{status}");
    assert!(status.contains("subtransaction"), "{status}");
    assert!(status.contains("divergence"), "{status}");
}
