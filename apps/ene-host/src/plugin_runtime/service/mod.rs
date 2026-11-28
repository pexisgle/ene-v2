//! Host services a plugin asks for, implemented once for every provider.
pub mod http;

pub use http::{DenyHttp, HttpBroker, HttpGrants};
