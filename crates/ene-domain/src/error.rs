//! Shared validation and invariant errors for pure domain models.

pub use ene_policy::DomainError;

/// Opaque ids are 1..=128 printable ASCII characters. Anything else is a
/// client mistake, not a lookup miss.
pub fn validate_id(id: &str) -> Result<(), DomainError> {
    if id.is_empty() || id.len() > 128 || id.bytes().any(|byte| !(0x21..=0x7e).contains(&byte)) {
        return Err(DomainError::Invalid(
            "id must be 1-128 printable ASCII characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_id;

    #[test]
    fn only_printable_ascii_ids_are_accepted() {
        assert!(validate_id("client-1").is_ok());
        for rejected in ["", "has space", "tab\there", "émoji"] {
            let outcome = validate_id(rejected);
            assert!(outcome.is_err(), "{rejected:?} must be rejected");
        }
        assert!(validate_id(&"a".repeat(128)).is_ok());
        assert!(validate_id(&"a".repeat(129)).is_err());
    }
}
