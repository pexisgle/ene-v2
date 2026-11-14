//! Approval requests: persistence, resolution and startup expiry.
use super::{Result, Store, enum_column, now};
use crate::host::error::HostError;
use crate::host::permission::{Approval, ApprovalStatus};
use rusqlite::{Row, params};

fn row_to_approval(row: &Row<'_>) -> rusqlite::Result<Approval> {
    let status = enum_column(
        row,
        "status",
        "unknown approval status",
        ApprovalStatus::parse,
    )?;
    Ok(Approval {
        request_id: row.get("request_id")?,
        session_id: row.get("session_id")?,
        run_id: row.get("run_id")?,
        work_id: row.get("work_id")?,
        action_payload_hash: row.get("action_payload_hash")?,
        tool_name: row.get("tool_name")?,
        normalized_arguments: row.get("normalized_arguments")?,
        status,
        created_at: row.get("created_at")?,
        resolved_at: row.get("resolved_at")?,
        client_id: row.get("client_id")?,
        activation_revision: row.get::<_, Option<u64>>("activation_revision")?,
        capability: row.get("capability")?,
    })
}

const COLUMNS: &str = "request_id, session_id, run_id, work_id, action_payload_hash, tool_name, \
     normalized_arguments, status, created_at, resolved_at, client_id, activation_revision, \
     capability";

impl Store {
    pub(crate) fn insert_approval(&self, approval: &Approval) -> Result<()> {
        self.conn()?.execute(
            &format!(
                "INSERT INTO approval_requests ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ),
            params![
                approval.request_id,
                approval.session_id,
                approval.run_id,
                approval.work_id,
                approval.action_payload_hash,
                approval.tool_name,
                approval.normalized_arguments,
                approval.status.as_str(),
                approval.created_at,
                approval.resolved_at,
                approval.client_id,
                approval.activation_revision,
                approval.capability,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn approval(&self, request_id: &str) -> Result<Option<Approval>> {
        self.fetch(
            &format!("SELECT {COLUMNS} FROM approval_requests WHERE request_id = ?1"),
            [request_id],
            row_to_approval,
        )
    }

    pub(crate) fn approvals(&self, status: ApprovalStatus) -> Result<Vec<Approval>> {
        self.fetch_all(
            &format!(
                "SELECT {COLUMNS} FROM approval_requests WHERE status = ?1 ORDER BY created_at"
            ),
            [status.as_str()],
            row_to_approval,
        )
    }

    /// Resolves a pending approval. Resolving it twice is a conflict, never a
    /// second execution.
    pub(crate) fn resolve_approval(
        &self,
        request_id: &str,
        status: ApprovalStatus,
    ) -> Result<Approval> {
        let resolved = now();
        self.fetch(
            &format!(
                "UPDATE approval_requests SET status = ?2, resolved_at = ?3 \
                 WHERE request_id = ?1 AND status = 'pending' RETURNING {COLUMNS}"
            ),
            params![request_id, status.as_str(), resolved],
            row_to_approval,
        )?
        .ok_or_else(|| HostError::conflict("approval is missing or no longer pending"))
    }

    /// Startup: an approval whose call cannot be resolved is expired rather
    /// than left pending forever.
    pub(crate) fn expire_pending_approvals(&self) -> Result<usize> {
        let expired = self.conn()?.execute(
            "UPDATE approval_requests SET status = 'expired', resolved_at = ?1 \
             WHERE status = 'pending'",
            [now()],
        )?;
        Ok(expired)
    }

    /// Expires only the pending approvals of one companion.
    pub(crate) fn expire_approvals_for(&self, companion: &str) -> Result<usize> {
        let expired = self.conn()?.execute(
            "UPDATE approval_requests SET status = 'expired', resolved_at = ?2 \
             WHERE status = 'pending' AND session_id = ?1",
            params![companion, now()],
        )?;
        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::permission::ApprovalStatus;

    fn approval(id: &str) -> Approval {
        Approval {
            request_id: id.into(),
            session_id: "ene".into(),
            run_id: "run".into(),
            work_id: Some("work-1".into()),
            action_payload_hash: vec![1, 2, 3],
            tool_name: "device_request".into(),
            normalized_arguments: "{\"capability\":\"x\"}".into(),
            status: ApprovalStatus::Pending,
            created_at: now(),
            resolved_at: None,
            client_id: Some("c1".into()),
            activation_revision: Some(4),
            capability: Some("device.x".into()),
        }
    }

    #[test]
    fn approvals_resolve_once_and_expire_on_startup() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        store.insert_approval(&approval("a1")).unwrap();
        store.insert_approval(&approval("a2")).unwrap();
        assert_eq!(store.approvals(ApprovalStatus::Pending).unwrap().len(), 2);
        // The work session is stored with the row; a head approval stores none.
        assert_eq!(
            store.approval("a1").unwrap().unwrap().work_id.as_deref(),
            Some("work-1")
        );
        store
            .insert_approval(&Approval {
                work_id: None,
                ..approval("a3")
            })
            .unwrap();
        assert_eq!(
            store.approval("a3").unwrap().unwrap().work_id,
            None,
            "a head approval may not carry a work session"
        );
        let resolved = store
            .resolve_approval("a1", ApprovalStatus::Approved)
            .unwrap();
        assert_eq!(resolved.status, ApprovalStatus::Approved);
        assert!(resolved.resolved_at.is_some());
        assert!(
            store
                .resolve_approval("a1", ApprovalStatus::Denied)
                .is_err()
        );
        assert_eq!(store.expire_pending_approvals().unwrap(), 2);
        assert!(store.approvals(ApprovalStatus::Pending).unwrap().is_empty());
        assert_eq!(store.approvals(ApprovalStatus::Expired).unwrap().len(), 2);
    }
}
