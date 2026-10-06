use super::*;

impl ene_core::ports::Repository for Store {
    fn snapshot(
        &self,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<StoredSnapshot>> {
        Box::pin(async move { self.snapshot().await.map_err(core_error) })
    }
    fn messages(
        &self,
        c: String,
        cursor: Option<String>,
        limit: usize,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<Page<Message>>> {
        Box::pin(async move { self.messages(c, cursor, limit).await.map_err(core_error) })
    }
    fn message(
        &self,
        c: String,
        id: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<Option<Message>>> {
        Box::pin(async move { self.message(c, id).await.map_err(core_error) })
    }
}
fn core_error(error: Error) -> ene_core::domain::Error {
    use ene_core::domain::{Error as CoreError, ErrorKind};
    match error {
        Error::InvalidCursor => CoreError::invalid(),
        Error::ResourceLimit | Error::OversizedMessage => CoreError::new(ErrorKind::ResourceLimit),
        _ => CoreError::new(ErrorKind::Unavailable),
    }
}
