//! What this client tells the host it can do.
//!
//! The host stores capability names, JSON Schema, presets and approval
//! declarations only (`docs/api.md` §クライアント能力). It never interprets them,
//! so this table is the whole of this client's contract with the host, and it
//! must describe what this machine can actually do: a capability is declared
//! only when its inputs can be honoured here. A motion whose asset is missing is
//! not declared, and no renderer means no `body.motion` at all.

use ene_rpc as w;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SchemaType {
    Object,
    String,
    Number,
    Boolean,
    Null,
}

#[derive(Serialize)]
#[serde(untagged)]
enum PropertyType {
    Scalar(SchemaType),
    Nullable([SchemaType; 2]),
}

#[derive(Serialize)]
struct Property<'a> {
    #[serde(rename = "type")]
    kind: PropertyType,
    #[serde(rename = "enum", skip_serializing_if = "Vec::is_empty")]
    choices: Vec<Option<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum: Option<f32>,
}

impl<'a> Property<'a> {
    fn new(kind: SchemaType, description: Option<&'static str>) -> Self {
        Self {
            kind: PropertyType::Scalar(kind),
            choices: Vec::new(),
            description,
            minimum: None,
            maximum: None,
        }
    }

    fn text_choices(choices: &'a [&'a str]) -> Self {
        let mut field = Self::new(SchemaType::String, None);
        field.choices.extend(choices.iter().copied().map(Some));
        field
    }

    fn nullable_text(choices: &'a [&'a str], description: &'static str) -> Self {
        let mut field = Self::text_choices(choices);
        field.kind = PropertyType::Nullable([SchemaType::String, SchemaType::Null]);
        if !choices.is_empty() {
            field.choices.push(None);
        }
        field.description = Some(description);
        field
    }

    fn angle(limit: f32, description: &'static str) -> Self {
        let mut field = Self::new(SchemaType::Number, Some(description));
        field.minimum = Some(-limit);
        field.maximum = Some(limit);
        field
    }
}

#[derive(Serialize)]
struct ObjectSchema<'a> {
    #[serde(rename = "type")]
    kind: SchemaType,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    required: &'static [&'static str],
    properties: BTreeMap<&'static str, Property<'a>>,
}

impl<'a> ObjectSchema<'a> {
    fn json(
        required: &'static [&'static str],
        properties: impl IntoIterator<Item = (&'static str, Property<'a>)>,
    ) -> String {
        serde_json::json!(Self {
            kind: SchemaType::Object,
            required,
            properties: properties.into_iter().collect(),
        })
        .to_string()
    }
}

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
    /// What this machine can do with its currently displayed avatar.
    pub fn probe(avatar_ready: bool) -> Self {
        let motions = if avatar_ready {
            crate::avatar::MOTIONS
                .iter()
                .map(|&(preset, _)| preset)
                // `idle` can also return to the model's rest pose.
                .filter(|motion| *motion == "idle" || crate::avatar::motion(motion).is_some())
                .collect()
        } else {
            Vec::new()
        };
        let expressions = if avatar_ready {
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
            specs.extend(self.body_specs());
        }
        if self.eyes {
            specs.push(eye_watch_spec());
        }
        specs
    }

    fn body_specs(&self) -> Vec<w::ClientCapabilitySpec> {
        vec![
            self.body_motion_spec(),
            body_look_at_spec(),
            body_perch_spec(),
        ]
    }

    fn body_motion_spec(&self) -> w::ClientCapabilitySpec {
        let mut properties = BTreeMap::from([("motion", Property::text_choices(&self.motions))]);
        if !self.expressions.is_empty() {
            properties.insert(
                "expression",
                Property::nullable_text(
                    &self.expressions,
                    "A face to hold, or null to keep the current one. Use neutral to clear.",
                ),
            );
        }
        w::ClientCapabilitySpec {
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
            input_schema: ObjectSchema::json(&["motion"], properties),
            presets: self
                .motions
                .iter()
                .map(|motion| (*motion).to_owned())
                .collect(),
            // A motion is reversible and cosmetic, so it runs without a card.
            approval: w::CapabilityApproval::Auto.into(),
            ..Default::default()
        }
    }
}

fn body_look_at_spec() -> w::ClientCapabilitySpec {
    let input_schema = ObjectSchema::json(
        &[],
        [
            (
                "yaw_deg",
                Property::angle(90.0, "Horizontal gaze angle in degrees (+left, -right)."),
            ),
            (
                "pitch_deg",
                Property::angle(60.0, "Vertical gaze angle in degrees (+up, -down)."),
            ),
            (
                "reset",
                Property::new(
                    SchemaType::Boolean,
                    Some("When true, clears any gaze override back to animation/neutral."),
                ),
            ),
        ],
    );
    w::ClientCapabilitySpec {
        name: "body.look_at".into(),
        tool_name: "body_look_at".into(),
        description: "Direct the avatar's eye gaze using yaw/pitch degrees, or reset to neutral."
            .into(),
        input_schema,
        presets: Vec::new(),
        approval: w::CapabilityApproval::Auto.into(),
        ..Default::default()
    }
}

fn body_perch_spec() -> w::ClientCapabilitySpec {
    let input_schema = ObjectSchema::json(
        &["x", "y"],
        [
            (
                "x",
                Property::new(
                    SchemaType::Number,
                    Some("Logical X coordinate on the monitor."),
                ),
            ),
            (
                "y",
                Property::new(
                    SchemaType::Number,
                    Some("Logical Y coordinate on the monitor."),
                ),
            ),
            (
                "monitor",
                Property::nullable_text(&[], "Optional target monitor name."),
            ),
            (
                "posture",
                Property::nullable_text(
                    &["sit_idle", "sit_swing_legs", "idle"],
                    "Optional resting posture to pair with the perch position.",
                ),
            ),
        ],
    );
    w::ClientCapabilitySpec {
        name: "body.perch".into(),
        tool_name: "body_perch".into(),
        description:
            "Move the avatar to logical screen coordinates (e.g. a window edge) and optionally sit."
                .into(),
        input_schema,
        presets: Vec::new(),
        approval: w::CapabilityApproval::Auto.into(),
        ..Default::default()
    }
}

fn eye_watch_spec() -> w::ClientCapabilitySpec {
    let input_schema = ObjectSchema::json(
        &["target_pattern", "match_kind", "notify_on", "enabled"],
        [
            (
                "target_pattern",
                Property::new(
                    SchemaType::String,
                    Some(
                        "A substring or regular expression matched against the window title or app id.",
                    ),
                ),
            ),
            ("match_kind", Property::text_choices(&["title", "app_id"])),
            (
                "notify_on",
                Property::text_choices(&["focus", "blur", "title_change"]),
            ),
            ("enabled", Property::new(SchemaType::Boolean, None)),
        ],
    );
    w::ClientCapabilitySpec {
        name: "eye.watch".into(),
        tool_name: "set_eye_watch".into(),
        description: "Watch for a window becoming active, losing focus, or changing title, \
                      and report it as soon as it matches."
            .into(),
        input_schema,
        presets: Vec::new(),
        // Watching is read-only perception, never an action on the machine.
        approval: w::CapabilityApproval::Auto.into(),
        ..Default::default()
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
        let names = names(&Declaration::probe(false).specs());
        assert!(!names.iter().any(|name| name.starts_with("body.")));
        assert_eq!(
            names.iter().any(|name| name == "eye.watch"),
            crate::perception::supported()
        );
    }

    #[test]
    fn the_bundled_ene_declares_the_selected_presets() {
        let declaration = Declaration::probe(true);
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
