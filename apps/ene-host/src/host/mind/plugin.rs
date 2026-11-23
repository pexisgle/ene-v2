//! A provider plugin as a rig model.
//!
//! The host keeps rig's catalog path for the providers rig ships. A provider
//! that is not in the catalog reaches the agent loop through the same
//! [`DynModel`] boundary: a [`Wire`] encodes the request, and a [`Transport`]
//! that runs the plugin's `ene_invoke` delivers the reply. Everything above
//! the transport (the driver, the fold, telemetry, error enrichment) is rig's,
//! so a plugin model behaves like a built-in one.
use super::super::providers::{ProviderInvocationContext, Running};
use ene_plugin::{EmbeddingRequest, Operation as PluginOperation};
use futures_util::stream;
use rig_core::{
    DynModel,
    driver::{Exchange, Opened, Opening, Transport},
    embeddings::{Embedding, EmbeddingResponse},
    error::{EncodeError, ProviderError},
    operation::Embedding as EmbeddingOp,
    wire::{Descriptor, Json, Mode, Wire, WireFrame, document::Unreassembled},
};
use std::sync::Arc;

/// What the transport carries: the texts to embed and the width asked for.
pub(crate) struct EmbedPayload {
    inputs: Vec<String>,
    dimensions: usize,
}

/// The embedding wire of one provider plugin.
#[derive(Clone)]
pub(crate) struct PluginEmbedding {
    /// The vendor this embedding model files under. Held rather than read back
    /// from `provider`, whose role is optional for the harness case.
    vendor: String,
    model: String,
    dimensions: usize,
}

/// A plugin-backed embedding model, erased to rig's operation.
pub(crate) fn embedding(
    provider: Arc<Running>,
    vendor: String,
    context: ProviderInvocationContext,
    model: String,
    dimensions: usize,
) -> DynModel<EmbeddingOp> {
    let wire = PluginEmbedding {
        vendor: vendor.clone(),
        model: model.clone(),
        dimensions,
    };
    let transport = PluginTransport {
        provider,
        vendor,
        context,
        model,
    };
    rig_core::Model::new(wire, transport).erase()
}

impl Wire for PluginEmbedding {
    type Op = EmbeddingOp;
    type Payload = EmbedPayload;
    type Frame = WireFrame;
    type Event = (EmbeddingResponse, serde_json::Value);
    type Decoder<'id> = Json;
    type Reassembler = Unreassembled;

    fn describe(&self) -> Descriptor<'_> {
        Descriptor::new(self.vendor.as_str()).model(self.model.as_str())
    }

    fn encode(&self, request: Vec<String>, _mode: Mode) -> Result<EmbedPayload, EncodeError> {
        Ok(EmbedPayload {
            inputs: request,
            dimensions: self.dimensions,
        })
    }

    fn decoder<'id>(&self) -> Self::Decoder<'id> {
        Json
    }
}

/// Sends one embedding request to the plugin.
#[derive(Clone)]
pub(crate) struct PluginTransport {
    provider: Arc<Running>,
    /// The vendor the reply is filed under.
    vendor: String,
    context: ProviderInvocationContext,
    model: String,
}

impl Transport<PluginEmbedding> for PluginTransport {
    fn send(&self, payload: EmbedPayload, _exchange: Exchange) -> Opening<WireFrame> {
        let provider = Arc::clone(&self.provider);
        let vendor = self.vendor.clone();
        let context = self.context.clone();
        let model = self.model.clone();
        Opening::new(async move {
            let request = EmbeddingRequest {
                endpoint: context.endpoint.clone(),
                options: context.options.clone(),
                model,
                inputs: payload.inputs,
                dimensions: payload.dimensions,
            };
            let reply = provider
                .invoke(PluginOperation::Embed { request }, context.services())
                .await
                .map_err(|error| ProviderError::Response(error.to_string()))?;
            let vectors: Vec<Vec<f32>> = reply
                .get("vectors")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .ok_or_else(|| ProviderError::Response("the plugin returned no vectors".into()))?;
            let embeddings = vectors
                .into_iter()
                .map(|vector| Embedding {
                    document: String::new(),
                    vec: vector.into_iter().map(f64::from).collect(),
                })
                .collect();
            let response = EmbeddingResponse {
                embeddings,
                usage: Default::default(),
                provider: vendor,
                model: None,
                response_id: None,
                provider_request_id: None,
                raw: reply,
            };
            Ok(Opened::new(stream::once(async move {
                Ok(WireFrame::Bytes(
                    serde_json::to_vec(&response).unwrap_or_default(),
                ))
            })))
        })
    }
}
