use super::*;

impl Window {
    pub(crate) fn setup_preset(
        &mut self,
        preset: crate::provider_choices::Preset,
        window: &mut GpuiWindow,
        cx: &mut Context<Self>,
    ) {
        self.onboarding.select_preset(preset, window, cx);
        cx.notify();
    }

    pub(crate) fn start_setup(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.submit(action) {
            self.onboarding.started();
            self.notice.clear();
        }
        cx.notify();
    }

    pub(crate) fn apply_settings(&mut self, action: Action, cx: &mut Context<Self>) {
        match &action {
            Action::Eyes(value) => {
                self.local.eyes = *value;
                self.persist();
            }
            Action::Input(value) => {
                self.local.input = *value;
                self.persist();
            }
            _ => {}
        }
        self.submit(action);
        cx.notify();
    }

    pub(crate) fn editor(&mut self) -> &mut Settings {
        &mut self.settings
    }
}

impl Window {
    pub(crate) fn toggle_voice_test(&mut self, cx: &mut Context<Self>) {
        let recording = !self.settings.voice.recording;
        if let Some(capture) = self.microphone() {
            if recording {
                capture.automatic.store(false, Ordering::Relaxed);
                capture.diagnostic.store(true, Ordering::Relaxed);
                capture.held.store(true, Ordering::Relaxed);
            } else {
                capture.held.store(false, Ordering::Relaxed);
                capture.diagnostic.store(false, Ordering::Relaxed);
            }
            self.settings.voice.recording = recording;
        }
        cx.notify();
    }

    pub(crate) fn test_speech(&mut self, text: String, cx: &mut Context<Self>) {
        if !text.is_empty() && !self.companion.is_empty() {
            self.submit(Action::TestSpeech(self.companion.clone(), text));
        }
        cx.notify();
    }

    pub(crate) fn choose_workspace(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("作業フォルダーを選ぶ".into()),
        });
        // Use the owner entity because the settings window may close while the dialog is active.
        cx.spawn(async move |owner, cx| {
            if let Ok(Ok(Some(mut paths))) = selection.await
                && let Some(path) = paths.pop()
            {
                let _closed = owner.update(cx, |owner, cx| {
                    owner.apply_settings(Action::Workspace(path), cx)
                });
            }
        })
        .detach();
    }

    pub(crate) fn confirm_reset(&self, window: &mut GpuiWindow, cx: &mut Context<Self>) {
        if self.managed_host {
            confirm(
                window,
                cx,
                "会話・作業履歴と記憶を消去しますか？",
                Some("実行中の作業は終了します。"),
                Action::ResetHistory,
            );
        }
    }

    pub(crate) fn confirm_delete(
        &self,
        id: &str,
        name: &str,
        revision: i64,
        window: &mut GpuiWindow,
        cx: &mut Context<Self>,
    ) {
        confirm(
            window,
            cx,
            &format!("「{name}」を削除しますか？"),
            None,
            Action::DeleteCompanion(w::DeleteRequest {
                id: id.into(),
                mutation: mutation(revision),
                ..Default::default()
            }),
        );
    }

    pub(super) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.submit(Action::Settings);
        cx.spawn(async move |owner, cx| {
            cx.update(|cx| {
                let Some(owner) = owner.upgrade() else { return };
                if let Some(handle) = owner.read(cx).settings_window
                    && handle
                        .update(cx, |_, window, _| window.activate_window())
                        .is_ok()
                {
                    return;
                }
                let source = owner.clone();
                let bounds = gpui::Bounds::centered(None, gpui::size(px(680.), px(650.)), cx);
                let opened = gpui::open_window(
                    gpui::WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("設定 — Ene".into()),
                            ..Default::default()
                        }),
                        focus: true,
                        ..Default::default()
                    },
                    cx,
                    move |_, cx| cx.new(|cx| SettingsWindow::new(source, cx)),
                );
                match opened {
                    Ok((handle, _)) => owner.update(cx, |owner, _| {
                        owner.settings_window = Some(handle);
                    }),
                    Err(_) => owner.update(cx, |owner, cx| {
                        owner.notice = "設定ウィンドウを開けませんでした。".into();
                        cx.notify();
                    }),
                }
            });
        })
        .detach();
    }
}

fn confirm(
    window: &mut GpuiWindow,
    cx: &mut Context<Window>,
    message: &str,
    detail: Option<&str>,
    action: Action,
) {
    let choice = window.prompt(
        gpui::PromptLevel::Warning,
        message,
        detail,
        &["キャンセル", "削除"],
        cx,
    );
    cx.spawn(async move |owner, cx| {
        if choice.await == Ok(1) {
            let _closed = owner.update(cx, |owner, cx| owner.apply_settings(action, cx));
        }
    })
    .detach();
}

struct SettingsWindow {
    owner: Entity<Window>,
    _observe: Subscription,
}

impl SettingsWindow {
    fn new(owner: Entity<Window>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&owner, |_, _, cx| cx.notify());
        Self {
            owner,
            _observe: observe,
        }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut GpuiWindow, cx: &mut Context<Self>) -> impl IntoElement {
        self.owner.update(cx, |owner, cx| {
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgb(BLUSH))
                .text_color(rgb(INK))
                .child(owner.settings.render(
                    cx,
                    &owner.snapshot,
                    &owner.companion,
                    &owner.local,
                    owner.managed_host,
                ))
                .when(!owner.notice.is_empty(), |content| {
                    content.child(div().p_3().bg(rgb(0xfff3df)).child(owner.notice.clone()))
                })
        })
    }
}
