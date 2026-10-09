//! Parsing of the versioned declarative documents (YAML or JSON; JSON is valid YAML).

use crate::flow::{FlowDocument, Metadata};
use crate::policy::PolicyDocument;
use crate::validate::{valid_name, Issue, ValidationReport};
use crate::API_VERSION;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentError {
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("document has no 'kind'")]
    MissingKind,
    #[error("unsupported apiVersion '{0}' (expected '{API_VERSION}')")]
    UnsupportedApiVersion(String),
    #[error("invalid {kind} document: {message}")]
    Schema { kind: String, message: String },
}

/// A reusable adapter binding, referenced from flows as `{ ref: <name> }` and inlined into the
/// flow at publish time so the published version is self-contained and reproducible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterConfigDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: AdapterConfigSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterConfigSpec {
    #[serde(rename = "type")]
    pub adapter_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub config: Value,
}

/// `soil.yaml` at a project root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: ProjectSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSpec {
    #[serde(default = "d_flows")]
    pub flows: String,
    #[serde(default = "d_policies")]
    pub policies: String,
    #[serde(default = "d_adapters")]
    pub adapters: String,
    #[serde(default = "d_scenarios")]
    pub scenarios: String,
}

fn d_flows() -> String {
    "flows".into()
}
fn d_policies() -> String {
    "policies".into()
}
fn d_adapters() -> String {
    "adapters".into()
}
fn d_scenarios() -> String {
    "scenarios".into()
}

impl Default for ProjectSpec {
    fn default() -> Self {
        Self {
            flows: d_flows(),
            policies: d_policies(),
            adapters: d_adapters(),
            scenarios: d_scenarios(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Document {
    Flow(Box<FlowDocument>),
    Policy(PolicyDocument),
    AdapterConfig(AdapterConfigDocument),
    Project(ProjectDocument),
    /// A document of a kind owned by another crate (e.g. `SimScenario`).
    Other { kind: String, value: Value },
}

impl Document {
    pub fn kind(&self) -> &str {
        match self {
            Document::Flow(_) => "Flow",
            Document::Policy(_) => "Policy",
            Document::AdapterConfig(_) => "AdapterConfig",
            Document::Project(_) => "Project",
            Document::Other { kind, .. } => kind,
        }
    }
}

pub fn parse_value(text: &str) -> Result<Value, DocumentError> {
    serde_yaml::from_str::<Value>(text).map_err(|e| DocumentError::Syntax(e.to_string()))
}

pub fn from_value<T: DeserializeOwned>(kind: &str, value: Value) -> Result<T, DocumentError> {
    serde_json::from_value(value).map_err(|e| DocumentError::Schema {
        kind: kind.to_string(),
        message: e.to_string(),
    })
}

pub fn parse_document(text: &str) -> Result<Document, DocumentError> {
    let value = parse_value(text)?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(DocumentError::MissingKind)?
        .to_string();
    let api = value
        .get("apiVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if api != API_VERSION {
        return Err(DocumentError::UnsupportedApiVersion(api.to_string()));
    }
    Ok(match kind.as_str() {
        "Flow" => Document::Flow(Box::new(from_value(&kind, value)?)),
        "Policy" => Document::Policy(from_value(&kind, value)?),
        "AdapterConfig" => Document::AdapterConfig(from_value(&kind, value)?),
        "Project" => Document::Project(from_value(&kind, value)?),
        _ => Document::Other { kind, value },
    })
}

pub fn parse_flow(text: &str) -> Result<FlowDocument, DocumentError> {
    match parse_document(text)? {
        Document::Flow(f) => Ok(*f),
        other => Err(DocumentError::Schema {
            kind: other.kind().to_string(),
            message: "expected a Flow document".into(),
        }),
    }
}

pub fn parse_policy(text: &str) -> Result<PolicyDocument, DocumentError> {
    match parse_document(text)? {
        Document::Policy(p) => Ok(p),
        other => Err(DocumentError::Schema {
            kind: other.kind().to_string(),
            message: "expected a Policy document".into(),
        }),
    }
}

pub fn to_yaml<T: Serialize>(value: &T) -> String {
    serde_yaml::to_string(&crate::canonical::to_value(value))
        .expect("JSON values always serialize to YAML")
}

pub fn to_json_pretty<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(&crate::canonical::to_value(value))
        .expect("JSON values always serialize")
}

/// Inlines `ref` adapter bindings from AdapterConfig documents.
pub fn resolve_adapter_refs(
    flow: &mut FlowDocument,
    configs: &[AdapterConfigDocument],
) -> Result<(), Vec<Issue>> {
    let mut issues = Vec::new();
    for (name, binding) in flow.spec.adapters.iter_mut() {
        let Some(r) = binding.config_ref.clone() else {
            continue;
        };
        match configs.iter().find(|c| c.metadata.name == r) {
            Some(cfg) => {
                binding.adapter_type = Some(cfg.spec.adapter_type.clone());
                binding.version = binding.version.or(cfg.spec.version);
                binding.config = merge(&cfg.spec.config, &binding.config);
                binding.config_ref = None;
            }
            None => issues.push(Issue {
                code: "adapter-ref".into(),
                path: format!("spec.adapters.{name}"),
                message: format!("no AdapterConfig named '{r}'"),
            }),
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

/// Shallow merge: keys in `overlay` win.
fn merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                out.insert(k.clone(), v.clone());
            }
            Value::Object(out)
        }
        (b, Value::Null) => b.clone(),
        (_, o) => o.clone(),
    }
}

pub fn validate_adapter_config(doc: &AdapterConfigDocument) -> ValidationReport {
    let mut r = ValidationReport::default();
    if doc.kind != "AdapterConfig" {
        r.error("kind", "kind", "expected 'AdapterConfig'");
    }
    if !valid_name(&doc.metadata.name) {
        r.error("name", "metadata.name", "invalid name");
    }
    if !doc.spec.config.is_null() && !doc.spec.config.is_object() {
        r.error("config", "spec.config", "must be a mapping");
    }
    r
}

pub fn validate_policy(doc: &PolicyDocument) -> ValidationReport {
    let mut r = ValidationReport::default();
    if doc.api_version != API_VERSION {
        r.error("api-version", "apiVersion", format!("expected '{API_VERSION}'"));
    }
    if doc.kind != "Policy" {
        r.error("kind", "kind", "expected 'Policy'");
    }
    if !valid_name(&doc.metadata.name) {
        r.error("name", "metadata.name", "invalid name");
    }
    if !(1..=10).contains(&doc.spec.max_reconcile_attempts) {
        r.error(
            "reconcile-attempts",
            "spec.maxReconcileAttempts",
            "must be between 1 and 10",
        );
    }
    for problem in doc.spec.constraints.problems() {
        r.error("constraint", "spec.constraints", problem);
    }
    for name in doc.spec.parameters.keys() {
        if !crate::validate::valid_id(name) || crate::validate::RESERVED_ROOTS.contains(&name.as_str()) {
            r.error(
                "parameter",
                format!("spec.parameters.{name}"),
                "parameter names must match [a-z_][a-z0-9_]* and not shadow a reserved name",
            );
        }
    }
    if doc.spec.allow_uncertainty {
        r.warn(
            "uncertainty",
            "spec.allowUncertainty",
            "commits may proceed with UNKNOWN invariants that opt in with onUnknown: allow",
        );
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_api_version_and_unknown_fields() {
        let bad = "apiVersion: soil.dev/v0\nkind: Policy\nmetadata: {name: p}\nspec: {}\n";
        assert!(matches!(
            parse_document(bad),
            Err(DocumentError::UnsupportedApiVersion(_))
        ));
        let typo = "apiVersion: soil.dev/v1alpha1\nkind: Policy\nmetadata: {name: p}\nspec: {allowUncertanty: true}\n";
        assert!(matches!(parse_document(typo), Err(DocumentError::Schema { .. })));
    }

    #[test]
    fn json_and_yaml_parse_identically() {
        let yaml = "apiVersion: soil.dev/v1alpha1\nkind: Policy\nmetadata: {name: p}\nspec: {allowIrreversible: true}\n";
        let json = r#"{"apiVersion":"soil.dev/v1alpha1","kind":"Policy","metadata":{"name":"p"},"spec":{"allowIrreversible":true}}"#;
        assert_eq!(parse_policy(yaml).unwrap(), parse_policy(json).unwrap());
        assert!(validate_policy(&parse_policy(yaml).unwrap()).is_ok());
    }

    #[test]
    fn other_kinds_pass_through() {
        let doc = parse_document("apiVersion: soil.dev/v1alpha1\nkind: SimScenario\nspec: {}\n").unwrap();
        assert_eq!(doc.kind(), "SimScenario");
    }
}
