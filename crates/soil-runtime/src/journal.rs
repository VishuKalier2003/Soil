//! Durable execution journal.
//!
//! The engine announces every state transition to its [`EventSink`] *before* acting on it (in
//! particular, `EXECUTING` is announced before the adapter is called). [`FileJournal`] makes
//! each announcement durable before returning:
//!
//! 1. `run.json` — the complete run state after the transition, replaced atomically
//!    (write to a temporary file, then rename). This is what [`Engine::resume`] restarts from.
//! 2. `journal.jsonl` — the evidence record appended to an append-only log.
//!
//! If the process dies between (1) and (2), [`FileJournal::load`] repairs the log from the
//! snapshot's (hash-verified) evidence chain. If writing fails, the journal panics: executing
//! further without durable state would break restartability, so the engine must stop.
//!
//! [`Engine::resume`]: crate::Engine::resume

use crate::providers::EventSink;
use crate::run::TransactionRun;
use soil_core::evidence::{EvidenceChain, EvidenceRecord};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_FILE: &str = "run.json";
pub const LOG_FILE: &str = "journal.jsonl";

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("{0}: {1}")]
    Io(String, io::Error),
    #[error("{0} is corrupt: {1}")]
    Corrupt(String, String),
}

pub struct FileJournal {
    dir: PathBuf,
}

impl FileJournal {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, JournalError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|e| JournalError::Io(dir.display().to_string(), e))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn write(&self, record: &EvidenceRecord, run: &TransactionRun) -> io::Result<()> {
        let snapshot = self.dir.join(SNAPSHOT_FILE);
        let tmp = self.dir.join(format!("{SNAPSHOT_FILE}.tmp"));
        fs::write(&tmp, serde_json::to_vec_pretty(run).map_err(io::Error::other)?)?;
        fs::rename(&tmp, &snapshot)?;
        // Records of nested transactions (compensation flows) have their own chain and log.
        let log_name = if record.transaction_id == run.id {
            LOG_FILE.to_string()
        } else {
            format!("subtx-{}.jsonl", record.transaction_id)
        };
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(log_name))?;
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        log.write_all(&line)?;
        log.sync_data()
    }

    /// Loads the latest durable run state, verifying its evidence chain and repairing the
    /// append-only log if the process died between the two writes of one event.
    pub fn load(&self) -> Result<TransactionRun, JournalError> {
        let snapshot = self.dir.join(SNAPSHOT_FILE);
        let name = snapshot.display().to_string();
        let text = fs::read_to_string(&snapshot).map_err(|e| JournalError::Io(name.clone(), e))?;
        let run: TransactionRun =
            serde_json::from_str(&text).map_err(|e| JournalError::Corrupt(name.clone(), e.to_string()))?;
        run.evidence
            .verify()
            .map_err(|e| JournalError::Corrupt(name.clone(), e.to_string()))?;

        let log_path = self.dir.join(LOG_FILE);
        let logged = self.read_log(&log_path)?;
        let records = run.evidence.records();
        if logged.len() > records.len() || logged.iter().zip(records).any(|(a, b)| a.hash != b.hash) {
            return Err(JournalError::Corrupt(
                log_path.display().to_string(),
                "journal disagrees with the run snapshot".into(),
            ));
        }
        if logged.len() < records.len() {
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|e| JournalError::Io(log_path.display().to_string(), e))?;
            for r in &records[logged.len()..] {
                let mut line = serde_json::to_vec(r).expect("records serialize");
                line.push(b'\n');
                log.write_all(&line)
                    .map_err(|e| JournalError::Io(log_path.display().to_string(), e))?;
            }
        }
        Ok(run)
    }

    /// The append-only log, verified as a chain on its own.
    pub fn records(&self) -> Result<Vec<EvidenceRecord>, JournalError> {
        let path = self.dir.join(LOG_FILE);
        let records = self.read_log(&path)?;
        if let Some(first) = records.first() {
            EvidenceChain::from_records(
                &first.transaction_id,
                first.transaction_version,
                &first.definition_hash,
                records.clone(),
            )
            .map_err(|e| JournalError::Corrupt(path.display().to_string(), e.to_string()))?;
        }
        Ok(records)
    }

    fn read_log(&self, path: &Path) -> Result<Vec<EvidenceRecord>, JournalError> {
        let name = path.display().to_string();
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(JournalError::Io(name, e)),
        };
        io::BufReader::new(file)
            .lines()
            .filter(|l| l.as_ref().map(|l| !l.trim().is_empty()).unwrap_or(true))
            .map(|l| {
                let l = l.map_err(|e| JournalError::Io(name.clone(), e))?;
                serde_json::from_str(&l).map_err(|e| JournalError::Corrupt(name.clone(), e.to_string()))
            })
            .collect()
    }
}

impl EventSink for FileJournal {
    fn on_event(&self, record: &EvidenceRecord, run: &TransactionRun) {
        if let Err(e) = self.write(record, run) {
            panic!("journal write to {} failed: {e}; refusing to continue without durable state", self.dir.display());
        }
    }
}
