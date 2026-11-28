use super::*;

pub(super) fn policy(settings: &Settings, cx: &Context<Window>) -> impl IntoElement {
    let policy = (*settings.data.policy).clone();
    let saved = policy.clone();
    let rules = settings.data.rules.iter().map(|rule| {
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
    let audit = settings.audit.iter().map(|row| {
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

pub(super) fn plugins(
    settings: &Settings,
    cx: &Context<Window>,
    local: &[PathBuf],
) -> impl IntoElement {
    let plugins = settings.data.plugins.iter().cloned().map(|plugin| {
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
