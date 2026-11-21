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
            crate::avatar::MOTIONS
                .iter()
                .map(|&(preset, _)| preset)
                // `idle` can also return to the model's rest pose.
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
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    pub fn specs(&self) -> Vec<w::ClientCapabilitySpec> {
        let mut specs = Vec::new();
        if !self.motions.is_empty() {
            let mut properties = serde_json::Map::new();
            properties.insert(
                "motion".into(),
                serde_json::json!({
                    "type": "string",
                    "enum": self.motions,
                }),
            );
            if !self.expressions.is_empty() {
                let mut expr_enum: Vec<serde_json::Value> = self
                    .expressions
                    .iter()
                    .map(|name| serde_json::Value::String((*name).to_owned()))
                    .collect();
                expr_enum.push(serde_json::Value::Null);
                properties.insert(
                    "expression".into(),
                    serde_json::json!({
                        "type": ["string", "null"],
                        "enum": expr_enum,
                        "description": "A face to hold, or null to keep the current one. Use neutral to clear.",
                    }),
                );
            }
            let schema = serde_json::json!({
                "type": "object",
                "required": ["motion"],
                "properties": properties,
            });
            specs.push(w::ClientCapabilitySpec {
                name: "body.motion".into(),
                tool_name: "body_motion".into(),
                description: "Control the avatar's body motion, choosing only motions listed in the \
                              schema. Looping postures stay active until changed: idle, idle_sway, \
                              idle_hands_behind (standing rest), sit_idle and sit_swing_legs \
                              (perched/seated rest), music_groove (rhythmic sway), and drag_held \
                              (suspended). One-shot gestures play once and return to the active \
                              resting posture: ambient (look_around, adjust_posture, sleepy_nod, \
                              yawn, deep_breath, stretch), greetings/presence (wave, bow, goodbye, \
                              peek, beckon, sit_wave while seated), reactions (head_pat, surprised, \
                              shy, happy_bounce, clap, cheer), and conversation/work (nod, \
                              shake_head, listen, explain, tilt_head, ask_question, aha, shrug, \
                              task_complete, apologize). An optional expression sets the face \
                              separately."
                    .into(),
                input_schema: schema.to_string(),
                presets: self
                    .motions
                    .iter()
                    .map(|motion| (*motion).to_owned())
                    .collect(),
                // A motion is reversible and cosmetic, so it runs without a card.
                approval: w::CapabilityApproval::Auto.into(),
                ..Default::default()
            });

            let look_at_schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "yaw_deg": {
                        "type": "number",
                        "minimum": -90.0,
                        "maximum": 90.0,
                        "description": "Horizontal gaze angle in degrees (+left, -right)."
                    },
                    "pitch_deg": {
                        "type": "number",
                        "minimum": -60.0,
                        "maximum": 60.0,
                        "description": "Vertical gaze angle in degrees (+up, -down)."
                    },
                    "reset": {
                        "type": "boolean",
                        "description": "When true, clears any gaze override back to animation/neutral."
                    }
                }
            });
            specs.push(w::ClientCapabilitySpec {
                name: "body.look_at".into(),
                tool_name: "body_look_at".into(),
                description:
                    "Direct the avatar's eye gaze using yaw/pitch degrees, or reset to neutral."
                        .into(),
                input_schema: look_at_schema.to_string(),
                presets: Vec::new(),
                approval: w::CapabilityApproval::Auto.into(),
                ..Default::default()
            });

            let perch_schema = serde_json::json!({
                "type": "object",
                "required": ["x", "y"],
                "properties": {
                    "x": {
                        "type": "number",
                        "description": "Logical X coordinate on the monitor."
                    },
                    "y": {
                        "type": "number",
                        "description": "Logical Y coordinate on the monitor."
                    },
                    "monitor": {
                        "type": ["string", "null"],
                        "description": "Optional target monitor name."
                    },
                    "posture": {
                        "type": ["string", "null"],
                        "enum": ["sit_idle", "sit_swing_legs", "idle", null],
                        "description": "Optional resting posture to pair with the perch position."
                    }
                }
            });
            specs.push(w::ClientCapabilitySpec {
                name: "body.perch".into(),
                tool_name: "body_perch".into(),
                description: "Move the avatar to logical screen coordinates (e.g. a window edge) and optionally sit."
                    .into(),
                input_schema: perch_schema.to_string(),
                presets: Vec::new(),
                approval: w::CapabilityApproval::Auto.into(),
                ..Default::default()
            });
        }
        if self.eyes {
            let schema = serde_json::json!({
                "type": "object",
                "required": ["target_pattern", "match_kind", "notify_on", "enabled"],
                "properties": {
                    "target_pattern": {
                        "type": "string",
                        "description": "A substring or regular expression matched against the window title or app id.",
                    },
                    "match_kind": {
                        "type": "string",
                        "enum": ["title", "app_id"],
                    },
                    "notify_on": {
                        "type": "string",
                        "enum": ["focus", "blur", "title_change"],
                    },
                    "enabled": {
                        "type": "boolean",
                    },
                },
            });
            specs.push(w::ClientCapabilitySpec {
                name: "eye.watch".into(),
                tool_name: "set_eye_watch".into(),
                description:
                    "Watch for a window becoming active, losing focus, or changing title, \
                              and report it as soon as it matches."
                        .into(),
                input_schema: schema.to_string(),
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

/// Capabilities a bundled desktop-launched `ene-host` accepts by default.
pub fn default_allowed_capabilities() -> String {
    let files = crate::WORKSPACE_FILE_CAPABILITIES.join(",");
    let shell = crate::work_tools::WORKSPACE_SHELL_CAPABILITY;
    format!(
        "body.motion,body.look_at,body.perch,eye.watch,{files},{shell},{}",
        ene_rpc::COMPUTER_USE
    )
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
        for spec in declaration(&["idle", "nod"]).specs() {
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
        for spec in declaration(&["idle", "nod"]).specs() {
            assert!(
                !ene_rpc::BUILTIN_TOOLS.contains(&spec.tool_name.as_str()),
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
    fn the_bundled_ene_declares_the_selected_presets() {
        let model = crate::avatar::bundled_ene_model().unwrap();
        let declaration = Declaration::probe(Some(Path::new("ene-avatar")), Some(&model));
        assert!(
            declaration
                .motions
                .into_iter()
                .eq(crate::avatar::MOTIONS.iter().map(|&(name, _)| name))
        );
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
