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
