use crate::{Error, Result};
use extism::convert::{FromBytesOwned, Json, ToBytes};
use serde::{Serialize, de::DeserializeOwned};

/// JSON with a limit on the actual ABI bytes, before decoding or after encoding.
pub(super) struct BoundedJson<T, const LIMIT: usize>(pub T);

impl<T: DeserializeOwned, const LIMIT: usize> FromBytesOwned for BoundedJson<T, LIMIT> {
    fn from_bytes_owned(raw: &[u8]) -> std::result::Result<Self, extism::Error> {
        if raw.len() > LIMIT {
            return Err(Error::Protocol.into());
        }
        Ok(Self(Json::<T>::from_bytes_owned(raw)?.0))
    }
}

impl<T: Serialize, const LIMIT: usize> ToBytes<'_> for BoundedJson<T, LIMIT> {
    type Bytes = Vec<u8>;

    fn to_bytes(&self) -> std::result::Result<Vec<u8>, extism::Error> {
        let bytes = Json(&self.0).to_bytes()?;
        if bytes.len() > LIMIT {
            return Err(Error::Protocol.into());
        }
        Ok(bytes)
    }
}

pub(super) fn error(error: extism::Error) -> Error {
    match error.downcast::<Error>() {
        Ok(error) => error,
        Err(error) => match error.downcast::<serde_json::Error>() {
            Ok(error) => Error::Json(error),
            Err(error) => Error::Execution(error),
        },
    }
}

pub(super) fn encode<T: Serialize, const LIMIT: usize>(value: T) -> Result<Vec<u8>> {
    BoundedJson::<_, LIMIT>(value).to_bytes().map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{MAX_CHUNK, MAX_MESSAGE_BYTES};
    use serde_json::{Value, json};

    fn decode<const LIMIT: usize>(raw: &[u8]) -> Result<Value> {
        BoundedJson::<Value, LIMIT>::from_bytes_owned(raw)
            .map(|value| value.0)
            .map_err(error)
    }

    #[test]
    fn raw_limits_precede_json_decoding() {
        for limit in [MAX_CHUNK, MAX_MESSAGE_BYTES] {
            let mut raw = b"{}".to_vec();
            raw.resize(limit, b' ');
            if limit == MAX_CHUNK {
                assert_eq!(decode::<MAX_CHUNK>(&raw).unwrap(), json!({}));
                raw.push(b' ');
                assert!(matches!(decode::<MAX_CHUNK>(&raw), Err(Error::Protocol)));
            } else {
                assert_eq!(decode::<MAX_MESSAGE_BYTES>(&raw).unwrap(), json!({}));
                raw.push(b' ');
                assert!(matches!(
                    decode::<MAX_MESSAGE_BYTES>(&raw),
                    Err(Error::Protocol)
                ));
            }
        }
        assert!(matches!(decode::<2>(b"bad"), Err(Error::Protocol)));
        assert!(matches!(decode::<3>(b"bad"), Err(Error::Json(_))));
        assert_eq!(decode::<5>("\"雪\"".as_bytes()).unwrap(), json!("雪"));
        assert!(matches!(decode::<5>(br#""\u96ea""#), Err(Error::Protocol)));
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
        assert!(matches!(
            encode::<_, MAX_CHUNK>(format!("{exact}x")),
            Err(Error::Protocol)
        ));
    }

    #[test]
    fn typed_documents_and_service_failures_keep_their_contract() {
        assert!(
            BoundedJson::<ene_plugin::LlmCapabilities, MAX_CHUNK>::from_bytes_owned(b"{}").is_err()
        );
        assert!(
            BoundedJson::<ene_plugin::EmbeddingResult, MAX_CHUNK>::from_bytes_owned(
                b"{\"vectors\":[[1.0]]}"
            )
            .is_ok()
        );
        let reply: std::result::Result<Value, ene_plugin::Failure> =
            Err(ene_plugin::Failure::new("denied"));
        let bytes = encode::<_, MAX_CHUNK>(reply).unwrap();
        let reply = BoundedJson::<std::result::Result<Value, ene_plugin::Failure>, MAX_CHUNK>::from_bytes_owned(&bytes).unwrap().0;
        assert_eq!(reply.unwrap_err().code, "denied");
    }
}
