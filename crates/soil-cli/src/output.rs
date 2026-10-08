//! Human-readable output.

use soil_adapters::sim::SimScenario;
use soil_core::adapter::AdapterCatalog;
use soil_core::evidence::EvidenceRecord;
use soil_core::plan::TransactionPlan;
use soil_core::state::MachineKind;
use soil_runtime::TransactionRun;

fn short(hash: &str) -> &str {
    &hash[..12.min(hash.len())]
}

fn machine(m: MachineKind) -> &'static str {
    match m {
        MachineKind::Action => "action",
        MachineKind::Transaction => "transaction",
        MachineKind::Effect => "effect",
        MachineKind::Guarantee => "guarantee",
        MachineKind::Policy => "policy",
        MachineKind::Invariant => "invariant",
        MachineKind::Recovery => "recovery",
    }
}

fn note(r: &EvidenceRecord) -> String {
    let d = &r.detail;
    let pick = [
        d["reason"].as_str().map(String::from),
        d["cause"]["reason"].as_str().map(String::from),
        d["outcome"]["kind"].as_str().map(|o| format!("adapter: {o}")),
        d["invariant"].as_str().map(|i| match d["result"].as_str() {
            Some(r) => format!("{i} = {r}"),
            None => format!("{i}: {}", d["expr"].as_str().unwrap_or("")),
        }),
        d["reconcileAttempt"].as_u64().map(|n| format!("reconcile attempt {n}")),
        d["skipped"].as_str().map(|s| format!("skipped ({s})")),
    ];
    pick.into_iter().flatten().next().unwrap_or_default()
}

/// Admission records (per-node VALIDATING/AUTHORIZED/SEALED) are summarized unless verbose.
pub fn is_admission(r: &EvidenceRecord) -> bool {
    r.machine == MachineKind::Action
        && matches!(r.state_after.as_str(), "VALIDATING" | "AUTHORIZED" | "SEALED")
}

pub fn print_record(r: &EvidenceRecord) {
    let node = r.node_id.as_deref().unwrap_or("");
    let before = r.state_before.as_deref().unwrap_or("∅");
    println!(
        "  {:>4}  {:<11} {:<17} {:>20} → {:<21} {}",
        r.seq,
        machine(r.machine),
        node,
        before,
        r.state_after,
        note(r)
    );
}

pub fn print_header(plan: &TransactionPlan, id: &str, scenario: &SimScenario) {
    println!("transaction {id}");
    println!("  flow      {}", plan.flow);
    println!("  intent    {}", plan.intent.summary);
    println!("  scenario  {} (simulated providers)", scenario.name);
    if let Some(d) = &scenario.spec.description {
        println!("            {d}");
    }
    println!();
}

pub fn print_summary(run: &TransactionRun, expect: Option<&str>) {
    println!();
    println!("  result    {}", run.state);
    println!("  recovery  {}", run.recovery.state);
    println!("  guarantee {}  {}", run.guarantee.state, run.guarantee.notes.last().cloned().unwrap_or_default());
    if let Some(f) = &run.failure {
        println!(
            "  cause     {} ({:?}): {}",
            f.node.as_deref().unwrap_or("commit boundary"),
            f.class,
            f.reason
        );
    }
    let reconciled: Vec<&String> = run.nodes.iter().filter(|(_, n)| n.reconciled).map(|(id, _)| id).collect();
    if !reconciled.is_empty() {
        println!("  reconciled after uncertainty: {reconciled:?}");
    }
    let verified = run.evidence.verify().is_ok();
    println!(
        "  evidence  {} records, head {} ({})",
        run.evidence.len(),
        short(run.evidence.head_hash()),
        if verified { "chain verified" } else { "CHAIN BROKEN" }
    );
    if let Some(e) = expect {
        let ok = run.state.to_string() == e;
        println!("  expected  {e} {}", if ok { "✓" } else { "✗" });
    }
    println!("  inspect   soil transaction status {}", run.id);
}

pub fn print_status(run: &TransactionRun) {
    println!("transaction {} v{}", run.id, run.version);
    println!("  flow      {}", run.flow);
    println!("  intent    {}", run.intent);
    println!("  state     {}", run.state);
    println!("  recovery  {}", run.recovery.state);
    println!("  guarantee {}", run.guarantee.state);
    if let Some(a) = &run.authority {
        println!("  authority {} {} ({})", a.provider, a.reference, a.decision_ref);
    }
    println!();
    println!(
        "  {:<17} {:<10} {:<8} {:<21} {:<13} {:<8} notes",
        "node", "kind", "phase", "action", "effect", "presence"
    );
    for (id, n) in &run.nodes {
        let effect = n.effect.map(|e| e.to_string()).unwrap_or_else(|| "-".into());
        let presence = n
            .presence
            .map(|p| format!("{p:?}").to_lowercase())
            .unwrap_or_else(|| "-".into());
        let mut notes = Vec::new();
        if n.reconciled {
            notes.push("reconciled".to_string());
        }
        if let Some(s) = n.skipped {
            notes.push(format!("skipped:{}", format!("{s:?}").to_lowercase()));
        }
        if let Some(r) = &n.reason {
            notes.push(r.clone());
        }
        if let Some(v) = &n.verification {
            notes.push(v.clone());
        }
        println!(
            "  {:<17} {:<10} {:<8} {:<21} {:<13} {:<8} {}",
            id,
            format!("{:?}", n.kind).to_uppercase(),
            format!("{:?}", n.phase).to_lowercase(),
            n.action.to_string(),
            effect,
            presence,
            notes.join("; ")
        );
    }
    if !run.invariants.is_empty() {
        println!();
        for (id, i) in &run.invariants {
            println!("  invariant {:<14} {:<10} [{}] {}", id, i.state.to_string(), i.scope, i.expr);
        }
    }
    if !run.recovery.plan.is_empty() || !run.recovery.classification.is_empty() {
        println!();
        println!("  recovery classification {:?}", run.recovery.classification);
        println!("  recovery plan           {:?}", run.recovery.plan);
        if !run.recovery.uncovered.is_empty() {
            println!("  uncovered               {:?}", run.recovery.uncovered);
        }
    }
}

pub fn print_evidence(run: &TransactionRun, node: Option<&str>) {
    match run.evidence.verify() {
        Ok(()) => println!("evidence chain verified: {} records", run.evidence.len()),
        Err(e) => println!("EVIDENCE CHAIN BROKEN: {e}"),
    }
    for r in run.evidence.records() {
        if node.is_some_and(|n| r.node_id.as_deref() != Some(n)) {
            continue;
        }
        println!(
            "#{:<4} {}  {} {:<17} {} → {}",
            r.seq,
            short(&r.hash),
            machine(r.machine),
            r.node_id.as_deref().unwrap_or("-"),
            r.state_before.as_deref().unwrap_or("∅"),
            r.state_after
        );
        println!("       at {}  actor {}  prev {}", r.at_ms, r.actor, short(&r.prev_hash));
        if let Some(op) = &r.operation {
            println!("       {} {}", op, r.resource.as_deref().unwrap_or(""));
        }
        if let Some(e) = &r.effect_summary {
            println!("       {e}");
        }
        if !r.detail.is_null() && r.detail != serde_json::json!({}) {
            println!("       {}", r.detail);
        }
    }
}

pub fn print_plan(plan: &TransactionPlan) {
    println!("plan {}", short(&plan.plan_hash));
    println!("  flow        {}", plan.flow);
    println!("  intent      {}", plan.intent.summary);
    println!("  inputs      {}", serde_json::to_string(&plan.intent.inputs).unwrap_or_default());
    println!("  authority   {} {}", plan.authority.provider, plan.authority.reference);
    println!("  policies    {:?}", plan.policy.refs);
    println!("  adapters    {:?}", plan.adapters);
    println!("  max risk    {:?}", plan.max_risk);
    let g = &plan.guarantees;
    println!(
        "  guarantees  atomicity={:?} verifiability={:?} reconcilability={:?} idempotency={:?} isolation={:?}",
        g.atomicity, g.verifiability, g.reconcilability, g.idempotency, g.isolation
    );
    println!("  required    {:?}", plan.required_guarantees);
    println!("\n  forward waves:");
    for (i, level) in plan.forward_levels.iter().enumerate() {
        println!("    {i}: {}", level.join(", "));
    }
    println!("  recovery waves:");
    for (i, level) in plan.recovery_levels.iter().enumerate() {
        println!("    {i}: {}", level.join(", "));
    }
    println!("  compensation:");
    for n in plan.nodes.values() {
        if let Some(c) = &n.compensator {
            println!("    {:<16} ← {c}  ({:?})", n.spec.id, n.reversibility);
        }
    }
    for w in &plan.warnings {
        println!("  warning[{}] {}: {}", w.code, w.path, w.message);
    }
}

pub fn print_adapters(catalog: &AdapterCatalog) {
    for d in catalog.values() {
        println!("{} v{} — {}", d.adapter_type, d.version, d.description);
        for o in &d.operations {
            println!(
                "  {:<22} {:<9} {:<13} idempotency={:<7} verify={:<9} uncertainty={:<14} {}{}",
                o.name,
                format!("{:?}", o.effect).to_lowercase(),
                format!("{:?}", o.reversibility).to_lowercase(),
                format!("{:?}", o.idempotency).to_lowercase(),
                format!("{:?}", o.verification).to_lowercase(),
                format!("{:?}", o.uncertainty).to_lowercase(),
                o.resource_type,
                o.compensation
                    .operation()
                    .map(|c| format!("  (compensation: {c})"))
                    .unwrap_or_default()
            );
            let fields = |s: &soil_core::adapter::Schema| {
                s.fields
                    .iter()
                    .map(|(k, f)| {
                        let req = if s.required.contains(k) { "" } else { "?" };
                        format!("{k}{req}: {}", format!("{:?}", f.ty).to_lowercase())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!("      id {}  resource {}", o.id, o.resource_pattern);
            println!("      in  {{{}}}  out {{{}}}", fields(&o.input), fields(&o.output));
            if !o.postconditions.is_empty() {
                println!("      post {}", o.postconditions.join(" && "));
            }
        }
        println!();
    }
}
