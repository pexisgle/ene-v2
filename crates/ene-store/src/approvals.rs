//! Approval requests: persistence, resolution and startup expiry.

use crate::row::{FromRow, row_fields};
use crate::{Store, StoreError, error::Result, util::now};
use ene_policy::{Approval, ApprovalStatus};
use rusqlite::params;

row_fields!(Approval, COLUMNS {
    request_id,
    session_id,
    run_id,
    work_id,
    action_payload_hash,
    tool_name,
    normalized_arguments,
    status => enum_name_column("unknown approval status"),
    created_at,
    resolved_at,
    client_id,
    capability,
    external_destination,
});

/// Repository managing `approval_requests`.
#[derive(Clone, Copy, Debug)]
pub struct ApprovalRepo<'a>(&'a Store);

impl<'a> ApprovalRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn insert_approval(&self, approval: &Approval) -> Result<()> {
        self.0
            .conn()
            .prepare_cached(&format!(
                "INSERT INTO approval_requests ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ))?
            .execute(params![
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
                approval.capability,
                approval.external_destination,
            ])?;
        Ok(())
    }

    pub fn approval(&self, request_id: &str) -> Result<Option<Approval>> {
        self.0.fetch(
            &format!("SELECT {COLUMNS} FROM approval_requests WHERE request_id = ?1"),
            [request_id],
            Approval::from_row,
        )
    }

    pub fn approvals(&self, status: ApprovalStatus) -> Result<Vec<Approval>> {
        self.0.fetch_all(
            &format!(
                "SELECT {COLUMNS} FROM approval_requests WHERE status = ?1 ORDER BY created_at"
            ),
            [status.as_str()],
            Approval::from_row,
        )
    }

    /// Moves an automatic evaluation to the user-visible approval queue.
    pub fn request_user_approval(&self, request_id: &str) -> Result<Option<Approval>> {
        self.0.fetch(
            &format!(
                "UPDATE approval_requests SET status = 'pending'
                 WHERE request_id = ?1 AND status = 'evaluating' RETURNING {COLUMNS}"
            ),
            [request_id],
            Approval::from_row,
        )
    }

    /// Resolves a pending approval. Resolving it twice is a conflict, never a
    /// second execution.
    pub fn resolve_approval(&self, request_id: &str, status: ApprovalStatus) -> Result<Approval> {
        self.resolve_approval_with_client(request_id, status, None)
    }

    pub fn resolve_approval_with_client(
        &self,
        request_id: &str,
        status: ApprovalStatus,
        client_id: Option<&str>,
    ) -> Result<Approval> {
        let resolved = now();
        self.0
            .fetch(
                &format!(
                    "UPDATE approval_requests SET status = ?2, resolved_at = ?3, \
                     client_id = COALESCE(?4, client_id) \
                     WHERE request_id = ?1 AND status IN ('pending','evaluating') RETURNING {COLUMNS}"
                ),
                params![request_id, status.as_str(), resolved, client_id],
                Approval::from_row,
            )?
            .ok_or_else(|| StoreError::Conflict("approval is missing or no longer pending".into()))
    }

    /// Startup: an approval whose call cannot be resolved is expired rather
    /// than left pending forever.
    pub fn expire_pending_approvals(&self) -> Result<usize> {
        let expired = self
            .0
            .conn()
            .prepare_cached(
                "UPDATE approval_requests SET status = 'expired', resolved_at = ?1 \
                 WHERE status IN ('pending','evaluating')",
            )?
            .execute([now()])?;
        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            capability: Some("device.x".into()),
            external_destination: None,
        }
    }

    #[test]
    fn approvals_resolve_once_and_expire_on_startup() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.approval_repo();
        repo.insert_approval(&approval("a1")).unwrap();
        repo.insert_approval(&approval("a2")).unwrap();
        assert_eq!(repo.approvals(ApprovalStatus::Pending).unwrap().len(), 2);
        assert_eq!(
            repo.approval("a1").unwrap().unwrap().work_id.as_deref(),
            Some("work-1")
        );
        repo.insert_approval(&Approval {
            work_id: None,
            ..approval("a3")
        })
        .unwrap();
        assert_eq!(
            repo.approval("a3").unwrap().unwrap().work_id,
            None,
            "a head approval may not carry a work session"
        );
        let resolved = repo
            .resolve_approval("a1", ApprovalStatus::Approved)
            .unwrap();
        assert_eq!(resolved.status, ApprovalStatus::Approved);
        assert!(resolved.resolved_at.is_some());
        assert!(repo.resolve_approval("a1", ApprovalStatus::Denied).is_err());
        assert_eq!(repo.expire_pending_approvals().unwrap(), 2);
        assert_eq!(repo.approvals(ApprovalStatus::Pending).unwrap().len(), 0);
        assert_eq!(repo.approvals(ApprovalStatus::Expired).unwrap().len(), 2);
    }
}
