use super::{CircleButtonStyle, Tone, circle_icon_button, pill_button};
use crate::{
    state::{Notice, NoticeKind},
    ui::{
        Hub,
        icons::Icon,
        theme::{Theme, radius},
    },
};
use gpui::{Entity, div, prelude::*, px};
use std::sync::Arc;

pub(crate) fn notice_strip(
    notice: &Arc<Notice>,
    compact: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let conflict = notice.kind == NoticeKind::Conflict;
    let (bg, fg) = if conflict {
        (theme.amber_soft, theme.amber_fg)
    } else {
        (theme.danger_soft, theme.danger_fg)
    };
    let id = Arc::as_ptr(notice).addr();
    let close_hub = hub.clone();
    let close_notice = Arc::clone(notice);
    div()
        .w_full()
        .rounded(radius::large())
        .bg(bg)
        .px(px(if compact { 10. } else { 12. }))
        .py(px(if compact { 6. } else { 8. }))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(fg)
                .when(compact, gpui::Styled::truncate)
                .child(format!("{}: {}", notice.context, notice.message)),
        )
        .when(conflict, |row| {
            let hub = hub.clone();
            let notice = Arc::clone(notice);
            row.child(pill_button(
                ("notice-reload", id),
                "再読み込み",
                Tone::Soft.icon(Icon::RefreshCw),
                theme,
                move |_, cx| hub.update(cx, |hub, cx| hub.dismiss_notice(&notice, true, cx)),
            ))
        })
        .child(circle_icon_button(
            ("notice-close", id),
            "通知を閉じる",
            CircleButtonStyle::new(Icon::Close, if compact { 19. } else { 22. }, 11.),
            theme,
            move |_, cx| {
                close_hub.update(cx, |hub, cx| hub.dismiss_notice(&close_notice, false, cx))
            },
        ))
}
