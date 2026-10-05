use ene_core::application::HostApplication;
use std::sync::Arc;
#[derive(Clone)]
pub struct App {
    pub application: HostApplication,
    pub(crate) token: Arc<str>,
}
impl App {
    pub fn new(application: HostApplication, token: String) -> Self {
        Self {
            application,
            token: token.into(),
        }
    }
}
