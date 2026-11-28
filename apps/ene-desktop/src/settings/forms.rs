use super::*;

impl Settings {
    pub(super) fn form(&self, form: Form, cx: &Context<Window>) -> impl IntoElement {
        let providers = [
            (w::ProviderKind::OpenaiCompatible, "OpenAI 互換"),
            (w::ProviderKind::Native, "専用 API"),
            (w::ProviderKind::Voicevox, "VOICEVOX"),
            (w::ProviderKind::Search, "Web 検索"),
            (w::ProviderKind::McpHttp, "MCP HTTP"),
            (w::ProviderKind::McpStdio, "MCP stdio"),
            (w::ProviderKind::Plugin, "プラグイン"),
        ];
        let fields = self
            .fields
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                if !matches!(form, Form::Provider) {
                    return true;
                }
                match self.provider {
                    w::ProviderKind::McpStdio => {
                        matches!(*index, 0 | 2 | 3 | 9) || (*index == 7 && !self.all_mcp_tools)
                    }
                    w::ProviderKind::McpHttp => {
                        matches!(*index, 0 | 1 | 6 | 9) || (*index == 7 && !self.all_mcp_tools)
                    }
                    w::ProviderKind::Plugin => matches!(*index, 0 | 1 | 4 | 5 | 6 | 9),
                    w::ProviderKind::Native => matches!(*index, 0 | 1 | 6 | 8 | 9),
                    w::ProviderKind::Search => matches!(*index, 0 | 1 | 6 | 8),
                    w::ProviderKind::Voicevox => matches!(*index, 0 | 1),
                    _ => matches!(*index, 0 | 1 | 6),
                }
            })
            .map(|(_, (label, field))| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().child(*label))
                    .child(Input::new(field))
            });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(rgb(BLUSH))
            .rounded_lg()
            .border_1()
            .border_color(rgb(PETAL))
            .when(matches!(form, Form::Provider), |view| {
                view.child(
                    div()
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .children(providers.into_iter().map(|(kind, label)| {
                            Button::new(label)
                                .label(label)
                                .when(self.provider == kind, Button::primary)
                                .on_click(cx.listener(move |window, _, _, cx| {
                                    window.editor().provider = kind;
                                    cx.notify();
                                }))
                        })),
                )
                .child(
                    Button::new("provider-enabled")
                        .label(if self.connection.enabled {
                            "有効"
                        } else {
                            "無効"
                        })
                        .on_click(cx.listener(|window, _, _, cx| {
                            let state = window.editor();
                            state.connection.enabled = !state.connection.enabled;
                            cx.notify();
                        })),
                )
            })
            .when(matches!(form, Form::Provider), |view| {
                view.child(
                    div().flex().flex_wrap().gap_2().children(
                        crate::provider_choices::CHAT
                            .iter()
                            .chain(crate::provider_choices::SEARCH)
                            .chain(std::iter::once(&crate::provider_choices::VOICEVOX))
                            .filter(|preset| {
                                preset.kind == self.provider && !preset.endpoint.is_empty()
                            })
                            .map(|preset| {
                                Button::new(format!("preset-{}", preset.label))
                                    .label(preset.label)
                                    .on_click(cx.listener({
                                        let preset = *preset;
                                        move |owner, _, window, cx| {
                                            let settings = owner.editor();
                                            for (index, value) in [
                                                (0, preset.label),
                                                (1, preset.endpoint),
                                                (8, preset.adapter),
                                            ] {
                                                if let Some((_, field)) = settings.fields.get(index)
                                                {
                                                    field.update(cx, |input, cx| {
                                                        input.set_value(value, window, cx)
                                                    });
                                                }
                                            }
                                            cx.notify();
                                        }
                                    }))
                            }),
                    ),
                )
            })
            .when(
                matches!(form, Form::Provider) && is_mcp(Some(self.provider)),
                |view| {
                    view.child(
                        Button::new("mcp-scope")
                            .label(if self.all_mcp_tools {
                                "すべてのツールを公開"
                            } else {
                                "指定したツールだけ公開"
                            })
                            .on_click(cx.listener(|window, _, _, cx| {
                                let editor = window.editor();
                                editor.all_mcp_tools = !editor.all_mcp_tools;
                                cx.notify();
                            })),
                    )
                },
            )
            .when(matches!(form, Form::Role), |view| {
                view.child(
                    div()
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .children(ROLES.iter().copied().map(|(role, name)| {
                            Button::new(name)
                                .label(name)
                                .when(self.role == role, Button::primary)
                                .on_click(cx.listener(move |window, _, _, cx| {
                                    window.editor().role = role;
                                    cx.notify();
                                }))
                        })),
                )
            })
            .when(matches!(form, Form::Policy), |view| {
                view.child(
                    Button::new("full-access")
                        .label(if self.full_access {
                            "FullAccess"
                        } else {
                            "標準の判定"
                        })
                        .on_click(cx.listener(|window, _, _, cx| {
                            let editor = window.editor();
                            editor.full_access = !editor.full_access;
                            cx.notify();
                        })),
                )
            })
            .children(fields)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("form-save")
                            .label("確定")
                            .primary()
                            .on_click(cx.listener(move |window, _, _, cx| {
                                if let Some(action) = window.editor().save(form, cx) {
                                    window.apply_settings(action, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("form-cancel")
                            .label("閉じる")
                            .on_click(cx.listener(|window, _, _, cx| {
                                window.editor().form = None;
                                window.editor().fields.clear();
                                cx.notify();
                            })),
                    ),
            )
    }

    fn save(&mut self, form: Form, cx: &Context<Window>) -> Option<Action> {
        let field = |index: usize| {
            self.fields
                .get(index)
                .map_or_else(String::new, |(_, field)| value(field, cx))
        };
        let list = |index| {
            field(index)
                .split('/')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let action = match form {
            Form::Provider => {
                let mut connection = self.connection.clone();
                connection.kind = self.provider.into();
                connection.name = field(0);
                connection.endpoint = field(1);
                connection.command = field(2);
                connection.args = field(3).lines().map(str::to_owned).collect();
                connection.plugin_id = field(4);
                connection.entrypoint = field(5);
                connection.credential_env = Some(field(6)).filter(|name| !name.is_empty());
                connection.adapter = field(8);
                connection.options_json = field(9);
                connection.mcp_tools = is_mcp(Some(self.provider))
                    .then(|| w::McpToolScope {
                        all: self.all_mcp_tools,
                        names: if self.all_mcp_tools {
                            Vec::new()
                        } else {
                            field(7)
                                .split(',')
                                .map(str::trim)
                                .filter(|name| !name.is_empty())
                                .map(str::to_owned)
                                .collect()
                        },
                        ..Default::default()
                    })
                    .into();
                connection.send_data = if matches!(
                    self.provider,
                    w::ProviderKind::McpHttp | w::ProviderKind::McpStdio
                ) {
                    vec!["承認済みの操作引数".into()]
                } else {
                    vec!["割り当てた用途の入力（会話・参照情報・音声など）".into()]
                };
                Action::PutConnection(w::ConnectionChange {
                    mutation: mutation(connection.revision),
                    connection: Some(connection).into(),
                    ..Default::default()
                })
            }
            Form::Credential => Action::Credential(w::CredentialChange {
                mutation: mutation(self.connection.revision),
                connection_id: self.connection.id.clone(),
                secret: self
                    .fields
                    .first()
                    .map(|(_, input)| input.read(cx).unmask_value().to_string())
                    .filter(|value| !value.is_empty()),
                ..Default::default()
            }),
            Form::Character => {
                let mut character = self.character.clone();
                character.profile = Some(w::Profile {
                    name: field(0),
                    persona: field(1),
                    speech: field(2),
                    facets: list(3),
                    examples: list(4),
                    background: list(5),
                    expressions: list(6),
                    ..Default::default()
                })
                .into();
                character.avatar = field(7);
                character.voice = field(8);
                Action::PutCharacter(w::CharacterChange {
                    mutation: mutation(character.revision),
                    character: Some(character).into(),
                    ..Default::default()
                })
            }
            Form::Companion | Form::Role => {
                let mut companion = self.companion.clone();
                if matches!(form, Form::Companion) {
                    companion.name = field(0);
                } else {
                    companion.models.retain(|model| model.role != self.role);
                    let model = field(0);
                    if !model.is_empty() {
                        companion.models.push(w::ModelSelection {
                            role: self.role.into(),
                            connection_id: self.connection.id.clone(),
                            model,
                            ..Default::default()
                        });
                    }
                }
                Action::PutCompanion(w::CompanionChange {
                    mutation: mutation(companion.revision),
                    companion: Some(companion).into(),
                    ..Default::default()
                })
            }
            Form::Policy => {
                let mut policy = (*self.data.policy).clone();
                policy.text = field(0);
                policy.always_confirm = field(1)
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
                policy.full_access = self.full_access;
                Action::Policy(w::PolicyChange {
                    mutation: mutation(policy.revision),
                    policy: Some(policy).into(),
                    ..Default::default()
                })
            }
            Form::ClientPlugin => {
                let path = PathBuf::from(field(0));
                if path.as_os_str().is_empty() {
                    return None;
                }
                Action::ClientPlugin {
                    path,
                    enabled: true,
                }
            }
            Form::Plugin => Action::PutPlugin(w::PluginChange {
                mutation: mutation(0),
                plugin: Some(w::Plugin {
                    path: field(0),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            }),
            Form::Package => {
                let path = field(0);
                if path.is_empty() {
                    return None;
                }
                if self.character.id.is_empty() {
                    Action::Import(PathBuf::from(path))
                } else {
                    let elements = field(1);
                    Action::Export(
                        w::ExportRequest {
                            id: self.character.id.clone(),
                            profile: elements.contains('心'),
                            avatar: elements.contains('姿'),
                            voice: elements.contains('声'),
                            ..Default::default()
                        },
                        PathBuf::from(path),
                    )
                }
            }
        };
        self.form = None;
        self.fields.clear();
        Some(action)
    }
}
