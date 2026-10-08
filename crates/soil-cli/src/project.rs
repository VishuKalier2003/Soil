//! A Soil project on disk and its file-based registry under `.soil/`.
//!
//! ```text
//! soil.yaml                      Project document
//! flows/ policies/ adapters/ scenarios/
//! .soil/registry/flows/<name>/v<N>.json      immutable flow versions
//! .soil/registry/policies/<name>/v<N>.json   policy versions with lifecycle state
//! .soil/runs/<tx-id>/{sealed,run,world,scenario}.json
//! ```

use crate::CliError;
use serde::{de::DeserializeOwned, Serialize};
use soil_adapters::sim::{SimScenario, WorldState};
use soil_core::document::{
    parse_document, AdapterConfigDocument, Document, ProjectDocument,
};
use soil_core::flow::FlowDocument;
use soil_core::policy::{PolicyDocument, PolicyVersion};
use soil_core::state::PolicyState;
use soil_core::transaction::{FlowVersion, SealedTransaction};
use soil_runtime::TransactionRun;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Project {
    pub root: PathBuf,
    pub doc: ProjectDocument,
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    let text = fs::read_to_string(path).map_err(|e| CliError::io(path, e))?;
    serde_json::from_str(&text).map_err(|e| CliError::Corrupt(path.display().to_string(), e.to_string()))
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CliError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| CliError::io(dir, e))?;
    }
    let text = serde_json::to_string_pretty(value).expect("serializable");
    fs::write(path, text).map_err(|e| CliError::io(path, e))
}

pub fn read_document(path: &Path) -> Result<Document, CliError> {
    let text = fs::read_to_string(path).map_err(|e| CliError::io(path, e))?;
    parse_document(&text).map_err(|e| CliError::Document(path.display().to_string(), e.to_string()))
}

impl Project {
    /// Finds `soil.yaml` in `start` or any parent directory.
    pub fn find(start: &Path) -> Result<Self, CliError> {
        let mut dir = plain_path(start.canonicalize().map_err(|e| CliError::io(start, e))?);
        loop {
            let file = dir.join("soil.yaml");
            if file.is_file() {
                return match read_document(&file)? {
                    Document::Project(doc) => Ok(Self { root: dir, doc }),
                    other => Err(CliError::Document(
                        file.display().to_string(),
                        format!("expected kind Project, found {}", other.kind()),
                    )),
                };
            }
            if !dir.pop() {
                return Err(CliError::NoProject(start.display().to_string()));
            }
        }
    }

    pub fn dir(&self, sub: &str) -> PathBuf {
        self.root.join(sub)
    }

    pub fn yaml_files(&self, sub: &str) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(self.dir(sub))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yaml" | "yml" | "json")
                )
            })
            .collect();
        files.sort();
        files
    }

    pub fn adapter_configs(&self) -> Result<Vec<AdapterConfigDocument>, CliError> {
        let mut out = Vec::new();
        for f in self.yaml_files(&self.doc.spec.adapters) {
            match read_document(&f)? {
                Document::AdapterConfig(c) => out.push(c),
                other => return Err(wrong_kind(&f, "AdapterConfig", other.kind())),
            }
        }
        Ok(out)
    }

    pub fn policy_docs(&self) -> Result<Vec<(PathBuf, PolicyDocument)>, CliError> {
        let mut out = Vec::new();
        for f in self.yaml_files(&self.doc.spec.policies) {
            match read_document(&f)? {
                Document::Policy(p) => out.push((f, p)),
                other => return Err(wrong_kind(&f, "Policy", other.kind())),
            }
        }
        Ok(out)
    }

    pub fn flow_docs(&self) -> Result<Vec<(PathBuf, FlowDocument)>, CliError> {
        let mut out = Vec::new();
        for f in self.yaml_files(&self.doc.spec.flows) {
            match read_document(&f)? {
                Document::Flow(d) => out.push((f, *d)),
                other => return Err(wrong_kind(&f, "Flow", other.kind())),
            }
        }
        Ok(out)
    }

    pub fn scenario(&self, name_or_path: &str) -> Result<SimScenario, CliError> {
        let direct = PathBuf::from(name_or_path);
        let path = if direct.is_file() {
            direct
        } else {
            let in_project = self
                .dir(&self.doc.spec.scenarios)
                .join(format!("{name_or_path}.yaml"));
            if !in_project.is_file() {
                return Err(CliError::NotFound(format!("scenario '{name_or_path}'")));
            }
            in_project
        };
        let text = fs::read_to_string(&path).map_err(|e| CliError::io(&path, e))?;
        SimScenario::parse(&text).map_err(|e| CliError::Document(path.display().to_string(), e))
    }

    /// Project policy documents treated as active drafts (for validating unpublished flows).
    pub fn draft_policies(&self) -> Result<Vec<PolicyVersion>, CliError> {
        Ok(self
            .policy_docs()?
            .iter()
            .map(|(_, d)| {
                let mut v = PolicyVersion::draft(d, 0);
                v.state = PolicyState::Active;
                v
            })
            .collect())
    }

    // -- registry ------------------------------------------------------------------------------

    fn registry(&self, kind: &str, name: &str) -> PathBuf {
        self.root.join(".soil").join("registry").join(kind).join(name)
    }

    fn versions<T: DeserializeOwned>(&self, kind: &str, name: &str) -> Result<Vec<(u32, T)>, CliError> {
        let dir = self.registry(kind, name);
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let n = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix('v'))
                .and_then(|s| s.parse::<u32>().ok());
            if let Some(n) = n {
                out.push((n, read_json(&path)?));
            }
        }
        out.sort_by_key(|(n, _)| *n);
        Ok(out)
    }

    pub fn flow_versions(&self, name: &str) -> Result<Vec<FlowVersion>, CliError> {
        let versions: Vec<FlowVersion> = self
            .versions::<FlowVersion>("flows", name)?
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        for v in &versions {
            if !v.verify() {
                return Err(CliError::Corrupt(
                    format!("{}@{}", v.name, v.version),
                    "content hash mismatch".into(),
                ));
            }
        }
        Ok(versions)
    }

    pub fn save_flow_version(&self, v: &FlowVersion) -> Result<PathBuf, CliError> {
        let path = self
            .registry("flows", &v.name)
            .join(format!("v{}.json", v.version));
        if path.exists() {
            return Err(CliError::Immutable(path.display().to_string()));
        }
        write_json(&path, v)?;
        Ok(path)
    }

    /// `name` (latest) or `name@N`.
    pub fn resolve_flow(&self, reference: &str) -> Result<FlowVersion, CliError> {
        let (name, version) = match reference.split_once('@') {
            Some((n, v)) => (
                n,
                Some(v.parse::<u32>().map_err(|_| CliError::Usage(format!("bad version in '{reference}'")))?),
            ),
            None => (reference, None),
        };
        let versions = self.flow_versions(name)?;
        let found = match version {
            Some(v) => versions.into_iter().find(|x| x.version == v),
            None => versions.into_iter().last(),
        };
        found.ok_or_else(|| {
            CliError::NotFound(format!(
                "published flow '{reference}' (run `soil flow publish` first)"
            ))
        })
    }

    pub fn policy_versions(&self) -> Result<Vec<PolicyVersion>, CliError> {
        let mut out = Vec::new();
        let dir = self.root.join(".soil").join("registry").join("policies");
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Some(name) = entry.file_name().to_str() {
                out.extend(self.versions::<PolicyVersion>("policies", name)?.into_iter().map(|(_, v)| v));
            }
        }
        Ok(out)
    }

    /// Publishes changed project policies as new ACTIVE versions, superseding the previous one.
    pub fn sync_policies(&self) -> Result<Vec<String>, CliError> {
        let mut published = Vec::new();
        for (_, doc) in self.policy_docs()? {
            let report = soil_core::document::validate_policy(&doc);
            if !report.is_ok() {
                return Err(CliError::Invalid(report.to_string()));
            }
            let versions = self.versions::<PolicyVersion>("policies", &doc.metadata.name)?;
            let mut draft = PolicyVersion::draft(&doc, versions.last().map(|v| v.0 + 1).unwrap_or(1));
            if versions.iter().any(|(_, v)| v.hash == draft.hash && v.state == PolicyState::Active) {
                continue;
            }
            for (n, mut old) in versions {
                if old.state == PolicyState::Active {
                    old.transition(PolicyState::Superseded)
                        .map_err(|e| CliError::Invalid(e.to_string()))?;
                    write_json(
                        &self.registry("policies", &doc.metadata.name).join(format!("v{n}.json")),
                        &old,
                    )?;
                }
            }
            for s in [PolicyState::Validating, PolicyState::Active] {
                draft.transition(s).map_err(|e| CliError::Invalid(e.to_string()))?;
            }
            write_json(
                &self.registry("policies", &draft.name).join(format!("v{}.json", draft.version)),
                &draft,
            )?;
            published.push(draft.reference());
        }
        Ok(published)
    }

    // -- runs ------------------------------------------------------------------------------------

    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.root.join(".soil").join("runs").join(id)
    }

    pub fn save_run(
        &self,
        sealed: Option<&SealedTransaction>,
        run: &TransactionRun,
        world: &WorldState,
        scenario: &serde_json::Value,
    ) -> Result<PathBuf, CliError> {
        let dir = self.run_dir(&run.id);
        if let Some(s) = sealed {
            write_json(&dir.join("sealed.json"), s)?;
        }
        write_json(&dir.join("run.json"), run)?;
        write_json(&dir.join("world.json"), world)?;
        write_json(&dir.join("scenario.json"), scenario)?;
        Ok(dir)
    }

    pub fn load_run(&self, id: &str) -> Result<TransactionRun, CliError> {
        let dir = self.find_run_dir(id)?;
        let run: TransactionRun = read_json(&dir.join("run.json"))?;
        run.evidence
            .verify()
            .map_err(|e| CliError::Corrupt(dir.display().to_string(), e.to_string()))?;
        Ok(run)
    }

    pub fn load_sealed(&self, id: &str) -> Result<SealedTransaction, CliError> {
        let dir = self.find_run_dir(id)?;
        let sealed: SealedTransaction = read_json(&dir.join("sealed.json"))?;
        sealed
            .verify()
            .map_err(|e| CliError::Corrupt(dir.display().to_string(), e.to_string()))?;
        Ok(sealed)
    }

    /// Accepts a full id or a unique prefix.
    pub fn find_run_dir(&self, id: &str) -> Result<PathBuf, CliError> {
        let exact = self.run_dir(id);
        if exact.is_dir() {
            return Ok(exact);
        }
        let matches: Vec<PathBuf> = fs::read_dir(self.root.join(".soil").join("runs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(id))
            .map(|e| e.path())
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(CliError::NotFound(format!("transaction '{id}'"))),
            _ => Err(CliError::Usage(format!("'{id}' matches several transactions"))),
        }
    }

    pub fn run_ids(&self) -> Vec<String> {
        let mut ids: Vec<(std::time::SystemTime, String)> = fs::read_dir(self.root.join(".soil").join("runs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let t = e.metadata().and_then(|m| m.modified()).ok()?;
                Some((t, e.file_name().to_string_lossy().to_string()))
            })
            .collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    }
}

/// Drops the Windows verbatim prefix (`\?\`) that `canonicalize` adds.
fn plain_path(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

fn wrong_kind(path: &Path, want: &str, got: &str) -> CliError {
    CliError::Document(
        path.display().to_string(),
        format!("expected kind {want}, found {got}"),
    )
}
