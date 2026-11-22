//! Approval resolution, policy rules/texts, and host plugin management RPCs.

use super::{RpcDispatcher, decode, failure};
use crate::update::Update;
use buffa::view::MessageView;
use ene_client_core::domain::{ApprovalDecision, PolicyRulesUpdate, PolicyTextsUpdate};
use ene_rpc as w;

pub(crate) fn load_approvals(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "承認一覧の取得",
        async move {
            let response = host
                .settings
                .list_pending_approvals(w::Empty::default())
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        |approvals| Update::Approvals(approvals.items.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn resolve_approval(
    rpc: &RpcDispatcher,
    epoch: u64,
    request_id: String,
    decision: ApprovalDecision,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let resolved = request_id.clone();
    let stale = request_id.clone();
    let always_approve = decision == ApprovalDecision::AlwaysApprove;
    let wire_decision = w::ApprovalDecision::from(decision);
    rpc.spawn_with(
        epoch,
        "承認の送信",
        async move {
            let response = host
                .settings
                .submit_approval(w::SubmitApprovalRequest {
                    request_id,
                    decision: wire_decision.into(),
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        move |reply| Update::Resolved {
            request_id: resolved,
            rule_warning: (always_approve && !reply.rule_saved).then(|| {
                reply
                    .rule_error
                    .unwrap_or_else(|| "常に許可ルールを保存できませんでした。".into())
            }),
        },
        move |context, error| match error.code {
            connectrpc::ErrorCode::Aborted => Update::ApprovalStale {
                message: format!("操作確認 {stale} は期限切れか別端末で処理されました。"),
            },
            _ => failure(context, error),
        },
    );
}

pub(crate) fn load_policy_rules(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "許可ルールの取得",
        async move {
            let response = host.settings.list_policy_rules(w::Empty::default()).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |rules| Update::PolicyRules(rules.items.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn update_policy_rules(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    update: PolicyRulesUpdate,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let request: w::UpdatePolicyRulesRequest = update.into();
    rpc.spawn(
        epoch,
        context,
        async move {
            let response = host.settings.update_policy_rules(request).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |rules| Update::PolicyRules(rules.items.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn load_policy_texts(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "自由文ポリシーの取得",
        async move {
            let response = host.settings.list_policy_texts(w::Empty::default()).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |texts| Update::PolicyTexts(texts.items.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn update_policy_texts(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    update: PolicyTextsUpdate,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let request: w::UpdatePolicyTextsRequest = update.into();
    rpc.spawn(
        epoch,
        context,
        async move {
            let response = host.settings.update_policy_texts(request).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |texts| Update::PolicyTexts(texts.items.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn load_plugins(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "プラグイン一覧の取得",
        async move {
            let response = host.settings.list_plugins(w::Empty::default()).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |list| Update::Plugins(list.plugins.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn install_plugin(rpc: &RpcDispatcher, epoch: u64, source_path: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "プラグインの導入",
        async move {
            let _ = host
                .settings
                .install_plugin(w::InstallPluginRequest {
                    source_path,
                    ..Default::default()
                })
                .await?;
            let list = host.settings.list_plugins(w::Empty::default()).await?;
            list.view().to_owned_message().map_err(decode)
        },
        |list| Update::Plugins(list.plugins.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn set_plugin_enabled(
    rpc: &RpcDispatcher,
    epoch: u64,
    plugin_id: String,
    enabled: bool,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "プラグイン状態の更新",
        async move {
            let _ = host
                .settings
                .set_plugin_enabled(w::SetPluginEnabledRequest {
                    plugin_id,
                    enabled,
                    ..Default::default()
                })
                .await?;
            let list = host.settings.list_plugins(w::Empty::default()).await?;
            list.view().to_owned_message().map_err(decode)
        },
        |list| Update::Plugins(list.plugins.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn remove_plugin(rpc: &RpcDispatcher, epoch: u64, plugin_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "プラグインの削除",
        async move {
            let _ = host
                .settings
                .remove_plugin(w::RemovePluginRequest {
                    plugin_id,
                    ..Default::default()
                })
                .await?;
            let list = host.settings.list_plugins(w::Empty::default()).await?;
            list.view().to_owned_message().map_err(decode)
        },
        |list| Update::Plugins(list.plugins.into_iter().map(Into::into).collect()),
    );
}
