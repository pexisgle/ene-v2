use crate::Error;

/// Explicit retry identity, checked against the server's 1..=200 byte contract.
#[derive(Clone, Debug)]
pub struct IdempotencyKey(pub(crate) String);
impl IdempotencyKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 200
            || value.bytes().any(|b| !(0x20..=0x7e).contains(&b))
        {
            return Err(Error::InvalidKey);
        }
        Ok(Self(value))
    }
}
