use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    fmt, mem,
    ops::{Deref, Index},
    os::fd::AsRawFd as _,
    ptr::{self, NonNull},
    rc::Rc,
    time::Instant,
};

use futures::channel::mpsc::UnboundedSender;
use iced::{
    Rectangle, Size,
    mouse::{self, Cursor, Interaction},
    window::RedrawRequest,
};
use iced_core::{
    renderer::Style,
    widget::{Operation, operation::Focusable},
};
use iced_runtime::user_interface::Cache;
use iced_tiny_skia::{Renderer, graphics};
use rustc_hash::FxHashMap;
use rustix::{
    fs::MemfdFlags,
    mm::{MapFlags, ProtFlags},
};
use tiny_skia::{Mask, PixmapMut};

use crate::{
    consumer::{Callbacks, Element, Runner, UserInterface, program::Message},
    wayland::{self, Callback, Event},
};

#[derive(Clone)]
pub enum Role {
    Layer {
        layer_surface: NonNull<wayland::ffi::zwlr_layer_surface_v1>,
    },
    Popup {
        xdg_surface: NonNull<wayland::ffi::xdg_surface>,
        popup: NonNull<wayland::ffi::xdg_popup>,
        positioner: NonNull<wayland::ffi::xdg_positioner>,
        size: Cell<Size>,
    },
}

impl Role {
    fn destroy(&self) {
        match self {
            Role::Layer { layer_surface } => unsafe {
                wayland::ffi::zwlr_layer_surface_v1_destroy(layer_surface.as_ptr())
            },
            Role::Popup {
                xdg_surface,
                popup,
                positioner,
                ..
            } => unsafe {
                wayland::ffi::xdg_popup_destroy(popup.as_ptr());
                wayland::ffi::xdg_surface_destroy(xdg_surface.as_ptr());
                wayland::ffi::xdg_positioner_destroy(positioner.as_ptr());
            },
        }
    }
    fn key(&self, mut cb: impl FnMut(NonNull<c_void>)) {
        match self {
            &Role::Layer { layer_surface } => cb(layer_surface.cast()),
            &Role::Popup { popup, .. } => cb(popup.cast()),
        }
    }
}

pub type WlSurface = NonNull<wayland::ffi::wl_surface>;

pub enum ConfigState<'ui> {
    Configured {
        buffer: Buffer,
        clip_mask: tiny_skia::Mask,
        ui: Option<UserInterface<'ui>>,
        last_layers: Option<Vec<iced_tiny_skia::Layer>>,
    },
    Unconfigured {
        scale_factor: u32,
    },
}

impl Default for ConfigState<'_> {
    fn default() -> Self {
        Self::Unconfigured { scale_factor: 1 }
    }
}

impl ConfigState<'_> {
    pub fn outdate(&mut self) {
        match self {
            ConfigState::Configured { ui, .. } => {
                *ui = None;
            }
            ConfigState::Unconfigured { .. } => {}
        }
    }
}

#[derive(Clone)]
pub struct Surface {
    pub role: Role,
    pub surface: WlSurface,
}

struct OwnedSurface(Surface);

impl Drop for OwnedSurface {
    fn drop(&mut self) {
        self.0.role.destroy();
        unsafe { wayland::ffi::wl_surface_destroy(self.0.surface.as_ptr()) };
    }
}

pub struct State {
    pub cursor: Cursor,
    serial: Option<u32>,
    shape: Option<Interaction>,
    pub config_state: ConfigState<'static>,
    pub renderer: Renderer,
}

fn rebuild_ui<'ui>(
    ptr: *mut UserInterface<'ui>,
    root: Element<'ui>,
    bounds: Size,
    renderer: &mut Renderer,
) {
    let ui = unsafe { ptr.read() };
    let ui = UserInterface::build(root, bounds, ui.into_cache(), renderer);
    let ui = unsafe { ptr.replace(ui) };
    mem::forget(ui);
}

impl State {
    pub fn resize(
        &mut self,
        size @ [width, height]: [u32; 2],
        surface: WlSurface,
        tag: Tag,
        runner: &Runner,
    ) {
        match mem::take(&mut self.config_state) {
            ConfigState::Configured { buffer, ui, .. } => {
                let viewport = buffer.viewport.with_surface_size(size);
                let ui = ui
                    .map(|ui| ui.relayout(Size::new(width as _, height as _), &mut self.renderer))
                    .unwrap_or_else(|| {
                        iced_runtime::UserInterface::build::<Element>(
                            unsafe { mem::transmute(runner.view(tag)) },
                            Size::new(width as _, height as _),
                            Cache::new(),
                            &mut self.renderer,
                        )
                    });
                self.config_state = ConfigState::Configured {
                    buffer: runner.wayland.globals.create_buffer(viewport),
                    ui: Some(ui),
                    clip_mask: viewport.mask(),
                    last_layers: None,
                };
            }
            ConfigState::Unconfigured { scale_factor } => {
                let viewport = Viewport {
                    surface_size: size,
                    buffer_scale: scale_factor,
                    _buffer_transform: wayland::ffi::WL_OUTPUT_TRANSFORM_NORMAL,
                };

                let buffer = runner.wayland.globals.create_buffer(viewport);
                unsafe {
                    wayland::ffi::wl_surface_attach(surface.as_ptr(), buffer.buffer.as_ptr(), 0, 0)
                };
                self.config_state = ConfigState::Configured {
                    buffer,
                    ui: Some(iced_runtime::UserInterface::build::<Element<'static>>(
                        unsafe { mem::transmute(runner.view(tag)) },
                        Size::new(width as _, height as _),
                        Cache::new(),
                        &mut self.renderer,
                    )),
                    clip_mask: viewport.mask(),
                    last_layers: None,
                };
            }
        }
    }
    fn redraw(&mut self, surface: WlSurface, tag: Tag, runner: &Runner) {
        if let ConfigState::Configured {
            buffer,
            clip_mask,
            last_layers,
            ui,
        } = &mut self.config_state
        {
            let [width, height] = buffer.viewport.surface_size;
            let ui = ui.get_or_insert_with(|| {
                iced_runtime::UserInterface::build::<Element<'static>>(
                    unsafe { mem::transmute(runner.view(tag)) },
                    Size::new(width as _, height as _),
                    Cache::new(),
                    &mut self.renderer,
                )
            });
            rebuild_ui(
                ui,
                unsafe { mem::transmute(runner.view(tag)) },
                Size::new(width as _, height as _),
                &mut self.renderer,
            );
            ui.update(
                &[iced::Event::Window(iced::window::Event::RedrawRequested(
                    Instant::now(),
                ))],
                self.cursor,
                &mut self.renderer,
                &mut vec![],
            );
            ui.draw(
                &mut self.renderer,
                &runner.theme,
                &Style {
                    text_color: runner.theme.palette().text,
                },
                self.cursor,
            );

            let layers = self.renderer.layers();
            let mut damage = if let Some(last_layers) = last_layers {
                graphics::damage::diff(
                    &last_layers,
                    layers,
                    |layer| vec![layer.bounds],
                    iced_tiny_skia::Layer::damage,
                )
            } else {
                let [width, height] = buffer.viewport.surface_size.map(|x| x as _);
                let bounds = Rectangle::with_size(Size { width, height });
                vec![bounds]
            };
            *last_layers = Some(layers.to_vec());

            for rect in &mut damage {
                rect.width = rect.width.ceil();
                rect.height = rect.height.ceil();
            }

            self.renderer.draw(
                &mut buffer.pixels(),
                clip_mask,
                &buffer.viewport.to_iced_viewport(),
                &damage,
                runner.background(tag),
            );

            let surface = surface.as_ptr();
            unsafe {
                wayland::ffi::wl_surface_attach(surface, buffer.buffer.as_ptr(), 0, 0);
                // let [width, height] = buffer.viewport.buffer_size().map(|x| x as _);
                // wayland::ffi::wl_surface_damage_buffer(surface, 0, 0, width, height);
                for rect in damage {
                    wayland::ffi::wl_surface_damage(
                        surface,
                        rect.x as _,
                        rect.y as _,
                        rect.width as _,
                        rect.height as _,
                    );
                }
                wayland::ffi::wl_surface_commit(surface);
                wayland::ffi::wl_display_flush(runner.display.as_ptr());
            }
        }
    }
}

#[derive(Clone)]
pub struct Window(Rc<Inner>);

#[derive(Debug, Clone, Copy)]
pub enum Tag {
    Bar,
    Tooltip,
}

pub struct Inner {
    surface: OwnedSurface,
    pub tag: Tag,
    pub state: RefCell<State>,
}

impl Inner {
    pub fn surface(&self) -> &Surface {
        &self.surface.0
    }
}

impl Deref for Window {
    type Target = Rc<Inner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.surface.0.surface)
    }
}

#[derive(Default)]
pub struct WindowManager {
    lut: FxHashMap<NonNull<c_void>, Window>,
    pub focused: Option<WlSurface>,
}

impl WindowManager {
    pub fn create_window(
        &mut self,
        surface: WlSurface,
        role: Role,
        tag: Tag,
        renderer: Renderer,
    ) -> &mut Window {
        let window = Window::new(Surface { surface, role }, tag, renderer);
        window.surface.0.role.key(|k| {
            self.lut.try_insert(k, window.clone()).unwrap();
        });
        self.lut.try_insert(surface.cast(), window).unwrap()
    }
    pub fn close_window(&mut self, surface: &Surface) {
        if Some(surface.surface) == self.focused {
            self.focused.take();
        }
        surface.role.key(|k| {
            self.lut.remove(&k).unwrap();
        });
        self.lut.remove(&surface.surface.cast());
    }
    pub fn find_by_object<T>(&self, obj: NonNull<T>) -> Option<&Window> {
        self.lut.get(&obj.cast())
    }
    pub fn focused(&self) -> Option<&Window> {
        self.lut.get(&self.focused?.cast())
    }
    pub fn iter(&self) -> impl Iterator<Item = &Window> {
        self.lut.values()
    }
}

impl<T> Index<NonNull<T>> for WindowManager {
    type Output = Window;
    fn index(&self, obj: NonNull<T>) -> &Self::Output {
        self.lut.get(&obj.cast()).unwrap()
    }
}

impl Window {
    fn new(surface: Surface, tag: Tag, renderer: Renderer) -> Self {
        Self(Rc::new(Inner {
            surface: OwnedSurface(surface),
            tag,
            state: RefCell::new(State {
                cursor: Cursor::Unavailable,
                serial: None,
                shape: None,
                config_state: ConfigState::default(),
                renderer,
            }),
        }))
    }
    pub fn resize(&self, size: [u32; 2], runner: &mut Runner) {
        let mut window = self.state.borrow_mut();
        window.resize(size, self.surface.0.surface, self.tag, runner);
        self.request_redraw(&mut runner.wayland.notifier, &mut runner.callbacks);
        unsafe { wayland::ffi::wl_display_flush(runner.display.as_ptr()) };
    }

    pub fn rescale(&self, scale: u32, runner: &mut Runner) {
        let mut window = self.state.borrow_mut();
        let surface = self.surface.0.surface;
        match &mut window.config_state {
            ConfigState::Configured {
                buffer,
                clip_mask,
                last_layers,
                ..
            } => {
                // for hyprland sends scale event when scale does not change
                if buffer.viewport.buffer_scale == scale {
                    return;
                }
                *buffer = runner
                    .wayland
                    .globals
                    .create_buffer(buffer.viewport.with_buffer_scale(scale));

                let [width, height] = buffer.viewport.buffer_size();
                *clip_mask = Mask::new(width, height).unwrap();
                *last_layers = None;

                self.request_redraw(&mut runner.wayland.notifier, &mut runner.callbacks);
            }
            ConfigState::Unconfigured { scale_factor } => {
                *scale_factor = scale;
            }
        }
        unsafe { wayland::ffi::wl_surface_set_buffer_scale(surface.as_ptr(), scale as _) }
        unsafe { wayland::ffi::wl_display_flush(runner.display.as_ptr()) };
    }

    pub fn request_redraw(&self, notifier: &mut UnboundedSender<Event>, callbacks: &mut Callbacks) {
        let surface = self.surface.0.surface.as_ptr();
        let callback = unsafe { wayland::ffi::wl_surface_frame(surface) };
        unsafe {
            wayland::ffi::wl_callback_add_listener(
                callback,
                &wayland::CALLBACK_LISTENER,
                notifier as *mut _ as _,
            )
        };
        unsafe {
            wayland::ffi::wl_surface_commit(surface);
        };

        let window = self.clone();
        callbacks
            .try_insert(
                Callback::from_raw(callback),
                Box::new(move |runner| {
                    window
                        .state
                        .borrow_mut()
                        .redraw(window.surface.0.surface, window.tag, runner)
                }),
            )
            .map_err(|e| e.entry.remove_entry().0)
            .unwrap();
    }

    pub fn enter(&self, serial: u32) {
        self.state.borrow_mut().serial = Some(serial);
    }

    pub async fn mouse(&self, event: mouse::Event, runner: &mut Runner) {
        let messages = {
            let State {
                ref mut cursor,
                ref mut serial,
                ref mut shape,
                ref mut config_state,
                ref mut renderer,
            } = *self.state.borrow_mut();

            let mut messages = vec![];

            if let ConfigState::Configured { ui, buffer, .. } = config_state {
                let [width, height] = buffer.viewport.buffer_size();
                match event {
                    mouse::Event::CursorMoved { position } => {
                        *cursor = Cursor::Available(position);
                    }
                    mouse::Event::CursorLeft => {
                        messages.push(Message::CloseTooltip);
                        // ui.operate(&mut runner.renderer, &mut Leave);
                        *cursor = Cursor::Unavailable;
                        *serial = None;
                        *shape = None;
                    }
                    _ => {}
                }
                let state;
                let ui = ui.get_or_insert_with(|| {
                    iced_runtime::UserInterface::build::<Element>(
                        unsafe { mem::transmute(runner.view(self.tag)) },
                        Size::new(width as _, height as _),
                        Cache::new(),
                        renderer,
                    )
                });
                (state, _) = ui.update(
                    &[iced::Event::Mouse(event)],
                    *cursor,
                    renderer,
                    &mut messages,
                );
                if let iced_runtime::user_interface::State::Updated {
                    mouse_interaction,
                    redraw_request,
                    ..
                } = state
                {
                    if let RedrawRequest::NextFrame = redraw_request {
                        self.request_redraw(&mut runner.wayland.notifier, &mut runner.callbacks);
                    }
                    if let Some(serial) = *serial
                        && Some(mouse_interaction) != *shape
                    {
                        match cursor_shape(mouse_interaction) {
                            CursorShape::Shape(shape) => unsafe {
                                wayland::ffi::wp_cursor_shape_device_v1_set_shape(
                                    runner.cursor_shape_device.as_ptr(),
                                    serial,
                                    shape,
                                )
                            },
                            CursorShape::Hide => unsafe {
                                wayland::ffi::wl_pointer_set_cursor(
                                    runner.pointer.as_ptr(),
                                    serial,
                                    ptr::null_mut(),
                                    0,
                                    0,
                                );
                            },
                        }
                    }
                }
            }
            messages
        };

        for message in messages {
            runner.update(message).await;
        }
        unsafe { wayland::ffi::wl_display_flush(runner.display.as_ptr()) };
    }
}

#[allow(dead_code)]
struct Leave;

impl Operation for Leave {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<()>)) {
        operate(self);
    }
    fn focusable(
        &mut self,
        _id: Option<&iced::widget::Id>,
        _bounds: Rectangle,
        state: &mut dyn Focusable,
    ) {
        state.unfocus();
    }
}

pub struct Buffer {
    pub buffer: NonNull<wayland::ffi::wl_buffer>,
    pub viewport: Viewport,
    pub ptr: NonNull<u8>,
}

impl Buffer {
    fn pixels(&self) -> tiny_skia::PixmapMut<'_> {
        PixmapMut::from_bytes(
            self.data(),
            self.viewport.buffer_width(),
            self.viewport.buffer_height(),
        )
        .unwrap()
    }
    fn data(&self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.viewport.buffer_byte_size())
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            wayland::ffi::wl_buffer_destroy(self.buffer.as_ptr());
            rustix::mm::munmap(self.ptr.as_ptr() as _, self.viewport.buffer_byte_size()).unwrap();
        }
    }
}

impl wayland::Globals {
    fn create_buffer(&self, viewport: Viewport) -> Buffer {
        let len = viewport.buffer_byte_size();
        let fd = rustix::fs::memfd_create(c"", MemfdFlags::empty()).unwrap();
        rustix::fs::ftruncate(&fd, len as _).unwrap();

        let ptr = unsafe {
            rustix::mm::mmap(
                ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .unwrap() as _;

        let pool =
            unsafe { wayland::ffi::wl_shm_create_pool(self.shm(), fd.as_raw_fd(), len as _) };
        let buffer = unsafe {
            wayland::ffi::wl_shm_pool_create_buffer(
                pool,
                0,
                viewport.buffer_width() as _,
                viewport.buffer_height() as _,
                viewport.buffer_width() as i32 * 4,
                wayland::ffi::WL_SHM_FORMAT_ARGB8888,
            )
        };
        unsafe { wayland::ffi::wl_shm_pool_destroy(pool) };
        Buffer {
            buffer: NonNull::new(buffer).unwrap(),
            viewport,
            ptr: NonNull::new(ptr).unwrap(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    surface_size: [u32; 2],
    buffer_scale: u32,
    _buffer_transform: wayland::ffi::wl_output_transform,
}

impl Viewport {
    fn with_surface_size(self, surface_size: [u32; 2]) -> Self {
        Self {
            surface_size,
            ..self
        }
    }
    fn with_buffer_scale(self, buffer_scale: u32) -> Self {
        Self {
            buffer_scale,
            ..self
        }
    }
    fn buffer_width(&self) -> u32 {
        self.surface_size[0] * self.buffer_scale
    }
    fn buffer_height(&self) -> u32 {
        self.surface_size[1] * self.buffer_scale
    }
    fn buffer_size(&self) -> [u32; 2] {
        self.surface_size.map(|x| x * self.buffer_scale)
    }
    fn buffer_byte_size(&self) -> usize {
        let [width, height] = self.buffer_size().map(|x| x as usize);
        width * height * 4
    }
    fn to_iced_viewport(&self) -> graphics::Viewport {
        let [width, height] = self.buffer_size();
        graphics::Viewport::with_physical_size(Size { width, height }, self.buffer_scale as _)
    }
    fn mask(&self) -> Mask {
        Mask::new(self.buffer_width(), self.buffer_height()).unwrap()
    }
}

enum CursorShape {
    Shape(wayland::ffi::wp_cursor_shape_device_v1_shape),
    Hide,
}

fn cursor_shape(interaction: Interaction) -> CursorShape {
    match interaction {
        Interaction::None => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DEFAULT)
        }
        Interaction::Hidden => CursorShape::Hide,
        Interaction::Idle => todo!(), // what is idle ?
        Interaction::ContextMenu => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CONTEXT_MENU)
        }
        Interaction::Help => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_HELP),
        Interaction::Pointer => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_POINTER)
        }
        Interaction::Progress => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_PROGRESS)
        }
        Interaction::Wait => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_WAIT),
        Interaction::Cell => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CELL),
        Interaction::Crosshair => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CROSSHAIR)
        }
        Interaction::Text => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_TEXT),
        Interaction::Alias => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALIAS)
        }
        Interaction::Copy => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_COPY),
        Interaction::Move => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_MOVE),
        Interaction::NoDrop => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NO_DROP)
        }
        Interaction::NotAllowed => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NOT_ALLOWED)
        }
        Interaction::Grab => CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_GRAB),
        Interaction::Grabbing => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_GRABBING)
        }
        Interaction::ResizingHorizontally => todo!(),
        Interaction::ResizingVertically => todo!(),
        Interaction::ResizingDiagonallyUp => todo!(),
        Interaction::ResizingDiagonallyDown => todo!(),
        Interaction::ResizingColumn => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_COL_RESIZE)
        }
        Interaction::ResizingRow => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ROW_RESIZE)
        }
        Interaction::AllScroll => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALL_SCROLL)
        }
        Interaction::ZoomIn => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ZOOM_IN)
        }
        Interaction::ZoomOut => {
            CursorShape::Shape(wayland::ffi::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ZOOM_OUT)
        }
    }
}
