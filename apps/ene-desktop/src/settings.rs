use crate::{
    network::{Action, mutation},
    window::{BLUSH, INK, MILK, PETAL, Window, value},
};
use ene_rpc::{self as w, model_selection::Role};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window as GpuiWindow, div, px, rgb,
};
use std::path::PathBuf;

#[derive(Clone, Copy, Default, PartialEq)]
enum Tab {
    #[default]
    Providers,
    Character,
    Policy,
    Plugins,
}

#[derive(Clone, Copy)]
enum Form {
    Provider,
    Character,
    Companion,
    Credential,
    Role,
    Policy,
    Plugin,
    ClientPlugin,
    Package,
}

pub(crate) struct Settings {
    pub data: w::Settings,
    pub models: Vec<String>,
    pub audit: Vec<w::AuditEntry>,
    tab: Tab,
    form: Option<Form>,
    fields: Vec<(&'static str, Entity<InputState>)>,
    connection: w::Connection,
    character: w::Character,
    companion: w::Companion,
    provider: w::ProviderKind,
    role: Role,
    full_access: bool,
}

impl Settings {
    pub(crate) fn new(_: &mut GpuiWindow, _: &mut Context<Window>) -> Self {
        Self {
            data: w::Settings::default(),
            models: Vec::new(),
            audit: Vec::new(),
            tab: Tab::default(),
            form: None,
            fields: Vec::new(),
            connection: w::Connection::default(),
            character: w::Character::default(),
            companion: w::Companion::default(),
            provider: w::ProviderKind::OpenaiCompatible,
            role: Role::Dialogue,
            full_access: false,
        }
    }

    pub(crate) fn render(
        &self,
        cx: &mut Context<Window>,
        snapshot: &w::Snapshot,
        companion_id: &str,
        local: &crate::Local,
    ) -> impl IntoElement {
        let nav = [
            (Tab::Providers, "プロバイダー"),
            (Tab::Character, "キャラクター"),
            (Tab::Policy, "ポリシー"),
            (Tab::Plugins, "プラグイン"),
        ]
        .into_iter()
        .map(|(tab, label)| {
            Button::new(label)
                .label(label)
                .when(self.tab == tab, Button::primary)
                .on_click(cx.listener(move |window, _, _, cx| {
                    window.editor().tab = tab;
                    window.editor().form = None;
                    cx.notify();
                }))
        });
        let content = match self.tab {
            Tab::Providers => self
                .providers(snapshot, companion_id, cx)
                .into_any_element(),
            Tab::Character => self.characters(snapshot, cx).into_any_element(),
            Tab::Policy => self.policy(cx).into_any_element(),
            Tab::Plugins => self.plugins(cx, &local.plugins).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .min_h_0()
            .flex_1()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .bg(rgb(MILK))
                    .border_b_1()
                    .border_color(rgb(PETAL))
                    .flex()
                    .gap_2()
                    .children(nav),
            )
            .child(
                div()
                    .id("settings-body")
                    .min_h_0()
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .child(content)
                    .children(self.form.map(|form| self.form(form, cx)))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .border_t_1()
                            .border_color(rgb(PETAL))
                            .pt_3()
                            .child("この端末の見守り")
                            .child(action(
                                "eyes",
                                if local.eyes { "有効" } else { "無効" },
                                Action::Eyes(!local.eyes),
                                cx,
                            )),
                    )
                    .when(crate::computer::available(), |view| {
                        view.child(
                            div()
                                .flex()
                                .justify_between()
                                .items_center()
                                .gap_2()
                                .child(if crate::computer::portal_session() {
                                    "共有ポータルでの画面取得"
                                } else {
                                    "この端末の画面取得と操作"
                                })
                                .child(action(
                                    "desktop-input",
                                    if local.input { "有効" } else { "無効" },
                                    Action::Input(!local.input),
                                    cx,
                                )),
                        )
                    }),
            )
    }

    fn providers(
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
                        .child(action(
                            format!("{id}-probe"),
                            "疎通確認",
                            Action::Probe(w::ProbeRequest {
                                connection_id: id.clone(),
                                ..Default::default()
                            }),
                            cx,
                        ))
                        .child(action(
                            format!("{id}-models"),
                            "モデル一覧",
                            Action::Models(id.clone()),
                            cx,
                        ))
                        .when(companion.is_some(), |row| {
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

    fn characters(&self, snapshot: &w::Snapshot, cx: &Context<Window>) -> impl IntoElement {
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
            .children(characters)
    }

    fn policy(&self, cx: &Context<Window>) -> impl IntoElement {
        let policy = (*self.data.policy).clone();
        let saved = policy.clone();
        let rules = self.data.rules.iter().map(|rule| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .p_2()
                .border_b_1()
                .border_color(rgb(PETAL))
                .child(format!("{} · {}", rule.operation, rule.effect))
                .child(action(
                    format!("rule-{}", rule.digest),
                    "解除",
                    Action::DeleteRule(w::DeleteRequest {
                        mutation: mutation(1),
                        id: rule.digest.clone(),
                        ..Default::default()
                    }),
                    cx,
                ))
        });
        let audit = self.audit.iter().map(|row| {
            div().py_1().text_sm().child(format!(
                "{}　{}　{}",
                row.operation, row.decision, row.source
            ))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section(
                "自由文ポリシー",
                Button::new("edit-policy")
                    .label("編集")
                    .on_click(cx.listener(move |window, _, w, cx| {
                        let editor = window.editor();
                        editor.full_access = saved.full_access;
                        editor.open(
                            Form::Policy,
                            [
                                ("自由文ポリシー", saved.text.clone()),
                                (
                                    "毎回確認する操作名（カンマ区切り）",
                                    saved.always_confirm.join(","),
                                ),
                            ],
                            w,
                            cx,
                        );
                        cx.notify();
                    })),
            ))
            .child(
                div()
                    .bg(rgb(MILK))
                    .p_3()
                    .rounded_lg()
                    .child(if policy.text.is_empty() {
                        "未設定".into()
                    } else {
                        policy.text
                    }),
            )
            .child(div().mt_2().child("常に承認した操作"))
            .children(rules)
            .child(section(
                "判定履歴",
                action("refresh-audit", "更新", Action::Settings, cx),
            ))
            .children(audit)
    }

    fn plugins(&self, cx: &Context<Window>, local: &[PathBuf]) -> impl IntoElement {
        let plugins = self.data.plugins.iter().cloned().map(|plugin| {
            let mut changed = plugin.clone();
            changed.enabled = !changed.enabled;
            let revision = plugin.revision;
            div()
                .flex()
                .flex_col()
                .gap_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(PETAL))
                .bg(rgb(MILK))
                .p_3()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(plugin.label)
                        .child(if plugin.enabled { "有効" } else { "無効" }),
                )
                .child(div().text_sm().child(plugin.capabilities.join(" / ")))
                .child(div().text_sm().child(plugin.destinations.join(" / ")))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(action(
                            format!("{}-switch", plugin.id),
                            if changed.enabled {
                                "有効にする"
                            } else {
                                "無効にする"
                            },
                            Action::PutPlugin(w::PluginChange {
                                plugin: Some(changed).into(),
                                mutation: mutation(revision),
                                ..Default::default()
                            }),
                            cx,
                        ))
                        .child(action(
                            format!("{}-remove", plugin.id),
                            "削除",
                            Action::DeletePlugin(w::DeleteRequest {
                                id: plugin.id,
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
                "プラグイン",
                Button::new("plugin-add")
                    .label("追加")
                    .primary()
                    .on_click(cx.listener(|window, _, w, cx| {
                        window.editor().open(
                            Form::Plugin,
                            [("マニフェストが入ったフォルダー", String::new())],
                            w,
                            cx,
                        );
                        cx.notify();
                    })),
            ))
            .children(plugins)
            .child(section(
                "端末プラグイン",
                Button::new("client-plugin-add")
                    .label("追加")
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.editor().open(
                            Form::ClientPlugin,
                            [("マニフェストが入った端末フォルダー", String::new())],
                            window,
                            cx,
                        );
                        cx.notify();
                    })),
            ))
            .children(local.iter().cloned().enumerate().map(|(index, path)| {
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(path.display().to_string())
                    .child(action(
                        format!("desktop-plugin-remove-{index}"),
                        "削除",
                        Action::ClientPlugin {
                            path,
                            enabled: false,
                        },
                        cx,
                    ))
            }))
    }

    fn open(
        &mut self,
        form: Form,
        fields: impl IntoIterator<Item = (&'static str, String)>,
        window: &mut GpuiWindow,
        cx: &mut Context<Window>,
    ) {
        self.form = Some(form);
        self.fields = fields
            .into_iter()
            .map(|(label, text)| {
                (
                    label,
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(label)
                            .default_value(text)
                            .masked(matches!(form, Form::Credential))
                    }),
                )
            })
            .collect();
    }

    fn edit_provider(
        &mut self,
        provider: w::Connection,
        window: &mut GpuiWindow,
        cx: &mut Context<Window>,
    ) {
        self.provider = provider
            .kind
            .as_known()
            .filter(|kind| *kind != w::ProviderKind::Unspecified)
            .unwrap_or(w::ProviderKind::OpenaiCompatible);
        let fields = [
            ("名前", provider.name.clone()),
            ("URL", provider.endpoint.clone()),
            ("コマンド", provider.command.clone()),
            ("引数（改行区切り）", provider.args.join("\n")),
            ("プラグイン ID", provider.plugin_id.clone()),
            ("エントリーポイント", provider.entrypoint.clone()),
            (
                "認証用環境変数",
                provider.credential_env.clone().unwrap_or_default(),
            ),
        ];
        self.connection = provider;
        self.open(Form::Provider, fields, window, cx);
    }

    fn edit_character(
        &mut self,
        character: w::Character,
        window: &mut GpuiWindow,
        cx: &mut Context<Window>,
    ) {
        let profile = &character.profile;
        let fields = [
            ("名前", profile.name.clone()),
            ("人格の核", profile.persona.clone()),
            ("話し方", profile.speech.clone()),
            ("性格の側面", profile.facets.join(" / ")),
            ("会話例", profile.examples.join(" / ")),
            ("背景", profile.background.join(" / ")),
            ("表現語彙", profile.expressions.join(" / ")),
            ("VRM パス", character.avatar.clone()),
            ("声のプリセット", character.voice.clone()),
        ];
        self.character = character;
        self.open(Form::Character, fields, window, cx);
    }

    fn form(&self, form: Form, cx: &Context<Window>) -> impl IntoElement {
        let providers = [
            (w::ProviderKind::OpenaiCompatible, "OpenAI 互換"),
            (w::ProviderKind::McpHttp, "MCP HTTP"),
            (w::ProviderKind::McpStdio, "MCP stdio"),
            (w::ProviderKind::Plugin, "プラグイン"),
        ];
        let roles = [
            (Role::Dialogue, "対話"),
            (Role::Work, "手"),
            (Role::Perception, "目"),
            (Role::Reflection, "記憶の整理"),
            (Role::Policy, "判定"),
            (Role::Embedding, "埋め込み"),
            (Role::Synthesis, "音声合成"),
            (Role::Transcription, "音声認識"),
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
                    w::ProviderKind::McpStdio => matches!(*index, 0 | 2 | 3),
                    w::ProviderKind::Plugin => matches!(*index, 0 | 1 | 4 | 5 | 6),
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
            .when(matches!(form, Form::Role), |view| {
                view.child(
                    div()
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .children(roles.into_iter().map(|(role, name)| {
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

fn section(label: &'static str, action: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .text_color(rgb(INK))
        .mb(px(4.0))
        .child(
            div()
                .text_lg()
                .font_weight(gpui::FontWeight::BOLD)
                .child(label),
        )
        .child(action)
}

fn action(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    action: Action,
    cx: &Context<Window>,
) -> Button {
    Button::new(id)
        .label(label)
        .on_click(cx.listener(move |window, _, _, cx| window.apply_settings(action.clone(), cx)))
}
