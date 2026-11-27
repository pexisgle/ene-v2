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
        (ApprovalStatus::Evaluating, "evaluating"),
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
        scope: None,
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

#[test]
fn a_saved_search_approval_requires_the_host_authenticated_scope() {
    let endpoint = "https://search.example:8443/query";
    let arguments = json!({"query": "test"});
    let original = SearchScope::new([3; 32], "Search example · test".into());
    let allow = original.allow_rule();
    assert_eq!(allow.search_label(), Some("Search example · test"));
    let search = |scope| CandidateAction::WebSearch {
        endpoint: endpoint.into(),
        query: "test".into(),
        input: arguments.clone(),
        scope,
    };
    let valid = search(Some(original));
    assert_eq!(
        decide_action(std::slice::from_ref(&allow), &valid),
        Verdict::Allow
    );
    let (canonical, digest) = canonicalise("web_search", &arguments);
    let legacy_scope = serde_json::json!({ "endpoint": endpoint, "arguments": arguments });
    let legacy_hash = format!(
        "search_sha256:{}",
        hex_digest(&canonicalise("web_search", &legacy_scope).1)
    );
    for legacy in [
        canonical,
        format!("exact_sha256:{}", hex_digest(&digest)),
        legacy_hash.clone(),
    ] {
        assert!(matches!(
            decide_action(&[rule(PolicyEffect::Allow, "web_search", &legacy)], &valid),
            Verdict::NeedsDecision { .. }
        ));
    }
    assert!(matches!(
        decide_action(
            &[
                allow.clone(),
                rule(PolicyEffect::Deny, "web_search", &legacy_hash)
            ],
            &valid
        ),
        Verdict::Deny { .. }
    ));
    for changed in [
        search(None),
        search(Some(SearchScope::new(
            [4; 32],
            "Search example · test".into(),
        ))),
    ] {
        assert!(matches!(
            decide_action(std::slice::from_ref(&allow), &changed),
            Verdict::NeedsDecision { .. }
        ));
    }
}

/// The badge on an approval card must not disagree with what the policy
/// engine blocks, so the level is derived from the same capability rules.
#[test]
fn risk_levels_follow_the_hard_deny_and_mutation_rules() {
    for capability in ["payment.transfer", "securities.trade", "banking.wire"] {
        assert_eq!(
            RiskLevel::of(Some(capability), "device_request", "{}"),
            RiskLevel::High,
            "{capability} is hard-denied, so its card must say High"
        );
    }

    for (capability, tool, args, expected) in [
        (
            "work.delete",
            "work_delete",
            r#"{"path":"a.txt"}"#,
            RiskLevel::High,
        ),
        (
            "work.shell",
            "work_powershell",
            r#"{"command":"rm a"}"#,
            RiskLevel::High,
        ),
        (
            "work.write",
            "work_write",
            r#"{"path":"a.txt"}"#,
            RiskLevel::Moderate,
        ),
        (
            "work.edit",
            "work_edit",
            r#"{"path":"a.txt"}"#,
            RiskLevel::Moderate,
        ),
        (
            "device.files",
            "device_files",
            r#"{"path":"a.txt"}"#,
            RiskLevel::Moderate,
        ),
        (
            "work.read",
            "work_read",
            r#"{"path":"a.txt"}"#,
            RiskLevel::Low,
        ),
        (
            "web.search",
            "web_search",
            r#"{"query":"rust"}"#,
            RiskLevel::Low,
        ),
    ] {
        assert_eq!(
            RiskLevel::of(Some(capability), tool, args),
            expected,
            "{capability} / {tool}"
        );
    }
}

/// A tool without a capability is judged by its name and arguments, which is
/// how an MCP or plugin tool reaches the card.
#[test]
fn a_tool_without_a_capability_is_judged_by_its_name_and_arguments() {
    assert_eq!(
        RiskLevel::of(None, "mcp__github__delete_file", "{}"),
        RiskLevel::High
    );
    assert_eq!(
        RiskLevel::of(None, "mcp__fs__write_file", r#"{"path":"a.txt"}"#),
        RiskLevel::Moderate
    );
    assert_eq!(
        RiskLevel::of(None, "mcp__fs__read_file", r#"{"path":"a.txt"}"#),
        RiskLevel::Low
    );
    // An empty capability falls back to the tool name rather than panicking.
    assert_eq!(
        RiskLevel::of(Some(""), "mcp__x__remove_thing", "{}"),
        RiskLevel::High
    );
}
