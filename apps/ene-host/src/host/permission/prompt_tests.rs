//! The decision prompt and its reply parsing.
//!
//! These exercise `decision_prompt` and `run_decision` in isolation: no world,
//! no store, no gate. Kept apart from the gated-call harness in `tests.rs`
//! because they fail for different reasons.

use super::decision::{
    DECISION_ARGUMENT_CHARS, DECISION_REQUEST_CHARS, DecisionInputs, decision_prompt, run_decision,
};
use super::*;
use ene_domain::PolicyText;
use ene_host_core::util::truncate;
use rig_core::test_utils::MockCompletionModel;
use rig_core::{DynModel, operation::Completion};

const USER_REQUEST: &str = "デスクトップのスクリーンショットを取って";
/// Longer than every prompt bound, so a truncation assert can find its tail.
const HUGE: &str = "huge-path-";

#[test]
fn the_reply_is_strict_json() {
    let inputs = || DecisionInputs {
        tool_name: "device_request".into(),
        target: "device.files /tmp/a".into(),
        arguments: "{}".into(),
        request: USER_REQUEST.into(),
        policies: Vec::new(),
        rules: Vec::new(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (reply, allow) in [
        (r#"{"decision":"allow","reason":"安全な操作"}"#, Some(true)),
        (
            r#"{"decision":"require_approval","reason":"外部送信のため確認"}"#,
            Some(false),
        ),
        // An unknown verdict, a missing field, an extra field and plain
        // text are all one thing: not the JSON asked for.
        (r#"{"decision":"maybe","reason":"?"}"#, None),
        (r#"{"decision":"allow"}"#, None),
        (r#"{"decision":"allow","reason":"r","extra":1}"#, None),
        ("はい、いいですよ", None),
    ] {
        let model: DynModel<Completion> = MockCompletionModel::text(reply).into();
        let decision = runtime.block_on(run_decision(model, inputs()));
        match allow {
            Some(allow) => {
                let decision = decision.expect(reply);
                assert_eq!(decision.allow, allow, "{reply}");
                assert!(!decision.reason.is_empty(), "{reply}");
            }
            None => assert!(decision.is_err(), "{reply} must fail safe"),
        }
    }
}

#[test]
fn the_prompt_carries_only_the_bounded_inputs() {
    // Built the way the gate builds them: every string truncated before
    // the inputs exist.
    let inputs = DecisionInputs {
        tool_name: "device_request".into(),
        target: truncate(
            &format!("device.files {}", HUGE.repeat(400)),
            DECISION_ARGUMENT_CHARS,
        ),
        arguments: truncate(
            &serde_json::json!({"path": HUGE.repeat(400)}).to_string(),
            DECISION_ARGUMENT_CHARS,
        ),
        request: truncate(
            &format!("{USER_REQUEST} {}", HUGE.repeat(400)),
            DECISION_REQUEST_CHARS,
        ),
        policies: vec![PolicyText {
            id: "p1".into(),
            text: "外部への送信は常に確認する".into(),
            created_at: String::new(),
        }],
        rules: vec![PolicyRule {
            id: "r1".into(),
            tool_name: "body_motion".into(),
            pattern: "*".into(),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        }],
    };
    let prompt = decision_prompt(&inputs);
    assert!(prompt.contains("device_request"));
    assert!(prompt.contains(USER_REQUEST));
    assert!(prompt.contains("外部への送信は常に確認する"));
    assert!(prompt.contains("body_motion / * → allow"));
    // Every oversized string is cut at its bound, so the far end of the
    // huge fixture appears in none of them, and the prompt holds the
    // bounded prefix of each.
    let tail = HUGE.repeat(400);
    for bounded in [&inputs.target, &inputs.arguments, &inputs.request] {
        assert!(
            !bounded.contains(&tail[DECISION_ARGUMENT_CHARS..]),
            "the bound must have cut the tail"
        );
    }
    assert!(prompt.contains(&tail[..1_900]));
    assert!(prompt.contains("[保存済みルール（参考）]"));
}

#[test]
fn an_empty_prompt_section_says_so() {
    let prompt = decision_prompt(&DecisionInputs {
        tool_name: "device_request".into(),
        target: "device.files /tmp/a".into(),
        arguments: "{}".into(),
        request: String::new(),
        policies: Vec::new(),
        rules: Vec::new(),
    });
    assert!(prompt.contains("[依頼]\n(なし)"));
    assert!(prompt.contains("[ユーザーの自由文ポリシー]\n(なし)"));
    assert!(prompt.contains("[保存済みルール（参考）]\n(なし)"));
}
