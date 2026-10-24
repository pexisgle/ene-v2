//! JSON Lines request and command definitions.

use crate::character::Config;
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct Request {
    pub(super) id: u64,
    #[serde(flatten)]
    pub(super) command: Command,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Add {
        character: String,
        config: Config,
    },
    Set {
        character: String,
        config: Config,
    },
    Get,
    Remove {
        character: String,
    },
    Center {
        character: String,
        monitor: Option<String>,
    },
    Fps {
        value: u32,
    },
    Quit,
}
