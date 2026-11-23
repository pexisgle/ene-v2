use crate::*;
use serde_json::json;

/// These spellings are persisted in SQLite and travel the RPC wire, so
/// they are pinned: a derive change must not move a single byte.
#[test]
fn effects_and_approval_statuses_keep_their_stored_spellings() {
    for (effect, text) in [
        (PolicyEffect::Allow, "allow"),
        (PolicyEffect::Deny, "deny"),
        (PolicyEffect::Confirm, "confirm"),
        (PolicyEffect::AskLlm, "ask_llm"),
    ] {
        assert_eq!(effect.as_str(), text);
        assert_eq!(serde_json::to_value(effect).unwrap(), json!(text));
        assert_eq!(text.parse::<PolicyEffect>().unwrap(), effect);
        assert_eq!(
            serde_json::from_value::<PolicyEffect>(json!(text)).unwrap(),
            effect
        );
    }
    for (status, text) in [
        (ApprovalStatus::Pending, "pending"),
        (ApprovalStatus::Approved, "approved"),
        (ApprovalStatus::Denied, "denied"),
        (ApprovalStatus::Expired, "expired"),
    ] {
        assert_eq!(status.as_str(), text);
        assert_eq!(serde_json::to_value(status).unwrap(), json!(text));
        assert_eq!(text.parse::<ApprovalStatus>().unwrap(), status);
        assert_eq!(
            serde_json::from_value::<ApprovalStatus>(json!(text)).unwrap(),
            status
        );
    }
    assert!("always_approve".parse::<PolicyEffect>().is_err());
}

fn rule(effect: PolicyEffect, tool: &str, pattern: &str) -> PolicyRule {
    PolicyRule {
        id: "r1".into(),
        tool_name: tool.into(),
        pattern: pattern.into(),
        effect,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn spec(approval: CapabilityApproval) -> CapabilitySpec {
    CapabilitySpec {
        name: "device.files".into(),
        description: "files".into(),
        input_schema: r#"{"type":"object"}"#.into(),
        approval,
        ..Default::default()
    }
}

#[test]
fn hard_denied_and_undeclared_operations_fail_safe() {
    let denied = decide_capability(
        &[rule(PolicyEffect::Allow, "*", "*")],
        Some(&spec(CapabilityApproval::Auto)),
        "payment.transfer",
        "device_request",
        &json!({}),
    );
    assert!(matches!(denied, Verdict::Deny { .. }));
    let undeclared = decide_capability(&[], None, "device.screen", "device_request", &json!({}));
    assert!(matches!(undeclared, Verdict::NeedsDecision { .. }));
    let auto = decide_capability(
        &[],
        Some(&spec(CapabilityApproval::Auto)),
        "body.motion",
        "body_motion",
        &json!({"motion": "nod"}),
    );
    assert_eq!(auto, Verdict::Allow);
}

#[test]
fn rules_decide_by_precedence() {
    let allow = rule(PolicyEffect::Allow, "*", "*");
    let confirm = rule(PolicyEffect::Confirm, "device_request", "/tmp/x");
    let verdict = decide_capability(
        &[allow.clone(), confirm],
        Some(&spec(CapabilityApproval::Auto)),
        "device.files",
        "device_request",
        &json!({"capability":"device.files","input":{"path": "/tmp/x"}}),
    );
    assert!(matches!(verdict, Verdict::HumanRequired { .. }));
    let allow_only = decide_capability(
        std::slice::from_ref(&allow),
        Some(&spec(CapabilityApproval::Auto)),
        "device.files",
        "device_request",
        &json!({"capability":"device.files","input":{"path": "/tmp/y"}}),
    );
    assert_eq!(allow_only, Verdict::Allow);
    let wildcard = decide_capability(
        &[rule(PolicyEffect::Confirm, "*", "*")],
        Some(&spec(CapabilityApproval::Auto)),
        "body.motion",
        "body_motion",
        &json!({"motion": "nod"}),
    );
    assert!(matches!(wildcard, Verdict::HumanRequired { .. }));
    let ask_llm = rule(PolicyEffect::AskLlm, "device_request", "/tmp/ask");
    let ask_over_allow = decide_capability(
        &[allow.clone(), ask_llm.clone()],
        Some(&spec(CapabilityApproval::Auto)),
        "device.files",
        "device_request",
        &json!({"capability":"device.files","input":{"path": "/tmp/ask"}}),
    );
    assert!(matches!(ask_over_allow, Verdict::NeedsDecision { .. }));
    let confirm_over_ask = decide_capability(
        &[
            ask_llm,
            rule(PolicyEffect::Confirm, "device_request", "/tmp/ask"),
        ],
        Some(&spec(CapabilityApproval::Auto)),
        "device.files",
        "device_request",
        &json!({"capability":"device.files","input":{"path": "/tmp/ask"}}),
    );
    assert!(matches!(confirm_over_ask, Verdict::HumanRequired { .. }));
    let mut deny = rule(PolicyEffect::Deny, "device_request", "*");
    deny.id = "r2".into();
    let verdict = decide_capability(
        &[rule(PolicyEffect::Confirm, "device_request", "*"), deny],
        Some(&spec(CapabilityApproval::Auto)),
        "device.files",
        "device_request",
        &json!({"capability":"device.files","input":{"path": "/tmp/x"}}),
    );
    assert!(matches!(verdict, Verdict::Deny { .. }));
    let verdict = decide_capability(
        &[allow],
        Some(&spec(CapabilityApproval::Confirm)),
        "device.files",
        "device_request",
        &json!({"capability":"device.files","input":{"path": "/tmp/x"}}),
    );
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn b02_rules_never_match_arbitrary_content_or_workspace_strings() {
    let allow_tmp = rule(PolicyEffect::Allow, "work.write", "/tmp/safe/*");
    let write_with_decoy_content = json!({
        "capability": "work.write",
        "input": {
            "path": "/etc/secret",
            "workspace": "/tmp/safe/ws",
            "content": "/tmp/safe/decoy"
        }
    });
    let verdict = decide_capability(
        &[allow_tmp],
        Some(&spec(CapabilityApproval::Confirm)),
        "work.write",
        "device_request",
        &write_with_decoy_content,
    );
    assert!(
        matches!(verdict, Verdict::NeedsDecision { .. }),
        "content/workspace must not trigger target rule match: {verdict:?}"
    );
}

#[test]
fn b19_hard_denied_blocks_system_paths_and_destructive_commands() {
    let allow_all = rule(PolicyEffect::Allow, "*", "*");
    let sys_write = decide_capability(
        std::slice::from_ref(&allow_all),
        Some(&spec(CapabilityApproval::Auto)),
        "work.write",
        "device_request",
        &json!({"capability": "work.write", "input": {"path": "C:\\Windows\\System32\\drivers\\etc\\hosts"}}),
    );
    assert!(matches!(sys_write, Verdict::Deny { .. }));

    let nuke_cmd = decide_capability(
        std::slice::from_ref(&allow_all),
        Some(&spec(CapabilityApproval::Auto)),
        "work.powershell",
        "device_request",
        &json!({"capability": "work.powershell", "input": {"command": "rm  -rf   / --no-preserve-root"}}),
    );
    assert!(matches!(nuke_cmd, Verdict::Deny { .. }));

    let mcp_sys = decide_action(
        &[allow_all],
        &CandidateAction::McpTool {
            server_id: "fs".into(),
            tool_name: "mcp_fs_write_file".into(),
            input: json!({"path": "/etc/passwd"}),
        },
    );
    assert!(matches!(mcp_sys, Verdict::Deny { .. }));
}

#[test]
fn b03_b04_mcp_and_web_search_require_gate_and_respect_rules() {
    let mcp_action = CandidateAction::McpTool {
        server_id: "notes".into(),
        tool_name: "mcp_notes_read".into(),
        input: json!({"path": "/notes/a.md"}),
    };
    assert!(matches!(
        decide_action(&[], &mcp_action),
        Verdict::NeedsDecision { .. }
    ));
    assert_eq!(
        decide_action(
            &[rule(PolicyEffect::Allow, "mcp:notes", "/notes/*")],
            &mcp_action
        ),
        Verdict::Allow
    );
    assert!(matches!(
        decide_action(
            &[rule(PolicyEffect::Confirm, "mcp_notes_read", "*")],
            &mcp_action
        ),
        Verdict::HumanRequired { .. }
    ));

    let search_action = CandidateAction::WebSearch {
        endpoint: "https://api.mwmbl.org/api/v2/search/".into(),
        query: "rust".into(),
        input: json!({"query": "rust"}),
    };
    assert!(matches!(
        decide_action(&[], &search_action),
        Verdict::NeedsDecision { .. }
    ));
    assert!(matches!(
        decide_action(
            &[rule(PolicyEffect::Deny, "web_search", "api.mwmbl.org")],
            &search_action
        ),
        Verdict::Deny { .. }
    ));
}
