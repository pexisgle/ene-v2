//! What this client tells the host it can do.
//!
//! The host stores capability names, JSON Schema, presets and approval
//! declarations only (`docs/api.md` §クライアント能力). It never interprets them,
//! so this table is the whole of this client's contract with the host, and it
//! must describe what this machine can actually do: a capability is declared
//! only when its inputs can be honoured here. A motion whose asset is missing is
//! not declared, and no renderer means no `body.motion` at all.

use ene_rpc as w;
use std::path::Path;

/// The motions the renderer has a clip for.
pub const MOTIONS: &[&str] = &["idle", "nod", "tilt_head", "stretch"];

/// The faces the renderer can blend. `neutral` clears an expression.
pub const EXPRESSIONS: &[&str] = &["neutral", "happy", "sad", "surprised", "relaxed"];

/// The capabilities this client can declare right now.
pub struct Declaration {
    /// The motion presets with an asset on this machine.
    pub motions: Vec<&'static str>,
    /// The expressions the renderer can blend, empty without a renderer.
    pub expressions: Vec<&'static str>,
    /// Whether the platform can report the foreground window.
    pub eyes: bool,
}

impl Declaration {
    /// What this machine can do, given its renderer and its assets.
    pub fn probe(renderer: Option<&Path>, model: Option<&Path>) -> Self {
        let usable = renderer.is_some() && model.is_some();
        let motions = if usable {
            MOTIONS
                .iter()
                .copied()
                // `idle` is a state rather than a clip, so it needs no asset.
                .filter(|motion| *motion == "idle" || crate::avatar::motion(motion).is_some())
                .collect()
        } else {
            Vec::new()
        };
        let expressions = if usable {
            EXPRESSIONS.to_vec()
        } else {
            Vec::new()
        };
        Self {
            motions,
            expressions,
            eyes: crate::perception::supported(),
        }
    }

    /// The declarations the host validates and stores.
    pub fn specs(&self) -> Vec<w::ClientCapabilitySpec> {
        let mut specs = Vec::new();
        if !self.motions.is_empty() {
            let allowed = self
                .motions
                .iter()
                .map(|motion| format!("\"{motion}\""))
                .collect::<Vec<_>>()
                .join(",");
            let expression = if self.expressions.is_empty() {
                String::new()
            } else {
                let names = self
                    .expressions
                    .iter()
                    .map(|expression| format!("\"{expression}\""))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    r#","expression":{{"type":["string","null"],"enum":[{names},null],"description":"A face to hold, or null to keep the current one. Use neutral to clear."}}"#
                )
            };
            specs.push(w::ClientCapabilitySpec {
                name: "body.motion".into(),
                tool_name: "body_motion".into(),
                description: "Move the character's body, and optionally set its face. Use it for \
                              a nod, a head tilt, a stretch, or a smile while listening."
                    .into(),
                input_schema: format!(
                    r#"{{"type":"object","required":["motion"],"properties":{{"motion":{{"type":"string","enum":[{allowed}]}}{expression}}}}}"#
                ),
                presets: self.motions.iter().map(|motion| (*motion).to_owned()).collect(),
                // A motion is reversible and cosmetic, so it runs without a card.
                approval: w::CapabilityApproval::Auto.into(),
                ..Default::default()
            });
        }
        if self.eyes {
            specs.push(w::ClientCapabilitySpec {
                name: "eye.watch".into(),
                tool_name: "set_eye_watch".into(),
                description: "Watch for a window becoming active, losing focus, or changing title, \
                              and report it as soon as it matches."
                    .into(),
                input_schema: r#"{"type":"object","required":["target_pattern","match_kind","notify_on","enabled"],"properties":{"target_pattern":{"type":"string","description":"A substring or regular expression matched against the window title or app id."},"match_kind":{"type":"string","enum":["title","app_id"]},"notify_on":{"type":"string","enum":["focus","blur","title_change"]},"enabled":{"type":"boolean"}}}"#.into(),
                presets: Vec::new(),
                // Watching is read-only perception, never an action on the machine.
                approval: w::CapabilityApproval::Auto.into(),
                ..Default::default()
            });
        }
        specs
    }
}

/// The capability names of one declaration.
pub fn names(specs: &[w::ClientCapabilitySpec]) -> Vec<String> {
    specs.iter().map(|spec| spec.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(motions: &[&'static str]) -> Declaration {
        Declaration {
            motions: motions.to_vec(),
            expressions: EXPRESSIONS.to_vec(),
            eyes: true,
        }
    }

    #[test]
    fn every_declaration_is_a_valid_object_schema() {
        for spec in declaration(MOTIONS).specs() {
            let schema: serde_json::Value =
                serde_json::from_str(&spec.input_schema).expect("the schema is JSON");
            assert_eq!(
                schema["type"], "object",
                "{} describes an object",
                spec.name
            );
            assert!(
                !spec.name.is_empty() && !spec.name.contains(char::is_whitespace),
                "{} is a valid capability name",
                spec.name
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
        for spec in declaration(MOTIONS).specs() {
            assert!(
                !HOST_TOOLS.contains(&spec.tool_name.as_str()),
                "{} shadows a host tool",
                spec.tool_name
            );
        }
    }

    /// The schema is the host's validation rule, so a preset it forbids is a
    /// motion the model cannot actually request.
    #[test]
    fn the_schema_allows_exactly_the_presets_that_have_an_asset() {
        let specs = declaration(&["idle", "nod"]).specs();
        let motion = specs
            .iter()
            .find(|spec| spec.name == "body.motion")
            .expect("a motion is declared");
        let schema: serde_json::Value = serde_json::from_str(&motion.input_schema).unwrap();
        let allowed: Vec<&str> = schema["properties"]["motion"]["enum"]
            .as_array()
            .expect("motion is enumerated")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(allowed, ["idle", "nod"]);
        assert_eq!(motion.presets, ["idle", "nod"]);
    }

    /// A machine that cannot show a motion must not claim it can.
    #[test]
    fn no_motion_is_declared_without_a_renderer() {
        let none = Declaration {
            motions: Vec::new(),
            expressions: Vec::new(),
            eyes: true,
        };
        let names = names(&none.specs());
        assert!(!names.iter().any(|name| name == "body.motion"));
        assert!(names.iter().any(|name| name == "eye.watch"));
    }

    #[test]
    fn a_machine_with_a_renderer_and_no_asset_declares_only_the_state() {
        assert!(crate::avatar::motion("idle").is_none());
        let declaration = Declaration::probe(None, None);
        assert!(declaration.motions.is_empty());
    }
    /// The schema is what the host enforces, so an expression it rejects is one
    /// the model cannot ask for.
    #[test]
    fn expressions_are_enumerated_and_nullable() {
        let specs = declaration(&["idle"]).specs();
        let motion = specs
            .iter()
            .find(|spec| spec.name == "body.motion")
            .expect("a motion is declared");
        let schema: serde_json::Value = serde_json::from_str(&motion.input_schema).unwrap();
        let allowed: Vec<&str> = schema["properties"]["expression"]["enum"]
            .as_array()
            .expect("expressions are enumerated")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(allowed, EXPRESSIONS);
        assert!(allowed.contains(&"neutral"));
        assert!(
            schema["properties"]["expression"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .any(serde_json::Value::is_null),
            "a request can leave the face alone"
        );

        // Without a renderer there is no face to set, so no property either.
        let silent = Declaration {
            motions: vec!["idle"],
            expressions: Vec::new(),
            eyes: false,
        };
        let schema = &silent.specs()[0];
        assert!(!schema.input_schema.contains("expression"));
    }
}
