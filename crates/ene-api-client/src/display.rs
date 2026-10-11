//! A display grant is useful only while its conservative local deadline is live.
use super::{
    DeviceRpcClient, Error,
    rpc::{options, rpc_error},
};
use ene_rpc as w;
use tokio::time::{Duration, Instant};
pub struct DisplayLease {
    activation: w::Activation,
    deadline: Instant,
}
impl DisplayLease {
    pub async fn acquire(
        client: &DeviceRpcClient,
        companion: &str,
        device: &str,
    ) -> Result<Self, Error> {
        let began = Instant::now();
        let activation = client
            .activate_with_options(
                w::ActivationRequest {
                    companion_id: companion.into(),
                    device_id: device.into(),
                    ..Default::default()
                },
                options(5),
            )
            .await
            .map_err(rpc_error)?
            .into_owned();
        Self::received(activation, began)
    }
    fn received(activation: w::Activation, began: Instant) -> Result<Self, Error> {
        if activation.lease_ms <= 1000 || activation.lease_ms > 15_000 {
            return Err(Error::Protocol);
        }
        let deadline = began + Duration::from_millis(u64::from(activation.lease_ms - 1000));
        if Instant::now() >= deadline {
            return Err(Error::Busy);
        }
        Ok(Self {
            activation,
            deadline,
        })
    }
    pub fn is_valid(&self) -> bool {
        Instant::now() < self.deadline
    }
    pub fn activation(&self) -> &w::Activation {
        &self.activation
    }
    /// Renew every five seconds. On any failure, stop displaying immediately.
    pub async fn renew(&mut self, client: &DeviceRpcClient) -> Result<(), Error> {
        let began = Instant::now();
        // Invalidate locally until the new host grant has been confirmed.
        self.deadline = began;
        let next = client
            .activate_with_options(
                w::ActivationRequest {
                    companion_id: self.activation.companion_id.clone(),
                    device_id: self.activation.device_id.clone(),
                    expected_revision: Some(self.activation.revision),
                    ..Default::default()
                },
                options(5),
            )
            .await
            .map_err(rpc_error)?
            .into_owned();
        *self = Self::received(next, began)?;
        Ok(())
    }
    /// Call only after the UI has stopped displaying; this acknowledges release.
    pub async fn release(self, client: &DeviceRpcClient) -> Result<(), Error> {
        client
            .activate_with_options(
                w::ActivationRequest {
                    companion_id: self.activation.companion_id,
                    device_id: self.activation.device_id,
                    expected_revision: Some(self.activation.revision),
                    release: true,
                    ..Default::default()
                },
                options(5),
            )
            .await
            .map_err(rpc_error)?;
        Ok(())
    }
}
