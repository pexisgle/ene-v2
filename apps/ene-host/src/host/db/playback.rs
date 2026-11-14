//! Playback progress: where each utterance actually stopped
//! (docs/design/voice-avatar.md §5.3).
//!
//! A client reports `PLAYBACK_COMPLETED`, `PLAYBACK_INTERRUPTED` or
//! `PLAYBACK_FAILED` once it knows how much of an utterance was heard. The row
//! keeps the sentence numbers and PCM bytes the client confirmed, so an
//! interrupted or failed position is recorded instead of being mistaken for a
//! fully delivered utterance.
#[cfg(test)]
use super::enum_column;
use super::{Result, Store, now, or_stamp, to_json};
use ene_rpc as w;
#[cfg(test)]
use rusqlite::Row;
use rusqlite::params;
#[cfg(test)]
use serde_json::Value;

/// The terminal playback states the host records. A report without a known
/// state is not acknowledged and is not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackStatus {
    Completed,
    Interrupted,
    Failed,
}

impl PlaybackStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }

    #[cfg(test)]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "completed" => Some(Self::Completed),
            "interrupted" => Some(Self::Interrupted),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub(crate) fn from_wire(status: w::playback_status_report::Status) -> Option<Self> {
        match status {
            w::playback_status_report::Status::PLAYBACK_COMPLETED => Some(Self::Completed),
            w::playback_status_report::Status::PLAYBACK_INTERRUPTED => Some(Self::Interrupted),
            w::playback_status_report::Status::PLAYBACK_FAILED => Some(Self::Failed),
            w::playback_status_report::Status::STATUS_UNSPECIFIED => None,
        }
    }
}

/// One accepted `ReportPlaybackStatus`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackReport {
    pub(crate) utterance_id: String,
    pub(crate) generation_id: u64,
    pub(crate) status: PlaybackStatus,
    pub(crate) completed_sentence_count: u32,
    pub(crate) played_sequence_nos: Vec<u32>,
    pub(crate) skipped_sequence_nos: Vec<u32>,
    pub(crate) completed_pcm_bytes: u64,
    pub(crate) client_id: String,
    pub(crate) activation_revision: u64,
    /// Empty when the caller did not stamp it; the insert fills in `now()`.
    pub(crate) received_at: String,
}

impl PlaybackReport {
    /// The report for a validated wire status. Unknown states yield `None`, as
    /// the host never acknowledges them.
    pub(crate) fn from_wire(report: &w::PlaybackStatusReport) -> Option<Self> {
        let status = report
            .status
            .as_known()
            .and_then(PlaybackStatus::from_wire)?;
        Some(Self {
            utterance_id: report.utterance_id.clone(),
            generation_id: report.generation_id,
            status,
            completed_sentence_count: report.completed_sentence_count,
            played_sequence_nos: report.played_sequence_nos.clone(),
            skipped_sequence_nos: report.skipped_sequence_nos.clone(),
            completed_pcm_bytes: report.completed_pcm_bytes,
            client_id: report.client_id.clone(),
            activation_revision: report.activation_revision,
            received_at: String::new(),
        })
    }
}

#[cfg(test)]
fn row_to_report(row: &Row<'_>) -> rusqlite::Result<PlaybackReport> {
    let status = enum_column(
        row,
        "status",
        "unknown playback status",
        PlaybackStatus::parse,
    )?;
    let sequences = |column: &str, what: &str| -> rusqlite::Result<Vec<u32>> {
        let value: Value = super::json_column(row, column, what)?;
        value
            .as_array()
            .ok_or_else(|| super::corrupt(&value.to_string(), what))?
            .iter()
            .map(|number| {
                number
                    .as_u64()
                    .and_then(|number| u32::try_from(number).ok())
                    .ok_or_else(|| super::corrupt(&value.to_string(), what))
            })
            .collect()
    };
    Ok(PlaybackReport {
        utterance_id: row.get("utterance_id")?,
        generation_id: row.get("generation_id")?,
        status,
        completed_sentence_count: row.get("completed_sentence_count")?,
        played_sequence_nos: sequences("played_sequence_nos", "played sequence numbers")?,
        skipped_sequence_nos: sequences("skipped_sequence_nos", "skipped sequence numbers")?,
        completed_pcm_bytes: row.get("completed_pcm_bytes")?,
        client_id: row.get("client_id")?,
        activation_revision: row.get("activation_revision")?,
        received_at: row.get("received_at")?,
    })
}

const COLUMNS: &str = "utterance_id, generation_id, status, completed_sentence_count, \
     played_sequence_nos, skipped_sequence_nos, completed_pcm_bytes, client_id, \
     activation_revision, received_at";

impl Store {
    /// Appends one accepted report. Reports are append-only: the same utterance
    /// may report an interruption and a later completion.
    pub(crate) fn insert_playback_report(&self, report: &PlaybackReport) -> Result<()> {
        let received_at = or_stamp(&report.received_at, &now());
        self.conn()?.execute(
            &format!(
                "INSERT INTO playback_reports ({COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
            ),
            params![
                report.utterance_id,
                report.generation_id,
                report.status.as_str(),
                report.completed_sentence_count,
                to_json(&report.played_sequence_nos)?,
                to_json(&report.skipped_sequence_nos)?,
                report.completed_pcm_bytes,
                report.client_id,
                report.activation_revision,
                received_at,
            ],
        )?;
        Ok(())
    }

    /// The reports for one utterance, oldest first.
    #[cfg(test)]
    pub(crate) fn playback_reports(&self, utterance_id: &str) -> Result<Vec<PlaybackReport>> {
        self.fetch_all(
            &format!(
                "SELECT {COLUMNS} FROM playback_reports WHERE utterance_id = ?1 ORDER BY rowid"
            ),
            [utterance_id],
            row_to_report,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(status: w::playback_status_report::Status) -> w::PlaybackStatusReport {
        w::PlaybackStatusReport {
            utterance_id: "u1".into(),
            generation_id: 7,
            status: status.into(),
            completed_sentence_count: 2,
            played_sequence_nos: vec![0, 1],
            skipped_sequence_nos: vec![2],
            completed_pcm_bytes: 4_096,
            client_id: "c1".into(),
            activation_revision: 3,
            ..Default::default()
        }
    }

    #[test]
    fn playback_reports_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let interrupted = PlaybackReport::from_wire(&wire(
            w::playback_status_report::Status::PLAYBACK_INTERRUPTED,
        ))
        .unwrap();
        assert_eq!(interrupted.status, PlaybackStatus::Interrupted);
        let received_at = now();
        store
            .insert_playback_report(&PlaybackReport {
                received_at: received_at.clone(),
                ..interrupted.clone()
            })
            .unwrap();
        let stored = store.playback_reports("u1").unwrap();
        assert_eq!(
            stored,
            vec![PlaybackReport {
                received_at,
                ..interrupted
            }]
        );

        // A later failure of the same utterance is its own row, not an update.
        let failed =
            PlaybackReport::from_wire(&wire(w::playback_status_report::Status::PLAYBACK_FAILED))
                .unwrap();
        store.insert_playback_report(&failed).unwrap();
        let stored = store.playback_reports("u1").unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].status, PlaybackStatus::Failed);
        assert!(!stored[1].received_at.is_empty());
        assert!(store.playback_reports("u2").unwrap().is_empty());
    }

    #[test]
    fn unknown_statuses_are_not_recorded() {
        assert!(
            PlaybackReport::from_wire(&wire(w::playback_status_report::Status::STATUS_UNSPECIFIED))
                .is_none()
        );
        assert_eq!(
            PlaybackStatus::parse("interrupted"),
            Some(PlaybackStatus::Interrupted)
        );
        assert_eq!(PlaybackStatus::parse("unknown"), None);
    }
}
