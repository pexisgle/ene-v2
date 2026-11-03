mod openai;
pub(crate) use openai::agent_connection;
pub(crate) mod plugin;
pub mod registry;

/// The parent conversation needs the host-owned Rig model connection, which
/// only the built-in provider implements today. A plugin adapter speaks its
/// own loop and is refused rather than silently treated as OpenAI.
pub(crate) fn can_serve_conversation(connection: &crate::domain::Connection) -> bool {
    connection.kind == "llm" && connection.adapter == openai::ID
}
