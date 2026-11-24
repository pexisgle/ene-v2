//! Capability operation dispatch for [`Session`].
//!
//! The window-command handlers live in [`super::window_commands`].

use super::Session;
use super::stream::{AdmitError, Claim, admits};
#[cfg(windows)]
use crate::avatar::Avatar;
use crate::{
    avatar, capabilities,
    host::Host,
    perception::{Watch, WatchParseError},
};
use buffa::view::MessageView;
use ene_rpc as w;
use std::time::Duration;

/// One validated `body.motion` request, before anything is sent anywhere.
#[derive(Debug, PartialEq)]
pub(super) struct MotionPlan {
    pub(super) motion: String,
    /// The clip to show. `idle` can fall back to the rest pose.
    pub(super) clip: Option<std::path::PathBuf>,
    /// The face to hold, `neutral` to clear it, or nothing to leave it alone.
    pub(super) expression: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum MotionPlanError {
    #[error("body.motion requires a `motion` name")]
    MissingMotion,
    #[error("`{0}` is not a declared motion preset")]
    UnknownPreset(String),
    #[error("`{0}` is not a declared expression")]
    UnknownExpression(String),
    #[error("the motion asset for `{0}` is not installed, so nothing was shown")]
    MissingMotionAsset(String),
}

impl MotionPlan {
    pub(super) fn parse(input: &serde_json::Value) -> Result<Self, MotionPlanError> {
        let motion = input
            .get("motion")
            .and_then(serde_json::Value::as_str)
            .ok_or(MotionPlanError::MissingMotion)?;
        if !avatar::MOTIONS.iter().any(|(preset, _)| *preset == motion) {
            return Err(MotionPlanError::UnknownPreset(motion.to_owned()));
        }
        let expression = input
            .get("expression")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(name) = &expression
            && !capabilities::EXPRESSIONS.contains(&name.as_str())
        {
            return Err(MotionPlanError::UnknownExpression(name.clone()));
        }
        let clip = avatar::motion(motion);
        if clip.is_none() && motion != "idle" {
            return Err(MotionPlanError::MissingMotionAsset(motion.to_owned()));
        }
        Ok(Self {
            motion: motion.to_owned(),
            clip,
            expression,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OperationError {
    #[error(transparent)]
    Admit(#[from] AdmitError),
    #[cfg(windows)]
    #[error(transparent)]
    ComputerUse(#[from] crate::computer::ComputerUseError),
    #[error(transparent)]
    WorkTool(#[from] crate::work_tools::WorkToolError),
    #[error(transparent)]
    ClientPlugin(#[from] crate::plugins::ClientPluginError),
    #[error(transparent)]
    Avatar(#[from] avatar::AvatarError),
    #[error(transparent)]
    MotionPlan(#[from] MotionPlanError),
    #[error(transparent)]
    Watch(#[from] WatchParseError),
    #[error("the operation input is not JSON: {0}")]
    InvalidInputJson(#[from] serde_json::Error),
    #[error("this client is not serving any companion")]
    NoCompanionServed,
    #[error("this client does not implement `{0}`")]
    UnimplementedCapability(String),
    #[cfg(windows)]
    #[error("Computer Use is disabled or the device stream is not ready")]
    ComputerUseUnavailable,
    #[cfg(windows)]
    #[error("Computer Use requires a work session")]
    ComputerUseMissingWorkSession,
    #[cfg(windows)]
    #[error("Computer Use requires a bound run")]
    ComputerUseMissingRun,
    #[cfg(windows)]
    #[error("could not verify the active run in time")]
    RunVerifyTimeout,
    #[cfg(windows)]
    #[error("could not verify the active run")]
    RunVerifyFailed,
    #[cfg(windows)]
    #[error("the work session is unavailable")]
    WorkSessionUnavailable,
    #[cfg(windows)]
    #[error("the Computer Use operation is no longer active on this device")]
    ComputerUseInactive,
    #[error("body.look_at requires yaw_deg in [-90, 90] and pitch_deg in [-60, 60]")]
    InvalidLookAtAngles,
    #[error("no renderer is configured, so gaze was not updated")]
    NoRendererForLookAt,
    #[error("body.perch requires numeric `{0}`")]
    MissingPerchCoord(&'static str),
    #[error("body.perch coordinates must be finite")]
    NonFinitePerchCoord,
    #[error("no renderer is configured, so the avatar could not perch")]
    NoRendererForPerch,
    #[error("no model is configured for the renderer")]
    NoModelForMotion,
    #[error("no renderer is configured, so no motion was shown")]
    NoRendererForMotion,
    #[error("「目」はオフです")]
    EyesDisabled,
    #[error("workspace operations require a work session")]
    WorkMissingSession,
    #[error("the workspace is busy")]
    WorkspaceBusy,
    #[error("no workspace is configured")]
    NoWorkspace,
}

impl Session {
    pub(super) async fn report_operation(host: &Host, result: w::DeviceOperationResult) {
        let operation_id = result.operation_id.clone();
        match host.client.complete_operation(result).await {
            Ok(response) => {
                if let Ok(state) = response.view().to_owned_message()
                    && state.status == w::DeviceOperationStatus::Uncertain
                {
                    tracing::warn!(operation = %operation_id, "the host could not confirm the operation result");
                }
            }
            Err(error) => {
                tracing::warn!(operation = %operation_id, "the operation result was not reported: {error}")
            }
        }
    }

    #[cfg(windows)]
    pub(super) async fn computer_operation(
        &mut self,
        host: &Host,
        operation: &w::DeviceOperation,
    ) -> Result<serde_json::Value, OperationError> {
        if !self.devices.computer_enabled || self.transport.subscription.is_empty() {
            return Err(OperationError::ComputerUseUnavailable);
        }
        let work_id = operation
            .work_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or(OperationError::ComputerUseMissingWorkSession)?;
        let run_id = operation
            .run_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or(OperationError::ComputerUseMissingRun)?;
        let (work, state) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::try_join!(
                host.work.get_work(w::WorkRequest {
                    work_id: work_id.to_owned(),
                    ..Default::default()
                }),
                host.client.get_operation(w::Id {
                    id: operation.operation_id.clone(),
                    ..Default::default()
                }),
            )
        })
        .await
        .map_err(|_| OperationError::RunVerifyTimeout)?
        .map_err(|_| OperationError::RunVerifyFailed)?;
        let work = work.view();
        let session = work
            .session
            .as_option()
            .ok_or(OperationError::WorkSessionUnavailable)?;
        if session.client_id != self.transport.options.id
            || !self.transport.bindings.contains_key(session.companion_id)
            || !work
                .runs
                .iter()
                .any(|run| run.run_id == run_id && run.state == w::RunStatus::Running)
            || state.view().status != w::DeviceOperationStatus::Dispatched
        {
            return Err(OperationError::ComputerUseInactive);
        }
        let input = Self::operation_input(operation)?;
        Ok(crate::computer::execute(
            &mut self.devices.computer_frame,
            run_id,
            input,
            self.devices.avatar.as_ref().map(Avatar::pid),
        )
        .await?)
    }

    pub(super) fn operation_result(
        client_id: &str,
        operation: &w::DeviceOperation,
        outcome: Result<serde_json::Value, OperationError>,
    ) -> w::DeviceOperationResult {
        match &outcome {
            Ok(_) => tracing::info!(
                operation = %operation.operation_id,
                capability = %operation.capability,
                "device operation applied"
            ),
            Err(reason) => tracing::warn!(
                operation = %operation.operation_id,
                capability = %operation.capability,
                reason = %reason,
                "device operation not applied"
            ),
        }
        let (success, output, failure) = match outcome {
            Ok(output) => (true, output.to_string(), None),
            Err(reason) => (
                false,
                String::new(),
                Some(w::Failure {
                    code: {
                        #[cfg(windows)]
                        if operation.capability == w::COMPUTER_USE
                            && matches!(
                                reason,
                                OperationError::ComputerUse(
                                    crate::computer::ComputerUseError::PartialInput,
                                )
                            )
                        {
                            "operation_uncertain"
                        } else {
                            "operation_failed"
                        }
                        #[cfg(not(windows))]
                        "operation_failed"
                    }
                    .into(),
                    message: reason.to_string(),
                    ..Default::default()
                }),
            ),
        };
        w::DeviceOperationResult {
            client_id: client_id.to_owned(),
            operation_id: operation.operation_id.clone(),
            success,
            output,
            failure: failure.into(),
            ..Default::default()
        }
    }

    /// Why this client must not execute the operation, if it must not.
    pub(super) fn refusal(&self, operation: &w::DeviceOperation) -> Option<AdmitError> {
        let claim = Claim {
            client_id: &operation.client_id,
            subscription_id: &operation.subscription_id,
            work_id: operation.work_id.as_deref(),
        };
        let binding = if !operation.companion_id.is_empty() {
            self.transport.bindings.get(&operation.companion_id)
        } else {
            self.transport.focused_binding()
        };
        admits(
            &self.transport.options.id,
            &self.transport.subscription,
            binding,
            &claim,
        )
        .err()
    }

    pub(super) fn operation_input(
        operation: &w::DeviceOperation,
    ) -> Result<serde_json::Value, OperationError> {
        if operation.input.trim().is_empty() {
            Ok(serde_json::json!({}))
        } else {
            Ok(serde_json::from_str(&operation.input)?)
        }
    }

    fn target_companion_id(
        &self,
        operation: &w::DeviceOperation,
    ) -> Result<String, OperationError> {
        if !operation.companion_id.is_empty() {
            Ok(operation.companion_id.clone())
        } else {
            self.transport
                .focused_companion_id()
                .ok_or(OperationError::NoCompanionServed)
                .map(str::to_owned)
        }
    }

    /// Runs an avatar or perception capability on the session thread.
    pub(super) async fn execute(
        &mut self,
        operation: &w::DeviceOperation,
    ) -> Result<serde_json::Value, OperationError> {
        let input = Self::operation_input(operation)?;
        match operation.capability.as_str() {
            "body.motion" => {
                let target = self.target_companion_id(operation)?;
                self.motion(&target, &input).await
            }
            "body.look_at" => {
                let target = self.target_companion_id(operation)?;
                self.look_at(&target, &input).await
            }
            "body.perch" => {
                let target = self.target_companion_id(operation)?;
                self.perch(&target, &input).await
            }
            "eye.watch" => self.watch(&input),
            other if other.starts_with("plugin.") => {
                Ok(self.devices.client_plugins.execute(other, input).await?)
            }
            other => Err(OperationError::UnimplementedCapability(other.to_owned())),
        }
    }

    async fn look_at(
        &mut self,
        companion_id: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OperationError> {
        let reset = input
            .get("reset")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let (yaw_deg, pitch_deg) = if reset {
            (None, None)
        } else {
            let yaw = input
                .get("yaw_deg")
                .and_then(serde_json::Value::as_f64)
                .map_or(0., |v| v as f32);
            let pitch = input
                .get("pitch_deg")
                .and_then(serde_json::Value::as_f64)
                .map_or(0., |v| v as f32);
            if !yaw.is_finite() || !pitch.is_finite() || yaw.abs() > 90. || pitch.abs() > 60. {
                return Err(OperationError::InvalidLookAtAngles);
            }
            (Some(yaw), Some(pitch))
        };
        let avatar = self
            .devices
            .avatar
            .as_mut()
            .ok_or(OperationError::NoRendererForLookAt)?;
        avatar.look_at(companion_id, yaw_deg, pitch_deg).await?;
        Ok(serde_json::json!({
            "reset": reset,
            "yaw_deg": yaw_deg,
            "pitch_deg": pitch_deg,
        }))
    }

    async fn perch(
        &mut self,
        companion_id: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OperationError> {
        let x = input
            .get("x")
            .and_then(serde_json::Value::as_f64)
            .map(|v| v as f32)
            .ok_or(OperationError::MissingPerchCoord("x"))?;
        let y = input
            .get("y")
            .and_then(serde_json::Value::as_f64)
            .map(|v| v as f32)
            .ok_or(OperationError::MissingPerchCoord("y"))?;
        if !x.is_finite() || !y.is_finite() || x.abs() > 1_000_000. || y.abs() > 1_000_000. {
            return Err(OperationError::NonFinitePerchCoord);
        }
        let monitor = input
            .get("monitor")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let posture = input
            .get("posture")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let model = self.transport.model_for(companion_id);
        let avatar = self
            .devices
            .avatar
            .as_mut()
            .ok_or(OperationError::NoRendererForPerch)?;
        avatar.place(companion_id, monitor.clone(), [x, y]).await?;
        if let Some(posture_name) = posture.as_deref()
            && matches!(posture_name, "sit_idle" | "sit_swing_legs" | "idle")
            && let Some(model_path) = model
        {
            let clip = avatar::motion(posture_name);
            if clip.is_some() || posture_name == "idle" {
                let _spawned = avatar
                    .show_preset(companion_id, &model_path, posture_name, clip.as_deref())
                    .await;
            }
        }
        Ok(serde_json::json!({
            "x": x,
            "y": y,
            "monitor": monitor,
            "posture": posture,
        }))
    }

    /// Shows a motion on `companion_id`. Without a renderer the operation fails.
    async fn motion(
        &mut self,
        companion_id: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OperationError> {
        let plan = MotionPlan::parse(input)?;
        let model = self
            .transport
            .model_for(companion_id)
            .ok_or(OperationError::NoModelForMotion)?;
        let avatar = self
            .devices
            .avatar
            .as_mut()
            .ok_or(OperationError::NoRendererForMotion)?;
        if avatar
            .show_preset(companion_id, &model, &plan.motion, plan.clip.as_deref())
            .await?
        {
            avatar
                .await_state(companion_id, Duration::from_secs(2))
                .await?;
        }
        // A face is separate from the motion: `neutral` clears it, and the
        // renderer keeps it across later motion changes.
        if let Some(name) = &plan.expression {
            let face = (name != "neutral").then_some(name.as_str());
            avatar.expression(companion_id, face).await?;
        }
        Ok(serde_json::json!({
            "applied": true,
            "motion": plan.motion,
            "expression": plan.expression,
        }))
    }

    /// Applies or clears a watch rule.
    fn watch(&mut self, input: &serde_json::Value) -> Result<serde_json::Value, OperationError> {
        if !self.devices.eyes_enabled {
            return Err(OperationError::EyesDisabled);
        }
        let watch = Watch::parse(input)?;
        let enabled = watch.enabled;
        self.devices
            .sensor
            .apply(if enabled { Some(watch) } else { None });
        Ok(serde_json::json!({"applied": true, "enabled": enabled}))
    }
}
