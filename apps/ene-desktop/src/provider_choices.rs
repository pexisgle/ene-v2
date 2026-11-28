use ene_rpc::ProviderKind;

#[derive(Clone, Copy)]
pub(crate) struct Preset {
    pub label: &'static str,
    pub kind: ProviderKind,
    pub adapter: &'static str,
    pub endpoint: &'static str,
}

pub(crate) const CHAT: &[Preset] = &[
    Preset {
        label: "OpenAI",
        kind: ProviderKind::OpenaiCompatible,
        adapter: "",
        endpoint: "https://api.openai.com/v1",
    },
    Preset {
        label: "OpenRouter",
        kind: ProviderKind::Native,
        adapter: "openrouter",
        endpoint: "https://openrouter.ai/api/v1",
    },
    Preset {
        label: "Anthropic",
        kind: ProviderKind::Native,
        adapter: "anthropic",
        endpoint: "https://api.anthropic.com",
    },
    Preset {
        label: "Google Gemini",
        kind: ProviderKind::Native,
        adapter: "gcp.gemini",
        endpoint: "https://generativelanguage.googleapis.com",
    },
    Preset {
        label: "Google Agent Platform",
        kind: ProviderKind::Native,
        adapter: ene_domain::agent_platform::ADAPTER,
        endpoint: ene_domain::agent_platform::ENDPOINT,
    },
    Preset {
        label: "さくらのAI Engine",
        kind: ProviderKind::OpenaiCompatible,
        adapter: "sakura",
        endpoint: "https://api.ai.sakura.ad.jp/v1",
    },
    Preset {
        label: "OpenAI 互換",
        kind: ProviderKind::OpenaiCompatible,
        adapter: "",
        endpoint: "",
    },
];

pub(crate) const VOICEVOX: Preset = Preset {
    label: "VOICEVOX",
    kind: ProviderKind::Voicevox,
    adapter: "",
    endpoint: "http://127.0.0.1:50021",
};

pub(crate) const SEARCH: &[Preset] = &[
    Preset {
        label: "Mwmbl",
        kind: ProviderKind::Search,
        adapter: "mwmbl",
        endpoint: "https://api.mwmbl.org/api/v2/search/",
    },
    Preset {
        label: "SearXNG",
        kind: ProviderKind::Search,
        adapter: "searxng",
        endpoint: "",
    },
    Preset {
        label: "Brave Search",
        kind: ProviderKind::Search,
        adapter: "brave",
        endpoint: "https://api.search.brave.com/res/v1/web/search",
    },
    Preset {
        label: "Tavily",
        kind: ProviderKind::Search,
        adapter: "tavily",
        endpoint: "https://api.tavily.com/search",
    },
    Preset {
        label: "Exa",
        kind: ProviderKind::Search,
        adapter: "exa",
        endpoint: "https://api.exa.ai/search",
    },
];
