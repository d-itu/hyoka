use std::{
    cell::Cell,
    fs, io,
    num::NonZero,
    path::Path,
    ptr::{self, NonNull},
    rc::Rc,
};

use ahash::AHashMap;
use arrayvec::ArrayVec;
use derive_more::{Deref, From};
use futures::{SinkExt as _, channel::mpsc::Sender};
use iced::{
    Border, Color, Font, Length, Pixels, Point, Size, Theme, color,
    font::{Family, Stretch, Style, Weight},
    mouse::Cursor,
    theme::Palette,
    widget::{container, image, svg, text},
};
use iced_core::{layout::Limits, text::Shaping, widget::Tree};
use iced_tiny_skia::Renderer;
use lru::LruCache;
use rustc_hash::FxHashMap;
use rustix::{
    fs::{Mode, OFlags},
    mm::{MapFlags, ProtFlags},
    path::Arg as _,
};

use crate::{
    TinyString,
    consumer::{
        AppEvent, BatteryEvent, Dispatcher, Element, HyprlandReqToken, HyprlandRequest,
        window::{Role, Tag, Window, WindowManager},
    },
    mapping::Mapping,
    modules::{
        self,
        clock::Clock,
        dbus::{Tray, TrayEvent},
        fs::{Backlight, Battery, ChargingStatus},
        hyprland, polling,
    },
    wayland,
};

const BAR_HEIGHT: u32 = 35;
const WORKSPACE_MAX: usize = 10;

#[derive(Debug, Clone)]
pub enum Message {
    Hello,
    Workspace { id: u8 },
    WindowInfo,
    Battery,
    TrayTooltip(Tray),
    TrayAction(Tray),
    CloseTooltip,
    BatteryStop,
    Backlight,
    Volume,
}

type Callbacks = FxHashMap<wayland::Callback, Box<dyn FnOnce(&mut Runner)>>;

struct BitSet(u16);

impl BitSet {
    fn new() -> Self {
        BitSet(0)
    }
    fn set(&mut self, idx: usize) {
        self.0 |= 1 << idx;
    }
    fn unset(&mut self, idx: usize) {
        self.0 &= !(1 << idx);
    }
    fn get(&self, idx: usize) -> bool {
        (self.0 & (1 << idx)) != 0
    }
}

struct WindowInfo {
    class: TinyString,
    title: TinyString,
    icon: Option<Handle>,
}

struct TrayItem {
    icon: Option<Handle>,
}

#[derive(From, Default, Deref)]
struct Attr<T>(T);

impl<T: PartialEq> Attr<T> {
    fn update(&mut self, value: T) -> bool {
        if &self.0 != &value {
            self.0 = value;
            true
        } else {
            false
        }
    }
}

struct BatteryStatus {
    device: Rc<Battery>,
    charging: Attr<Option<bool>>,
    status: Attr<ChargingStatus>,
    capacity: Attr<u8>,
    icon: Option<Handle>,
}

impl BatteryStatus {
    fn new(icon_cache: &mut IconCache) -> Option<Self> {
        let device = Battery::new()?;
        let mut res = Self {
            charging: None.into(),
            status: device.status().into(),
            capacity: device.capacity().into(),
            device: Rc::new(device),
            icon: None,
        };
        res.load_icon(icon_cache);
        Some(res)
    }
    fn load_icon(&mut self, icon_cache: &mut IconCache) {
        self.icon = icon_cache.load(&self.icon().into(), true);
    }
    fn charged(&self) -> bool {
        self.status.0 == ChargingStatus::Full || self.capacity.0 >= 99
    }
    fn charging(&self) -> bool {
        self.charging.0 == Some(true) || self.status.0 == ChargingStatus::Charging
    }
    fn icon(&self) -> String {
        if self.charged() {
            "battery-level-100-charged-symbolic".into()
        } else {
            let level = self.capacity.0 / 10 * 10;
            let state = if self.charging() { "-charging" } else { "" };
            format!("battery-level-{level}{state}-symbolic")
        }
    }
}

struct BacklightStatus {
    device: Backlight,
    percentage: u32,
    icon: Option<Handle>,
}

impl BacklightStatus {
    fn new(icon_cache: &mut IconCache) -> Option<Self> {
        Backlight::new()
            .map(|device| Self {
                percentage: device.percentage(),
                icon: None,
                device,
            })
            .map(|mut x| {
                x.load_icon(icon_cache);
                x
            })
    }
    fn icon(&self) -> &'static str {
        match self.percentage {
            ..33 => "display-brightness-low-symbolic",
            33..66 => "display-brightness-medium-symbolic",
            66.. => "display-brightness-high-symbolic",
        }
    }
    fn load_icon(&mut self, icon_cache: &mut IconCache) {
        self.icon = icon_cache.load(&self.icon().to_string().into(), true)
    }
    fn update(&mut self, icon_cache: &mut IconCache) {
        self.percentage = self.device.percentage();
        self.load_icon(icon_cache)
    }
    fn tooltip(&self) -> TooltipContent {
        let mut text = TinyString::new();
        use std::fmt::Write;
        write!(&mut text, "{}", self.percentage).unwrap();
        TooltipContent {
            token: TooltipToken::Backlight,
            text,
        }
    }
}

#[derive(Default)]
struct Volume {
    display: TinyString,
    route: TinyString,
    mute: Option<bool>,
    value: Option<f32>,
    icon: Option<Handle>,
}

impl Volume {
    fn icon(&self) -> Option<&'static str> {
        Some(if self.route == "Headphones" {
            "headphones-symbolic"
        } else if self.mute? {
            "audio-volume-muted-symbolic"
        } else {
            match self.value? {
                ..0.33 => "audio-volume-low-symbolic",
                0.33..=0.67 => "audio-volume-medium-symbolic",
                0.67..=1.0 => "audio-volume-high-symbolic",
                1.0.. => "audio-volume-overamplified-symbolic",
                _ => None?,
            }
        })
    }
    fn load_icon(&mut self, icon_cache: &mut IconCache) -> Option<()> {
        self.icon = icon_cache.with_size(&self.icon()?.to_string().into(), 16, true);
        Some(())
    }
    fn tooltip(&self) -> Option<TooltipContent> {
        let mut text = TinyString::new();
        use std::fmt::Write;
        if self.mute? {
            write!(&mut text, "Muted").unwrap();
        } else {
            write!(&mut text, "{:.0}", self.value? * 100.0).unwrap();
        }
        Some(TooltipContent {
            token: TooltipToken::Volume,
            text,
        })
    }
}

#[derive(Debug, Clone)]
enum Handle {
    Pixmap(image::Handle),
    Svg(svg::Handle),
}

impl Handle {
    fn load(&self) -> Element<'_> {
        self.load_size(24)
    }
    fn load_size(&self, size: impl Into<Length> + Copy) -> Element<'_> {
        match self {
            Handle::Pixmap(handle) => image(handle).width(size).into(),
            Handle::Svg(handle) => svg(handle.clone()).width(size).height(size).into(),
        }
    }
}

#[derive(PartialEq)]
enum TooltipToken {
    WindowInfo,
    Tray(Tray),
    Volume,
    Backlight,
    Battery,
}

struct TooltipContent {
    token: TooltipToken,
    text: TinyString,
}

impl TooltipContent {
    fn view(&self) -> Element<'_> {
        match self.token {
            TooltipToken::WindowInfo => tooltip_text(&self.text.trim_end(), 13.0, Shaping::Auto),
            _ => tooltip_text(&self.text, 10.0, Shaping::Basic),
        }
    }
}

struct Tooltip {
    content: TooltipContent,
    window: Window,
}

pub struct Runner {
    pub wayland: wayland::Proxy,
    hyprctl: Sender<HyprlandRequest>,
    dbus: Option<modules::dbus::Proxy<Dispatcher>>,
    polling: Sender<polling::Signal>,

    pub display: NonNull<wayland::ffi::wl_display>,
    window_manager: WindowManager,
    pub callbacks: Callbacks,
    tooltip: Option<Tooltip>,
    pub pointer: NonNull<wayland::ffi::wl_pointer>,
    pub cursor_shape_device: NonNull<wayland::ffi::wp_cursor_shape_device_v1>,
    pub theme: Theme,

    workspaces: BitSet,
    urgent_workspaces: BitSet,
    workspace_focused: usize,
    window: WindowInfo,

    tray_items: AHashMap<Tray, TrayItem>,

    battery_status: Option<BatteryStatus>,
    backlight: Option<BacklightStatus>,
    volume: Volume,

    date: ArrayVec<u8, 12>,
    time: [u8; 8],
    weekday: &'static str,

    icon_cache: IconCache,
}

struct IconCache(LruCache<TinyString, Option<Handle>, ahash::RandomState>);

impl IconCache {
    fn new() -> Self {
        Self(LruCache::with_hasher(
            NonZero::new(16).unwrap(),
            ahash::RandomState::with_seeds(114, 514, 1919, 810),
        ))
    }
    #[must_use]
    fn with_size(&mut self, key: &TinyString, size: u16, symbolic: bool) -> Option<Handle> {
        self.0
            .get_or_insert_ref(key, || {
                cosmic_freedesktop_icons::lookup(key)
                    .with_size(size)
                    // .with_theme("Adwaita")
                    .with_theme("Tela-dracula-dark")
                    // .with_theme("Papirus")
                    .find()
                    .and_then(|path| match path.extension()?.as_encoded_bytes() {
                        b"svg" => {
                            if symbolic {
                                load_symbolic(path, &theme()).map(Handle::Svg)
                            } else {
                                load_svg(path).map(Handle::Svg)
                            }
                        }
                        b"png" => load_png(path).map(Handle::Pixmap),
                        _ => None,
                    })
            })
            .clone()
    }
    #[must_use]
    fn load(&mut self, key: &TinyString, symbolic: bool) -> Option<Handle> {
        self.with_size(key, 64, symbolic)
    }
}

impl Runner {
    pub fn new(
        mut wayland: wayland::Proxy,
        display: NonNull<wayland::ffi::wl_display>,
        hyprctl: Sender<HyprlandRequest>,
        dbus: Option<modules::dbus::Proxy<Dispatcher>>,
        polling: Sender<polling::Signal>,
    ) -> Self {
        let mut window_manager = WindowManager::default();
        let surface =
            unsafe { wayland::ffi::wl_compositor_create_surface(wayland.globals.compositer()) };
        unsafe {
            wayland::ffi::wl_surface_add_listener(
                surface,
                &wayland::SURFACE_LISTENER,
                &raw mut *wayland.notifier as _,
            );
        };
        let layer_surface = unsafe {
            wayland::ffi::zwlr_layer_shell_v1_get_layer_surface(
                wayland.globals.layer_shell(),
                surface,
                ptr::null_mut(),
                wayland::ffi::ZWLR_LAYER_SHELL_V1_LAYER_TOP,
                c"hyoka".as_ptr(),
            )
        };
        unsafe {
            wayland::ffi::zwlr_layer_surface_v1_add_listener(
                layer_surface,
                &wayland::LAYER_SURFACE_LISTENER,
                &raw mut *wayland.notifier as _,
            );
            wayland::ffi::zwlr_layer_surface_v1_set_size(layer_surface, 0, BAR_HEIGHT);
            wayland::ffi::zwlr_layer_surface_v1_set_anchor(
                layer_surface,
                wayland::ffi::ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP
                    | wayland::ffi::ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT
                    | wayland::ffi::ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT,
            );
            wayland::ffi::zwlr_layer_surface_v1_set_exclusive_zone(layer_surface, 35);
            wayland::ffi::wl_surface_commit(surface);

            wayland::ffi::wl_display_flush(display.as_ptr());
        }
        window_manager.create_window(
            NonNull::new(surface).unwrap(),
            Role::Layer {
                layer_surface: NonNull::new(layer_surface).unwrap(),
            },
            Tag::Bar,
            renderer(),
        );

        let pointer = unsafe { wayland::ffi::wl_seat_get_pointer(wayland.globals.seat()) };
        unsafe {
            wayland::ffi::wl_pointer_add_listener(
                pointer,
                &wayland::POINTER_LISTENERL,
                &raw mut *wayland.notifier as _,
            )
        };
        let cursor_shape_device = unsafe {
            wayland::ffi::wp_cursor_shape_manager_v1_get_pointer(
                wayland.globals.cursor_shape_manager(),
                pointer,
            )
        };

        let now = Clock::now();
        let mut icon_cache = IconCache::new();
        Self {
            wayland,
            display,
            hyprctl,
            dbus,
            polling,

            tooltip: None,
            window_manager,
            theme: theme(),
            pointer: NonNull::new(pointer).unwrap(),
            cursor_shape_device: NonNull::new(cursor_shape_device).unwrap(),
            callbacks: Default::default(),

            workspaces: BitSet::new(),
            urgent_workspaces: BitSet::new(),
            workspace_focused: usize::MAX,
            window: WindowInfo {
                class: TinyString::new(),
                title: TinyString::new(),
                icon: None,
            },
            tray_items: AHashMap::with_hasher(ahash::RandomState::with_seeds(114, 514, 1919, 810)),
            battery_status: BatteryStatus::new(&mut icon_cache),
            backlight: BacklightStatus::new(&mut icon_cache),
            volume: Default::default(),
            date: now.date(),
            time: now.time(),
            weekday: now.weekday(),
            icon_cache,
        }
    }
    pub fn view(&self, tag: Tag) -> Element<'_> {
        match tag {
            Tag::Bar => self.bar(),
            Tag::Tooltip => match self.tooltip {
                Some(Tooltip { ref content, .. }) => content.view(),
                None => "".into(),
            },
        }
    }
    fn close_tooltip(&mut self) {
        if let Some(tooltip) = self.tooltip.take() {
            self.window_manager.close_window(tooltip.window.surface());
        }
    }
    fn set_tooltip(&mut self, content: TooltipContent) -> Option<()> {
        if self.tooltip.is_some() {
            None?
        }
        let w = self.window_manager.focused()?.clone();
        let state = w.state.borrow();
        if let Cursor::Available(Point { x, .. }) = state.cursor {
            self.tooltip = popup(
                &mut self.wayland,
                &mut self.window_manager,
                self.display,
                content.view(),
                [x as _, BAR_HEIGHT + 1],
                &w.surface().role,
            )
            .cloned()
            .map(|window| Tooltip { content, window });
        }
        Some(())
    }
    pub async fn update(&mut self, message: Message) -> Option<()> {
        match message {
            Message::Hello => {}
            Message::Workspace { id } => {
                self.hyprctl
                    .send(hyprland::Command::Workspace(id).into())
                    .await
                    .unwrap();
            }
            Message::WindowInfo => {
                self.hyprctl
                    .send(HyprlandRequest::Query {
                        query: hyprland::Query::ActiveWindow,
                        token: HyprlandReqToken::ActiveWindow,
                    })
                    .await
                    .unwrap();
            }
            Message::Battery => {
                self.set_tooltip(TooltipContent {
                    token: TooltipToken::Battery,
                    text: self.battery_status.as_ref()?.device.info().tooltip(),
                });
                self.polling
                    .send(polling::Signal::Battery(
                        self.battery_status.as_ref()?.device.clone(),
                    ))
                    .await
                    .unwrap();
            }
            Message::Backlight => {
                self.set_tooltip(self.backlight.as_ref()?.tooltip())?;
            }
            Message::Volume => {
                self.set_tooltip(self.volume.tooltip()?)?;
            }
            Message::TrayTooltip(service) => {
                let content = TinyString::from_string(
                    self.dbus.as_mut()?.tray_tooltip(service.clone()).await?,
                );
                self.set_tooltip(TooltipContent {
                    token: TooltipToken::Tray(service),
                    text: content,
                });
            }
            Message::BatteryStop => {
                self.close_tooltip();
                self.polling
                    .send(polling::Signal::BatteryStop)
                    .await
                    .unwrap();
            }
            Message::TrayAction(service) => self.dbus.as_mut()?.tray_action(service).await,
            Message::CloseTooltip => self.close_tooltip(),
        }
        None
    }
    pub async fn dispatch_wayland_event(&mut self, event: wayland::Event) -> Option<()> {
        match event {
            wayland::Event::Resize { object, size } => {
                self.window_manager
                    .find_by_object(object)?
                    .clone()
                    .resize(size, self);
            }
            wayland::Event::Rescale { surface, factor } => {
                self.window_manager
                    .find_by_object(surface)?
                    .clone()
                    .rescale(factor, self);
            }
            wayland::Event::Enter { surface, serial } => {
                let win = self.window_manager.find_by_object(surface)?.clone();
                win.mouse(iced::mouse::Event::CursorEntered, self).await;
                win.enter(serial);
                self.window_manager.focused = Some(surface);
            }
            wayland::Event::Mouse(event) => {
                let window = self.window_manager.focused()?;
                window.clone().mouse(event, self).await;
                match event {
                    iced::mouse::Event::CursorLeft => {
                        self.window_manager.focused.take();
                    }
                    _ => (),
                }
            }
            wayland::Event::CallbackDone(cb) => self.callbacks.remove(&cb).unwrap()(self),
        }
        Some(())
    }
    pub async fn dispatch_app_event(&mut self, event: AppEvent) {
        let mut update_tooltip = false;
        match event {
            AppEvent::Hyprland(event) => match event {
                hyprland::Event::Workspace { id } => {
                    self.workspace_focused = id - 1;
                    self.urgent_workspaces.unset(id - 1);
                }
                hyprland::Event::CreateWorkspace { id } => self.workspaces.set(id - 1),
                hyprland::Event::DestroyWorkspace { id } => {
                    self.workspaces.unset(id - 1);
                    self.urgent_workspaces.unset(id - 1);
                }
                hyprland::Event::ActiveWindow { class, title } => {
                    self.window = WindowInfo {
                        icon: if class.is_empty() {
                            None
                        } else {
                            self.icon_cache.load(&class, false)
                        },
                        class: truncate(class.clone(), 15, "…"),
                        title: truncate(title.clone(), 50, "…"),
                    }
                }
                hyprland::Event::Urgent { win } => self
                    .hyprctl
                    .send(HyprlandRequest::Query {
                        query: hyprland::Query::Clients,
                        token: HyprlandReqToken::Urgent { win },
                    })
                    .await
                    .unwrap(),
            },
            AppEvent::WindowInfo(x) => {
                self.set_tooltip(TooltipContent {
                    token: TooltipToken::WindowInfo,
                    text: x.replace('\t', "        ").into(),
                });
            }
            AppEvent::Urgent(id) => {
                self.urgent_workspaces.set(id - 1);
            }
            AppEvent::Battery(e) => {
                if let Some(bat) = &mut self.battery_status {
                    match e {
                        BatteryEvent::PowerOnline => {
                            if bat.charging.update(Some(true)) {
                                bat.load_icon(&mut self.icon_cache);
                            }
                        }
                        BatteryEvent::PowerOffline => {
                            if bat.charging.update(Some(false)) {
                                bat.load_icon(&mut self.icon_cache);
                            }
                        }
                        BatteryEvent::Capacity(x) => {
                            if bat.capacity.update(x) {
                                bat.load_icon(&mut self.icon_cache);
                            }
                        }
                        BatteryEvent::Status(x) => {
                            if bat.status.update(x) {
                                bat.load_icon(&mut self.icon_cache);
                            }
                        }
                    };
                }
            }
            AppEvent::Polling(e) => match e {
                polling::Event::Clock(e) => {
                    self.date = e.date();
                    self.time = e.time();
                    self.weekday = e.weekday();
                }
                polling::Event::Battery(info) => {
                    if let Some(Tooltip {
                        content:
                            TooltipContent {
                                token: TooltipToken::Battery,
                                text,
                            },
                        ..
                    }) = &mut self.tooltip
                    {
                        update_tooltip = true;
                        *text = info.tooltip()
                    }
                }
            },
            AppEvent::Tray(e) => match e {
                TrayEvent::Registered { service, icon_name } => {
                    let icon_name = TinyString::from_str(unsafe {
                        str::from_utf8_unchecked(icon_name.as_bytes())
                    });
                    let icon = self.icon_cache.load(&icon_name.into(), false);
                    self.tray_items.insert(service.clone(), TrayItem { icon });
                }
                TrayEvent::NewIcon { service, icon_name } => {
                    let icon_name = TinyString::from_str(unsafe {
                        str::from_utf8_unchecked(icon_name.as_bytes())
                    });
                    let icon = self.icon_cache.load(&icon_name.into(), false);
                    if let Some(item) = self.tray_items.get_mut(&service) {
                        item.icon = icon;
                    }
                }
                TrayEvent::Unregistered(service) => {
                    self.tray_items.remove(&service);
                }
                TrayEvent::Disconnected => {
                    self.tray_items.clear();
                }
            },
            AppEvent::Backlight => {
                self.backlight
                    .as_mut()
                    .unwrap()
                    .update(&mut self.icon_cache);
                if let (
                    Some(Tooltip {
                        content:
                            TooltipContent {
                                token: TooltipToken::Backlight,
                                text,
                            },
                        ..
                    }),
                    Some(backlight),
                ) = (&mut self.tooltip, &self.backlight)
                {
                    update_tooltip = true;
                    *text = backlight.tooltip().text
                }
            }
            AppEvent::Pipewire(e) => {
                match e {
                    modules::pipewire::Event::DefaultChanged { display, route } => {
                        self.volume.display = display;
                        self.volume.route = route;
                    }
                    modules::pipewire::Event::Volume(x) => {
                        self.volume.value.replace(x);
                    }
                    modules::pipewire::Event::Mute(x) => {
                        self.volume.mute.replace(x);
                    }
                };
                self.volume.load_icon(&mut self.icon_cache);
                if let (
                    Some(Tooltip {
                        content:
                            TooltipContent {
                                token: TooltipToken::Volume,
                                text,
                            },
                        ..
                    }),
                    Some(x),
                ) = (&mut self.tooltip, self.volume.tooltip())
                {
                    update_tooltip = true;
                    *text = x.text
                }
            }
        }
        for w in self.window_manager.iter() {
            w.state.borrow_mut().config_state.outdate();
            match w.tag {
                Tag::Bar => w.request_redraw(&mut self.wayland.notifier, &mut self.callbacks),
                Tag::Tooltip => {
                    if update_tooltip {
                        match &w.surface().role {
                            Role::Layer { .. } => {}
                            Role::Popup { size, .. } => {
                                let mut state = w.state.borrow_mut();
                                let new_size = {
                                    let mut view = self.view(w.tag);
                                    let mut tree = Tree::new(&view);
                                    let node = view.as_widget_mut().layout(
                                        &mut tree,
                                        &mut state.renderer,
                                        &Limits::new(Size::ZERO, Size::INFINITE),
                                    );
                                    node.bounds().size()
                                };
                                let size = size.replace(new_size);
                                if size.width < new_size.width || size.height < new_size.height {
                                    state.resize(
                                        [new_size.width, new_size.height].map(|x| x as _),
                                        w.surface().surface,
                                        w.tag,
                                        self,
                                    );
                                }
                            }
                        }
                        w.request_redraw(&mut self.wayland.notifier, &mut self.callbacks);
                    }
                }
            }
        }
        unsafe {
            wayland::ffi::wl_display_flush(self.display.as_ptr());
        }
    }

    pub fn background(&self, tag: Tag) -> Color {
        match tag {
            Tag::Bar => self.theme.palette().background,
            Tag::Tooltip => Color::TRANSPARENT,
        }
    }
}

fn popup<'a>(
    wayland: &mut wayland::Proxy,
    wm: &'a mut WindowManager,
    display: NonNull<wayland::ffi::wl_display>,
    mut view: Element,
    [x, y]: [u32; 2],
    parent: &Role,
) -> Option<&'a Window> {
    let mut renderer = renderer();
    let Size { width, height } = {
        let mut tree = Tree::new(&view);
        let node = view.as_widget_mut().layout(
            &mut tree,
            &mut renderer,
            &Limits::new(Size::ZERO, Size::INFINITE),
        );
        node.bounds().size()
    };
    let size = width * height;
    if size == 0.0 {
        return None;
    }
    if size == f32::INFINITY {
        tracing::error!("window has infinity size");
        return None;
    }

    let surface =
        unsafe { wayland::ffi::wl_compositor_create_surface(wayland.globals.compositer()) };
    unsafe {
        wayland::ffi::wl_surface_add_listener(
            surface,
            &wayland::SURFACE_LISTENER,
            &raw mut *wayland.notifier as _,
        );
    };
    let xdg_surface =
        unsafe { wayland::ffi::xdg_wm_base_get_xdg_surface(wayland.globals.wm_base(), surface) };
    unsafe {
        wayland::ffi::xdg_surface_add_listener(
            xdg_surface,
            &wayland::XDG_SURFACE_LISTENER,
            ptr::null_mut(),
        );
    }
    let positioner =
        unsafe { wayland::ffi::xdg_wm_base_create_positioner(wayland.globals.wm_base()) };

    unsafe {
        wayland::ffi::xdg_positioner_set_size(positioner, width as _, height as _);
        wayland::ffi::xdg_positioner_set_anchor_rect(positioner, x as _, y as _, 1, 1);
        wayland::ffi::xdg_positioner_set_anchor(
            positioner,
            wayland::ffi::XDG_POSITIONER_ANCHOR_BOTTOM,
        );
        wayland::ffi::xdg_positioner_set_gravity(
            positioner,
            wayland::ffi::XDG_POSITIONER_GRAVITY_BOTTOM,
        );
        wayland::ffi::xdg_positioner_set_constraint_adjustment(
            positioner,
            wayland::ffi::XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_SLIDE_X
                | wayland::ffi::XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_SLIDE_Y,
        );
    }

    let popup = unsafe {
        match parent {
            Role::Layer { layer_surface } => {
                let popup =
                    wayland::ffi::xdg_surface_get_popup(xdg_surface, ptr::null_mut(), positioner);
                wayland::ffi::zwlr_layer_surface_v1_get_popup(layer_surface.as_ptr(), popup);
                popup
            }
            Role::Popup {
                xdg_surface: parent,
                ..
            } => wayland::ffi::xdg_surface_get_popup(xdg_surface, parent.as_ptr(), positioner),
        }
    };
    unsafe {
        wayland::ffi::xdg_popup_add_listener(
            popup,
            &wayland::XDG_POPUP_LISTENER,
            &raw mut *wayland.notifier as _,
        );
    };
    unsafe {
        wayland::ffi::wl_surface_commit(surface);
        wayland::ffi::wl_display_flush(display.as_ptr());
    };
    let surface = NonNull::new(surface).unwrap();
    let win = wm.create_window(
        surface,
        Role::Popup {
            xdg_surface: NonNull::new(xdg_surface).unwrap(),
            popup: NonNull::new(popup).unwrap(),
            positioner: NonNull::new(positioner).unwrap(),
            size: Cell::new(Size::new(width, height)),
        },
        Tag::Tooltip,
        renderer,
    );
    Some(win)
}

fn renderer() -> iced_tiny_skia::Renderer {
    Renderer::new(
        Font {
            family: Family::Name("SF Pro Display"),
            weight: Weight::Normal,
            stretch: Stretch::Normal,
            style: Style::Normal,
        },
        Pixels(15.5),
    )
}

trait ColorExt {
    fn with_alpha(self, a: f32) -> Self;
}

impl ColorExt for Color {
    fn with_alpha(self, a: f32) -> Self {
        let Self { r, g, b, a: _ } = self;
        Self { r, g, b, a }
    }
}

fn load_svg(path: impl AsRef<Path>) -> Option<svg::Handle> {
    let path = path.as_ref();
    let fd = rustix::fs::open(path, OFlags::CLOEXEC, Mode::empty()).unwrap();
    let mapping = Mapping::map(fd, ProtFlags::READ, MapFlags::PRIVATE).unwrap();
    let text = unsafe { str::from_utf8_unchecked(mapping.as_bytes()) };
    let tree = usvg::Tree::from_str(text, &usvg::Options::default())
        .inspect_err(|err| tracing::warn!("cannot parse {path:?}: {err:?}"))
        .ok()?;
    Some(svg::Handle::from_tree(tree))
}

fn load_symbolic(path: impl AsRef<Path>, theme: &Theme) -> Option<svg::Handle> {
    let path = path.as_ref();
    let data = fs::read_to_string(path)
        .unwrap()
        .replace("currentColor", &theme.palette().text.to_string());
    let tree = usvg::Tree::from_str(
        &data,
        &usvg::Options {
            style_sheet: Some(theme.css_injection()),
            ..Default::default()
        },
    )
    .inspect_err(|err| tracing::warn!("cannot parse {path:?}: {err:?}"))
    .ok()?;
    Some(svg::Handle::from_tree(tree))
}

fn load_png(path: impl AsRef<Path>) -> Option<image::Handle> {
    let path = path.as_ref();
    let path = path.as_cow_c_str().unwrap();
    let fd = rustix::fs::open(path.as_c_str(), OFlags::CLOEXEC, Mode::empty()).ok()?;
    let data = Mapping::map(fd, ProtFlags::READ, MapFlags::PRIVATE).ok()?;
    let cursor = io::Cursor::new(data.as_bytes());
    let decoder = png::Decoder::new(cursor);

    let mut reader = decoder.read_info().unwrap();
    let len = reader.output_buffer_size().unwrap();
    let buf = Mapping::anon(len, ProtFlags::READ | ProtFlags::WRITE, MapFlags::PRIVATE).unwrap();
    let info = reader
        .next_frame(buf.as_bytes_mut())
        .inspect_err(|e| tracing::warn!("cannot decode {path:?}: {e}"))
        .ok()?;
    match info.color_type {
        png::ColorType::Rgba => {}
        x => {
            tracing::warn!("{path:?} has unsupported color type: {x:?}");
            return None;
        }
    }
    match info.bit_depth {
        png::BitDepth::Eight => {}
        x => {
            tracing::warn!("{path:?} has unsupported {}-bit depth", x as u32);
            return None;
        }
    }
    let handle = image::Handle::from_rgba(info.width, info.height, buf);
    Some(handle)
}

fn truncate(mut s: TinyString, mut len: usize, ellipsis: &str) -> TinyString {
    if s.len() < len {
        s
    } else {
        len -= ellipsis.len();
        while !s.is_char_boundary(len) {
            len -= 1
        }
        s.truncate(len);
        s.push_str(ellipsis);
        s
    }
}

const BACKGROUND: Color = Color::from_rgba8(30, 28, 34, 0.38);

const PURPLE: Color = color!(0xa476f7);
const WHITE: Color = color!(0xcdd6f5);
const GREEN: Color = color!(0x92b673);
const YELLOW: Color = color!(0xe09733);
const RED: Color = color!(0xf25b4f);

fn theme() -> Theme {
    Theme::custom(
        "paper dark",
        Palette {
            background: BACKGROUND,
            text: WHITE,
            primary: PURPLE,
            success: GREEN,
            warning: YELLOW,
            danger: RED,
        },
    )
}

trait StyleSheet {
    fn css_injection(&self) -> String;
}
// foreground’, ‘success’, ‘warning’, ‘error’, ‘accent’
impl StyleSheet for Theme {
    fn css_injection(&self) -> String {
        let Palette {
            text,
            primary,
            success,
            warning,
            danger,
            ..
        } = self.palette();
        format!(
            concat!(
                "* {{ fill:{} }}",
                ".foreground {{ fill:{} }}",
                ".success {{ fill:{} }}",
                ".warning {{ fill:{} }}",
                ".error {{ fill:{} }}",
                ".accent {{ fill:{} }}",
            ),
            text, text, success, warning, danger, primary
        )
    }
}

fn tooltip_text(s: &str, padding: f32, shaping: Shaping) -> Element<'_> {
    let text = text(s).wrapping(text::Wrapping::None).shaping(shaping);
    container(text)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.into()),
            border: Border::default().rounded(13),
            snap: false,
            ..Default::default()
        })
        .padding(padding)
        .center(Length::Shrink)
        .into()
}

mod bar;
