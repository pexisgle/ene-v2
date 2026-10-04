use super::*;

pub async fn initialize(app: &HostApplication) -> Result<(), Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(app, &mut controls).await?;
    for mut p in c.settings.connections.clone() {
        p.credential_state = "missing".into();
        if let Some(reference) = c.credential_refs.get(&p.id)
            && let Ok(value) = app.0.credentials.load(reference.clone()).await
        {
            controls.secrets.insert(
                p.id.clone(),
                Secret {
                    value,
                    nonce: Uuid::new_v4().to_string(),
                },
            );
            p.credential_state = "stored".into();
        }
        update_connection(&mut c, p);
    }
    commit(app, &mut controls, c).await
}

impl HostApplication {
    pub async fn initialize(&self) -> Result<(), Error> {
        initialize(self).await
    }
}
