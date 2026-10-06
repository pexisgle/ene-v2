use ene_core::application::HostApplication;
use std::sync::Arc;
use tokio::sync::watch;
#[derive(Clone)]
pub struct App {
    pub application: HostApplication,
    pub(crate) token: Arc<str>,
    pub shutdown: watch::Sender<bool>,
}
impl App {
    pub fn new(application: HostApplication, token: String, shutdown: watch::Sender<bool>) -> Self {
        Self {
            application,
            token: token.into(),
            shutdown,
        }
    }
}
