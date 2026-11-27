//! The read paths: a companion's work sessions and one session's view.
use super::{require_companion, require_session};
use crate::host::{
    db::{Result, Store},
    wire,
};
use ene_domain::WorkSession;
use ene_rpc as w;

pub(crate) fn list(store: &Store, companion: &str) -> Result<w::WorkViews> {
    require_companion(store, companion)?;
    let items = store
        .work_repo()
        .work_sessions(companion)?
        .iter()
        .map(|session| view(store, session))
        .collect::<Result<_>>()?;
    Ok(w::WorkViews {
        items,
        ..Default::default()
    })
}

pub(crate) fn get(store: &Store, work_id: &str) -> Result<w::WorkView> {
    view(store, &require_session(store, work_id)?)
}

fn view(store: &Store, session: &WorkSession) -> Result<w::WorkView> {
    Ok(wire::work_view(
        session,
        &store.work_repo().runs(&session.id)?,
    ))
}
