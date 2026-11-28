use super::*;

impl Network {
    pub(super) async fn execute(
        &self,
        command: w::DeviceOperation,
        device: &str,
        frame: &mut Option<crate::computer::Frame>,
    ) -> w::DeviceResult {
        if command.capability.starts_with(crate::workspace::PREFIX) {
            return self.execute_file(command, device).await;
        }
        let result = async {
            let args: Value = serde_json::from_str(&command.arguments_json)?;
            if command.capability == "desktop.input" {
                ensure!(
                    self.device
                        .capabilities
                        .iter()
                        .any(|cap| cap.name == command.capability),
                    "desktop input disabled"
                );
                return crate::computer::execute(frame, command.run_id.clone(), args).await;
            }
            let renderer = self.renderer.as_ref().context("renderer unavailable")?;
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .context("gesture name missing")?;
            let body = match command.capability.as_str() {
                "body.expression" => Body::Expression {
                    character: command.companion_id.clone(),
                    name: (name != "neutral").then(|| name.to_owned()),
                },
                "body.motion" => {
                    let file = match name {
                        "nod" => "acknowledge",
                        "wave" => "welcome-wave",
                        "listen" => "listen-active",
                        "bow" => "polite-bow",
                        "idle" => "idle-breath",
                        "sit" => "perch-idle",
                        other => other,
                    };
                    ensure!(
                        !file.is_empty()
                            && file
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                        "invalid motion name"
                    );
                    let path = self
                        .assets
                        .join("vrma")
                        .join(format!("{file}.vrma"))
                        .canonicalize()?;
                    Body::Motion {
                        character: command.companion_id.clone(),
                        motion: Some(path.to_string_lossy().into_owned()),
                        looping: false,
                    }
                }
                capability => {
                    let plugin = self
                        .plugins
                        .iter()
                        .find(|plugin| plugin.serves_body(capability))
                        .context("capability unsupported")?;
                    plugin
                        .body(&command, device, args, &self.assets, &self.stop)
                        .await?
                }
            };
            ensure!(
                renderer.command(body).await?,
                "renderer rejected the request"
            );
            Ok(None)
        }
        .await;
        device_result(command, device, result)
    }
    async fn execute_file(&self, command: w::DeviceOperation, device: &str) -> w::DeviceResult {
        use base64::Engine as _;
        let result = async {
            let workspace = self.workspace.clone().context("workspace not configured")?;
            let args = serde_json::from_str(&command.arguments_json)?;
            let mut output = workspace.execute(&command.capability, args).await?;
            let frame = if let Some(encoded) = output
                .as_object_mut()
                .and_then(|object| object.remove("image_base64"))
            {
                let encoded = encoded.as_str().context("invalid image data")?;
                let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
                let mut reader =
                    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(8192);
                limits.max_image_height = Some(8192);
                reader.limits(limits);
                Some(crate::computer::encode(reader.decode()?.into_rgb8())?)
            } else {
                None
            };
            Ok::<_, anyhow::Error>((output.to_string(), frame))
        }
        .await;
        match result {
            Ok((output, frame)) => {
                let mut result = device_result(command, device, Ok(frame));
                result.result_json = output;
                result
            }
            Err(_) => device_result(
                command,
                device,
                Err(anyhow::anyhow!("file operation failed")),
            ),
        }
    }
}

pub(super) fn device_result(
    command: w::DeviceOperation,
    device: &str,
    result: Result<Option<w::ScreenFrame>>,
) -> w::DeviceResult {
    let (outcome, result_json, frame) = match result {
        Ok(frame) => (
            w::Outcome::Completed,
            frame
                .as_ref()
                .map_or_else(
                    || json!({"applied":true}),
                    |frame| json!({"frame_id":frame.id,"width":frame.width,"height":frame.height}),
                )
                .to_string(),
            frame,
        ),
        Err(_) => (w::Outcome::Failed, String::new(), None),
    };
    w::DeviceResult {
        device_id: device.into(),
        generation: command.generation,
        operation_id: command.id,
        outcome: outcome.into(),
        result_json,
        frame: frame.into(),
        ..Default::default()
    }
}
