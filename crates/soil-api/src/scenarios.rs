//! Built-in demo material: the remediation example and its simulation scenarios.

use soil_adapters::sim::SimScenario;

pub const FLOW: &str = include_str!("../../../examples/remediation/flows/prod-remediation.yaml");
pub const POLICY: &str = include_str!("../../../examples/remediation/policies/prod-safety.yaml");
pub const ADAPTER_CONFIGS: [&str; 2] = [
    include_str!("../../../examples/remediation/adapters/github-acme.yaml"),
    include_str!("../../../examples/remediation/adapters/k8s-prod-eu.yaml"),
];

const SCENARIOS: [&str; 7] = [
    include_str!("../../../examples/remediation/scenarios/happy-path.yaml"),
    include_str!("../../../examples/remediation/scenarios/timeout-but-applied.yaml"),
    include_str!("../../../examples/remediation/scenarios/partial-traffic-shift.yaml"),
    include_str!("../../../examples/remediation/scenarios/slo-violation.yaml"),
    include_str!("../../../examples/remediation/scenarios/compensation-fails.yaml"),
    include_str!("../../../examples/remediation/scenarios/unobservable-timeout.yaml"),
    include_str!("../../../examples/remediation/scenarios/tests-fail.yaml"),
];

pub fn all() -> Vec<SimScenario> {
    SCENARIOS
        .iter()
        .map(|t| SimScenario::parse(t).expect("built-in scenarios are valid"))
        .collect()
}

pub fn by_name(name: &str) -> Option<SimScenario> {
    all().into_iter().find(|s| s.name == name)
}
