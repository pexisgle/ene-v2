use super::*;

impl Settings {
    pub(super) fn providers(
        &self,
        snapshot: &w::Snapshot,
        companion_id: &str,
        cx: &Context<Window>,
    ) -> impl IntoElement {
        let companion = snapshot
            .companions
            .iter()
            .find(|row| row.id == companion_id)
            .cloned();
        let providers = self.data.connections.iter().cloned().map(|connection| {
            let id = connection.id.clone();
            let revision = connection.revision;
            let is_mcp = is_mcp(connection.kind.as_known());
            let is_voicevox = connection.kind == w::ProviderKind::Voicevox;
            let is_search = connection.kind == w::ProviderKind::Search;
            let for_edit = connection.clone();
            let for_secret = connection.clone();
            let for_model = connection.clone();
            let companion = companion.clone();
            let description = match connection.kind.as_known() {
                Some(w::ProviderKind::McpStdio) => connection.command.clone(),
                _ => connection.endpoint.clone(),
            };
            div()
                .flex()
                .flex_col()
                .p_3()
                .gap_2()
                .rounded_lg()
                .bg(rgb(MILK))
                .border_1()
                .border_color(rgb(PETAL))
                .child(div().flex().justify_between().child(connection.name).child(
                    if connection.enabled {
                        "有効"
                    } else {
                        "無効"
                    },
                ))
                .child(div().text_sm().child(description))
                .when(!connection.send_data.is_empty(), |row| {
                    row.child(
                        div()
                            .text_sm()
                            .child(format!("送信データ · {}", connection.send_data.join(" / "))),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(Button::new(format!("{id}-edit")).label("編集").on_click(
                            cx.listener(move |window, _, w, cx| {
                                window.editor().edit_provider(for_edit.clone(), w, cx);
                                cx.notify();
                            }),
                        ))
                        .child(
                            Button::new(format!("{id}-secret"))
                                .label(if connection.credential_configured {
                                    "認証情報を変更"
                                } else {
                                    "認証情報を登録"
                                })
                                .on_click(cx.listener(move |window, _, w, cx| {
                                    let editor = window.editor();
                                    editor.connection = for_secret.clone();
                                    editor.open(
                                        Form::Credential,
                                        [("API キー", String::new())],
                                        w,
                                        cx,
                                    );
                                    cx.notify();
                                })),
                        )
                        .when(!is_search, |row| {
                            row.child(action(
                                format!("{id}-probe"),
                                if is_mcp {
                                    "MCP ツールを取得"
                                } else if is_voicevox {
                                    "話者一覧を取得"
                                } else {
                                    "モデル一覧を取得"
                                },
                                if is_mcp {
                                    Action::McpTools(id.clone())
                                } else {
                                    Action::Models(id.clone())
                                },
                                cx,
                            ))
                        })
                        .when(!is_mcp, |row| {
                            row.children(
                                companion
                                    .as_ref()
                                    .into_iter()
                                    .flat_map(|companion| &companion.models)
                                    .filter(|selection| selection.connection_id == id)
                                    .cloned()
                                    .map(|model| {
                                        let role = ROLES
                                            .iter()
                                            .find(|(role, _)| model.role == *role)
                                            .map_or("用途", |(_, name)| *name);
                                        action(
                                            format!(
                                                "{id}-probe-{}",
                                                model
                                                    .role
                                                    .as_known()
                                                    .map_or("unknown".to_owned(), |role| format!(
                                                        "{role:?}"
                                                    ))
                                            ),
                                            format!("{role}を確認"),
                                            Action::Probe(w::ProbeRequest {
                                                connection_id: id.clone(),
                                                voice: companion
                                                    .as_ref()
                                                    .and_then(|companion| {
                                                        snapshot.characters.iter().find(
                                                            |character| {
                                                                character.id
                                                                    == companion.character_id
                                                            },
                                                        )
                                                    })
                                                    .map_or_else(String::new, |character| {
                                                        character.voice.clone()
                                                    }),
                                                model: Some(model).into(),
                                                ..Default::default()
                                            }),
                                            cx,
                                        )
                                    }),
                            )
                        })
                        .when(companion.is_some() && !is_search && !is_mcp, |row| {
                            row.child(
                                Button::new(format!("{id}-assign"))
                                    .label("用途を割り当て")
                                    .on_click(cx.listener(move |window, _, w, cx| {
                                        if let Some(companion) = &companion {
                                            let editor = window.editor();
                                            editor.companion = companion.clone();
                                            editor.connection = for_model.clone();
                                            editor.open(
                                                Form::Role,
                                                [("モデル ID", String::new())],
                                                w,
                                                cx,
                                            );
                                            cx.notify();
                                        }
                                    })),
                            )
                        })
                        .child(action(
                            format!("{id}-delete"),
                            "削除",
                            Action::DeleteConnection(w::DeleteRequest {
                                id,
                                mutation: mutation(revision),
                                ..Default::default()
                            }),
                            cx,
                        )),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section(
                "プロバイダー",
                Button::new("new-provider")
                    .label("追加")
                    .primary()
                    .on_click(cx.listener(|window, _, w, cx| {
                        window.editor().edit_provider(
                            w::Connection {
                                enabled: true,
                                ..Default::default()
                            },
                            w,
                            cx,
                        );
                        cx.notify();
                    })),
            ))
            .children(providers)
            .when(!self.models.is_empty(), |view| {
                view.child(
                    div()
                        .p_3()
                        .text_sm()
                        .bg(rgb(MILK))
                        .child(self.models.join(" / ")),
                )
            })
    }

    pub(super) fn characters(
        &self,
        snapshot: &w::Snapshot,
        cx: &Context<Window>,
    ) -> impl IntoElement {
        let characters = snapshot.characters.iter().map(|character| {
            let id = character.id.clone();
            let edit = character.clone();
            let make = character.clone();
            let export = character.clone();
            let preferred_model = snapshot
                .companions
                .iter()
                .find(|companion| companion.character_id == character.id)
                .and_then(|companion| {
                    companion
                        .models
                        .iter()
                        .find(|model| model.role == Role::Dialogue)
                })
                .cloned();
            let preview = character.profile.clone();
            div()
                .p_3()
                .rounded_lg()
                .bg(rgb(MILK))
                .border_1()
                .border_color(rgb(PETAL))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(character.profile.name.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new(format!("{id}-character"))
                                .label("編集")
                                .on_click(cx.listener(move |window, _, w, cx| {
                                    window.editor().edit_character(edit.clone(), w, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(format!("{id}-new-companion"))
                                .label("コンパニオンを作成")
                                .on_click(cx.listener(move |window, _, w, cx| {
                                    let editor = window.editor();
                                    editor.companion = w::Companion {
                                        character_id: make.id.clone(),
                                        ..Default::default()
                                    };
                                    editor.open(
                                        Form::Companion,
                                        [("名前", make.profile.name.clone())],
                                        w,
                                        cx,
                                    );
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(format!("{id}-export"))
                                .label("書き出し")
                                .on_click(cx.listener(move |window, _, w, cx| {
                                    let editor = window.editor();
                                    editor.character = export.clone();
                                    editor.open(
                                        Form::Package,
                                        [
                                            ("保存先ファイル", String::new()),
                                            ("要素（姿,声,心）", "姿,声,心".into()),
                                        ],
                                        w,
                                        cx,
                                    );
                                    cx.notify();
                                })),
                        )
                        .when(preferred_model.is_some(), |row| {
                            row.child(
                                Button::new(format!("{id}-preview"))
                                    .label("応答を試す")
                                    .on_click(cx.listener(move |window, _, _, cx| {
                                        if let Some(model) = &preferred_model {
                                            window.apply_settings(
                                                Action::Preview(w::PreviewRequest {
                                                    profile: preview.clone(),
                                                    model: Some(model.clone()).into(),
                                                    input:
                                                        "初対面です。簡単に自己紹介をお願いします。"
                                                            .into(),
                                                    ..Default::default()
                                                }),
                                                cx,
                                            );
                                        }
                                    })),
                            )
                        }),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section(
                "キャラクター",
                Button::new("new-character")
                    .label("追加")
                    .primary()
                    .on_click(cx.listener(|window, _, w, cx| {
                        window
                            .editor()
                            .edit_character(w::Character::default(), w, cx);
                        cx.notify();
                    })),
            ))
            .child(
                Button::new("import")
                    .label("パッケージを取り込む")
                    .on_click(cx.listener(|window, _, w, cx| {
                        let editor = window.editor();
                        editor.character.id.clear();
                        editor.open(
                            Form::Package,
                            [("パッケージのファイルパス", String::new())],
                            w,
                            cx,
                        );
                        cx.notify();
                    })),
            )
            .children(snapshot.companions.iter().map(|companion| {
                let id = companion.id.clone();
                let name = companion.name.clone();
                let revision = companion.revision;
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(companion.name.clone())
                    .child(
                        Button::new(format!("companion-remove-{id}"))
                            .label("コンパニオンを削除")
                            .on_click(cx.listener(move |owner, _, window, cx| {
                                owner.confirm_delete(&id, &name, revision, window, cx);
                            })),
                    )
            }))
            .children(characters)
    }
}

pub(super) fn is_mcp(kind: Option<w::ProviderKind>) -> bool {
    matches!(
        kind,
        Some(w::ProviderKind::McpHttp | w::ProviderKind::McpStdio)
    )
}
