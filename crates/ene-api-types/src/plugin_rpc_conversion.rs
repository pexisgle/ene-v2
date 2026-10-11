//! Explicit protobuf envelopes around plugin identities and extension payloads.
use crate::{
    plugin as p, rpc as w,
    rpc_conversion::{ConversionError, RpcConvert},
};
use serde::{Serialize, de::DeserializeOwned};

fn name<T: Serialize>(value: T) -> Result<String, ConversionError> {
    serde_json::from_value(serde_json::to_value(value)?)
}
fn parse<T: DeserializeOwned>(value: String) -> Result<T, ConversionError> {
    serde_json::from_value(serde_json::Value::String(value))
}
fn invalid() -> ConversionError {
    crate::rpc_conversion::invalid()
}

impl RpcConvert for p::EntryInfo {
    type Wire = w::PluginEntry;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::PluginEntry {
            id: self.id,
            target: name(self.target)?,
            runtime: name(self.runtime)?,
            provides: self
                .provides
                .into_iter()
                .map(name)
                .collect::<Result<_, _>>()?,
            uses: self.uses.into_iter().map(name).collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    fn from_wire(v: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: v.id,
            target: parse(v.target)?,
            runtime: parse(v.runtime)?,
            provides: v
                .provides
                .into_iter()
                .map(parse)
                .collect::<Result<_, _>>()?,
            uses: v.uses.into_iter().map(parse).collect::<Result<_, _>>()?,
        })
    }
}
impl RpcConvert for p::PluginInfo {
    type Wire = w::Plugin;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Plugin {
            id: self.id,
            name: self.name,
            version: self.version,
            revision: self.revision,
            state: self.state,
            error: self.error,
            entrypoints: self
                .entrypoints
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    fn from_wire(v: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: v.id,
            name: v.name,
            version: v.version,
            revision: v.revision,
            state: v.state,
            error: v.error,
            entrypoints: v
                .entrypoints
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
        })
    }
}
impl RpcConvert for p::PluginBinding {
    type Wire = w::PluginBinding;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::PluginBinding {
            id: self.id,
            plugin_id: self.plugin_id,
            entrypoint: self.entrypoint,
            revision: self.revision,
            companion_id: self.companion_id,
            services: self
                .services
                .into_iter()
                .map(name)
                .collect::<Result<_, _>>()?,
            configuration: self.configuration.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(v: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: v.id,
            plugin_id: v.plugin_id,
            entrypoint: v.entrypoint,
            revision: v.revision,
            companion_id: v.companion_id,
            services: v
                .services
                .into_iter()
                .map(parse)
                .collect::<Result<_, _>>()?,
            configuration: RpcConvert::from_wire(
                v.configuration.into_option().ok_or_else(invalid)?,
            )?,
        })
    }
}
impl RpcConvert for p::PluginInvocation {
    type Wire = w::PluginInvocation;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::PluginInvocation {
            binding_id: self.binding_id,
            operation: self.operation.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(v: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            binding_id: v.binding_id,
            operation: RpcConvert::from_wire(v.operation.into_option().ok_or_else(invalid)?)?,
        })
    }
}
impl RpcConvert for p::PluginResult {
    type Wire = w::PluginResult;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::PluginResult {
            generation: self.generation,
            events: self
                .events
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            result: self.result.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(v: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            generation: v.generation,
            events: v
                .events
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
            result: RpcConvert::from_wire(v.result.into_option().ok_or_else(invalid)?)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plugin_result_preserves_large_generations_and_nested_extension_values() {
        let result = p::PluginResult {
            generation: u64::MAX,
            events: vec![
                serde_json::json!({"type":"character","action":{"type":"speak","text":"日本語🙂"}}),
            ],
            result: serde_json::json!({"count":u64::MAX}),
        };
        let restored = p::PluginResult::from_wire(result.clone().into_wire().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(result).unwrap()
        );
    }
    #[test]
    fn unknown_extension_and_missing_payload_are_rejected() {
        assert!(
            p::EntryInfo::from_wire(w::PluginEntry {
                target: "host".into(),
                runtime: "luau".into(),
                provides: vec!["future_extension".into()],
                ..Default::default()
            })
            .is_err()
        );
        assert!(p::PluginInvocation::from_wire(w::PluginInvocation::default()).is_err());
    }
}
