use super::*;

impl Model {
    pub(crate) async fn respond<F, O, T>(
        &self,
        prompt: Prompt<'_>,
        tools: Vec<ToolDefinition>,
        cancel: &CancellationToken,
        progress: Option<tokio::sync::mpsc::Sender<Option<String>>>,
        mut invoke: F,
    ) -> Result<String>
    where
        F: FnMut(ToolCall) -> O,
        O: Future<Output = Result<T>>,
        T: Into<ToolOutput>,
    {
        let tool_bytes = serde_json::to_vec(&tools)?.len();
        let prompt = prompt.fit(self.budget().saturating_sub(tool_bytes + 2048))?;
        let mut request = CompletionRequest::new(format!(
            "[Current context]\n{}\n[Latest input]\n{}",
            prompt.dynamic, prompt.input
        ));
        request.chat_history.splice(
            0..0,
            std::iter::once(Message::system(prompt.system))
                .chain((!prompt.summary.is_empty()).then(|| {
                    Message::system(format!("Previous conversation summary: {}", prompt.summary))
                }))
                .chain(prompt.history.iter().map(|message| match message.role {
                    Role::User => Message::user(&message.text),
                    Role::Assistant => Message::assistant(&message.text),
                    Role::Trigger => Message::user(format!("[Host event] {}", message.text)),
                }))
                .map(Arc::new),
        );
        request.tools = tools.into_iter().map(Arc::new).collect();
        request.max_tokens = Some(1024);
        let mut tool_context = 0;
        let mut image_message: Option<usize> = None;
        for _ in 0..12 {
            let response = tokio::select! {
                () = cancel.cancelled() => anyhow::bail!("turn cancelled"),
                response = async {
                    let Some(progress) = &progress else { return self.complete(request.clone()).await; };
                    let adapter = self.completion()?;
                    let mut stream = adapter.stream(request.clone()).map_err(provider_error)?;
                    let mut text = String::new();
                    while let Some(item) = stream.next().await {
                        if let rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { text: delta, .. }) = item.map_err(provider_error)? {
                            text.push_str(&delta);
                            if text.len() >= 12 {
                                let _closed = progress.send(Some(std::mem::take(&mut text))).await;
                            }
                        }
                    }
                    if !text.is_empty() { let _closed = progress.send(Some(text)).await; }
                    stream.finish().await.map_err(provider_error)
                } => response?,
            };
            match response.stop() {
                StopReason::Stop | StopReason::Length => return Ok(response.text()),
                StopReason::ToolUse => {
                    if let Some(progress) = &progress {
                        let _closed = progress.send(None).await;
                    }
                    let message = response.message().context("missing tool call message")?;
                    tool_context += serde_json::to_vec(&message)?.len();
                    request.chat_history.push(Arc::new(message));
                    for call in response.tool_calls() {
                        let output = if request
                            .tools
                            .iter()
                            .any(|tool| tool.name == call.function.name)
                        {
                            invoke(call.clone())
                                .await
                                .map(Into::into)
                                .unwrap_or_else(|_| {
                                    String::from("Operation failed or was refused.").into()
                                })
                        } else {
                            String::from("Tool unavailable for this turn.").into()
                        };
                        let text = truncate(&output.text, 12_000);
                        tool_context += text.len() + 128;
                        let mut content = vec![ToolResultContent::text(text)];
                        if let Some(frame) = output.frame {
                            use base64::Engine as _;
                            if let Some(index) = image_message.replace(request.chat_history.len())
                                && let Message::User { content } =
                                    Arc::make_mut(&mut request.chat_history[index])
                            {
                                for part in content {
                                    if let UserContent::ToolResult(result) = part {
                                        result.content.retain(|item| {
                                            !matches!(item, ToolResultContent::Image(_))
                                        });
                                    }
                                }
                            }
                            content.push(ToolResultContent::Image(Image {
                                data: DocumentSourceKind::Base64(
                                    base64::engine::general_purpose::STANDARD.encode(frame.jpeg),
                                ),
                                media_type: Some(ImageMediaType::JPEG),
                                ..Default::default()
                            }));
                        }
                        request
                            .chat_history
                            .push(Arc::new(Message::tool_results(vec![call.result(content)])));
                    }
                    ensure!(
                        tool_context + usize::from(image_message.is_some()) * 8192
                            <= self.budget() / 2,
                        "tool outputs exceed context"
                    );
                }
                _ => anyhow::bail!("unhandled completion status"),
            }
        }
        anyhow::bail!("turn exceeded tool-call limit")
    }
}
