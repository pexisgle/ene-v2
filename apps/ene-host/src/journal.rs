use crate::error::conflict;
use anyhow::{Context, Result, ensure};
use ene_core::mind::{Message, Receipt, Record, Summary};
use std::{collections::HashMap, path::PathBuf};
use tokio::io::AsyncWriteExt;

pub(crate) struct Journal {
    path: PathBuf,
    pub entries: Vec<Message>,
    pub summary: Option<Summary>,
    pub injected: HashMap<String, Vec<(String, i64)>>,
    receipts: HashMap<String, (String, String)>,
}

impl Journal {
    pub(crate) async fn open(path: PathBuf) -> Result<Self> {
        let mut journal = Self {
            path,
            entries: Vec::new(),
            summary: None,
            injected: HashMap::new(),
            receipts: HashMap::new(),
        };
        let bytes = match tokio::fs::read(&journal.path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(journal),
            Err(error) => return Err(error.into()),
        };
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        for line in bytes[..end].split_inclusive(|byte| *byte == b'\n') {
            journal.apply(serde_json::from_slice(line).context("invalid conversation entry")?)?;
        }
        if end != bytes.len() {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&journal.path)
                .await?;
            file.set_len(end as u64).await?;
            file.sync_data().await?;
        }
        Ok(journal)
    }

    pub(crate) fn receipt(&self, key: &str, fingerprint: &str) -> Result<Option<&str>> {
        match self.receipts.get(key) {
            Some((saved, _)) if saved != fingerprint => Err(conflict("request key already used")),
            Some((_, id)) => Ok(Some(id)),
            None => Ok(None),
        }
    }

    pub(crate) async fn append(&mut self, record: Record) -> Result<()> {
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        let previous = file.metadata().await?.len();
        if let Err(error) = async {
            file.write_all(&line).await?;
            file.sync_data().await
        }
        .await
        {
            file.set_len(previous).await?;
            file.sync_data().await?;
            return Err(error.into());
        }
        self.apply(record)
    }

    fn apply(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Entry { message, receipt } => {
                if let Some(Receipt { key, fingerprint }) = receipt {
                    ensure!(
                        !self.receipts.contains_key(&key),
                        "duplicate conversation receipt"
                    );
                    self.receipts.insert(key, (fingerprint, message.id.clone()));
                }
                self.entries.push(message);
            }
            Record::Injection { turn_id, memories } => {
                self.injected.insert(turn_id, memories);
            }
            Record::Summary(summary) => {
                ensure!(
                    summary.through <= self.entries.len() as u64,
                    "invalid summary boundary"
                );
                ensure!(
                    self.summary
                        .as_ref()
                        .is_none_or(|old| old.through < summary.through),
                    "nonmonotonic summary"
                );
                self.summary = Some(summary);
            }
        }
        Ok(())
    }
}
