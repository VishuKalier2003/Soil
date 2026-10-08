//! `soil` — validate, publish, plan, run, inspect and recover Soil transactions.
//!
//! Exit codes for `transaction run` / `recover`:
//! 0 committed · 10 recovered or aborted (no lasting effect) · 20 needs attention
//! (partially recovered, manual intervention, halted) · 1 usage/validation errors.

mod output;
mod project;

use clap::{Args, Parser, Subcommand};
use project::{read_document, Project};
use serde_json::{json, Value};
use soil_adapters::sim::{SimWorld, WorldState};
use soil_core::document::{parse_value, validate_adapter_config, validate_policy, Document};
use soil_core::plan::{plan, TransactionPlan};
use soil_core::policy::EffectivePolicy;
use soil_core::state::TransactionState;
use soil_core::transaction::{FlowVersion, PublishOutcome, TransactionId};
use soil_core::validate::validate_flow;
use soil_runtime::{
    Clock, Engine, EngineOptions, LogicalClock, Resolution, ResolutionOutcome, StaticApprovals,
    StaticAuthority, SystemClock, TransactionRun,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("{0}: {1}")]
    Document(String, String),
    #[error("{0} is corrupt or was modified: {1}")]
    Corrupt(String, String),
    #[error("no soil.yaml found in {0} or its parents (run `soil init`)")]
    NoProject(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0} already exists and is immutable")]
    Immutable(String),
    #[error("validation failed:\n{0}")]
    Invalid(String),
    #[error("{0}")]
    Usage(String),
    #[error("engine: {0}")]
    Engine(#[from] soil_runtime::EngineError),
    #[error("plan: {0}")]
    Plan(#[from] soil_core::plan::PlanError),
}

impl CliError {
    pub fn io(path: &Path, e: std::io::Error) -> Self {
        Self::Io(path.display().to_string(), e.to_string())
    }
}

#[derive(Parser)]
#[command(name = "soil", version, about = "Transaction integrity for autonomous agents' external side effects")]
struct Cli {
    /// Project directory (default: search upward from the current directory).
    #[arg(long, short = 'C', global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project with the production-remediation example.
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Validate every document in the project.
    Validate,
    #[command(subcommand)]
    Flow(FlowCmd),
    #[command(subcommand)]
    Transaction(TxCmd),
    #[command(subcommand)]
    Policy(PolicyCmd),
    #[command(subcommand)]
    Adapter(AdapterCmd),
}

#[derive(Subcommand)]
enum FlowCmd {
    /// Validate a flow document.
    Validate { file: PathBuf },
    /// Publish a flow as a new immutable version (no-op if unchanged).
    Publish { file: PathBuf },
}

#[derive(Args)]
struct PlanArgs {
    /// Published flow: `name` (latest) or `name@N`.
    #[arg(long)]
    flow: String,
    /// Input as key=value (value parsed as YAML scalar). Repeatable.
    #[arg(long = "input", short = 'i')]
    inputs: Vec<String>,
    /// Simulation scenario (name in scenarios/ or a path). Supplies default inputs.
    #[arg(long)]
    scenario: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum TxCmd {
    /// Plan a transaction: inputs, pinned versions, guarantees, execution waves.
    Plan(PlanArgs),
    /// Plan, authorize, seal and execute a transaction.
    Run {
        #[command(flatten)]
        plan: PlanArgs,
        /// Approve APPROVAL nodes: node id or `all`. Repeatable. Default: none approved.
        #[arg(long)]
        approve: Vec<String>,
        /// Halt in FAILED/UNCERTAIN instead of running recovery immediately.
        #[arg(long)]
        no_recover: bool,
        /// Use a logical clock so evidence hashes are reproducible.
        #[arg(long)]
        deterministic: bool,
        /// Only print the summary.
        #[arg(long, short)]
        quiet: bool,
        /// Also print per-node admission records (VALIDATING/AUTHORIZED/SEALED).
        #[arg(long, short)]
        verbose: bool,
        /// Nonce for the transaction id (default: current time).
        #[arg(long)]
        nonce: Option<String>,
    },
    /// Show a transaction's state.
    Status { id: Option<String> },
    /// Run recovery for a halted transaction, or record an operator resolution.
    Recover {
        id: String,
        /// Resolve a MANUAL_INTERVENTION transaction: recovered | partially_recovered | aborted.
        #[arg(long)]
        resolve: Option<String>,
        #[arg(long, default_value = "operator")]
        operator: String,
        #[arg(long, default_value = "")]
        note: String,
        /// Simulate the outage ending: drop injected faults before recovering.
        #[arg(long)]
        clear_faults: bool,
        #[arg(long, short)]
        quiet: bool,
    },
    /// Print the evidence chain (verifying every hash).
    Inspect {
        id: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PolicyCmd {
    /// Validate a policy document.
    Validate { file: PathBuf },
}

#[derive(Subcommand)]
enum AdapterCmd {
    /// List built-in adapters and their declared operation semantics.
    List {
        #[arg(long)]
        json: bool,
    },
}

const EXAMPLE: &[(&str, &str)] = &[
    ("soil.yaml", include_str!("../../../examples/remediation/soil.yaml")),
    ("flows/prod-remediation.yaml", include_str!("../../../examples/remediation/flows/prod-remediation.yaml")),
    ("policies/prod-safety.yaml", include_str!("../../../examples/remediation/policies/prod-safety.yaml")),
    ("adapters/github-acme.yaml", include_str!("../../../examples/remediation/adapters/github-acme.yaml")),
    ("adapters/k8s-prod-eu.yaml", include_str!("../../../examples/remediation/adapters/k8s-prod-eu.yaml")),
    ("scenarios/happy-path.yaml", include_str!("../../../examples/remediation/scenarios/happy-path.yaml")),
    ("scenarios/timeout-but-applied.yaml", include_str!("../../../examples/remediation/scenarios/timeout-but-applied.yaml")),
    ("scenarios/partial-traffic-shift.yaml", include_str!("../../../examples/remediation/scenarios/partial-traffic-shift.yaml")),
    ("scenarios/slo-violation.yaml", include_str!("../../../examples/remediation/scenarios/slo-violation.yaml")),
    ("scenarios/compensation-fails.yaml", include_str!("../../../examples/remediation/scenarios/compensation-fails.yaml")),
    ("scenarios/unobservable-timeout.yaml", include_str!("../../../examples/remediation/scenarios/unobservable-timeout.yaml")),
    ("scenarios/tests-fail.yaml", include_str!("../../../examples/remediation/scenarios/tests-fail.yaml")),
];

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn project(cli_project: &Option<PathBuf>) -> Result<Project, CliError> {
    let start = cli_project.clone().unwrap_or_else(|| PathBuf::from("."));
    Project::find(&start)
}

fn dispatch(cli: Cli) -> Result<u8, CliError> {
    match cli.command {
        Command::Init { dir, force } => init(&dir, force),
        Command::Validate => validate_project(&project(&cli.project)?),
        Command::Flow(FlowCmd::Validate { file }) => flow_validate(&project(&cli.project)?, &file),
        Command::Flow(FlowCmd::Publish { file }) => flow_publish(&project(&cli.project)?, &file),
        Command::Transaction(TxCmd::Plan(args)) => {
            let p = project(&cli.project)?;
            let (plan, _) = make_plan(&p, &args)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&plan).expect("serializable"));
            } else {
                output::print_plan(&plan);
            }
            Ok(0)
        }
        Command::Transaction(TxCmd::Run {
            plan: args,
            approve,
            no_recover,
            deterministic,
            quiet,
            verbose,
            nonce,
        }) => tx_run(
            &project(&cli.project)?,
            &args,
            &approve,
            RunFlags { no_recover, deterministic, quiet, verbose },
            nonce,
        ),
        Command::Transaction(TxCmd::Status { id }) => {
            let p = project(&cli.project)?;
            let id = match id {
                Some(id) => id,
                None => p
                    .run_ids()
                    .last()
                    .cloned()
                    .ok_or_else(|| CliError::NotFound("any transaction run".into()))?,
            };
            output::print_status(&p.load_run(&id)?);
            Ok(0)
        }
        Command::Transaction(TxCmd::Recover {
            id,
            resolve,
            operator,
            note,
            clear_faults,
            quiet,
        }) => tx_recover(&project(&cli.project)?, &id, resolve, operator, note, clear_faults, quiet),
        Command::Transaction(TxCmd::Inspect { id, node, json }) => {
            let p = project(&cli.project)?;
            let run = p.load_run(&id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&run).expect("serializable"));
            } else {
                output::print_evidence(&run, node.as_deref());
            }
            Ok(0)
        }
        Command::Policy(PolicyCmd::Validate { file }) => match read_document(&file)? {
            Document::Policy(doc) => report(&file, &validate_policy(&doc)),
            other => Err(CliError::Document(
                file.display().to_string(),
                format!("expected kind Policy, found {}", other.kind()),
            )),
        },
        Command::Adapter(AdapterCmd::List { json }) => {
            let catalog = soil_adapters::catalog();
            if json {
                println!("{}", serde_json::to_string_pretty(&catalog).expect("serializable"));
            } else {
                output::print_adapters(&catalog);
            }
            Ok(0)
        }
    }
}

fn report(path: &Path, r: &soil_core::validate::ValidationReport) -> Result<u8, CliError> {
    print!("{r}");
    if r.is_ok() {
        println!("ok  {} ({} warnings)", path.display(), r.warnings.len());
        Ok(0)
    } else {
        println!("FAIL {} ({} errors)", path.display(), r.errors.len());
        Ok(1)
    }
}

fn init(dir: &Path, force: bool) -> Result<u8, CliError> {
    if dir.join("soil.yaml").exists() && !force {
        return Err(CliError::Usage(format!(
            "{} already contains soil.yaml (use --force to overwrite)",
            dir.display()
        )));
    }
    for (rel, content) in EXAMPLE {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CliError::io(parent, e))?;
        }
        std::fs::write(&path, content).map_err(|e| CliError::io(&path, e))?;
        println!("created {}", path.display());
    }
    println!(
        "\nNext:\n  soil validate\n  soil flow publish flows/prod-remediation.yaml\n  soil transaction run --flow prod-remediation --scenario timeout-but-applied --approve all"
    );
    Ok(0)
}

/// Resolves adapter refs and validates a flow against the project's policies.
fn check_flow(p: &Project, mut doc: soil_core::flow::FlowDocument) -> Result<(soil_core::flow::FlowDocument, soil_core::validate::ValidationReport), CliError> {
    let configs = p.adapter_configs()?;
    let mut report = soil_core::validate::ValidationReport::default();
    if let Err(issues) = soil_core::document::resolve_adapter_refs(&mut doc, &configs) {
        report.errors.extend(issues);
        return Ok((doc, report));
    }
    let policy = match EffectivePolicy::resolve(&doc.spec.policies, &p.draft_policies()?) {
        Ok(policy) => policy,
        Err(e) => {
            report.error("policy", "spec.policies", e.to_string());
            return Ok((doc, report));
        }
    };
    let r = validate_flow(&doc, &soil_adapters::catalog(), &policy);
    Ok((doc, r))
}

fn validate_project(p: &Project) -> Result<u8, CliError> {
    let mut failed = false;
    for f in p.yaml_files(&p.doc.spec.adapters) {
        match read_document(&f)? {
            Document::AdapterConfig(c) => failed |= report(&f, &validate_adapter_config(&c))? != 0,
            other => return Err(CliError::Document(f.display().to_string(), format!("unexpected kind {}", other.kind()))),
        }
    }
    for (f, doc) in p.policy_docs()? {
        failed |= report(&f, &validate_policy(&doc))? != 0;
    }
    for (f, doc) in p.flow_docs()? {
        let (_, r) = check_flow(p, doc)?;
        failed |= report(&f, &r)? != 0;
    }
    for f in p.yaml_files(&p.doc.spec.scenarios) {
        let text = std::fs::read_to_string(&f).map_err(|e| CliError::io(&f, e))?;
        match soil_adapters::sim::SimScenario::parse(&text) {
            Ok(_) => println!("ok  {}", f.display()),
            Err(e) => {
                println!("FAIL {}: {e}", f.display());
                failed = true;
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn read_flow(file: &Path) -> Result<soil_core::flow::FlowDocument, CliError> {
    match read_document(file)? {
        Document::Flow(d) => Ok(*d),
        other => Err(CliError::Document(
            file.display().to_string(),
            format!("expected kind Flow, found {}", other.kind()),
        )),
    }
}

fn flow_validate(p: &Project, file: &Path) -> Result<u8, CliError> {
    let (_, r) = check_flow(p, read_flow(file)?)?;
    report(file, &r)
}

fn flow_publish(p: &Project, file: &Path) -> Result<u8, CliError> {
    let doc = read_flow(file)?;
    let synced = p.sync_policies()?;
    for s in synced {
        println!("published policy {s}");
    }
    let policies = p.policy_versions()?;
    let effective = EffectivePolicy::resolve(&doc.spec.policies, &policies)
        .map_err(|e| CliError::Invalid(e.to_string()))?;
    let latest = p.flow_versions(&doc.metadata.name)?.into_iter().last();
    let outcome = FlowVersion::publish(
        doc,
        latest.as_ref(),
        &p.adapter_configs()?,
        &soil_adapters::catalog(),
        &effective,
    )
    .map_err(|r| CliError::Invalid(r.to_string()))?;
    match outcome {
        PublishOutcome::Unchanged(v) => println!("unchanged: {} is already the latest version", v.reference()),
        PublishOutcome::Published(v) => {
            let path = p.save_flow_version(&v)?;
            println!("published {} -> {}", v.reference(), path.display());
        }
    }
    Ok(0)
}

fn parse_inputs(args: &PlanArgs, defaults: BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>, CliError> {
    let mut inputs = defaults;
    for kv in &args.inputs {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("--input expects key=value, got '{kv}'")))?;
        let value = parse_value(v).unwrap_or_else(|_| Value::String(v.to_string()));
        inputs.insert(k.to_string(), if value.is_null() { Value::String(v.into()) } else { value });
    }
    Ok(inputs)
}

fn make_plan(p: &Project, args: &PlanArgs) -> Result<(TransactionPlan, Option<soil_adapters::sim::SimScenario>), CliError> {
    let version = p.resolve_flow(&args.flow)?;
    let scenario = args.scenario.as_deref().map(|s| p.scenario(s)).transpose()?;
    let defaults = scenario.as_ref().map(|s| s.spec.inputs.clone()).unwrap_or_default();
    let inputs = parse_inputs(args, defaults)?;
    let plan = plan(&version, inputs, &soil_adapters::catalog(), &p.policy_versions()?)?;
    Ok((plan, scenario))
}

struct Printer {
    quiet: bool,
    verbose: bool,
}

impl soil_runtime::EventSink for Printer {
    fn on_event(&self, record: &soil_core::evidence::EvidenceRecord, run: &TransactionRun) {
        if self.quiet {
            return;
        }
        if !self.verbose && output::is_admission(record) {
            // One summary line when the last node of a phase is admitted.
            let last = run.nodes.keys().next_back().map(String::as_str);
            if record.node_id.as_deref() == last {
                println!(
                    "  {:>4}  {:<11} {:<17} {:>20} {} nodes → {}",
                    record.seq, "action", "(all nodes)", "", run.nodes.len(), record.state_after
                );
            }
            return;
        }
        output::print_record(record);
    }
}

struct RunFlags {
    no_recover: bool,
    deterministic: bool,
    quiet: bool,
    verbose: bool,
}

fn exit_code(state: TransactionState) -> u8 {
    match state {
        TransactionState::Committed => 0,
        TransactionState::Recovered | TransactionState::Aborted | TransactionState::Cancelled => 10,
        _ => 20,
    }
}

fn approvals(approve: &[String]) -> StaticApprovals {
    if approve.iter().any(|a| a == "all") {
        StaticApprovals::approve_all("cli")
    } else {
        StaticApprovals::only(approve.iter().cloned(), "cli")
    }
}

fn tx_run(
    p: &Project,
    args: &PlanArgs,
    approve: &[String],
    flags: RunFlags,
    nonce: Option<String>,
) -> Result<u8, CliError> {
    let RunFlags { no_recover, deterministic, quiet, verbose } = flags;
    let (plan, scenario) = make_plan(p, args)?;
    let scenario = scenario.ok_or_else(|| {
        CliError::Usage(
            "the MVP ships simulated transports only; pass --scenario <name|file> (see scenarios/)".into(),
        )
    })?;
    let sim = Arc::new(SimWorld::from_scenario(&scenario));
    let registry = soil_adapters::registry(sim.clone());
    let clock: Box<dyn Clock> = if deterministic {
        Box::new(LogicalClock::new(1_760_000_000_000, 1))
    } else {
        Box::new(SystemClock)
    };
    let nonce = nonce.unwrap_or_else(|| SystemClock.now_ms().to_string());
    let id = TransactionId::derive(&plan.plan_hash, &nonce);
    let approvals = approvals(approve);
    let authority = StaticAuthority::grant_all();
    let printer = Printer { quiet, verbose };
    let engine = Engine {
        adapters: &registry,
        clock: clock.as_ref(),
        authority: &authority,
        approvals: &approvals,
        sink: &printer,
        options: EngineOptions { auto_recover: !no_recover },
    };
    output::print_header(&plan, id.as_str(), &scenario);
    let result = engine.run(plan, id)?;
    let scenario_json = json!({
        "name": scenario.name,
        "faults": scenario.spec.faults,
        "latencyMs": scenario.spec.latency_ms,
        "expect": scenario.spec.expect,
    });
    let dir = p.save_run(result.sealed.as_ref(), &result.run, &sim.snapshot(), &scenario_json)?;
    output::print_summary(&result.run, scenario.spec.expect.as_deref());
    println!("  stored    {}", dir.display());
    Ok(exit_code(result.run.state))
}

fn tx_recover(
    p: &Project,
    id: &str,
    resolve: Option<String>,
    operator: String,
    note: String,
    clear_faults: bool,
    quiet: bool,
) -> Result<u8, CliError> {
    let run = p.load_run(id)?;
    let sealed = p.load_sealed(&run.id)?;
    let dir = p.run_dir(&run.id);
    let world: WorldState = project::read_json(&dir.join("world.json"))?;
    let scenario: Value = project::read_json(&dir.join("scenario.json"))?;
    let faults: BTreeMap<String, Vec<soil_adapters::sim::Fault>> = if clear_faults {
        BTreeMap::new()
    } else {
        serde_json::from_value::<BTreeMap<String, soil_adapters::sim::Faults>>(scenario["faults"].clone())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, f)| (k, f.list()))
            .collect()
    };
    let sim = Arc::new(SimWorld::new(world, faults, 0));
    let registry = soil_adapters::registry(sim.clone());
    let approvals = StaticApprovals::approve_all("cli");
    let authority = StaticAuthority::grant_all();
    let printer = Printer { quiet, verbose: false };
    let engine = Engine {
        adapters: &registry,
        clock: &SystemClock,
        authority: &authority,
        approvals: &approvals,
        sink: &printer,
        options: EngineOptions::default(),
    };
    let updated = match resolve {
        Some(outcome) => {
            let outcome = match outcome.as_str() {
                "recovered" => ResolutionOutcome::Recovered,
                "partially_recovered" => ResolutionOutcome::PartiallyRecovered,
                "aborted" => ResolutionOutcome::Aborted,
                other => return Err(CliError::Usage(format!("unknown resolution '{other}'"))),
            };
            engine.resolve(&sealed, run, &Resolution { outcome, operator, note })?
        }
        None => engine.recover(&sealed, run)?,
    };
    p.save_run(Some(&sealed), &updated, &sim.snapshot(), &scenario)?;
    output::print_summary(&updated, None);
    Ok(exit_code(updated.state))
}
