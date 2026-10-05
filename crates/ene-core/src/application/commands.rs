//! Admitted commands belong to the host, including their final writes and notifications.
use super::HostApplication;
use crate::domain::{Error, ErrorKind};
use std::future::Future;

impl HostApplication {
    /// Dropping the caller only drops its reply. Shutdown drains every admitted command.
    pub async fn execute<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> Result<T, Error> {
        let task = {
            // Admission and shutdown use the same lock: a closed tracker cannot
            // acquire new work after shutdown has observed it as empty.
            let _controls = self.0.controls.lock().await;
            if *self.0.shutdown.borrow() || self.0.commands.is_closed() {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            self.0.commands.spawn(operation)
        };
        task.await.map_err(|_| Error::new(ErrorKind::Unavailable))?
    }

    pub async fn finish_commands(&self) {
        {
            let _controls = self.0.controls.lock().await;
            self.0.commands.close();
        }
        self.0.commands.wait().await;
    }
}
