//! App draft state and theme for the GPUI Kit text controls.

use gpui::base::input::{InputBaseState, InputModeKind};
use gpui::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui::{
    App, Context, Entity, EntityInputHandler, EventEmitter, Hsla, SharedString, Window, prelude::*,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldLook {
    pub bg: Hsla,
    pub text: Hsla,
    pub borderless: bool,
}

enum Editor {
    Line(Entity<InputState>),
    Multiline(Entity<TextareaState>),
}

pub struct SubmitDraft;

pub struct Draft {
    editor: Option<Editor>,
    content: SharedString,
    placeholder: SharedString,
    multiline: bool,
    masked: bool,
    pending: bool,
    look: FieldLook,
}

impl EventEmitter<SubmitDraft> for Draft {}

impl Draft {
    pub fn new(placeholder: impl Into<SharedString>, multiline: bool) -> Self {
        Self {
            editor: None,
            content: SharedString::default(),
            placeholder: placeholder.into(),
            multiline,
            masked: false,
            pending: false,
            look: crate::ui::theme::Theme::default().field_look(false),
        }
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        if self.masked != masked {
            self.masked = masked;
            self.invalidate(cx);
        }
    }

    pub fn apply_look(&mut self, look: FieldLook, cx: &mut Context<Self>) {
        if self.look != look {
            self.look = look;
            cx.notify();
        }
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        if self.placeholder != placeholder {
            self.placeholder = placeholder;
            self.invalidate(cx);
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn composing(&self, window: &mut Window, cx: &mut App) -> bool {
        match &self.editor {
            Some(Editor::Line(state)) => state.update(cx, |state, cx| {
                state.marked_text_range(window, cx).is_some()
            }),
            _ => false,
        }
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_editor(window, cx);
        match &self.editor {
            Some(Editor::Line(state)) => state.update(cx, |state, cx| state.focus(window, cx)),
            Some(Editor::Multiline(state)) => state.update(cx, |state, cx| state.focus(window, cx)),
            None => unreachable!("sync_editor creates the input state"),
        }
    }

    pub fn set(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        let content = if self.multiline {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.replace(['\r', '\n'], " ")
        };
        self.content = content.into();
        self.invalidate(cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set("", cx);
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.pending = true;
        cx.notify();
    }

    fn observe<M: InputModeKind>(&self, state: &Entity<InputBaseState<M>>, cx: &mut Context<Self>) {
        cx.observe(state, |this, state, cx| {
            if !this.pending {
                let value = state.read(cx).value();
                if this.content != value {
                    this.content = value;
                    cx.notify();
                }
            }
        })
        .detach();
    }

    fn apply_pending<M: InputModeKind>(
        &self,
        state: &mut InputBaseState<M>,
        window: &mut Window,
        cx: &mut Context<InputBaseState<M>>,
    ) {
        state.set_placeholder(self.placeholder.clone(), window, cx);
        if state.value() != self.content {
            state.set_value(self.content.clone(), window, cx);
        }
    }

    // Forms may edit drafts through App callbacks before a window paints them.
    fn sync_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut self.pending);
        match &self.editor {
            Some(Editor::Line(state)) if pending => {
                state.update(cx, |state, cx| {
                    state.set_masked(self.masked, window, cx);
                    self.apply_pending(state, window, cx);
                });
            }
            Some(Editor::Multiline(state)) if pending => {
                state.update(cx, |state, cx| self.apply_pending(state, window, cx));
            }
            Some(_) => {}
            None if self.multiline => {
                let state = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .rows(2)
                        .placeholder(self.placeholder.clone())
                        .default_value(self.content.clone())
                });
                self.observe(&state, cx);
                self.editor = Some(Editor::Multiline(state));
            }
            None => {
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .masked(self.masked)
                        .placeholder(self.placeholder.clone())
                        .default_value(self.content.clone())
                });
                self.observe(&state, cx);
                cx.subscribe_in(&state, window, |_, state, event, window, cx| {
                    if matches!(
                        event,
                        InputEvent::PressEnter {
                            secondary: false,
                            shift: false
                        }
                    ) && !state.update(cx, |state, cx| {
                        state.marked_text_range(window, cx).is_some()
                    }) {
                        cx.emit(SubmitDraft);
                    }
                })
                .detach();
                self.editor = Some(Editor::Line(state));
            }
        }
    }
}

impl Render for Draft {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_editor(window, cx);
        match &self.editor {
            Some(Editor::Line(state)) => Input::new(state)
                .appearance(!self.look.borderless)
                .when(!self.look.borderless, |input| input.bg(self.look.bg))
                .text_color(self.look.text)
                .into_any_element(),
            Some(Editor::Multiline(state)) => Textarea::new(state)
                .bg(self.look.bg)
                .text_color(self.look.text)
                .into_any_element(),
            None => unreachable!("sync_editor creates the input state"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::test::TestWindowExt;
    use gpui::{AppContext, TestAppContext, WindowOptions, div};

    struct Form {
        line: Entity<Draft>,
        multiline: Entity<Draft>,
        submitted: usize,
    }

    impl Render for Form {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .child(self.line.clone())
                .child(self.multiline.clone())
        }
    }

    #[gpui::test]
    fn draft_dispatches_enter_after_composition_and_keeps_multiline_text(cx: &mut TestAppContext) {
        let (window, form) = cx.update(|cx| {
            crate::ui::theme::init_components(cx);
            gpui::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| {
                    let line = cx.new(|_| Draft::new("チャット", false));
                    cx.subscribe_in(
                        &line,
                        window,
                        |form: &mut Form, _, _: &SubmitDraft, _, _| {
                            form.submitted += 1;
                        },
                    )
                    .detach();
                    Form {
                        line,
                        multiline: cx.new(|_| Draft::new("説明", true)),
                        submitted: 0,
                    }
                })
            })
            .expect("open draft test window")
        });
        window
            .update(cx, |_, window, cx| {
                let line = form.read(cx).line.clone();
                line.update(cx, |this, cx| this.focus(window, cx));
                window.render_frame(cx);

                let Some(Editor::Line(editor)) = &line.read(cx).editor else {
                    panic!("line input state missing");
                };
                editor.clone().update(cx, |editor, cx| {
                    editor.replace_and_mark_text_in_range(None, "にほん", None, window, cx);
                });
                window.press("enter", cx);
            })
            .unwrap();
        cx.read(|cx| assert_eq!(form.read(cx).submitted, 0));

        window
            .update(cx, |_, window, cx| {
                window.input("日本", cx);
                window.press("enter", cx);
                let multiline = form.read(cx).multiline.clone();
                multiline.update(cx, |this, cx| {
                    this.set("一行目\r\n二行目", cx);
                    this.focus(window, cx);
                });
                window.render_frame(cx);
                window.press("end", cx);
                window.press("enter", cx);
                window.input("三行目", cx);
            })
            .unwrap();
        cx.read(|cx| {
            let form = form.read(cx);
            assert_eq!(form.submitted, 1);
            assert_eq!(form.line.read(cx).text(), "日本");
            let multiline = form.multiline.read(cx).text();
            assert!(multiline.contains("一行目\n"));
            assert!(multiline.contains("二行目"));
            assert!(multiline.contains("三行目"));
            assert_eq!(multiline.lines().count(), 3);
        });
    }
}
