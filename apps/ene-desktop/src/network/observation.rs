use super::*;

pub(super) async fn audio_worker(
    clients: Clients,
    device: String,
    mut enabled: watch::Receiver<bool>,
    updates: mpsc::Sender<Update>,
    stop: CancellationToken,
) {
    loop {
        let yes = *enabled.borrow_and_update();
        if yes {
            let stream = async {
                let mut stream = clients.voice.output(subscription(&device, 0)).await?;
                while let Some(message) = stream.message::<w::AudioOutput>().await? {
                    if updates
                        .send(Update::Audio(message.to_owned_message()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok::<_, connectrpc::ConnectError>(())
            };
            tokio::select! {
                () = stop.cancelled() => break,
                changed = enabled.changed() => { if changed.is_err() { break; } }
                _ = stream => tokio::time::sleep(Duration::from_millis(500)).await,
            }
            let _closed = updates
                .send(Update::Audio(w::AudioOutput {
                    stop: true,
                    ..Default::default()
                }))
                .await;
        } else {
            tokio::select! {
                () = stop.cancelled() => break,
                changed = enabled.changed() => { if changed.is_err() { break; } }
            }
        }
    }
}

pub(super) async fn observe_enabled(
    clients: Clients,
    device: String,
    mut active: watch::Receiver<Vec<w::Activation>>,
    mut enabled: watch::Receiver<bool>,
    mut plugins: watch::Receiver<std::sync::Arc<[crate::plugins::ClientPlugin]>>,
    stop: CancellationToken,
) {
    let mut previous = None;
    #[cfg(target_os = "linux")]
    let mut observer = None::<crate::observe_linux::Observer>;
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            changed = enabled.changed() => {
                if changed.is_err() { break; }
                previous = None;
                #[cfg(target_os = "linux")]
                { observer = None; }
            }
            changed = active.changed() => {
                if changed.is_err() { break; }
                previous = None;
            }
            changed = plugins.changed() => {
                if changed.is_err() { break; }
                previous = None;
            }
            () = tokio::time::sleep(Duration::from_secs(5)) => {
                let targets = active.borrow().iter().filter(|active| active.device_id == device)
                    .map(|active| active.generation).collect::<Vec<_>>();
                if !*enabled.borrow() || targets.is_empty() { continue; }
                #[cfg(target_os = "linux")]
                let observation = {
                    if observer.is_none() { observer = crate::observe_linux::Observer::open(); }
                    observer.as_mut().map(crate::observe_linux::Observer::observe).unwrap_or_default()
                };
                #[cfg(windows)]
                let observation = ene_platform_os::observe();
                #[cfg(not(any(windows,target_os = "linux")))]
                let observation = ene_platform_os::Observation::default();
                let ene_platform_os::ObservedWindow::Window(window) = observation.window else {
                    if previous.take().is_some() {
                        report_observation(&clients, &device, &targets, "", "").await;
                    }
                    continue;
                };
                let idle = observation.idle_seconds.map(|idle| idle >= 120);
                let input = (window.app_id, window.title, idle);
                if previous.as_ref() == Some(&input) { continue; }
                let mut text = json!({"application":input.0, "idle":idle}).to_string();
                let mut interpretation = String::new();
                let loaded = plugins.borrow().clone();
                if let Some(plugin) = loaded.iter().find(|plugin| plugin.serves_perception()) {
                    let result = tokio::select! {
                        () = stop.cancelled() => break,
                        _ = active.changed() => { previous = None; continue; }
                        _ = enabled.changed() => { previous = None; continue; }
                        _ = plugins.changed() => { previous = None; continue; }
                        result = plugin.observe(&input.0, &input.1, idle, &stop) => result,
                    };
                    if let Ok((observed, inferred)) = result {
                        text = observed;
                        interpretation = inferred;
                    }
                }
                if !*enabled.borrow() || active.has_changed().unwrap_or(true) { continue; }
                report_observation(&clients, &device, &targets, &text, &interpretation).await;
                previous = Some(input);
            }
        }
    }
}

async fn report_observation(
    clients: &Clients,
    device: &str,
    targets: &[u64],
    text: &str,
    interpretation: &str,
) {
    for &generation in targets {
        let _stale = clients
            .device
            .report_perception(w::Perception {
                device_id: device.into(),
                generation,
                observation: text.into(),
                interpretation: interpretation.into(),
                ..Default::default()
            })
            .await;
    }
}
