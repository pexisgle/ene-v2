//! Settings service.

use super::super::Services;
use super::rule_from;
use crate::host::db::Store;
use crate::host::notify::Notifier;
use crate::host::wire;
use connectrpc::RequestContext;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use ene_rpc as w;

pub(super) async fn list_policy_rules(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::PolicyRules> {
    services
        .read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PolicyRules {
                items: store
                    .policy_repo()
                    .policy_rules()?
                    .iter()
                    .map(wire::policy_rule)
                    .collect(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn update_policy_rules(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::UpdatePolicyRulesRequest>,
) -> ServiceResult<w::PolicyRules> {
    let request = request.to_owned_message();
    let mut rules = Vec::with_capacity(request.add_or_update.len());
    for rule in request.add_or_update {
        rules.push(rule_from(rule)?);
    }
    let remove_ids = request.remove_ids;
    services
        .call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let updated = store
                .policy_repo()
                .apply_policy_rules(&rules, &remove_ids)?;
            let items: Vec<w::PolicyRule> = updated.iter().map(wire::policy_rule).collect();
            app.world_mut().resource_mut::<Notifier>().push(
                None,
                None,
                w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
                    rules: items.clone(),
                    ..Default::default()
                })),
            );
            Ok(w::PolicyRules {
                items,
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn list_policy_texts(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::PolicyTexts> {
    services
        .read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PolicyTexts {
                items: store
                    .policy_repo()
                    .policy_texts()?
                    .iter()
                    .map(wire::policy_text)
                    .collect(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn update_policy_texts(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::UpdatePolicyTextsRequest>,
) -> ServiceResult<w::PolicyTexts> {
    let request = request.to_owned_message();
    let mut texts = Vec::with_capacity(request.add_or_update.len());
    for text in request.add_or_update {
        texts.push(wire::policy_text_from(&text));
    }
    let remove_ids = request.remove_ids;
    services
        .call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let updated = store
                .policy_repo()
                .apply_policy_texts(&texts, &remove_ids)?;
            let items: Vec<w::PolicyText> = updated.iter().map(wire::policy_text).collect();
            app.world_mut().resource_mut::<Notifier>().push(
                None,
                None,
                w::notification::Event::PolicyTextsUpdated(Box::new(w::PolicyTextsUpdated {
                    texts: items.clone(),
                    ..Default::default()
                })),
            );
            Ok(w::PolicyTexts {
                items,
                ..Default::default()
            })
        })
        .await
}
