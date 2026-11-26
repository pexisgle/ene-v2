//! Settings service.

use super::super::Services;
use crate::host::error::HostError;
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
        .with_store(|store| {
            Ok(w::PolicyRules {
                items: store
                    .policy_repo()
                    .policy_rules()?
                    .iter()
                    .map(Into::into)
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
        rules.push(ene_domain::PolicyRule::try_from(rule).map_err(HostError::from)?);
    }
    let remove_ids = request.remove_ids;
    services
        .change(move |services| {
            let store = &services.store;
            let updated = store
                .policy_repo()
                .apply_policy_rules(&rules, &remove_ids)?;
            let items: Vec<w::PolicyRule> = updated.iter().map(Into::into).collect();
            services.notifier.push(
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
        .with_store(|store| {
            Ok(w::PolicyTexts {
                items: store
                    .policy_repo()
                    .policy_texts()?
                    .iter()
                    .map(Into::into)
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
    let texts: Vec<_> = request.add_or_update.into_iter().map(Into::into).collect();
    let remove_ids = request.remove_ids;
    services
        .change(move |services| {
            let store = &services.store;
            let updated = store
                .policy_repo()
                .apply_policy_texts(&texts, &remove_ids)?;
            let items: Vec<w::PolicyText> = updated.iter().map(Into::into).collect();
            services.notifier.push(
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
