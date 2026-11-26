//! Shared validation and invariant errors for pure domain models.

pub use ene_policy::DomainError;

/// Opaque ids are 1..=128 printable ASCII characters. Anything else is a
/// client mistake, not a lookup miss.
pub fn validate_id(id: &str) -> Result<(), DomainError> {
    if id.is_empty()
        || id.chars().count() > 128
        || id.bytes().any(|byte| !(0x21..=0x7e).contains(&byte))
    {
        return Err(DomainError::Invalid(
            "id must be 1-128 printable ASCII characters".into(),
        ));
    }
    Ok(())
}
