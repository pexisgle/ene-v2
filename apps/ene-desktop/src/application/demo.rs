/// Offline demo responses; normal conversations use the host API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    Demo,
    #[default]
    Disconnected,
}

impl Mode {
    pub fn accepts(self, text: &str) -> bool {
        self == Self::Demo && !text.trim().is_empty()
    }

    pub fn respond(self, text: &str) -> Option<&'static str> {
        self.accepts(text)
            .then_some("こんにちは、eneです。メッセージを受け取りました。")
    }
}
