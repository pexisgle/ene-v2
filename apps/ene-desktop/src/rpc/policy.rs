//! Approval resolution, policy rules/texts, and host plugin management RPCs.

use super::{RpcDispatcher, failure};
use crate::update::Update;
use ene_client_core::domain::{ApprovalDecision, PolicyRulesUpdate, PolicyTextsUpdate};
use ene_rpc as w;

pub(crate) fn load_plugins(rpc: &RpcDispatcher, epoch: u64) {
    rpc.call(
        epoch,
        "プラグイン一覧の取得",
        request!(settings.list_plugins(w::Empty::default())),
        |response| plugins_update(response.view()),
    );
}

pub(crate) fn resolve_approval(
    rpc: &RpcDispatcher,
    epoch: u64,
    request_id: String,
    decision: ApprovalDecision,
) {
    let stale = request_id.clone();
    let always_approve = decision == ApprovalDecision::AlwaysApprove;
    let wire_decision = w::ApprovalDecision::from(decision);
    rpc.call_with(
        epoch,
        "承認の送信",
        request!(settings.submit_approval(w::SubmitApprovalRequest {
            request_id,
            decision: wire_decision.into(),
            ..Default::default()
        })),
        move |response| {
            let reply = response.into_owned();
            Update::Resolved {
                rule_warning: (always_approve && !reply.rule_saved).then(|| {
                    reply
                        .rule_error
                        .unwrap_or_else(|| "常に許可ルールを保存できませんでした。".into())
                }),
            }
        },
        move |context, error| match error.code {
            connectrpc::ErrorCode::Aborted => Update::ApprovalStale {
                message: format!("操作確認 {stale} は期限切れか別端末で処理されました。"),
            },
            _ => failure(context, error),
        },
    );
}

pub(crate) fn update_policy_rules(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    update: PolicyRulesUpdate,
) {
    let request: w::UpdatePolicyRulesRequest = update.into();
    rpc.call(
        epoch,
        context,
        request!(settings.update_policy_rules(request)),
        |_| Update::Resnapshot,
    );
}

pub(crate) fn update_policy_texts(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    update: PolicyTextsUpdate,
) {
    let request: w::UpdatePolicyTextsRequest = update.into();
    rpc.call(
        epoch,
        context,
        request!(settings.update_policy_texts(request)),
        |_| Update::Resnapshot,
    );
}

pub(crate) fn install_plugin(rpc: &RpcDispatcher, epoch: u64, source_path: String) {
    rpc.call(
        epoch,
        "プラグインの導入",
        request!(settings.install_plugin(w::InstallPluginRequest {
            source_path,
            ..Default::default()
        })),
        |_| Update::PluginsChanged,
    );
}

pub(crate) fn set_plugin_enabled(
    rpc: &RpcDispatcher,
    epoch: u64,
    plugin_id: String,
    enabled: bool,
) {
    rpc.call(
        epoch,
        "プラグイン状態の更新",
        request!(settings.set_plugin_enabled(w::SetPluginEnabledRequest {
            plugin_id,
            enabled,
            ..Default::default()
        })),
        |_| Update::PluginsChanged,
    );
}

pub(crate) fn remove_plugin(rpc: &RpcDispatcher, epoch: u64, plugin_id: String) {
    rpc.call(
        epoch,
        "プラグインの削除",
        request!(settings.remove_plugin(w::RemovePluginRequest {
            plugin_id,
            ..Default::default()
        })),
        |_| Update::PluginsChanged,
    );
}

fn plugins_update(list: &w::PluginListView<'_>) -> Update {
    Update::Plugins(list.plugins.iter().map(Into::into).collect())
}
