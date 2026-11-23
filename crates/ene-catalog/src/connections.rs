//! Search adapter predicates.
//!
//! Connection inspection and wire input building live in `ene-client-core`,
//! which owns the transport-agnostic `Connection` model.

/// Whether a search adapter needs a credential before it can be used.
pub fn search_adapter_requires_key(adapter: &str) -> bool {
    matches!(
        adapter.trim().to_ascii_lowercase().as_str(),
        "exa" | "tavily" | "brave"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_keyed_search_adapters_require_a_credential() {
        for adapter in ["exa", "tavily", "brave", " EXA ", "Tavily"] {
            assert!(
                search_adapter_requires_key(adapter),
                "{adapter} should require a key"
            );
        }
        for adapter in ["mwmbl", "searxng", "", "exabay"] {
            assert!(
                !search_adapter_requires_key(adapter),
                "{adapter} should not require a key"
            );
        }
    }
}
