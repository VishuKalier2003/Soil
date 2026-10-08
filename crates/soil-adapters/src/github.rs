//! GitHub: repository inspection, branch commits and pull requests.

use crate::support::{self, absent, failure, inconclusive, observed, outcome, param_str, read, Op, Read};
use crate::transport::Transport;
use serde_json::{json, Value};
use soil_core::adapter::{Adapter, AdapterDescriptor, AdapterResult, Invocation, Schema, SchemaType as T};
use soil_core::model::{EffectKind, IdempotencyBehavior, Presence, Reversibility};
use std::sync::Arc;

const PROVIDER: &str = "github";

fn pull_output() -> Schema {
    Schema::of(&[
        ("number", T::Integer, true),
        ("state", T::String, true),
        ("head", T::String, true),
        ("base", T::String, false),
        ("url", T::String, false),
    ])
}

fn ops() -> Vec<soil_core::adapter::OperationDescriptor> {
    vec![
        Op::read(
            PROVIDER,
            "repo.inspect",
            "Read repository metadata and the default branch head",
            "github.repository",
            "github://{owner}/{repo}",
        )
        .output(Schema::of(&[
            ("full_name", T::String, true),
            ("default_branch", T::String, true),
            ("head_sha", T::String, true),
            ("archived", T::Boolean, true),
        ]))
        .build(),
        Op::write(
            PROVIDER,
            "branch.commit",
            "Create a branch containing one commit with the given file changes",
            "github.branch",
            "github://{owner}/{repo}/branches/{branch}",
            EffectKind::Create,
            "a new branch whose head commit carries the given message and files",
        )
        .input(
            Schema::of(&[
                ("branch", T::String, true),
                ("message", T::String, true),
                ("files", T::Object, true),
                ("base", T::String, false),
            ])
            .allowing_additional(),
        )
        .output(Schema::of(&[
            ("branch", T::String, true),
            ("sha", T::String, true),
            ("base", T::String, false),
            ("message", T::String, false),
        ]))
        .pre(&["exists(params.files)"])
        .post(&["observed.branch == params.branch", "observed.message == params.message"])
        .idempotency(IdempotencyBehavior::Keyed)
        .reversibility(Reversibility::Compensatable)
        .compensated_by("branch.delete", &[("branch", "${target.params.branch}")])
        .build(),
        Op::write(
            PROVIDER,
            "branch.delete",
            "Delete a branch",
            "github.branch",
            "github://{owner}/{repo}/branches/{branch}",
            EffectKind::Delete,
            "the branch no longer exists",
        )
        .input(Schema::of(&[("branch", T::String, true)]))
        .output(Schema::of(&[("deleted", T::Boolean, true)]))
        .reversibility(Reversibility::Irreversible)
        .build(),
        Op::write(
            PROVIDER,
            "pull_request.create",
            "Open a pull request from a head branch",
            "github.pull_request",
            "github://{owner}/{repo}/pulls/{head}",
            EffectKind::Create,
            "an open pull request from head into base",
        )
        .input(Schema::of(&[
            ("head", T::String, true),
            ("base", T::String, true),
            ("title", T::String, true),
            ("body", T::String, false),
        ]))
        .output(pull_output())
        .post(&["observed.state == \"open\"", "observed.head == params.head"])
        .reversibility(Reversibility::Compensatable)
        .compensated_by("pull_request.close", &[("number", "${target.observed.number}")])
        .build(),
        Op::write(
            PROVIDER,
            "pull_request.close",
            "Close a pull request without merging",
            "github.pull_request",
            "github://{owner}/{repo}/pulls/{head}",
            EffectKind::Update,
            "the pull request is closed",
        )
        .input(Schema::of(&[("number", T::Integer, true)]))
        .output(pull_output())
        .post(&["observed.state == \"closed\""])
        .build(),
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
                operations: ops(),
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

    fn execute(&self, inv: &Invocation) -> AdapterResult {
        let Some(repo) = Self::repo_path(inv) else {
            return failure(format!("'{}' is not a github:// repository resource", inv.resource));
        };
        let (method, path, body, normalize): (&str, String, Value, fn(&Value) -> Value) =
            match inv.operation.as_str() {
                "repo.inspect" => ("GET", repo, Value::Null, normalize_repo),
                "branch.commit" => {
                    let Some(branch) = param_str(inv, "branch") else {
                        return failure("missing param 'branch'");
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
                        return failure("missing param 'branch'");
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
                        return failure("missing numeric param 'number'");
                    };
                    ("PATCH", format!("{repo}/pulls/{n}"), json!({"state": "closed"}), normalize_pull)
                }
                other => return failure(format!("unknown operation '{other}'")),
            };
        outcome(
            self.transport.call(&support::request(PROVIDER, inv, method, path, body)),
            normalize,
        )
    }

    fn observe(&self, inv: &Invocation) -> AdapterResult {
        let Some(repo) = Self::repo_path(inv) else {
            return inconclusive("invalid resource");
        };
        match inv.operation.as_str() {
            "repo.inspect" => match self.get(inv, repo) {
                Read::Found(b) => observed(Presence::Present, normalize_repo(&b)),
                Read::NotFound => absent(),
                Read::Unknown(r) => r,
            },
            "branch.commit" | "branch.delete" => {
                let branch = param_str(inv, "branch").unwrap_or_default();
                let found = self.get(inv, format!("{repo}/git/ref/heads/{branch}"));
                let deleting = inv.operation == "branch.delete";
                match found {
                    Read::Found(b) if deleting => observed(Presence::Absent, normalize_ref(&b)),
                    Read::NotFound if deleting => observed(Presence::Present, json!({ "deleted": true })),
                    // The branch exists; postconditions decide whether it is *our* commit.
                    Read::Found(b) => observed(Presence::Present, normalize_ref(&b)),
                    Read::NotFound => absent(),
                    Read::Unknown(r) => r,
                }
            }
            "pull_request.create" => {
                let head = param_str(inv, "head").unwrap_or_default();
                match self.get(inv, format!("{repo}/pulls?head={head}&state=open")) {
                    Read::Found(Value::Array(prs)) => match prs.first() {
                        Some(pr) => observed(Presence::Present, normalize_pull(pr)),
                        None => absent(),
                    },
                    Read::Found(_) => inconclusive("unexpected pull list shape"),
                    Read::NotFound => absent(),
                    Read::Unknown(r) => r,
                }
            }
            "pull_request.close" => {
                let n = inv.params.get("number").and_then(Value::as_u64).unwrap_or(0);
                match self.get(inv, format!("{repo}/pulls/{n}")) {
                    Read::Found(b) => {
                        let state = normalize_pull(&b);
                        let p = if state["state"] == "closed" { Presence::Present } else { Presence::Absent };
                        observed(p, state)
                    }
                    Read::NotFound => absent(),
                    Read::Unknown(r) => r,
                }
            }
            other => inconclusive(format!("unknown operation '{other}'")),
        }
    }
}
