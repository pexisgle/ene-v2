//! Built-in search providers shared by host adapters and connection setup.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchAdapter {
    Exa,
    Tavily,
    Brave,
    Mwmbl,
    Searxng,
}

#[derive(Clone, Copy, Debug)]
pub struct SearchProvider {
    pub adapter_id: &'static str,
    pub name: &'static str,
    pub preset_id: &'static str,
    pub connection_name: &'static str,
    pub default_endpoint: Option<&'static str>,
    pub key_environment: Option<&'static str>,
}

impl SearchAdapter {
    const ALL: [Self; 5] = [
        Self::Exa,
        Self::Tavily,
        Self::Brave,
        Self::Mwmbl,
        Self::Searxng,
    ];

    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|adapter| adapter.provider().adapter_id.eq_ignore_ascii_case(raw))
    }

    pub const fn provider(self) -> SearchProvider {
        match self {
            Self::Exa => SearchProvider {
                adapter_id: "exa",
                name: "Exa",
                preset_id: "search-exa",
                connection_name: "Web検索 (Exa)",
                default_endpoint: Some("https://api.exa.ai/search"),
                key_environment: Some("EXA_API_KEY"),
            },
            Self::Tavily => SearchProvider {
                adapter_id: "tavily",
                name: "Tavily",
                preset_id: "search-tavily",
                connection_name: "Web検索 (Tavily)",
                default_endpoint: Some("https://api.tavily.com/search"),
                key_environment: Some("TAVILY_API_KEY"),
            },
            Self::Brave => SearchProvider {
                adapter_id: "brave",
                name: "Brave",
                preset_id: "search-brave",
                connection_name: "Web検索 (Brave)",
                default_endpoint: Some("https://api.search.brave.com/res/v1/web/search"),
                key_environment: Some("BRAVE_API_KEY"),
            },
            Self::Mwmbl => SearchProvider {
                adapter_id: "mwmbl",
                name: "Mwmbl",
                preset_id: "search-mwmbl",
                connection_name: "Web検索 (Mwmbl)",
                default_endpoint: Some("https://api.mwmbl.org/api/v2/search/"),
                key_environment: None,
            },
            Self::Searxng => SearchProvider {
                adapter_id: "searxng",
                name: "SearXNG",
                preset_id: "search-searxng",
                connection_name: "Web検索 (SearXNG)",
                default_endpoint: None,
                key_environment: None,
            },
        }
    }
}

/// Whether a search adapter needs a credential before it can be used.
pub fn search_adapter_requires_key(adapter: &str) -> bool {
    SearchAdapter::parse(adapter)
        .is_some_and(|adapter| adapter.provider().key_environment.is_some())
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
