use super::*;
use crate::ui::connections::ConnectionsPane;
use ene_client_core::state::{Link, Notice, NoticeKind};
use gpui::{AppContext as _, TestAppContext};
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn only_revision_conflicts_request_a_refresh_for_notice_and_check_results() {
    for (kind, refresh) in [
        (NoticeKind::Conflict, true),
        (NoticeKind::Denied, false),
        (NoticeKind::Failed, false),
    ] {
        let notice = Notice {
            context: "fixture action".into(),
            message: "fixture result".into(),
            kind,
        };
        for update in [
            Update::Notice {
                notice: notice.clone(),
                read: None,
            },
            Update::Checked {
                connection_id: "fixture".into(),
                ticket: "fixture-check".into(),
                result: Err(notice.clone()),
            },
        ] {
            assert_eq!(ApplyFlags::from_update(&update).conflicted, refresh);
            let mut model = Model::new("fixture".into());
            model.apply_accepted(update);
            assert_eq!(*model.notices[0], notice);
        }
    }
}

fn test_hub(cx: &mut App, runtime: &tokio::runtime::Handle) -> gpui::Entity<Hub> {
    let (commands, _sent) = unbounded_channel();
    let (updates_tx, updates) = unbounded_channel();
    cx.new(|cx| {
        Hub::new(
            cx,
            HubInit {
                options: Options::default(),
                connection: Default::default(),
                commands,
                updates_tx,
                updates,
                tray: None,
                tray_rx: None,
                runtime: runtime.clone(),
                discovery: "fixture".into(),
            },
        )
    })
}

#[gpui::test]
fn accepted_credential_replies_clear_each_hidden_draft(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (hub, drafts) = cx.update(|cx| {
        let hub = test_hub(cx, runtime.handle());
        let pane = cx.new(|cx| ConnectionsPane::new(hub.clone(), cx));
        let theme = hub.read(cx).theme();
        let drafts = pane.update(cx, |pane, cx| {
            ["a", "b", "stale"].map(|id| {
                pane.drafts
                    .draft(cx, format!("conn:secret:{id}"), "", Some("edited"), &theme)
            })
        });
        (hub, (pane, drafts))
    });
    cx.update(|cx| {
        hub.update(cx, |hub, cx| {
            let epoch = hub.model.connections.link_epoch;
            for (epoch, seq, id) in [(epoch, 3, "a"), (epoch, 2, "b"), (epoch - 1, 4, "stale")] {
                hub.apply(
                    Update::Scoped {
                        epoch,
                        seq,
                        inner: Box::new(Update::CredentialRegistered {
                            connection_id: id.into(),
                        }),
                    },
                    cx,
                );
            }
        });
    });
    cx.update(|cx| {
        let (_pane, [a, b, stale]) = drafts;
        assert_eq!(a.read(cx).text(), "");
        assert_eq!(b.read(cx).text(), "");
        assert_eq!(stale.read(cx).text(), "edited");
    });
}

#[gpui::test]
fn a_probe_settles_only_its_own_pending_check(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    cx.update(|cx| {
        let hub = test_hub(cx, runtime.handle());
        hub.update(cx, |hub, cx| {
            hub.apply(
                Update::Link(Box::new(Link::Live {
                    version: "1".into(),
                })),
                cx,
            );
            let epoch = hub.model.connections.link_epoch;
            hub.setup
                .checking_connections
                .extend([("a".into(), "current".into()), ("b".into(), "other".into())]);
            let reply = |seq, id: &str, ticket: &str, result| Update::Scoped {
                epoch,
                seq,
                inner: Box::new(Update::Checked {
                    connection_id: id.into(),
                    ticket: ticket.into(),
                    result,
                }),
            };
            hub.apply(
                reply(
                    4,
                    "b",
                    "other",
                    Err(Notice {
                        context: "接続確認".into(),
                        message: "fixture failure".into(),
                        kind: NoticeKind::Failed,
                    }),
                ),
                cx,
            );
            hub.apply(reply(2, "a", "replaced", Ok(false)), cx);
            assert_eq!(hub.setup.checking_connections["a"], "current");
            assert!(!hub.setup.checking_connections.contains_key("b"));
            hub.apply(reply(3, "a", "current", Ok(false)), cx);
            assert_eq!(hub.setup.checking_connections.len(), 0);
            hub.setup
                .checking_connections
                .insert("a".into(), "disconnected".into());
            hub.apply(
                Update::Link(Box::new(Link::Unset {
                    path: "fixture".into(),
                    reason: "disconnected".into(),
                })),
                cx,
            );
            assert_eq!(hub.setup.checking_connections.len(), 0);
        });
    });
}

#[gpui::test]
fn setup_can_retry_after_a_failed_save_or_disconnect(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    cx.update(|cx| {
        let hub = test_hub(cx, runtime.handle());
        hub.update(cx, |hub, cx| {
            hub.apply(
                Update::Link(Box::new(Link::Live {
                    version: "1".into(),
                })),
                cx,
            );
            let epoch = hub.model.connections.link_epoch;
            hub.setup.stage = SetupStage::SavingConnection;
            hub.apply(
                Update::Scoped {
                    epoch,
                    seq: 1,
                    inner: Box::new(Update::SetupSaved(Err(Notice {
                        context: "接続設定の保存".into(),
                        message: "fixture timeout".into(),
                        kind: NoticeKind::Failed,
                    }))),
                },
                cx,
            );
            assert_eq!(hub.setup.stage, SetupStage::Idle);
            assert_eq!(hub.setup.error.as_ref().unwrap().message, "fixture timeout");

            hub.setup.stage = SetupStage::SavingConnection;
            hub.apply(
                Update::Link(Box::new(Link::Unset {
                    path: "fixture".into(),
                    reason: "disconnected".into(),
                })),
                cx,
            );
            hub.apply(
                Update::Scoped {
                    epoch,
                    seq: 2,
                    inner: Box::new(Update::SetupSaved(Ok("retired-connection".into()))),
                },
                cx,
            );
            assert_eq!(hub.setup.stage, SetupStage::Idle);
            assert!(hub.setup.error.is_some());
            assert!(hub.setup.connection_id().is_none());
        });
    });
}

#[gpui::test]
fn failed_model_assignment_keeps_the_chosen_connection(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    cx.update(|cx| {
        let hub = test_hub(cx, runtime.handle());
        hub.update(cx, |hub, cx| {
            let chosen = "chosen-provider";
            hub.setup.stage = SetupStage::Applying(chosen.into());
            hub.apply(
                Update::SetupApplied(Err(Notice {
                    context: "モデル割当".into(),
                    message: "fixture conflict".into(),
                    kind: NoticeKind::Failed,
                })),
                cx,
            );
            assert_eq!(hub.setup.stage, SetupStage::Choosing(chosen.into()));
            assert_eq!(
                hub.setup.error.as_ref().unwrap().message,
                "fixture conflict"
            );

            hub.setup.stage = SetupStage::Applying(chosen.into());
            hub.setup.disconnect();
            hub.apply(Update::SetupApplied(Ok("retired-result".into())), cx);
            assert_eq!(hub.setup.stage, SetupStage::Choosing(chosen.into()));
        });
    });
}
