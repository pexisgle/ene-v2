use crate::input::Draft;
use crate::ui::{DraftStore, studio::labeled_field, theme::Theme};
use ene_client_core::{
    connections::{McpExtraConfig, format_kv_lines, parse_mcp_kv},
    domain::McpConnection,
};
use gpui::{AnyElement, App, Div, Entity, SharedString, div, prelude::*, px};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldMode {
    Process,
    Http,
    Both,
}

pub(super) struct McpFields {
    process: Option<ProcessFields>,
    http: Option<HttpFields>,
}

impl McpFields {
    pub(super) fn new(
        drafts: &mut DraftStore,
        prefix: &str,
        mode: FieldMode,
        saved: Option<&McpConnection>,
        theme: &Theme,
        cx: &mut App,
    ) -> Self {
        let mut field = |suffix: &str, placeholder, initial, multiline| {
            let key = format!("{prefix}{suffix}");
            if multiline {
                drafts.description_draft(cx, key, placeholder, initial, theme)
            } else {
                drafts.draft(cx, key, placeholder, initial, theme)
            }
        };
        let env = saved.map(|mcp| format_kv_lines(&mcp.env, '='));
        let headers = saved.map(|mcp| format_kv_lines(&mcp.headers, ':'));
        let cwd = saved.map(|mcp| mcp.cwd.as_str());
        let secret_env = saved.map(|mcp| mcp.secret_env_var.as_str());
        let auth = saved.map(|mcp| mcp.auth_header.as_str());
        Self {
            process: (mode != FieldMode::Http).then(|| ProcessFields {
                cwd: field("cwd", "例: C:/work/project", cwd, false),
                secret_env: field(
                    "secret-env",
                    "例: GITHUB_TOKEN / API_KEY",
                    secret_env,
                    false,
                ),
                env: field(
                    "env",
                    "例: NODE_ENV=production（1行1件）",
                    env.as_deref(),
                    true,
                ),
            }),
            http: (mode != FieldMode::Process).then(|| HttpFields {
                auth: field("auth-header", "例: Authorization / X-Api-Key", auth, false),
                headers: field(
                    "headers",
                    "例: X-Custom-Header: value（1行1件）",
                    headers.as_deref(),
                    true,
                ),
            }),
        }
    }

    pub(super) fn read(&self, cx: &App) -> Result<McpExtraConfig, &'static str> {
        let mut config = McpExtraConfig::default();
        if let Some(fields) = &self.process {
            config.env = parse_mcp_kv(fields.env.read(cx).text(), '=')
                .map_err(|error| error.message('='))?;
            config.cwd = fields.cwd.read(cx).text().trim().to_owned();
            config.secret_env_var = fields.secret_env.read(cx).text().trim().to_owned();
        }
        if let Some(fields) = &self.http {
            config.headers = parse_mcp_kv(fields.headers.read(cx).text(), ':')
                .map_err(|error| error.message(':'))?;
            config.auth_header = fields.auth.read(cx).text().trim().to_owned();
        }
        Ok(config)
    }

    pub(super) fn append(
        self,
        mut panel: Div,
        mut secret: Option<Entity<Draft>>,
        scroll_id: Option<&str>,
        theme: &Theme,
    ) -> Div {
        if let Some(fields) = self.process {
            panel = fields.append(
                panel,
                secret.take(),
                scroll_id.map(|id| format!("conn-mcp-env-{id}").into()),
                theme,
            );
        }
        if let Some(fields) = self.http {
            let headers = multiline(
                fields.headers,
                scroll_id.map(|id| format!("conn-mcp-headers-{id}").into()),
            );
            panel = panel
                .when_some(secret, |panel, input| {
                    panel.child(labeled_field("認証トークン（任意）", input, theme))
                })
                .child(labeled_field(
                    "認証ヘッダー名（HTTP・空欄時は Bearer）",
                    fields.auth,
                    theme,
                ))
                .child(labeled_field(
                    "HTTP ヘッダー（Name: Value を1行ずつ）",
                    headers,
                    theme,
                ));
        }
        panel
    }
}

struct HttpFields {
    auth: Entity<Draft>,
    headers: Entity<Draft>,
}

struct ProcessFields {
    cwd: Entity<Draft>,
    secret_env: Entity<Draft>,
    env: Entity<Draft>,
}

impl ProcessFields {
    fn append(
        self,
        panel: Div,
        secret: Option<Entity<Draft>>,
        scroll: Option<SharedString>,
        theme: &Theme,
    ) -> Div {
        let secret_env = labeled_field("認証環境変数名 (stdio)", self.secret_env, theme);
        let credentials = match secret {
            Some(secret) => div()
                .w_full()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(secret_env))
                .child(div().flex_1().min_w_0().child(labeled_field(
                    "認証シークレット",
                    secret,
                    theme,
                )))
                .into_any_element(),
            None => secret_env,
        };
        panel
            .child(labeled_field("作業ディレクトリ (cwd)", self.cwd, theme))
            .child(credentials)
            .child(labeled_field(
                "環境変数（KEY=VALUE を1行ずつ）",
                multiline(self.env, scroll),
                theme,
            ))
    }
}

fn multiline(input: Entity<Draft>, scroll: Option<SharedString>) -> AnyElement {
    match scroll {
        Some(id) => div()
            .id(id)
            .max_h(px(110.))
            .overflow_y_scroll()
            .child(input)
            .into_any_element(),
        None => input.into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn mcp_creation_validates_only_its_transport_and_edit_preserves_both(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let theme = Theme::default();
            let mut drafts = DraftStore::default();
            let add = "add:mcp:";
            let process = McpFields::new(&mut drafts, add, FieldMode::Process, None, &theme, cx);
            process
                .process
                .as_ref()
                .unwrap()
                .env
                .update(cx, |draft, cx| {
                    draft.set("missing separator", cx);
                });
            assert!(process.read(cx).is_err());

            let http = McpFields::new(&mut drafts, add, FieldMode::Http, None, &theme, cx);
            http.http.as_ref().unwrap().headers.update(cx, |draft, cx| {
                draft.set("X-Fixture: here:there", cx);
            });
            let new = http.read(cx).unwrap();
            assert_eq!(new.headers["X-Fixture"], "here:there");
            assert_eq!(new.env, std::collections::BTreeMap::new());
            assert!(
                McpFields::new(&mut drafts, add, FieldMode::Process, None, &theme, cx)
                    .read(cx)
                    .is_err()
            );

            let saved = McpConnection {
                endpoint: None,
                allowed_tools: Vec::new(),
                command: "fixture".into(),
                args: Vec::new(),
                cwd: "/fixture".into(),
                secret_env_var: "FIXTURE_SECRET".into(),
                auth_header: "X-Auth".into(),
                env: [("LOCALE".into(), "日本語=保存".into())].into(),
                headers: [("X-Other".into(), "a:b".into())].into(),
            };
            let edit = McpFields::new(
                &mut drafts,
                "conn:a:r1:mcp-",
                FieldMode::Both,
                Some(&saved),
                &theme,
                cx,
            )
            .read(cx)
            .unwrap();
            assert_eq!(edit.env, saved.env);
            assert_eq!(edit.headers, saved.headers);
            assert_eq!(edit.cwd, saved.cwd);
            assert_eq!(edit.secret_env_var, saved.secret_env_var);
            assert_eq!(edit.auth_header, saved.auth_header);
        });
    }
}
