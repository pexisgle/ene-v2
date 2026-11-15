//! The host services a provider plugin may call.
//!
//! A provider reaches its endpoint through the host's broker, never directly,
//! so the credential stays here and every transport guard applies. This is the
//! one implementation of that boundary: the speech and completion adapters
//! both hand it to their instance.
use ene_plugin::{Failure, ServiceRequest};
use ene_plugin_runtime::{
    instance::Services,
    service::{HttpBroker, HttpGrants},
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn http_broker(grants: Arc<dyn HttpGrants>) -> Result<Arc<HttpBroker>, reqwest::Error> {
    #[cfg(test)]
    if construction_failure::active() {
        // A real builder error without depending on TLS or process-wide state.
        reqwest::Client::builder().user_agent("\n").build()?;
    }
    HttpBroker::new(grants).map(Arc::new)
}

/// The services a provider call declares: the broker, and nothing else.
pub(crate) struct BrokerServices {
    broker: Arc<HttpBroker>,
    provider: String,
}

impl BrokerServices {
    /// Serves `provider`'s exchanges through `broker`.
    pub(crate) fn new(broker: Arc<HttpBroker>, provider: String) -> Arc<Self> {
        Arc::new(Self { broker, provider })
    }
}

impl Services for BrokerServices {
    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        let broker = Arc::clone(&self.broker);
        let provider = self.provider.clone();
        Box::pin(async move {
            Ok(match request {
                ServiceRequest::HttpOpen { request } => {
                    serde_json::to_value(broker.open(&provider, request).await?)
                        .map_err(|_| Failure::new("serialize"))?
                }
                ServiceRequest::HttpRead { handle } => serde_json::to_value(broker.read(&handle)?)
                    .map_err(|_| Failure::new("serialize"))?,
                ServiceRequest::HttpClose { handle } => {
                    broker.close(&handle)?;
                    Value::Null
                }
                // Audio is read by the host from its own synthesis answer, not
                // by the guest, so a plugin never asks for it.
                _ => return Err(Failure::new("undeclared_service")),
            })
        })
    }

    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
pub(crate) mod construction_failure {
    use std::{cell::Cell, marker::PhantomData, rc::Rc};

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn active() -> bool {
        ACTIVE.get()
    }

    pub(crate) struct Guard {
        previous: bool,
        _thread: PhantomData<Rc<()>>,
    }

    impl Guard {
        pub(crate) fn enter() -> Self {
            Self {
                previous: ACTIVE.replace(true),
                _thread: PhantomData,
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE.set(self.previous);
        }
    }
}
