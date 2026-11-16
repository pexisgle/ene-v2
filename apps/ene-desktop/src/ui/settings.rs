//! Settings: this device, the host's connections, credentials, declared models,
//! and the approval cards.
//!
//! The window never holds a credential beyond the field that collected it: it is
//! sent with `RegisterCredentials` and the field is cleared. Connections are
//! edited with the revision they were read at, and an edit wipes the host's
//! recorded credential state, which the panel says out loud.

use super::{Desktop, Tone, button, card, decode, failure, line, muted, page_title};
use crate::{capabilities, update::Update};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::{AnyElement, ClickEvent, Context, SharedString, div, prelude::*, px, rgb};

impl Desktop {
    /// Fetches the provider catalog once; it is a static list on the host.
    pub(crate) fn load_providers(&mut self, _cx: &mut Context<Self>) {
        if self.model.providers.is_some() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プロバイダ一覧の取得",
            async move {
                let response = host.settings.list_providers(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |providers| Update::Providers(providers.items),
        );
    }

    /// Fetches the declared model ids of every LLM connection not yet cached.
    pub(crate) fn load_models(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        for connection in settings
            .connections
            .iter()
            .filter(|connection| connection.kind == w::ConnectionKind::Llm)
            .filter(|connection| !self.model.models.contains_key(&connection.id))
        {
            let id = connection.id.clone();
            let request_id = id.clone();
            let host = host.clone();
            self.request(
                "モデル一覧の取得",
                async move {
                    let response = host
                        .settings
                        .list_models(w::ModelsRequest {
                            id: request_id,
                            ..Default::default()
                        })
                        .await?;
                    response.view().to_owned_message().map_err(decode)
                },
                move |models| Update::Models {
                    connection_id: id,
                    models: models.items,
                },
            );
        }
    }

    /// Refetches the pending approvals, which is the only way to learn that one
    /// was resolved elsewhere: the host sends no structured event for it.
    pub(crate) fn load_approvals(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "承認一覧の取得",
            async move {
                let response = host
                    .settings
                    .list_pending_approvals(w::Empty::default())
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |approvals| Update::Approvals(approvals.items),
        );
    }

    /// Sets a connection's enabled flag, sending the revision it was read at.
    fn set_enabled(&mut self, connection: w::Connection, enabled: bool, cx: &mut Context<Self>) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "接続設定の更新",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![w::ConnectionInput {
                            id: connection.id.clone(),
                            name: connection.name.clone(),
                            enabled,
                            expected_revision: connection.revision,
                            kind: connection.kind,
                            config: connection.config.clone(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |settings| {
                Update::Notification(Box::new(w::Notification {
                    kind: "settings".into(),
                    event: Some(w::notification::Event::SettingsUpdated(Box::new(
                        w::SettingsUpdated {
                            settings: Some(settings).into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    /// Probes one connection. The host persists the result but sends no event,
    /// so the answer is carried back here.
    fn check(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let id = connection_id.clone();
        let companion = self.model.companion_id().map(str::to_owned);
        self.request(
            "接続確認",
            async move {
                let response = host
                    .settings
                    .check_connection(w::CheckRequest {
                        id: connection_id,
                        request_key: uuid::Uuid::new_v4().to_string(),
                        companion_id: companion,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |result| Update::Checked {
                connection_id: id,
                check: result.check.into_option().unwrap_or_default(),
            },
        );
        cx.notify();
    }

    /// Sends a secret with the RPC that exists for it, then forgets it.
    fn register_secret(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let key = format!("secret:{connection_id}");
        let secret = self
            .drafts
            .get(&key)
            .map(|draft| draft.read(cx).text().to_owned())
            .unwrap_or_default();
        if secret.is_empty() {
            return;
        }
        let id = connection_id.clone();
        self.request(
            "資格情報の登録",
            async move {
                let response = host
                    .settings
                    .register_credentials(w::CredentialRequest {
                        id: connection_id,
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |_result| Update::CredentialRegistered { connection_id: id },
        );
        cx.notify();
    }

    fn revoke_secret(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "資格情報の解除",
            async move {
                host.settings
                    .revoke_credentials(w::Id {
                        id: connection_id,
                        ..Default::default()
                    })
                    .await
            },
            |_response| Update::Resnapshot,
        );
        cx.notify();
    }

    /// Adds an LLM connection from the host's provider catalog.
    fn add_connection(&mut self, provider: w::LlmProvider, cx: &mut Context<Self>) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let model_ids: Vec<String> = self
            .drafts
            .get("new:models")
            .map(|draft| {
                draft
                    .read(cx)
                    .text()
                    .split(',')
                    .map(|item| item.trim().to_owned())
                    .filter(|item| !item.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let name = provider.name.clone();
        let adapter = provider.adapter.clone();
        let endpoint = provider.endpoint.clone();
        self.request(
            "接続の追加",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![w::ConnectionInput {
                            id: uuid::Uuid::new_v4().to_string(),
                            name,
                            enabled: true,
                            expected_revision: 0,
                            kind: w::ConnectionKind::Llm.into(),
                            config: Some(w::ConnectionConfig {
                                config: Some(w::connection_config::Config::Llm(Box::new(
                                    w::LlmConnection {
                                        endpoint: Some(w::ConnectionEndpoint {
                                            adapter,
                                            endpoint,
                                            // An empty schema validates any object,
                                            // which is the honest default when the
                                            // provider declares no option fields here.
                                            options_schema: String::new(),
                                            options: "{}".into(),
                                            ..Default::default()
                                        })
                                        .into(),
                                        model_ids,
                                        ..Default::default()
                                    },
                                ))),
                                ..Default::default()
                            })
                            .into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |settings| {
                Update::Notification(Box::new(w::Notification {
                    kind: "settings".into(),
                    event: Some(w::notification::Event::SettingsUpdated(Box::new(
                        w::SettingsUpdated {
                            settings: Some(settings).into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    /// The VOICEVOX speaker this window is configured for.
    fn speaker(&mut self, cx: &mut Context<Self>) -> u32 {
        self.draft(cx, "tts:speaker", "話者番号", "TextField")
            .read(cx)
            .text()
            .trim()
            .parse()
            .unwrap_or(3)
    }

    /// Adds a local VOICEVOX connection.
    ///
    /// The engine listens on a fixed local port, needs no credential, and speaks
    /// Japanese sentence by sentence, which is what this client's companion
    /// does. It is the documented default for speech here, so it gets a button
    /// instead of a hand-written connection form.
    fn add_voicevox(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let speaker = self.speaker(cx);
        self.request(
            "VOICEVOX接続の追加",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![w::ConnectionInput {
                            id: "voicevox".into(),
                            name: "VOICEVOX（ローカル）".into(),
                            enabled: true,
                            expected_revision: 0,
                            kind: w::ConnectionKind::Tts.into(),
                            config: Some(w::ConnectionConfig {
                                config: Some(w::connection_config::Config::Tts(Box::new(
                                    w::TtsConnection {
                                        endpoint: Some(w::ConnectionEndpoint {
                                            adapter: "voicevox".into(),
                                            endpoint: "http://127.0.0.1:50021".into(),
                                            options_schema: String::new(),
                                            options: format!("{{\"speaker\":{speaker}}}"),
                                            ..Default::default()
                                        })
                                        .into(),
                                        voice_ids: vec![speaker.to_string()],
                                        ..Default::default()
                                    },
                                ))),
                                ..Default::default()
                            })
                            .into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |settings| {
                Update::Notification(Box::new(w::Notification {
                    kind: "settings".into(),
                    event: Some(w::notification::Event::SettingsUpdated(Box::new(
                        w::SettingsUpdated {
                            settings: Some(settings).into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    /// Changes the speaker of an existing TTS connection, keeping its endpoint.
    fn set_speaker(&mut self, connection: w::Connection, speaker: u32, cx: &mut Context<Self>) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let Some(mut config) = connection
            .config
            .as_option()
            .and_then(|config| config.config.clone())
        else {
            return;
        };
        if let w::connection_config::Config::Tts(tts) = &mut config {
            if let Some(endpoint) = tts.endpoint.as_option_mut() {
                endpoint.options = format!("{{\"speaker\":{speaker}}}");
            }
            tts.voice_ids = vec![speaker.to_string()];
        }
        self.request(
            "話者の変更",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![w::ConnectionInput {
                            id: connection.id,
                            name: connection.name,
                            enabled: connection.enabled,
                            expected_revision: connection.revision,
                            kind: connection.kind,
                            config: Some(w::ConnectionConfig {
                                config: Some(config),
                                ..Default::default()
                            })
                            .into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |settings| {
                Update::Notification(Box::new(w::Notification {
                    kind: "settings".into(),
                    event: Some(w::notification::Event::SettingsUpdated(Box::new(
                        w::SettingsUpdated {
                            settings: Some(settings).into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    /// The speech setup: the TTS connection and the voice it uses.
    fn render_voice(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let speaker = self.draft(cx, "tts:speaker", "話者番号", "TextField");
        let connection = self.model.settings().and_then(|settings| {
            settings
                .connections
                .iter()
                .find(|connection| connection.kind == w::ConnectionKind::Tts)
                .cloned()
        });
        let mut body: Vec<AnyElement> = vec![speaker.into_any_element()];
        match connection {
            Some(connection) => {
                let endpoint = connection
                    .config
                    .as_option()
                    .and_then(|config| config.config.as_ref())
                    .and_then(|config| match config {
                        w::connection_config::Config::Tts(tts) => tts
                            .endpoint
                            .as_option()
                            .map(|endpoint| endpoint.endpoint.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                body.push(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .child(format!(
                            "{} {}",
                            connection.name,
                            if connection.enabled {
                                "有効"
                            } else {
                                "無効"
                            }
                        ))
                        .into_any_element(),
                );
                body.push(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .child(endpoint)
                        .into_any_element(),
                );
                let chosen = connection.clone();
                let value = self.speaker(cx);
                body.push(button(
                    cx,
                    "voicevox-speaker",
                    "話者を変更",
                    Tone::Plain,
                    self.live(),
                    move |this, _window, cx| this.set_speaker(chosen.clone(), value, cx),
                ));
            }
            None => {
                let value = self.speaker(cx);
                body.push(button(
                    cx,
                    "voicevox-add",
                    "VOICEVOX を追加",
                    Tone::Primary,
                    self.live(),
                    move |this, _window, cx| this.add_voicevox(cx),
                ));
                let _ = value;
                body.push(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .child("ローカルの VOICEVOX ENGINE（既定は 127.0.0.1:50021）を話者として使います。")
                        .into_any_element(),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(card())
            .border_1()
            .border_color(line())
            .children(body)
            .into_any_element()
    }

    /// Settings is its own two-pane surface: a short list of what can be changed,
    /// and one page per area. The conversation is one click away in the sidebar.
    pub(crate) fn render_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut nav: Vec<AnyElement> = Vec::new();
        for section in super::SettingsSection::ALL {
            let selected = self.settings_section == section;
            nav.push(
                div()
                    .id(section.key())
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .when(selected, |this| {
                        this.bg(rgb(0xecebe8)).text_color(rgb(0x1c1917))
                    })
                    .when(!selected, |this| {
                        this.text_color(rgb(0x57534e))
                            .hover(|this| this.bg(rgb(0xf5f5f4)))
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.settings_section = section;
                        this.open_panel(cx);
                        cx.notify();
                    }))
                    .child(section.label())
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_row()
            .flex_1()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(180.))
                    .h_full()
                    .gap_1()
                    .p_2()
                    .border_r_1()
                    .border_color(line())
                    .bg(rgb(0xfaf9f7))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(muted())
                            .child("設定"),
                    )
                    .children(nav),
            )
            .child(
                div()
                    .id("settings-pane")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_3()
                    .px_6()
                    .py_4()
                    .overflow_y_scroll()
                    .child(self.render_settings_page(cx)),
            )
            .into_any_element()
    }

    /// Which terminal serves this companion right now, and the one way to take it.
    fn render_activation(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.model.is_active_client(&self.options.id);
        let id = self.model.companion_id().map(str::to_owned);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(card())
            .border_1()
            .border_color(line())
            .child(div().flex_1().text_sm().child(if active {
                "この端末が会話を受け持っています"
            } else {
                "この端末は会話を受け持っていません"
            }))
            .when(!active, |this| {
                this.child(button(
                    cx,
                    "activate",
                    "この端末で話す",
                    Tone::Primary,
                    self.live() && id.is_some(),
                    move |this, _window, cx| {
                        let Some(id) = this.model.companion_id().map(str::to_owned) else {
                            return;
                        };
                        this.model.selected = Some(id.clone());
                        let _ = this.commands.send(crate::update::Command::Serve(id));
                        cx.notify();
                    },
                ))
            })
            .into_any_element()
    }
    fn render_settings_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut body: Vec<AnyElement> = Vec::new();
        match self.settings_section {
            super::SettingsSection::Companion => {
                body.push(page_title(
                    "コンパニオン",
                    "話す相手です。名前と人物像を決め、使うモデルを選びます。",
                ));
                body.push(self.render_companion(cx));
            }
            super::SettingsSection::New => {
                body.push(page_title(
                    "新しいコンパニオン",
                    "名前と、話すときに使うモデルを選びます。あとから「コンパニオン」で変えられます。",
                ));
                body.push(self.render_new_companion(cx));
            }
            super::SettingsSection::Connections => {
                body.push(page_title(
                    "接続とモデル",
                    "コンパニオンが使う LLM と、声のエンジンの接続です。鍵はこの端末からホストへ送られ、ここには残りません。",
                ));
                let connections: Vec<w::Connection> = self
                    .model
                    .settings()
                    .map(|settings| settings.connections.clone())
                    .unwrap_or_default();
                if connections.is_empty() {
                    body.push(
                        div()
                            .p_3()
                            .rounded_md()
                            .bg(card())
                            .text_sm()
                            .text_color(muted())
                            .child("まだ接続がありません。下から追加してください。")
                            .into_any_element(),
                    );
                }
                for connection in connections {
                    body.push(self.render_connection(connection, cx));
                }
                body.push(section("接続を追加"));
                body.push(self.render_providers(cx));
            }
            super::SettingsSection::Voice => {
                body.push(page_title(
                    "声",
                    "コンパニオンがどう話すかです。VOICEVOX ENGINE をこの端末で起動しておくと、返答を読み上げます。",
                ));
                body.push(self.render_voice(cx));
            }
            super::SettingsSection::Device => {
                body.push(page_title(
                    "この端末",
                    "この窓がホストに見せる能力と、実際に使える状態です。",
                ));
                body.push(self.render_activation(cx));
                body.push(self.render_device_card(cx));
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .max_w(px(760.))
            .children(body)
            .into_any_element()
    }
    fn render_device_card(&mut self, _cx: &mut Context<Self>) -> AnyElement {
        // The declaration this machine actually sends, not a fixed list.
        let declared = capabilities::names(
            &capabilities::Declaration::probe(
                self.options.renderer.as_deref(),
                self.options.model.as_deref(),
            )
            .specs(),
        );
        let devices = &self.model.devices;
        let row = |label: &str, value: String| {
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(div().text_xs().text_color(muted()).child(label.to_owned()))
                .child(div().text_xs().child(value))
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_md()
            .bg(card())
            .border_1()
            .border_color(line())
            .child(row("端末ID", self.options.id.clone()))
            .child(row("プラットフォーム", self.options.platform.clone()))
            .child(row(
                "宣言した能力",
                if declared.is_empty() {
                    "なし".into()
                } else {
                    declared.join(", ")
                },
            ))
            .child(row(
                "許可された能力",
                self.model
                    .snapshot()
                    .and_then(|snapshot| {
                        snapshot
                            .clients
                            .iter()
                            .find(|client| client.id == self.options.id)
                    })
                    .map(|client| client.granted_capabilities.clone())
                    .filter(|granted| !granted.is_empty())
                    .map(|granted| granted.join(", "))
                    .unwrap_or_else(|| "なし（ホストが許可していません）".into()),
            ))
            .child(row("アバター", device_detail(&devices.avatar)))
            .child(row("音声出力", device_detail(&devices.output)))
            .child(row("マイク", device_detail(&devices.input)))
            .into_any_element()
    }

    fn render_connection(
        &mut self,
        connection: w::Connection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = connection.id.clone();
        let secret = self.draft(
            cx,
            format!("secret:{id}"),
            "資格情報（保存されません）",
            "TextField",
        );
        let enabled = connection.enabled;
        let toggle_label = if enabled {
            "無効にする"
        } else {
            "有効にする"
        };
        let toggled = connection.clone();
        let checked = self.model.checks.get(&id).cloned();
        let endpoint = connection
            .config
            .as_option()
            .and_then(|config| match config.config.as_ref() {
                Some(w::connection_config::Config::Llm(llm)) => llm.endpoint.as_option(),
                Some(w::connection_config::Config::Tts(tts)) => tts.endpoint.as_option(),
                Some(w::connection_config::Config::Stt(stt)) => stt.endpoint.as_option(),
                Some(w::connection_config::Config::Embedding(embedding)) => {
                    embedding.endpoint.as_option()
                }
                Some(w::connection_config::Config::Mcp(mcp)) => mcp.endpoint.as_option(),
                Some(w::connection_config::Config::Acp(acp)) => acp.endpoint.as_option(),
                None => None,
            })
            .cloned()
            .unwrap_or_default();
        let models = self.model.models.get(&id).cloned().unwrap_or_default();
        let kind = match connection.kind.as_known() {
            Some(w::ConnectionKind::Llm) => "LLM",
            Some(w::ConnectionKind::Tts) => "TTS",
            Some(w::ConnectionKind::Stt) => "STT",
            Some(w::ConnectionKind::Embedding) => "埋め込み",
            Some(w::ConnectionKind::Mcp) => "MCP",
            Some(w::ConnectionKind::Acp) => "ACP",
            _ => "不明",
        };
        let last = checked
            .as_ref()
            .map(|check| {
                if check.healthy {
                    "確認: 成功".to_owned()
                } else {
                    format!(
                        "確認: 失敗 ({})",
                        check
                            .failure
                            .as_option()
                            .map(|failure| failure.message.clone())
                            .unwrap_or_else(|| "理由不明".into())
                    )
                }
            })
            .or_else(|| {
                connection.last_check.as_option().map(|check| {
                    if check.healthy {
                        "前回の確認: 成功".to_owned()
                    } else {
                        "前回の確認: 失敗".to_owned()
                    }
                })
            })
            .unwrap_or_else(|| "未確認".to_owned());

        let check_id = id.clone();
        let register_id = id.clone();
        let revoke_id = id.clone();
        let mut model_chips: Vec<AnyElement> = Vec::new();
        for model in models {
            model_chips.push(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(0xf5f5f4))
                    .text_xs()
                    .child(if model.name.is_empty() {
                        model.id
                    } else {
                        format!("{} ({})", model.name, model.id)
                    })
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(card())
            .border_1()
            .border_color(line())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(connection.name.clone()))
                            .child(div().text_xs().text_color(muted()).child(format!(
                                "{kind} ・ {} ・ revision {} ・ 資格情報 {}",
                                if enabled { "有効" } else { "無効" },
                                connection.revision,
                                connection.credential_state
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted())
                                    .child(format!("{} / {}", endpoint.adapter, endpoint.endpoint)),
                            )
                            .child(div().text_xs().text_color(muted()).child(last)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(button(
                                cx,
                                SharedString::from(format!("toggle-{id}")),
                                toggle_label,
                                Tone::Plain,
                                self.live(),
                                move |this, _window, cx| {
                                    this.set_enabled(toggled.clone(), !enabled, cx)
                                },
                            ))
                            .child(button(
                                cx,
                                SharedString::from(format!("check-{id}")),
                                "接続確認",
                                Tone::Plain,
                                self.live(),
                                move |this, _window, cx| this.check(check_id.clone(), cx),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(secret))
                    .child(button(
                        cx,
                        SharedString::from(format!("register-{id}")),
                        "資格情報を登録",
                        Tone::Primary,
                        self.live(),
                        move |this, _window, cx| this.register_secret(register_id.clone(), cx),
                    ))
                    .child(button(
                        cx,
                        SharedString::from(format!("revoke-{id}")),
                        "解除",
                        Tone::Danger,
                        self.live(),
                        move |this, _window, cx| this.revoke_secret(revoke_id.clone(), cx),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .children(model_chips),
            )
            .into_any_element()
    }

    fn render_providers(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let models = self.draft(cx, "new:models", "モデルID（カンマ区切り）", "TextField");
        let providers = self.model.providers.clone();
        let mut buttons: Vec<AnyElement> = Vec::new();
        match providers {
            Some(providers) if providers.is_empty() => {
                buttons.push(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .child("ホストがプロバイダを公開していません")
                        .into_any_element(),
                );
            }
            Some(providers) => {
                for provider in providers {
                    let label = provider.name.clone();
                    let provider = provider.clone();
                    buttons.push(button(
                        cx,
                        SharedString::from(format!("provider-{}", provider.id)),
                        label,
                        Tone::Plain,
                        self.live(),
                        move |this, _window, cx| this.add_connection(provider.clone(), cx),
                    ));
                }
            }
            None => {
                buttons.push(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .child("プロバイダを取得していません")
                        .into_any_element(),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(card())
            .border_1()
            .border_color(line())
            .child(models)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .children(buttons),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted())
                    .child("接続を編集すると、ホストは資格情報の状態と前回の確認をリセットします。")
                    .into_any_element(),
            )
            .into_any_element()
    }

    pub(crate) fn resolve(
        &mut self,
        request_id: &str,
        decision: w::ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let request = request_id.to_owned();
        let resolved = request_id.to_owned();
        let stale = request_id.to_owned();
        self.request_with(
            "承認の送信",
            async move {
                host.settings
                    .submit_approval(w::SubmitApprovalRequest {
                        request_id: request,
                        decision: decision.into(),
                        ..Default::default()
                    })
                    .await
            },
            move |_response| Update::Resolved {
                request_id: resolved,
            },
            move |context, error| match error.code {
                connectrpc::ErrorCode::Aborted => Update::ApprovalStale {
                    message: format!(
                        "承認 {stale} は未決ではありません（別の端末で解決されたか失効しました）。"
                    ),
                },
                _ => failure(context, error),
            },
        );
        cx.notify();
    }
}

/// The panel's scrolling frame.
fn section(title: &str) -> AnyElement {
    div()
        .pt_2()
        .text_sm()
        .text_color(rgb(0x44403c))
        .child(title.to_owned())
        .into_any_element()
}

fn device_detail(device: &crate::state::Device) -> String {
    match device {
        crate::state::Device::Off => "停止".into(),
        crate::state::Device::Ready(detail) => format!("利用可 ({detail})"),
        crate::state::Device::Unavailable(reason) => format!("利用不可 ({reason})"),
    }
}
