use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
#[derive(Clone)]
pub(crate) enum Action {
    Chat,
    Select(String),
    Settings,
    Hide,
    Mute,
    Center,
    Quit,
}

#[cfg(any(windows, target_os = "linux"))]
const MENU: [(&str, Action); 6] = [
    ("会話を開く", Action::Chat),
    ("設定を開く", Action::Settings),
    ("ウィンドウを隠す", Action::Hide),
    ("音声のオン・オフ", Action::Mute),
    ("アバターを中央に戻す", Action::Center),
    ("Ene を終了", Action::Quit),
];

pub(crate) type Resident = Arc<AtomicBool>;
type Sender = mpsc::Sender<Action>;

pub(crate) fn start(
    runtime: &tokio::runtime::Runtime,
    stop: tokio_util::sync::CancellationToken,
    send: Sender,
) -> Resident {
    let resident = Arc::new(AtomicBool::new(false));
    launch(runtime, resident.clone(), send, stop);
    resident
}

#[cfg(any(windows, target_os = "linux"))]
fn icon_rgba() -> Vec<u8> {
    (0_i32..32)
        .flat_map(|y| {
            (0_i32..32).flat_map(move |x| {
                let r = (2 * x - 31).pow(2) + (2 * y - 31).pow(2);
                match r {
                    0..=120 => [255, 253, 253, 255],
                    121..=790 => [207, 46, 104, 255],
                    _ => [0, 0, 0, 0],
                }
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn launch(
    runtime: &tokio::runtime::Runtime,
    resident: Resident,
    send: Sender,
    stop: tokio_util::sync::CancellationToken,
) {
    use ksni::TrayMethods as _;
    runtime.spawn(async move {
        while !stop.is_cancelled() {
            let tray = Linux {
                resident: resident.clone(),
                send: send.clone(),
            };
            if let Ok(handle) = tray.spawn().await {
                resident.store(true, Ordering::Release);
                stop.cancelled().await;
                handle.shutdown().await;
                resident.store(false, Ordering::Release);
                return;
            }
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            }
        }
    });
}

#[cfg(target_os = "linux")]
struct Linux {
    resident: Resident,
    send: Sender,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for Linux {
    fn id(&self) -> String {
        "ene".into()
    }

    fn title(&self) -> String {
        "Ene".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let mut argb = icon_rgba();
        for pixel in argb.as_chunks_mut::<4>().0 {
            pixel.rotate_right(1);
        }
        vec![ksni::Icon {
            width: 32,
            height: 32,
            data: argb,
        }]
    }

    fn activate(&mut self, _: i32, _: i32) {
        let _busy = self.send.try_send(Action::Chat);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        MENU.into_iter()
            .map(|(label, action)| {
                ksni::menu::StandardItem {
                    label: label.into(),
                    activate: Box::new(move |tray: &mut Self| {
                        let _busy = tray.send.try_send(action.clone());
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect()
    }

    fn watcher_online(&self) {
        self.resident.store(true, Ordering::Release);
    }

    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        self.resident.store(false, Ordering::Release);
        true
    }
}

#[cfg(windows)]
fn launch(
    _: &tokio::runtime::Runtime,
    resident: Resident,
    send: Sender,
    _: tokio_util::sync::CancellationToken,
) {
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconEvent, menu};
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, EventLoop},
        platform::windows::EventLoopBuilderExtWindows as _,
        window::WindowId,
    };

    struct Worker {
        resident: Resident,
        send: Sender,
        icon: Option<TrayIcon>,
    }

    impl ApplicationHandler for Worker {
        fn resumed(&mut self, _: &ActiveEventLoop) {
            if self.icon.is_some() {
                return;
            }
            let install = || -> anyhow::Result<TrayIcon> {
                let menu = menu::Menu::new();
                for (index, (label, _)) in MENU.iter().enumerate() {
                    menu.append(&menu::MenuItem::with_id(
                        index.to_string(),
                        *label,
                        true,
                        None,
                    ))?;
                }
                let menu_send = self.send.clone();
                menu::MenuEvent::set_event_handler(Some(Arc::new(
                    move |event: menu::MenuEvent| {
                        if let Some((_, action)) = event
                            .id
                            .0
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| MENU.get(index))
                        {
                            let _busy = menu_send.try_send(action.clone());
                        }
                    },
                )));
                let click_send = self.send.clone();
                TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        let _busy = click_send.try_send(Action::Chat);
                    }
                }));
                let icon = tray_icon::Icon::from_resource(1, None)
                    .or_else(|_| tray_icon::Icon::from_rgba(icon_rgba(), 32, 32))?;
                Ok(TrayIcon::builder()
                    .with_icon(icon)
                    .with_tooltip("Ene")
                    .with_menu(Box::new(menu))
                    .with_menu_on_left_click(false)
                    .build()?)
            };
            self.icon = install().ok();
            self.resident.store(self.icon.is_some(), Ordering::Release);
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    let _thread = std::thread::Builder::new()
        .name("ene-tray".into())
        .spawn(move || {
            if let Ok(events) = EventLoop::builder()
                .with_any_thread(true)
                .with_dpi_aware(false)
                .build()
            {
                let _closed = events.run_app(&mut Worker {
                    resident: resident.clone(),
                    send,
                    icon: None,
                });
            }
            resident.store(false, Ordering::Release);
        });
}

#[cfg(not(any(windows, target_os = "linux")))]
fn launch(
    _: &tokio::runtime::Runtime,
    _: Resident,
    _: Sender,
    _: tokio_util::sync::CancellationToken,
) {
}
