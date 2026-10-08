//! GitHub: repository inspection, branch commits and pull requests.

use crate::support::{self, observation, outcome, param_str, read, rejected, OpSpec, Read};
use crate::transport::Transport;
use serde_json::{json, Value};
use soil_core::adapter::{Adapter, AdapterDescriptor, ExecutionOutcome, Invocation, Observation};
use soil_core::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};
use std::sync::Arc;

const PROVIDER: &str = "github";

fn ops() -> Vec<OpSpec> {

    vec![
        OpSpec::read(
            "repo.inspect",
            "Read repository metadata and the default branch head",
            "github://{owner}/{repo}",
        ),
        OpSpec {
            name: "branch.commit",
            description: "Create a branch containing one commit with the given file changes",
            read_only: false,
            effect: EffectKind::Create,
            resource: "github://{owner}/{repo}/branches/{branch}",
            params: &["branch", "message", "files"],
            preconditions: &["exists(params.files)"],
            idempotency: IdempotencyBehavior::Keyed,
            reversibility: Reversibility::Compensatable,
            verification: VerificationStrategy::ReadBack,
            compensation: Some("branch.delete"),
            uncertainty: UncertaintySemantics::Reconcilable,
        },
        OpSpec {
            name: "branch.delete",
            description: "Delete a branch",
            read_only: false,
            effect: EffectKind::Delete,
            resource: "github://{owner}/{repo}/branches/{branch}",
            params: &["branch"],
            preconditions: &[],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Irreversible,
            verification: VerificationStrategy::ReadBack,
            compensation: None,
            uncertainty: UncertaintySemantics::Reconcilable,
        },
        OpSpec {
            name: "pull_request.create",
            description: "Open a pull request from a head branch",
            read_only: false,
            effect: EffectKind::Create,
            resource: "github://{owner}/{repo}/pulls/{head}",
            params: &["head", "base", "title"],
            preconditions: &[],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Compensatable,
            verification: VerificationStrategy::ReadBack,
            compensation: Some("pull_request.close"),
            uncertainty: UncertaintySemantics::Reconcilable,
        },
        OpSpec {
            name: "pull_request.close",
            description: "Close a pull request without merging",
            read_only: false,
            effect: EffectKind::Update,
            resource: "github://{owner}/{repo}/pulls/{head}",
            params: &["number"],
            preconditions: &[],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Reversible,
            verification: VerificationStrategy::ReadBack,
            compensation: None,
            uncertainty: UncertaintySemantics::Reconcilable,
        },
    ]
}

pub struct GitHubAdapter {
    transport: Arc<dyn Transport>,
    descriptor: AdapterDescriptor,
}

impl GitHubAdapter {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            descriptor: AdapterDescriptor {
                adapter_type: PROVIDER.into(),
                version: 1,
                description: "GitHub repositories, branches and pull requests".into(),
                operations: ops().iter().map(OpSpec::descriptor).collect(),
            },
        }
    }

    fn repo_path(inv: &Invocation) -> Option<String> {
        let segs = support::segments(&inv.resource, PROVIDER)?;
        (segs.len() >= 2).then(|| format!("/repos/{}/{}", segs[0], segs[1]))
    }

    fn get(&self, inv: &Invocation, path: String) -> Read {
        read(&*self.transport, support::request(PROVIDER, inv, "GET", path, Value::Null))
    }
}

fn normalize_repo(b: &Value) -> Value {
    json!({
        "full_name": b["full_name"],
        "default_branch": b["default_branch"],
        "head_sha": b["branch_head"]["sha"],
        "archived": b["archived"],
    })
}

fn normalize_ref(b: &Value) -> Value {
    json!({
        "branch": b["ref"].as_str().map(|r| r.trim_start_matches("refs/heads/")),
        "sha": b["object"]["sha"],
        "base": b["base_sha"],
        "message": b["message"],
    })
}

fn normalize_pull(b: &Value) -> Value {
    json!({
        "number": b["number"],
        "state": b["state"],
        "head": b["head"]["ref"],
        "base": b["base"]["ref"],
        "url": b["html_url"],
    })
}

impl Adapter for GitHubAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn execute(&self, inv: &Invocation) -> ExecutionOutcome {
        let Some(repo) = Self::repo_path(inv) else {
            return rejected(format!("'{}' is not a github:// repository resource", inv.resource));
        };
        let (method, path, body, normalize): (&str, String, Value, fn(&Value) -> Value) =
            match inv.operation.as_str() {
                "repo.inspect" => ("GET", repo, Value::Null, normalize_repo),
                "branch.commit" => {
                    let Some(branch) = param_str(inv, "branch") else {
                        return rejected("missing param 'branch'");
                    };
                    (
                        "POST",
                        format!("{repo}/git/refs"),
                        json!({
                            "ref": format!("refs/heads/{branch}"),
                            "sha": inv.params.get("base"),
                            "message": inv.params.get("message"),
                            "files": inv.params.get("files"),
                        }),
                        normalize_ref,
                    )
                }
                "branch.delete" => {
                    let Some(branch) = param_str(inv, "branch") else {
                        return rejected("missing param 'branch'");
                    };
                    ("DELETE", format!("{repo}/git/refs/heads/{branch}"), Value::Null, |_| {
                        json!({ "deleted": true })
                    })
                }
                "pull_request.create" => (
                    "POST",
                    format!("{repo}/pulls"),
                    json!({
                        "head": inv.params.get("head"),
                        "base": inv.params.get("base"),
                        "title": inv.params.get("title"),
                        "body": inv.params.get("body"),
                    }),
                    normalize_pull,
                ),
                "pull_request.close" => {
                    let Some(n) = inv.params.get("number").and_then(Value::as_u64) else {
                        return rejected("missing numeric param 'number'");
                    };
                    ("PATCH", format!("{repo}/pulls/{n}"), json!({"state": "closed"}), normalize_pull)
                }
                other => return rejected(format!("unknown operation '{other}'")),
            };
        let response = self
            .transport
            .call(&support::request(PROVIDER, inv, method, path, body));
        outcome(response, normalize)
    }

    fn observe(&self, inv: &Invocation) -> Observation {
        let Some(repo) = Self::repo_path(inv) else {
            return Observation::unknown("invalid resource");
        };
        let unknown = |r: String| Observation::unknown(r);
        match inv.operation.as_str() {
            "repo.inspect" => match self.get(inv, repo) {
                Read::Found(b) => observation(Presence::Present, normalize_repo(&b)),
                Read::NotFound => observation(Presence::Absent, Value::Null),
                Read::Unknown(r) => unknown(r),
            },
            "branch.commit" | "branch.delete" => {
                let branch = param_str(inv, "branch").unwrap_or_default();
                let found = self.get(inv, format!("{repo}/git/ref/heads/{branch}"));
                let deleting = inv.operation == "branch.delete";
                match found {
                    Read::Found(b) if deleting => observation(Presence::Absent, normalize_ref(&b)),
                    Read::NotFound if deleting => {
                        observation(Presence::Present, json!({ "deleted": true }))
                    }
                    Read::Found(b) => {
                        let state = normalize_ref(&b);
                        let matches = state["message"] == inv.params["message"];
                        let p = if matches { Presence::Present } else { Presence::Partial };
                        observation(p, state)
                    }
                    Read::NotFound => observation(Presence::Absent, Value::Null),
                    Read::Unknown(r) => unknown(r),
                }
            }
            "pull_request.create" => {
                let head = param_str(inv, "head").unwrap_or_default();
                match self.get(inv, format!("{repo}/pulls?head={head}&state=open")) {
                    Read::Found(Value::Array(prs)) => match prs.first() {
                        Some(pr) => observation(Presence::Present, normalize_pull(pr)),
                        None => observation(Presence::Absent, Value::Null),
                    },
                    Read::Found(_) => unknown("unexpected pull list shape".into()),
                    Read::NotFound => observation(Presence::Absent, Value::Null),
                    Read::Unknown(r) => unknown(r),
                }
            }
            "pull_request.close" => {
                let n = inv.params.get("number").and_then(Value::as_u64).unwrap_or(0);
                match self.get(inv, format!("{repo}/pulls/{n}")) {
                    Read::Found(b) => {
                        let state = normalize_pull(&b);
                        let p = if state["state"] == "closed" {
                            Presence::Present
                        } else {
                            Presence::Absent
                        };
                        observation(p, state)
                    }
                    Read::NotFound => observation(Presence::Absent, Value::Null),
                    Read::Unknown(r) => unknown(r),
                }
            }
            other => unknown(format!("unknown operation '{other}'")),
        }
    }
}
