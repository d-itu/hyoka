use crate::{
    Split, TinyString,
    consumer::program::{Message, Runner},
    modules::{self, dbus::TrayEvent, fs, hyprland, polling, uevent},
    wayland,
};
use derive_more::From;
use futures::{
    SinkExt as _, StreamExt as _,
    channel::mpsc::{self, Sender},
};
use iced::Theme;
use iced_tiny_skia::Renderer;
use rustc_hash::FxHashMap;

#[derive(Debug, From)]
enum Event {
    Wayland(wayland::Event),
    #[from(forward)]
    App(AppEvent),
}

#[derive(Debug, From)]
enum AppEvent {
    Hyprland(hyprland::Event),
    WindowInfo(String),
    Urgent(usize),
    Battery(BatteryEvent),
    Tray(TrayEvent),
    Polling(polling::Event),
    Pipewire(modules::pipewire::Event),
    Backlight,
}

#[derive(Debug)]
enum BatteryEvent {
    PowerOnline,
    PowerOffline,
    Capacity(u8),
    Status(fs::ChargingStatus),
}

enum HyprlandReqToken {
    ActiveWindow,
    Urgent { win: TinyString },
}

#[derive(From)]
enum HyprlandRequest {
    Command(hyprland::Command),
    Query {
        query: hyprland::Query,
        token: HyprlandReqToken,
    },
}

pub async fn run() {
    let (mut sender, mut receiver) = mpsc::channel(4);

    let (wayland_daemon, wayland_proxy, mut wayland_events) = wayland::new();
    let mut events = sender.clone();
    let wayland = async move {
        loop {
            events
                .send(Event::Wayland(wayland_events.next().await.unwrap()))
                .await
                .unwrap();
        }
    };

    let (hyprland_daemon, hyprland_ctx) = hyprland::new().await.split();
    let init = if let Some(x) = hyprland_ctx.as_ref() {
        Some(x.controller().await)
    } else {
        None
    };
    let (hyprland_requests_sender, mut hyprland_requests_receiver) = mpsc::channel(1);
    let mut events = sender.clone();
    let hyprland = async {
        let mut sender = events.clone();
        let daemon = async {
            match hyprland_daemon {
                Some(daemon) => {
                    daemon
                        .run(init.unwrap(), async |event| {
                            sender
                                .send(Event::App(AppEvent::Hyprland(event)))
                                .await
                                .unwrap();
                        })
                        .await
                }
                None => {}
            }
        };
        let queries = async {
            if let Some(ctx) = hyprland_ctx {
                loop {
                    let requests = hyprland_requests_receiver.next().await.unwrap();
                    match requests {
                        HyprlandRequest::Command(command) => {
                            ctx.controller().await.command(command).await
                        }
                        HyprlandRequest::Query { query, token } => {
                            let res = ctx.controller().await.query(query).await;
                            match res {
                                hyprland::Response::Raw(res) => match token {
                                    HyprlandReqToken::ActiveWindow => {
                                        events.send(AppEvent::WindowInfo(res).into()).await.unwrap()
                                    }
                                    HyprlandReqToken::Urgent { win } => {
                                        fn find_workspace(
                                            win: TinyString,
                                            res: String,
                                        ) -> Option<usize> {
                                            let mut window_found = false;
                                            for line in res.as_bytes().split(|&x| x == b'\n') {
                                                if line.starts_with(b"Window") {
                                                    if line[b"Window ".len()..]
                                                        .starts_with(win.as_bytes())
                                                    {
                                                        window_found = true;
                                                    }
                                                } else if window_found
                                                    && line.starts_with(b"\tworkspace")
                                                {
                                                    let workspace = &line[b"\tworkspace: ".len()..];
                                                    let (id, _) =
                                                        workspace.split_once(|&x| x == b' ')?;
                                                    return Some(usize::from_ascii(id).unwrap());
                                                }
                                            }
                                            None
                                        }
                                        if let Some(workspace) = find_workspace(win, res) {
                                            events
                                                .send(AppEvent::Urgent(workspace).into())
                                                .await
                                                .unwrap()
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
        };
        std::future::join!(daemon, queries).await
    };

    let mut events = sender.clone();
    let uevent = uevent::new();
    let uevent = async {
        uevent
            .serve(async move |e| match e {
                uevent::Event::PowerOnline => {
                    events.send(BatteryEvent::PowerOnline.into()).await.unwrap()
                }
                uevent::Event::PowerOffline => events
                    .send(BatteryEvent::PowerOffline.into())
                    .await
                    .unwrap(),
                uevent::Event::BatCapacity(x) => {
                    events.send(BatteryEvent::Capacity(x).into()).await.unwrap()
                }
                uevent::Event::BatStatus(x) => {
                    events.send(BatteryEvent::Status(x).into()).await.unwrap()
                }
                uevent::Event::Backlight => {
                    events.send(AppEvent::Backlight.into()).await.unwrap();
                }
            })
            .await;
    };

    let mut events = sender.clone();
    let pipewire = modules::pipewire::Daemon::new().ok();
    let pipewire = async {
        if let Some(daemon) = pipewire {
            daemon
                .listen(async |e| events.send(e.into()).await.unwrap())
                .await
        }
    };

    let mut events = sender.clone();
    let (polling_controller, mut signals) = mpsc::channel(1);
    let polling = polling::run(&mut signals, async |e| {
        events.send(e.into()).await.unwrap();
    });

    let events = sender.clone();
    let (dbus_daemon, dbus_proxy) = modules::dbus::new(Dispatcher(events)).await.split();
    let dbus = async {
        if let Some(daemon) = dbus_daemon {
            daemon.serve().await;
        }
    };

    sender.flush().await.unwrap();
    let mut runner = Runner::new(
        wayland_proxy,
        wayland_daemon.display(),
        hyprland_requests_sender,
        dbus_proxy,
        polling_controller,
    );
    let consumer = async move {
        loop {
            // TODO: dispatch all pending events at once
            match receiver.next().await.unwrap() {
                Event::Wayland(event) => {
                    runner.dispatch_wayland_event(event).await;
                }
                Event::App(event) => runner.dispatch_app_event(event).await,
            }
        }
    };

    std::future::join!(
        wayland_daemon.run(),
        wayland,
        consumer,
        hyprland,
        pipewire,
        uevent,
        polling,
        dbus
    )
    .await;
}

type Callbacks = FxHashMap<wayland::Callback, Box<dyn FnOnce(&mut Runner)>>;

#[derive(Clone)]
struct Dispatcher(Sender<Event>);
impl modules::dbus::Dispatcher for Dispatcher {
    async fn dispatch(&mut self, e: impl Into<modules::dbus::Event>) {
        match e.into() {
            modules::dbus::Event::Tray(tray_event) => self
                .0
                .send(Event::App(AppEvent::Tray(tray_event)))
                .await
                .unwrap(),
        }
    }
}

type UserInterface<'ui> = iced_runtime::UserInterface<'ui, Message, Theme, Renderer>;
type Element<'ui> = iced::Element<'ui, Message, Theme, Renderer>;

mod program;
mod window;
