//! Pure host form state. Slint projects this model and only reports edits/results.
use crate::infrastructure::host::{ActionEvent, Feedback, OperationTarget, RequestContext};
use ene_api_client::SessionIdentity;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DraftResource {
    Connection,
    Companion,
    Permission,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftStamp {
    pub id: String,
    pub revision: u64,
}
impl Default for DraftStamp {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 0,
        }
    }
}
#[derive(Default)]
struct Draft {
    stamp: DraftStamp,
    dirty: bool,
}

#[derive(Default)]
pub struct HostUiModel {
    pub session: Option<SessionIdentity>,
    pub companion: String,
    pub connection: String,
    drafts: BTreeMap<(String, DraftResource), Draft>,
    latest: BTreeMap<(String, OperationTarget), String>,
    pub busy: Option<(String, Feedback)>,
    pub feedback: String,
    pub feedback_error: bool,
}
pub struct Reduction {
    pub clear_draft: Option<DraftResource>,
    pub select_connection: Option<String>,
    pub foreground: bool,
    pub same_edit: bool,
}
impl HostUiModel {
    pub fn stamp(&mut self, companion: &str, resource: DraftResource) -> DraftStamp {
        self.drafts
            .entry((companion.into(), resource))
            .or_default()
            .stamp
            .clone()
    }
    pub fn dirty(&self, resource: DraftResource) -> bool {
        self.drafts
            .get(&(self.companion.clone(), resource))
            .is_some_and(|draft| draft.dirty)
    }
    pub fn edit(&mut self, companion: &str, resource: DraftResource) {
        let draft = self.drafts.entry((companion.into(), resource)).or_default();
        if let Some(next) = draft.stamp.revision.checked_add(1) {
            draft.stamp.revision = next;
        } else {
            draft.stamp = DraftStamp::default();
        }
        draft.dirty = true;
    }
    pub fn reset(&mut self, companion: &str, resource: DraftResource) {
        self.drafts
            .insert((companion.into(), resource), Draft::default());
    }
    pub fn set_session(&mut self, session: SessionIdentity) {
        if self.session.as_ref() != Some(&session) {
            self.latest.clear();
            self.busy = None;
        }
        self.session = Some(session);
    }
    pub fn begin(&mut self, context: &RequestContext, feedback: Feedback) {
        self.latest.insert(
            (context.companion.clone(), context.target.clone()),
            context.id.clone(),
        );
        if feedback != Feedback::Quiet {
            self.busy = Some((context.id.clone(), feedback));
            self.feedback = "処理しています…".into();
            self.feedback_error = false;
        }
    }
    pub fn reduce(&mut self, context: &RequestContext, event: &ActionEvent) -> Option<Reduction> {
        if self.session.as_ref() != Some(&context.session)
            || self.companion != context.companion
            || self
                .latest
                .get(&(context.companion.clone(), context.target.clone()))
                != Some(&context.id)
        {
            return None;
        }
        if let OperationTarget::Models(id)
        | OperationTarget::Availability(id)
        | OperationTarget::Credential(id)
        | OperationTarget::Permission(id)
        | OperationTarget::Check(id) = &context.target
            && id != &self.connection
        {
            return None;
        }
        let same_edit = context
            .resource
            .is_none_or(|resource| self.stamp(&context.companion, resource) == context.draft);
        let clear_draft = if matches!(event, ActionEvent::Done(_, true, _)) && same_edit {
            context.saved_resource
        } else {
            None
        };
        if let Some(resource) = clear_draft {
            self.drafts
                .entry((context.companion.clone(), resource))
                .or_default()
                .dirty = false;
        }
        let select_connection = if same_edit {
            match event {
                ActionEvent::Done(_, _, Some(id)) | ActionEvent::ConnectionSaved(id) => {
                    Some(id.clone())
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(id) = &select_connection {
            self.connection = id.clone();
        }
        let foreground = self.busy.as_ref().is_some_and(|(id, _)| id == &context.id);
        if foreground {
            self.busy = None;
            match event {
                ActionEvent::Error(error) => {
                    self.feedback = error.clone();
                    self.feedback_error = true;
                }
                ActionEvent::Done(text, ..) => {
                    self.feedback = text.clone();
                    self.feedback_error = false;
                }
                ActionEvent::Models(..) => {
                    self.feedback = "モデル一覧・情報を取得しました".into();
                    self.feedback_error = false;
                }
                ActionEvent::Older(..) => {
                    self.feedback = "過去の会話を読み込みました。".into();
                    self.feedback_error = false;
                }
                ActionEvent::ConnectionSaved(_) => {
                    self.busy = Some((context.id.clone(), Feedback::Connection));
                }
                ActionEvent::Availability(_) => {}
            }
        }
        Some(Reduction {
            clear_draft,
            select_connection,
            foreground,
            same_edit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(subscription: &str) -> SessionIdentity {
        SessionIdentity {
            host_id: "h".into(),
            epoch: "e".into(),
            subscription: subscription.into(),
        }
    }
    fn request(model: &mut HostUiModel, id: &str, resource: DraftResource) -> RequestContext {
        RequestContext {
            companion: "c".into(),
            draft: model.stamp("c", resource),
            resource: Some(resource),
            saved_resource: Some(resource),
            session: identity("s"),
            id: id.into(),
            target: match resource {
                DraftResource::Connection => OperationTarget::Configuration(Some("p".into())),
                DraftResource::Companion => OperationTarget::Companion,
                DraftResource::Permission => OperationTarget::Permission("p".into()),
            },
        }
    }
    fn model() -> HostUiModel {
        let mut model = HostUiModel {
            companion: "c".into(),
            connection: "p".into(),
            ..Default::default()
        };
        model.set_session(identity("s"));
        model
    }
    #[test]
    fn unrelated_edits_do_not_block_a_save_but_same_resource_edits_do() {
        let mut model = model();
        model.edit("c", DraftResource::Connection);
        let first = request(&mut model, "first", DraftResource::Connection);
        model.begin(&first, Feedback::Connection);
        model.edit("c", DraftResource::Companion);
        assert_eq!(
            model
                .reduce(&first, &ActionEvent::Done("saved".into(), true, None))
                .unwrap()
                .clear_draft,
            Some(DraftResource::Connection)
        );
        assert!(model.dirty(DraftResource::Companion));
        model.edit("c", DraftResource::Connection);
        let second = request(&mut model, "second", DraftResource::Connection);
        model.begin(&second, Feedback::Connection);
        model.edit("c", DraftResource::Connection);
        let reduction = model
            .reduce(
                &second,
                &ActionEvent::Done("saved".into(), true, Some("old".into())),
            )
            .unwrap();
        assert_eq!(reduction.clear_draft, None);
        assert_eq!(reduction.select_connection, None);
        assert!(model.dirty(DraftResource::Connection));
    }
    #[test]
    fn stale_requests_sessions_and_companions_cannot_apply_results() {
        let mut model = model();
        let first = request(&mut model, "first", DraftResource::Companion);
        model.begin(&first, Feedback::Companion);
        let second = request(&mut model, "second", DraftResource::Companion);
        model.begin(&second, Feedback::Companion);
        let result = ActionEvent::Done("saved".into(), true, None);
        assert!(model.reduce(&first, &result).is_none());
        model.companion = "other".into();
        assert!(model.reduce(&second, &result).is_none());
        model.companion = "c".into();
        model.set_session(identity("reconnected"));
        assert!(model.reduce(&second, &result).is_none());
    }
    #[test]
    fn credential_completion_cannot_confirm_unsaved_connection_fields() {
        let mut model = model();
        model.edit("c", DraftResource::Connection);
        let mut context = request(&mut model, "credential", DraftResource::Connection);
        context.target = OperationTarget::Credential("p".into());
        context.saved_resource = None;
        model.begin(&context, Feedback::General);
        assert_eq!(
            model
                .reduce(&context, &ActionEvent::Done("key saved".into(), true, None))
                .unwrap()
                .clear_draft,
            None
        );
        assert!(model.dirty(DraftResource::Connection));
    }
    #[test]
    fn draft_revision_overflow_uses_a_new_identity_without_aba() {
        let mut model = model();
        let draft = model
            .drafts
            .entry(("c".into(), DraftResource::Connection))
            .or_default();
        draft.stamp.revision = u64::MAX;
        let original = draft.stamp.clone();
        model.edit("c", DraftResource::Connection);
        let next = model.stamp("c", DraftResource::Connection);
        assert_ne!(next.id, original.id);
        assert_eq!(next.revision, 0);
    }
}
