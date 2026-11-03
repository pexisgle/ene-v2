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
            if state.journal.devices.len() >= 64 {
                return Err(ConnectError::resource_exhausted("端末数の上限です"));
            }
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let d = w::Device {
                id: Uuid::new_v4().to_string(),
                name: request.name,
                capabilities: request.capabilities,
                ..Default::default()
            };
            let mut journal = state.journal.clone();
            journal.devices.push(w::DeviceRecord {
                device: d.clone().into(),
                token_hash: Sha256::digest(token.as_bytes()).to_vec(),
                ..Default::default()
            });
            this.commit(&mut state, journal).await?;
            Ok(w::DeviceRegistration {
                device: d.into(),
                token,
                ..Default::default()
            })
        })
        .await
    }
    pub async fn list(&self) -> Result<w::Devices, ConnectError> {
        Ok(w::Devices {
            items: self
                .lock()
                .await?
                .journal
                .devices
                .iter()
                .map(|d| (*d.device).clone())
                .collect(),
            ..Default::default()
        })
    }
    pub async fn approve(&self, id: String) -> Result<w::Device, ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            let mut journal = state.journal.clone();
            let d = device_mut(&mut journal, &id)?;
            if d.revoked {
                return Err(conflict());
            }
            d.approved = true;
            let result = d.clone();
            this.commit(&mut state, journal).await?;
            Ok(result)
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
            let mut journal = state.journal.clone();
            let d = device_mut(&mut journal, &id)?;
            if let Some(c) = &capability {
                if d.revoked || !d.capabilities.contains(c) {
                    return Err(denied());
                }
                d.permissions.retain(|p| p != c);
                if allowed {
                    d.permissions.push(c.clone());
                }
            } else {
                d.approved = false;
                d.revoked = true;
                d.permissions.clear();
            }
            if !allowed {
                for op in &mut journal.operations {
                    if op.request.device_id == id
                        && capability
                            .as_ref()
                            .is_none_or(|c| op.request.capability == *c)
                        && op.state.status == w::OperationStatus::Dispatched
                    {
                        op.state.as_option_mut().ok_or_else(unavailable)?.status =
                            w::OperationStatus::Revoked.into();
                    }
                }
            }
            this.commit(&mut state, journal).await?;
            if let Some(signal) = state.changes.get(&id) {
                signal.cancel();
            }
            if !allowed {
                state.connections.remove(&id);
            }
            Ok(())
        })
        .await
    }
    pub async fn changes(&self, id: &str) -> Result<CancellationToken, ConnectError> {
        let mut state = self.lock().await?;
        permitted(&state.journal, id, None)?;
        Ok(state.changes.entry(id.into()).or_default().clone())
    }
    pub async fn subscribe(&self, id: String) -> Result<Subscription, ConnectError> {
        let mut state = self.lock().await?;
        permitted(&state.journal, &id, None)?;
        if state
            .connections
            .get(&id)
            .is_some_and(|c| !c.queue.is_closed())
        {
            return Err(conflict());
        }
        let session = Uuid::new_v4().to_string();
        let (queue, incoming) = mpsc::channel(32);
        let changed = state.changes.entry(id.clone()).or_default().clone();
        state.connections.insert(
            id,
            Connection {
                id: session.clone(),
                queue,
            },
        );
        Ok(Subscription {
            id: session,
            queue: incoming,
            changed,
        })
    }
}
