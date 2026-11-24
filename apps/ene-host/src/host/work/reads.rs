//! The read paths: a companion's work sessions and one session's view.
use super::{require_companion, require_session};
use crate::host::{
    db::{Result, Store},
    wire,
};
use ene_rpc as w;

pub(crate) fn list(store: &Store, companion: &str) -> Result<w::WorkViews> {
    require_companion(store, companion)?;
    let mut items = Vec::new();
    for session in store.work_repo().work_sessions(companion)? {
        items.push(view(store, &session.id)?);
    }
    Ok(w::WorkViews {
        items,
        ..Default::default()
    })
}

pub(crate) fn get(store: &Store, work_id: &str) -> Result<w::WorkView> {
    view(store, work_id)
}

fn view(store: &Store, work_id: &str) -> Result<w::WorkView> {
    let session = require_session(store, work_id)?;
    let runs = store.work_repo().runs(work_id)?;
    Ok(wire::work_view(&session, &runs))
}
