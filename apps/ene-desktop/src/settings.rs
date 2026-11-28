mod administration;
mod connections;
mod forms;
mod voice;

use connections::is_mcp;
use gpui::base::Disableable as _;

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

const ROLES: &[(Role, &str)] = &[
    (Role::Dialogue, "対話"),
    (Role::Work, "作業"),
    (Role::Perception, "知覚"),
    (Role::Reflection, "記憶の整理"),
    (Role::Policy, "安全判定"),
    (Role::Embedding, "埋め込み"),
    (Role::Synthesis, "音声合成"),
    (Role::Transcription, "音声認識"),
];

#[derive(Clone, Copy, Default, PartialEq)]
enum Tab {
    #[default]
    Providers,
    Character,
    Policy,
    Plugins,
    Voice,
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
    pub(crate) voice: voice::VoiceTest,
    tab: Tab,
    form: Option<Form>,
    fields: Vec<(&'static str, Entity<InputState>)>,
    connection: w::Connection,
    character: w::Character,
    companion: w::Companion,
    provider: w::ProviderKind,
    all_mcp_tools: bool,
    role: Role,
    full_access: bool,
}

impl Settings {
    pub(crate) fn new(window: &mut GpuiWindow, cx: &mut Context<Window>) -> Self {
        Self {
            data: w::Settings::default(),
            models: Vec::new(),
            audit: Vec::new(),
            voice: voice::VoiceTest::new(window, cx),
            tab: Tab::default(),
            form: None,
            fields: Vec::new(),
            connection: w::Connection::default(),
            character: w::Character::default(),
            companion: w::Companion::default(),
            provider: w::ProviderKind::OpenaiCompatible,
            all_mcp_tools: false,
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
        managed: bool,
    ) -> impl IntoElement {
        let nav = [
            (Tab::Providers, "プロバイダー"),
            (Tab::Character, "キャラクター"),
            (Tab::Policy, "ポリシー"),
            (Tab::Plugins, "プラグイン"),
            (Tab::Voice, "音声テスト"),
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
            Tab::Policy => administration::policy(self, cx).into_any_element(),
            Tab::Plugins => administration::plugins(self, cx, &local.plugins).into_any_element(),
            Tab::Voice => self.voice.render(cx).into_any_element(),
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child("アバターの大きさ")
                            .children([75., 100., 125., 150.].into_iter().map(|scale| {
                                Button::new(format!("avatar-scale-{scale}"))
                                    .label(format!("{scale}%"))
                                    .when(local.avatar_scale == scale, Button::primary)
                                    .on_click(cx.listener(move |owner, _, _, cx| {
                                        owner.apply_settings(Action::AvatarScale(scale), cx)
                                    }))
                            })),
                    )
                    .child(
                        Button::new("workspace")
                            .label(local.workspace.as_ref().map_or_else(
                                || "作業フォルダーを選ぶ".to_owned(),
                                |path| format!("作業フォルダー · {}", path.display()),
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.choose_workspace(cx))),
                    )
                    .child(
                        Button::new("reset-history")
                            .label("履歴を消去")
                            .when(!managed, |button| button.disabled(true))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_reset(window, cx)),
                            ),
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
        self.all_mcp_tools = provider.mcp_tools.all;
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
            (
                "許可するツール名（カンマ区切り）",
                provider.mcp_tools.names.join(", "),
            ),
            ("アダプター ID", provider.adapter.clone()),
            ("詳細設定（JSON）", provider.options_json.clone()),
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
    label: impl Into<gpui::SharedString>,
    action: Action,
    cx: &Context<Window>,
) -> Button {
    Button::new(id)
        .label(label)
        .on_click(cx.listener(move |window, _, _, cx| window.apply_settings(action.clone(), cx)))
}
