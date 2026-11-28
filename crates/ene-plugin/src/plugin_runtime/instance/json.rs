use crate::plugin_runtime::Result;
use extism::convert::{FromBytesOwned, Json, ToBytes};
use serde::{Serialize, de::DeserializeOwned};

/// JSON with a limit on the actual ABI bytes, before decoding or after encoding.
pub(super) struct BoundedJson<T, const LIMIT: usize>(pub T);

impl<T: DeserializeOwned, const LIMIT: usize> FromBytesOwned for BoundedJson<T, LIMIT> {
    fn from_bytes_owned(raw: &[u8]) -> std::result::Result<Self, extism::Error> {
        if raw.len() > LIMIT {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
        }
        Ok(Self(Json::<T>::from_bytes_owned(raw)?.0))
    }
}

impl<T: Serialize, const LIMIT: usize> ToBytes<'_> for BoundedJson<T, LIMIT> {
    type Bytes = Vec<u8>;

    fn to_bytes(&self) -> std::result::Result<Vec<u8>, extism::Error> {
        let bytes = Json(&self.0).to_bytes()?;
        if bytes.len() > LIMIT {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
        }
        Ok(bytes)
    }
}

pub(super) fn encode<T: Serialize, const LIMIT: usize>(value: T) -> Result<Vec<u8>> {
    BoundedJson::<_, LIMIT>(value).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::limits::{MAX_CHUNK, MAX_MESSAGE_BYTES};
    use serde_json::{Value, json};

    fn decode<const LIMIT: usize>(raw: &[u8]) -> Result<Value> {
        BoundedJson::<Value, LIMIT>::from_bytes_owned(raw).map(|value| value.0)
    }

    #[test]
    fn raw_limits_precede_json_decoding() {
        for limit in [MAX_CHUNK, MAX_MESSAGE_BYTES] {
            let mut raw = b"{}".to_vec();
            raw.resize(limit, b' ');
            if limit == MAX_CHUNK {
                assert_eq!(decode::<MAX_CHUNK>(&raw).unwrap(), json!({}));
                raw.push(b' ');
                assert!(
                    (decode::<MAX_CHUNK>(&raw))
                        .is_err_and(|error| error.to_string() == "invalid_plugin_contract")
                );
            } else {
                assert_eq!(decode::<MAX_MESSAGE_BYTES>(&raw).unwrap(), json!({}));
                raw.push(b' ');
                assert!(
                    (decode::<MAX_MESSAGE_BYTES>(&raw))
                        .is_err_and(|error| error.to_string() == "invalid_plugin_contract")
                );
            }
        }
        assert!(
            (decode::<2>(b"bad"))
                .is_err_and(|error| error.to_string() == "invalid_plugin_contract")
        );
        assert!(decode::<3>(b"bad").is_err_and(|error| error.is::<serde_json::Error>()));
        assert_eq!(decode::<5>("\"雪\"".as_bytes()).unwrap(), json!("雪"));
        assert!(
            (decode::<5>(br#""\u96ea""#))
                .is_err_and(|error| error.to_string() == "invalid_plugin_contract")
        );
    }

    #[test]
    fn encoded_limits_count_escaping_and_utf8() {
        for value in [
            json!("\"\\\n"),
            json!("雪🦀"),
            json!({"key": [null, 12, true]}),
        ] {
            let bytes = encode::<_, MAX_CHUNK>(&value).unwrap();
            assert_eq!(bytes, serde_json::to_vec(&value).unwrap());
            assert_eq!(decode::<MAX_CHUNK>(&bytes).unwrap(), value);
        }
        let exact = "x".repeat(MAX_CHUNK - 2);
        assert_eq!(encode::<_, MAX_CHUNK>(&exact).unwrap().len(), MAX_CHUNK);
        assert!(
            encode::<_, MAX_CHUNK>(format!("{exact}x"))
                .is_err_and(|error| error.to_string() == "invalid_plugin_contract")
        );
    }

    #[test]
    fn typed_documents_and_service_failures_keep_their_contract() {
        assert!(BoundedJson::<crate::LlmCapabilities, MAX_CHUNK>::from_bytes_owned(b"{}").is_err());
        assert!(
            BoundedJson::<crate::EmbeddingResult, MAX_CHUNK>::from_bytes_owned(
                b"{\"vectors\":[[1.0]]}"
            )
            .is_ok()
        );
        let reply: std::result::Result<Value, crate::Failure> = Err(crate::Failure::new("denied"));
        let bytes = encode::<_, MAX_CHUNK>(reply).unwrap();
        let reply =
            BoundedJson::<std::result::Result<Value, crate::Failure>, MAX_CHUNK>::from_bytes_owned(
                &bytes,
            )
            .unwrap()
            .0;
        assert_eq!(reply.unwrap_err().code, "denied");
    }
}
