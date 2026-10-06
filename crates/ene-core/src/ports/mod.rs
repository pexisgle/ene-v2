use crate::domain::{Error, Message, Page, StoredSnapshot};
use futures_util::future::BoxFuture;
pub type Result<T> = std::result::Result<T, Error>;
pub trait Repository: Send + Sync {
    fn snapshot(&self) -> BoxFuture<'_, Result<StoredSnapshot>>;
    fn messages(
        &self,
        companion: String,
        cursor: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Page<Message>>>;
    fn message(&self, companion: String, id: String) -> BoxFuture<'_, Result<Option<Message>>>;
}
