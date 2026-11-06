//! Device registration, approval and permission changes.
use super::*;

impl Devices {
    pub async fn register(
        &self,
        request: w::RegisterDeviceRequest,
    ) -> Result<w::DeviceRegistration, ConnectError> {
        validate(&request.name)?;
        if request.capabilities.len() > 32 {
            return Err(ConnectError::resource_exhausted("端末能力が多すぎます"));
        }
        for c in &request.capabilities {
            validate(c)?;
        }
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            if this
                .0
                .store
                .device_records()
                .await
                .map_err(|_| unavailable())?
                .len()
                >= MAX_DEVICES
            {
                return Err(ConnectError::resource_exhausted("端末数の上限です"));
            }
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let d = w::Device {
                id: Uuid::new_v4().to_string(),
                name: request.name,
                capabilities: request.capabilities,
                ..Default::default()
            };
            if this
                .0
                .store
                .device_register(d.clone(), Sha256::digest(token.as_bytes()).to_vec())
                .await
                .is_err()
            {
                return Err(fail_closed(&mut state));
            }
            Ok(w::DeviceRegistration {
                device: d.into(),
                token,
                ..Default::default()
            })
        })
        .await
    }
    pub async fn list(&self) -> Result<w::Devices, ConnectError> {
        let _state = self.lock().await?;
        Ok(w::Devices {
            items: self
                .0
                .store
                .device_records()
                .await
                .map_err(|_| unavailable())?
                .into_iter()
                .map(|(device, _)| device)
                .collect(),
            ..Default::default()
        })
    }
    pub async fn approve(&self, id: String) -> Result<w::Device, ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            let mut device = this.device(&id).await?;
            if device.revoked {
                return Err(conflict());
            }
            device.approved = true;
            if this
                .0
                .store
                .device_write(device.clone(), None, false)
                .await
                .is_err()
            {
                return Err(fail_closed(&mut state));
            }
            Ok(device)
        })
        .await
    }
    pub async fn revoke(&self, id: String) -> Result<(), ConnectError> {
        self.permission(id, None, false).await
    }
    pub async fn permission(
        &self,
        id: String,
        capability: Option<String>,
        allowed: bool,
    ) -> Result<(), ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            let mut device = this.device(&id).await?;
            if let Some(c) = &capability {
                if device.revoked || !device.capabilities.contains(c) {
                    return Err(denied());
                }
                device.permissions.retain(|p| p != c);
                if allowed {
                    device.permissions.push(c.clone());
                }
            } else {
                device.approved = false;
                device.revoked = true;
                device.permissions.clear();
            }
            // The device and the operations its change revokes move together.
            if this
                .0
                .store
                .device_write(device, capability.clone(), !allowed)
                .await
                .is_err()
            {
                return Err(fail_closed(&mut state));
            }
            if let Some(session) = state.sessions.get_mut(&id) {
                session.changed.cancel();
                // The old token stays cancelled for the streams it ends; a
                // reconnecting subscription must not inherit it.
                session.changed = CancellationToken::new();
                if !allowed {
                    session.connection = None;
                }
            }
            Ok(())
        })
        .await
    }
    pub async fn subscribe(&self, id: String) -> Result<Subscription, ConnectError> {
        let mut state = self.lock().await?;
        permitted(&self.device(&id).await?, None)?;
        let entry = state.sessions.entry(id).or_default();
        if entry
            .connection
            .as_ref()
            .is_some_and(|c| !c.queue.is_closed())
        {
            return Err(conflict());
        }
        let session = Uuid::new_v4().to_string();
        let (queue, incoming) = mpsc::channel(32);
        let changed = entry.changed.clone();
        entry.connection = Some(Connection {
            id: session.clone(),
            queue,
        });
        Ok(Subscription {
            id: session,
            queue: incoming,
            changed,
        })
    }
}
