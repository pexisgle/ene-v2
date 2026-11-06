//! Operation dispatch, completion and activation receipts.
use super::*;
use buffa::Message;

impl Devices {
    pub async fn request(
        &self,
        request: w::DeviceOperationRequest,
    ) -> Result<w::DeviceOperationState, ConnectError> {
        validate(&request.operation_id)?;
        validate(&request.capability)?;
        if request.encoded_len() > 64 * 1024
            || !request.input.as_option().is_some_and(|document| {
                serde_json::from_str::<serde_json::Value>(&document.json).is_ok()
            })
        {
            return Err(ConnectError::invalid_argument("端末操作の入力が不正です"));
        }
        let this = self.clone();
        self.owned(async move {
            let state = this.lock().await?;
            let existing = this
                .0
                .store
                .device_operation(request.operation_id.clone())
                .await
                .map_err(|_| unavailable())?;
            if let Some(op) = existing {
                if *op.request != request {
                    return Err(conflict());
                }
                return Ok(this.current(&state, &op));
            }
            permitted(
                &this.device(&request.device_id).await?,
                Some(&request.capability),
            )?;
            let operations = this
                .0
                .store
                .device_operations()
                .await
                .map_err(|_| unavailable())?;
            if operations >= MAX_OPERATIONS {
                return Err(ConnectError::resource_exhausted("端末操作履歴の上限です"));
            }
            let connection = state
                .sessions
                .get(&request.device_id)
                .and_then(|session| session.connection.as_ref())
                .filter(|c| !c.queue.is_closed())
                .ok_or_else(conflict)?;
            let permit = connection
                .queue
                .clone()
                .try_reserve_owned()
                .map_err(|_| ConnectError::resource_exhausted("端末操作キューが満杯です"))?;
            let subscription_id = connection.id.clone();
            let result = w::DeviceOperationState {
                operation_id: request.operation_id.clone(),
                status: w::OperationStatus::Dispatched.into(),
                ..Default::default()
            };
            let command = w::DeviceOperation {
                operation_id: request.operation_id.clone(),
                capability: request.capability.clone(),
                input: request.input.clone(),
                subscription_id: subscription_id.clone(),
                ..Default::default()
            };
            let record = w::OperationRecord {
                request: request.into(),
                state: result.clone().into(),
                subscription_id,
                ..Default::default()
            };
            if !this
                .0
                .store
                .device_operation_insert(record)
                .await
                .map_err(|_| unavailable())?
            {
                return Err(conflict());
            }
            // The record is durable before the device can see the operation.
            permit.send(command);
            Ok(result)
        })
        .await
    }
    fn current(&self, state: &State, op: &w::OperationRecord) -> w::DeviceOperationState {
        let mut result = (*op.state).clone();
        if result.status == w::OperationStatus::Dispatched
            && !state
                .sessions
                .get(&op.request.device_id)
                .and_then(|session| session.connection.as_ref())
                .is_some_and(|c| c.id == op.subscription_id && !c.queue.is_closed())
        {
            result.status = w::OperationStatus::Uncertain.into();
        }
        result
    }
    pub async fn operation(
        &self,
        device_id: &str,
        id: &str,
    ) -> Result<w::DeviceOperationState, ConnectError> {
        let state = self.lock().await?;
        let op = self
            .0
            .store
            .device_operation(id.into())
            .await
            .map_err(|_| unavailable())?
            .filter(|op| op.request.device_id == device_id)
            .ok_or_else(missing)?;
        Ok(self.current(&state, &op))
    }
    pub async fn complete(
        &self,
        result: w::DeviceOperationResult,
    ) -> Result<w::DeviceOperationState, ConnectError> {
        if result.encoded_len() as usize > MAX_RESULT_BYTES
            || !result.output.as_option().is_some_and(|document| {
                serde_json::from_str::<serde_json::Value>(&document.json).is_ok()
            })
        {
            return Err(ConnectError::invalid_argument("端末操作の結果が不正です"));
        }
        let this = self.clone();
        self.owned(async move {
            let state = this.lock().await?;
            let previous = this
                .0
                .store
                .device_operation(result.operation_id.clone())
                .await
                .map_err(|_| unavailable())?
                .filter(|op| op.request.device_id == result.device_id)
                .ok_or_else(missing)?;
            if previous.state.status == w::OperationStatus::Completed {
                if previous.state.result.as_option() == Some(&result) {
                    return Ok((*previous.state).clone());
                }
                return Err(conflict());
            }
            permitted(
                &this.device(&result.device_id).await?,
                Some(&previous.request.capability),
            )?;
            if this.current(&state, &previous).status != w::OperationStatus::Dispatched
                || previous.subscription_id != result.subscription_id
            {
                return Err(conflict());
            }
            let response = w::DeviceOperationState {
                operation_id: result.operation_id.clone(),
                status: w::OperationStatus::Completed.into(),
                result: result.into(),
                ..Default::default()
            };
            let record = w::OperationRecord {
                state: response.clone().into(),
                ..previous
            };
            if !this
                .0
                .store
                .device_operation_complete(record)
                .await
                .map_err(|_| unavailable())?
            {
                // A revoke or another completion reached the row first.
                return Err(conflict());
            }
            Ok(response)
        })
        .await
    }
    pub async fn activate(
        &self,
        request: w::ActivationRequest,
    ) -> Result<w::Activation, ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            permitted(
                &this.device(&request.device_id).await?,
                Some("companion.display"),
            )?;
            if state
                .activation_not_before
                .is_some_and(|deadline| deadline > Instant::now())
            {
                return Err(conflict());
            }
            let old = state
                .activations
                .get(&request.companion_id)
                .filter(|(_, deadline)| *deadline > Instant::now());
            if let Some((old, _)) = old {
                if old.device_id != request.device_id
                    || request.expected_revision != Some(old.revision)
                {
                    return Err(conflict());
                }
            } else if request.release {
                return Err(conflict());
            }
            let revision = state
                .activation_revision
                .checked_add(1)
                .ok_or_else(unavailable)?;
            this.0
                .store
                .device_activate(revision)
                .await
                .map_err(|_| unavailable())?;
            state.activation_revision = revision;
            let result = w::Activation {
                companion_id: request.companion_id,
                device_id: request.device_id,
                revision,
                lease_ms: if request.release { 0 } else { 15_000 },
                ..Default::default()
            };
            if request.release {
                state.activations.remove(&result.companion_id);
            } else {
                state.activations.insert(
                    result.companion_id.clone(),
                    (
                        result.clone(),
                        Instant::now() + Duration::from_millis(result.lease_ms.into()),
                    ),
                );
            }
            Ok(result)
        })
        .await
    }
}
