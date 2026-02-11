use crate::{
    Split,
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
    Battery(BatteryEvent),
    Tray(TrayEvent),
    Polling(polling::Event),
    Backlight,
}

#[derive(Debug)]
enum BatteryEvent {
    PowerOnline,
    PowerOffline,
    Capacity(u8),
    Status(fs::ChargingStatus),
}

pub async fn run() {
    let (mut notifier, mut receiver) = mpsc::channel(4);

    let (wayland_daemon, wayland_proxy, mut wayland_events) = wayland::new();
    let mut sender = notifier.clone();
    let wayland = async move {
        loop {
            sender
                .send(Event::Wayland(wayland_events.next().await.unwrap()))
                .await
                .unwrap();
        }
    };

    let mut sender = notifier.clone();
    let (hyprland_daemon, hyprctl) = hyprland::new().await.split();
    let init = if let Some(x) = hyprctl.as_ref() {
        Some(x.controller().await)
    } else {
        None
    };
    let hyprland = async {
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

    let mut events = notifier.clone();
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

    let mut sender = notifier.clone();
    let (polling_controller, mut signals) = mpsc::channel(1);
    let polling = polling::run(&mut signals, async |e| {
        sender.send(e.into()).await.unwrap();
    });

    let sender = notifier.clone();
    let (dbus_daemon, dbus_proxy) = modules::dbus::new(Dispatcher(sender)).await.split();
    let dbus = async {
        if let Some(daemon) = dbus_daemon {
            daemon.serve().await;
        }
    };

    notifier.flush().await.unwrap();
    let mut runner = Runner::new(
        wayland_proxy,
        wayland_daemon.display(),
        hyprctl,
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
                Event::App(event) => runner.dispatch_app_event(event),
            }
        }
    };

    std::future::join!(
        wayland_daemon.run(),
        wayland,
        consumer,
        hyprland,
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
