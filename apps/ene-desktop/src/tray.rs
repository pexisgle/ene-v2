//! ksni owns registration/reconnection. A separate bounded probe leases hiding.
use crate::AppWindow;
use ksni::TrayMethods;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const LEASE: Duration = Duration::from_secs(2);
pub struct Registration {
    pub checked_at: Instant,
    pub registered: bool,
}
impl Registration {
    pub fn usable_at(&self, now: Instant) -> bool {
        self.registered && now.saturating_duration_since(self.checked_at) < LEASE
    }
}
type Callback = RefCell<Box<dyn Fn()>>;
pub struct Ui {
    actions: [Callback; 6],
    state: watch::Sender<(bool, bool)>,
    stop: CancellationToken,
    worker: RefCell<Option<std::thread::JoinHandle<()>>>,
}
macro_rules! callback {
    ($method:ident, $index:expr) => {
        pub fn $method(&self, callback: impl Fn() + 'static) {
            *self.actions[$index].borrow_mut() = Box::new(callback);
        }
    };
}
impl Ui {
    callback!(on_show_chat, 0);
    callback!(on_open_settings, 1);
    callback!(on_toggle_avatar, 2);
    callback!(on_recenter_avatar, 3);
    callback!(on_restart_avatar, 4);
    callback!(on_quit, 5);
    pub fn set_avatar_visible(&self, visible: bool) {
        self.state.send_if_modified(|state| {
            let changed = state.0 != visible;
            state.0 = visible;
            changed
        });
    }
    pub fn set_avatar_failed(&self, failed: bool) {
        self.state.send_if_modified(|state| {
            let changed = state.1 != failed;
            state.1 = failed;
            changed
        });
    }
    pub fn new(chat: slint::Weak<AppWindow>) -> (Rc<Self>, Receiver<Registration>) {
        let (state, updates) = watch::channel((true, false));
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = CancellationToken::new();
        let cancel = stop.clone();
        let worker_chat = chat.clone();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tray runtime");
            runtime.block_on(serve(worker_chat, updates, sender, cancel));
        });
        let ui = Rc::new(Self {
            actions: std::array::from_fn(|_| RefCell::new(Box::new(|| {}) as Box<dyn Fn()>)),
            state,
            stop,
            worker: RefCell::new(Some(worker)),
        });
        let weak = Rc::downgrade(&ui);
        if let Some(chat) = chat.upgrade() {
            chat.on_tray_action(move |index| {
                if let Some(ui) = weak.upgrade()
                    && let Some(callback) = ui.actions.get(index as usize)
                {
                    callback.borrow()();
                }
            });
        }
        (ui, receiver)
    }
}
impl Drop for Ui {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.get_mut().take() {
            let _ = worker.join();
        }
    }
}
struct Item {
    chat: slint::Weak<AppWindow>,
    visible: bool,
    failed: bool,
    online: Arc<AtomicBool>,
}
impl Item {
    fn action(&self, index: i32) {
        let _ = self
            .chat
            .upgrade_in_event_loop(move |chat| chat.invoke_tray_action(index));
    }
}
impl ksni::Tray for Item {
    fn id(&self) -> String {
        "ene".into()
    }
    fn title(&self) -> String {
        "ene".into()
    }
    fn icon_name(&self) -> String {
        "face-smile".into()
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.action(0);
    }
    fn watcher_online(&self) {
        self.online.store(true, Ordering::Release);
    }
    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        self.online.store(false, Ordering::Release);
        true
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let labels = [
            "対話を表示",
            "設定",
            if self.visible {
                "キャラクターを隠す"
            } else {
                "キャラクターを表示"
            },
            "キャラクターを画面中央へ戻す",
            "描画エラーから再起動",
            "ホスト選択・終了",
        ];
        labels
            .into_iter()
            .enumerate()
            .map(|(index, label)| {
                StandardItem {
                    label: label.into(),
                    enabled: index != 4 || self.failed,
                    activate: Box::new(move |item: &mut Self| item.action(index as i32)),
                    ..Default::default()
                }
                .into()
            })
            .collect()
    }
}
async fn serve(
    chat: slint::Weak<AppWindow>,
    mut updates: watch::Receiver<(bool, bool)>,
    sender: mpsc::SyncSender<Registration>,
    stop: CancellationToken,
) {
    let online = Arc::new(AtomicBool::new(true));
    let item = Item {
        chat,
        visible: true,
        failed: false,
        online: online.clone(),
    };
    // Starting may wait for D-Bus; it never blocks the UI or shutdown.
    let handle = tokio::select! { biased; _ = stop.cancelled() => return, result = item.assume_sni_available(true).spawn() => match result { Ok(handle) => handle, Err(error) => { eprintln!("tray: {error}"); return; } } };
    let mut connection = None;
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            changed = updates.changed() => {
                if changed.is_err() { break; }
                let (visible, failed) = *updates.borrow_and_update();
                handle.update(|item| { item.visible = visible; item.failed = failed; }).await;
            }
            _ = interval.tick() => {
                let checked_at = Instant::now();
                let registered = tokio::select! {
                    _ = stop.cancelled() => break,
                    result = tokio::time::timeout(Duration::from_millis(500), async {
                        if connection.is_none() { connection = Some(zbus::Connection::session().await?); }
                        probe(connection.as_ref().unwrap()).await
                    }) => match result {
                        Ok(Ok(registered)) => registered,
                        _ => { connection = None; false }
                    },
                };
                let registered = registered && online.load(Ordering::Acquire) && !handle.is_closed();
                if matches!(sender.try_send(Registration { checked_at, registered }), Err(mpsc::TrySendError::Disconnected(_))) { break; }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_millis(500), handle.shutdown()).await;
}
async fn probe(connection: &zbus::Connection) -> zbus::Result<bool> {
    let watcher = zbus::Proxy::new(
        connection,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await?;
    if !watcher
        .get_property::<bool>("IsStatusNotifierHostRegistered")
        .await?
    {
        return Ok(false);
    }
    let items: Vec<String> = watcher
        .get_property("RegisteredStatusNotifierItems")
        .await?;
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    for item in items {
        let service = item.split('/').next().unwrap_or_default();
        let Ok(service) = zbus::names::BusName::try_from(service) else {
            continue;
        };
        if bus.get_connection_unix_process_id(service).await.ok() == Some(std::process::id()) {
            return Ok(true);
        }
    }
    Ok(false)
}
