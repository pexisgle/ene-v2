use super::*;
use crate::ui::connections::ConnectionsPane;
use gpui::{AppContext as _, TestAppContext};
use tokio::sync::mpsc::unbounded_channel;

#[gpui::test]
fn accepted_credential_replies_clear_each_hidden_draft(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (commands, _sent) = unbounded_channel();
    let (updates_tx, updates) = unbounded_channel();
    let (hub, drafts) = cx.update(|cx| {
        let hub = cx.new(|cx| {
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
                    runtime: runtime.handle().clone(),
                    discovery: "fixture".into(),
                },
            )
        });
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
            for (seq, id) in [(2, "a"), (3, "b"), (1, "stale")] {
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
