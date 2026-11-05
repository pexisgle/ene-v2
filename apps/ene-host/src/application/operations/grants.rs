use super::*;
pub async fn post_grant(
    app: HostApplication,
    key: String,
    request: GrantInput,
) -> Result<(Outcome, Value), Error> {
    let consent = request.consent.as_option().ok_or_else(Error::invalid)?;
    // Keep the historical receipt body shape so grants accepted by an older
    // host still replay with the same key.
    let body = json!({
        "companion_id": request.companion_id, "kind": "data_transfer",
        "connection_id": request.connection_id, "destinations": request.destinations,
        "data_kinds": request.data_kinds, "purpose": request.purpose,
        "consent": { "settings_revision": consent.settings_revision, "provider_revision": consent.provider_revision },
    });
    // Persisted receipt identity: retain this key across transport/version changes.
    let key_lookup = key.clone();
    let lookup = body.clone();
    let mut controls = app.0.controls.lock().await;
    if let Some(receipt) = app
        .0
        .repository
        .receipt(key_lookup, "/v1/grants".into(), lookup)
        .await?
    {
        return Ok(receipt);
    }
    let mut c = config(&app, &mut controls).await?;
    if !c.companions.contains_key(&request.companion_id) {
        return Err(Error::new(ErrorKind::NotFound));
    }
    let connection = rules::connection(&c, &request.connection_id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let policy = app.0.providers.policy(&connection)?;
    if consent.settings_revision != c.settings.revision
        || consent.provider_revision != policy.revision
    {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    if !["conversation", "connection_check", "task", "memory"].contains(&request.purpose.as_str())
        || request
            .destinations
            .iter()
            .any(|d| !policy.destinations.contains(d))
        || request.data_kinds.iter().any(|d| {
            !["conversation", "metadata", "test_input", "task", "memory"].contains(&d.as_str())
        })
        || request.destinations.is_empty()
        || request.data_kinds.is_empty()
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let grant = Grant {
        id: Uuid::new_v4().to_string(),
        companion_id: request.companion_id,
        connection_id: request.connection_id,
        destinations: request.destinations,
        data_kinds: request.data_kinds,
        purpose: GrantPurpose::parse(&request.purpose)?,
        status: GrantStatus::Active,
        revision: 1,
        invalidation_reason: None,
        // Bind the exact policy whose revision was confirmed above. A reload
        // after this point must not attach a newer implementation to consent.
        connection_fingerprint: policy.fingerprint,
    };
    c.grants.push(grant.clone());
    let response = json!(ene_rpc::GrantResult {
        grant: ene_rpc::Grant::from(grant).into(),
        ..Default::default()
    });
    crate::application::commit::configuration(
        &app,
        &mut controls,
        c,
        crate::application::commit::ReceiptEffect::CreateGrant(Receipt {
            key,
            target: "/v1/grants".into(),
            request: body,
            outcome: Outcome::Created,
            response: response.clone(),
        }),
    )
    .await?;
    Ok((Outcome::Created, response))
}
pub async fn delete_grant(app: HostApplication, id: String) -> Result<(), Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if let Some(g) = c.grants.iter_mut().find(|g| g.id == id)
        && g.status == GrantStatus::Active
    {
        g.status = GrantStatus::Invalidated;
        g.revision = next_revision(g.revision)?;
        g.invalidation_reason = Some("revoked".into());
        commit(&app, &mut controls, c.clone()).await?;
    }
    Ok(())
}

pub async fn patch_grant(
    app: HostApplication,
    id: String,
    request: GrantPatch,
) -> Result<ene_rpc::GrantResult, Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let connection_id = c
        .grants
        .iter()
        .find(|g| g.id == id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?
        .connection_id
        .clone();
    let p = rules::connection(&c, &connection_id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let policy = app.0.providers.policy(&p)?;
    let g = c
        .grants
        .iter_mut()
        .find(|g| g.id == id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    if g.revision != request.revision {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    if g.status != GrantStatus::Active {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    if let Some(destinations) = request.destinations.into_option() {
        let destinations = destinations.values;
        if destinations
            .iter()
            .any(|d| !policy.destinations.contains(d))
        {
            return Err(Error::invalid());
        }
        g.destinations = destinations;
    }
    if let Some(data) = request.data_kinds.into_option() {
        let data = data.values;
        if data.iter().any(|d| {
            !["conversation", "metadata", "test_input", "task", "memory"].contains(&d.as_str())
        }) {
            return Err(Error::invalid());
        }
        g.data_kinds = data;
    }
    g.revision = next_revision(g.revision)?;
    let grant = g.clone();
    commit(&app, &mut controls, c.clone()).await?;
    Ok(ene_rpc::GrantResult {
        grant: ene_rpc::Grant::from(grant).into(),
        ..Default::default()
    })
}
