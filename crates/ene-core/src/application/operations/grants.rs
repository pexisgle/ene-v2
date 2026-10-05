use super::*;
pub async fn post_grant(
    app: HostApplication,
    key: String,
    request: GrantInput,
) -> Result<(Outcome, Value), Error> {
    let body = serde_json::to_value(&request).map_err(|_| Error::invalid())?;
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
    if request.consent.settings_revision != c.settings.revision
        || request.consent.provider_revision != policy.revision
    {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    if request.kind != "data_transfer"
        || !["conversation", "connection_check"].contains(&request.purpose.as_str())
        || request
            .destinations
            .iter()
            .any(|d| !policy.destinations.contains(d))
        || request
            .data_kinds
            .iter()
            .any(|d| !["conversation", "metadata", "test_input"].contains(&d.as_str()))
        || request.destinations.is_empty()
        || request.data_kinds.is_empty()
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let grant = Grant {
        id: Uuid::new_v4().to_string(),
        companion_id: request.companion_id,
        kind: request.kind,
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
    refresh(&app, &mut c);
    let response = json!({"grant":grant,"questions":[]});
    let saved = c.clone();
    let saved_response = response.clone();
    let mut boundary = app.0.boundary.lock().await;
    app.0
        .repository
        .save_configuration(
            saved,
            Some(Receipt {
                key,
                target: "/v1/grants".into(),
                request: body,
                outcome: Outcome::Created,
                response: saved_response,
            }),
        )
        .await?;
    controls.config = Some(c.clone());
    publish(&mut boundary, &app, "grants.updated", None, json!(c.grants));
    for companion in c.companions.values() {
        publish(
            &mut boundary,
            &app,
            "companion.updated",
            Some(&companion.id),
            json!(companion),
        );
    }
    Ok((Outcome::Created, response))
}
pub async fn delete_grant(app: HostApplication, id: String) -> Result<(), Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if let Some(g) = c.grants.iter_mut().find(|g| g.id == id)
        && g.status == GrantStatus::Active
    {
        g.status = GrantStatus::Invalidated;
        g.revision += 1;
        g.invalidation_reason = Some("revoked".into());
        commit(&app, &mut controls, c.clone()).await?;
        cancel_unpermitted(&controls, &c);
    }
    Ok(())
}

pub async fn patch_grant(
    app: HostApplication,
    id: String,
    request: GrantPatch,
) -> Result<ene_api_types::GrantResult, Error> {
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
    if let Some(destinations) = request.destinations {
        if destinations
            .iter()
            .any(|d| !policy.destinations.contains(d))
        {
            return Err(Error::invalid());
        }
        g.destinations = destinations;
    }
    if let Some(data) = request.data_kinds {
        if data
            .iter()
            .any(|d| !["conversation", "metadata", "test_input"].contains(&d.as_str()))
        {
            return Err(Error::invalid());
        }
        g.data_kinds = data;
    }
    g.revision += 1;
    let grant = g.clone();
    commit(&app, &mut controls, c.clone()).await?;
    cancel_unpermitted(&controls, &c);
    Ok(ene_api_types::GrantResult {
        grant: grant.into(),
        questions: vec![],
    })
}
