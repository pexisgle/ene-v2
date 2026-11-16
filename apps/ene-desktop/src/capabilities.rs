//! What this client tells the host it can do.
//!
//! The host stores capability names, JSON Schema, presets and approval
//! declarations only (`docs/api.md` §クライアント能力). It never interprets
//! them, so the table below is the whole of this client's contract with the
//! host: a capability the host has not granted is not dispatched here, and a
//! declaration whose schema disagrees with the handler is a bug in this file.

use ene_rpc as w;

/// One capability this client declares.
pub struct Capability {
    /// The name the host dispatches operations under.
    pub name: &'static str,
    /// The tool the parent agent may call, or empty for `device_request` only.
    pub tool_name: &'static str,
    /// The model-facing description.
    pub description: &'static str,
    /// The JSON Schema every operation input is validated against.
    pub input_schema: &'static str,
    /// The motion and expression names the model may ask for.
    pub presets: &'static [&'static str],
    /// Whether the host must confirm every operation.
    pub approval: w::CapabilityApproval,
}

/// The avatar's motion and expression presets. These are the names
/// `apps/ene-avatar` accepts as `config.motion` and `config.expression`.
pub const MOTIONS: &[&str] = &["idle", "nod", "tilt_head", "stretch"];

/// The capabilities this client declares.
pub const CAPABILITIES: &[Capability] = &[
    Capability {
        name: "body.motion",
        tool_name: "body_motion",
        description: "Move the character's body and face. Use it for a nod, a head tilt, \
                      or a stretch while listening.",
        input_schema: r#"{"type":"object","required":["motion"],"properties":{"motion":{"type":"string","enum":["idle","nod","tilt_head","stretch"]},"expression":{"type":"string","description":"An optional VRM expression name."}}}"#,
        presets: MOTIONS,
        // A motion is reversible and cosmetic, so it runs without a card.
        approval: w::CapabilityApproval::Auto,
    },
    Capability {
        name: "eye.watch",
        tool_name: "set_eye_watch",
        description: "Watch for a window becoming active, losing focus, or changing title, \
                      and report it as soon as it matches.",
        input_schema: r#"{"type":"object","required":["target_pattern","match_kind","notify_on","enabled"],"properties":{"target_pattern":{"type":"string","description":"A substring or regular expression matched against the window title or app id."},"match_kind":{"type":"string","enum":["title","app_id"]},"notify_on":{"type":"string","enum":["focus","blur","title_change"]},"enabled":{"type":"boolean"}}}"#,
        presets: &[],
        // Watching is read-only perception, never an action on the machine.
        approval: w::CapabilityApproval::Auto,
    },
];

/// The capability names, in declaration order.
pub fn names() -> Vec<String> {
    CAPABILITIES.iter().map(|c| c.name.to_owned()).collect()
}

/// The declarations the host validates and stores.
pub fn specs() -> Vec<w::ClientCapabilitySpec> {
    CAPABILITIES
        .iter()
        .map(|capability| w::ClientCapabilitySpec {
            name: capability.name.into(),
            tool_name: capability.tool_name.into(),
            description: capability.description.into(),
            input_schema: capability.input_schema.into(),
            presets: capability.presets.iter().map(|p| (*p).to_owned()).collect(),
            approval: capability.approval.into(),
            ..Default::default()
        })
        .collect()
}

/// The declaration for one capability name.
#[cfg(test)]
pub fn spec(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|c| c.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declaration_is_a_valid_object_schema() {
        for capability in CAPABILITIES {
            let schema: serde_json::Value =
                serde_json::from_str(capability.input_schema).expect("the schema is JSON");
            assert_eq!(
                schema["type"], "object",
                "{} must describe an object",
                capability.name
            );
            assert!(
                !capability.name.is_empty() && !capability.name.contains(char::is_whitespace),
                "{} is a valid capability name",
                capability.name
            );
        }
    }

    /// The host rejects a tool name that shadows one of its own
    /// (`host/client/mod.rs` `BUILTIN_TOOLS`).
    #[test]
    fn no_tool_name_shadows_a_host_tool() {
        const HOST_TOOLS: &[&str] = &[
            "mind_recall",
            "mind_read",
            "mind_update",
            "start_work",
            "send_work",
            "list_works",
            "stop_work",
            "device_request",
            "noop",
            "schedule_create",
            "schedule_list",
            "schedule_cancel",
            "report",
        ];
        for capability in CAPABILITIES {
            assert!(
                !HOST_TOOLS.contains(&capability.tool_name),
                "{} shadows a host tool",
                capability.tool_name
            );
        }
    }

    /// The host validates every operation against the declared schema, so a
    /// preset the schema forbids would be a motion the model cannot request.
    #[test]
    fn every_motion_preset_is_allowed_by_its_schema() {
        let schema: serde_json::Value =
            serde_json::from_str(spec("body.motion").unwrap().input_schema).unwrap();
        let allowed: Vec<&str> = schema["properties"]["motion"]["enum"]
            .as_array()
            .expect("motion is enumerated")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        for motion in MOTIONS {
            assert!(allowed.contains(motion), "{motion} is requestable");
        }
    }
}
