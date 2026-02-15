use std::{
    borrow::Cow,
    cell::RefCell,
    env,
    ffi::OsStr,
    fmt::{self, Debug},
    hash::Hash,
    io,
    mem::MaybeUninit,
    path::Path,
    ptr,
    rc::Rc,
    result,
    time::Duration,
};

use compio::{
    io::{AsyncRead as _, AsyncWrite as _},
    net::UnixStream,
};
use dbus::{
    self, Fields, Flags, MessageIterator, MessageType, OwnedMessage, Serial,
    authentication::Io,
    marshal::Marshal,
    signature::{MultiSignature, SignatureProxy},
    unmarshal::{self, ArrayIter, Unmarshal},
};
use derive_more::From;
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedSender},
};
use thiserror::Error;

pub use cookie::*;

type Raw = OwnedMessage<Box<[u8]>>;
type Return = Result<Raw>;

#[derive(Clone)]
pub struct Connection<D> {
    stream: UnixStream,
    serial: Rc<RefCell<Serial>>,
    cookie: Cookie,
    events: D,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("address not found")]
    AddrNotFound,
    #[error("failed to parse address")]
    FailedParseAddr,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Authentication(#[from] dbus::authentication::Error<io::Error>),
    #[error(transparent)]
    Unmarshal(#[from] dbus::unmarshal::Error),
    #[error("{}", ErrorMessage { name, desc: desc.as_ref().map(|x| &**x) })]
    ErrorMessage {
        name: Box<dbus::String>,
        desc: Option<Box<dbus::String>>,
    },
    #[error("deadline has elapsed")]
    Elapsed,
}

struct ErrorMessage<'a> {
    name: &'a dbus::String,
    desc: Option<&'a dbus::String>,
}

impl fmt::Display for ErrorMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { name, desc } = self;
        write!(f, "{name}")?;
        if let Some(desc) = desc {
            write!(f, ": {desc}")?;
        }
        Ok(())
    }
}

pub type Result<T> = result::Result<T, Error>;

fn address(addr: &[u8]) -> Option<Cow<'_, [u8]>> {
    for addr in addr.get("unix:".len()..)?.split(|&x| x == b',') {
        if let Some((k, v)) = addr.split_once(|&x| x == b'=') {
            match k {
                b"path" => return Some(v.into()),
                b"abstract" => {
                    return Some({
                        let mut res = Vec::with_capacity(v.len() + 1);
                        res.push(0);
                        res.extend_from_slice(v);
                        res.into()
                    });
                }
                _ => continue,
            }
        }
    }
    None
}

async fn connect(addr: &[u8]) -> Result<UnixStream> {
    let addr = address(addr).ok_or(Error::FailedParseAddr)?;
    let path = Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(addr.as_ref()) });
    Ok(UnixStream::connect(path).await?)
}

impl<D> dbus::authentication::Io for Connection<D> {
    type Error = io::Error;

    #[allow(refining_impl_trait)]
    async fn read(&mut self) -> result::Result<impl AsRef<[u8]> + 'static, Self::Error> {
        let buf = Vec::with_capacity(4096);
        let (n, buf) = self.stream.read(buf).await?;
        if n == 0 {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "peer closed"))?
        }
        Ok(buf)
    }
    async fn write(&mut self, data: impl AsRef<[u8]> + 'static) -> result::Result<(), Self::Error> {
        struct Buf<T>(T);

        impl<T: AsRef<[u8]> + 'static> compio::buf::IoBuf for Buf<T> {
            fn as_init(&self) -> &[u8] {
                self.0.as_ref()
            }
        }
        let _ = self.stream.write(Buf(data)).await?;
        Ok(())
    }
}

pub const DBUS: dbus::Proxy = dbus::Proxy {
    name: "org.freedesktop.DBus".into(),
    path: "/org/freedesktop/DBus".into(),
    interface: "org.freedesktop.DBus".into(),
};

pub trait Dispatcher: Clone {
    async fn dispatch(&mut self, e: impl Into<Event>);
}

#[derive(Debug)]
enum Task {
    NewItem(Tray),
    IconName(Tray),
    NewWatcher,
}
impl Task {
    async fn execute<D: Dispatcher>(self, conn: &mut Connection<D>) {
        match self {
            Task::NewItem(service) => {
                conn.new_item(service).await;
            }
            Task::IconName(service) => {
                if let Some(icon_name) = conn.icon_name(service.proxy()).await {
                    conn.events
                        .dispatch(TrayEvent::NewIcon { service, icon_name })
                        .await;
                }
            }
            Task::NewWatcher => conn.new_watcher().await,
        }
    }
}

impl<D: Dispatcher> Connection<D> {
    pub async fn session(dispatch: D) -> Result<Self> {
        let addr = env::var_os("DBUS_SESSION_BUS_ADDRESS").ok_or(Error::AddrNotFound)?;
        let addr = addr.as_encoded_bytes();
        Ok(Self::new(connect(addr).await?, dispatch).await?)
    }
    async fn new(stream: UnixStream, dispatch: D) -> Result<Self> {
        let mut this = Self {
            stream,
            serial: Default::default(),
            cookie: Default::default(),
            events: dispatch,
        };
        this.authenticate().await?;
        this.stream
            .write(this.serial.borrow_mut().method_call(
                Flags::empty().with_no_reply_expected(),
                DBUS,
                "Hello",
                dbus::Empty,
            ))
            .await?;
        Ok(this)
    }
    async fn authenticate(&mut self) -> Result<()> {
        dbus::authentication::authenticate(self, rustix::process::getuid().as_raw()).await?;
        Ok(())
    }

    pub async fn method_call<'a>(
        &mut self,
        proxy: dbus::Proxy<'_>,
        member: impl Into<&'a dbus::String>,
        arguments: impl Marshal + MultiSignature,
    ) -> Result<Notifier> {
        let mut serial = self.serial.borrow_mut();
        self.stream
            .write(serial.method_call(Flags::empty(), proxy, member, arguments))
            .await?;
        Ok(self.cookie.wait(serial.clone(), Duration::from_secs(1)))
    }

    pub async fn get_property<'a>(
        &mut self,
        proxy: dbus::Proxy<'_>,
        prop: impl Into<&'a dbus::String>,
    ) -> Result<Notifier> {
        self.method_call(
            dbus::Proxy {
                interface: "org.freedesktop.DBus.Properties".into(),
                ..proxy
            },
            "Get",
            dbus::multiple_new!(proxy.interface, prop.into()),
        )
        .await
    }

    pub async fn method_call_silent<'a>(
        &mut self,
        proxy: dbus::Proxy<'_>,
        member: impl Into<&'a dbus::String>,
        arguments: impl Marshal + MultiSignature,
    ) -> Result<()> {
        self.stream
            .write(self.serial.borrow_mut().method_call(
                Flags::empty().with_no_reply_expected(),
                proxy,
                member,
                arguments,
            ))
            .await?;
        Ok(())
    }

    async fn read_dispatch(&mut self, tasks: &mut UnboundedSender<Task>) -> Result<()> {
        let buf = self.read().await?;
        for msg in MessageIterator::new(buf.as_ref()) {
            let msg = msg?;
            let fields = msg.header.fields;
            match msg.header.message_type {
                MessageType::MethodCall => {
                    self.stream
                        .write(self.serial.borrow_mut().error(
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            &msg.header,
                            "Unknown method",
                        ))
                        .await?;
                }
                MessageType::MethodReturn => self.cookie.notify(
                    Serial::from_raw(
                        fields
                            .reply_serial
                            .ok_or(Error::Unmarshal(unmarshal::Error::InvalidHeader))?,
                    ),
                    Ok(msg.to_owned()),
                ),
                MessageType::Error => self.cookie.notify(
                    Serial::from_raw(
                        fields
                            .reply_serial
                            .ok_or(Error::Unmarshal(unmarshal::Error::InvalidHeader))?,
                    ),
                    Err(Error::ErrorMessage {
                        name: fields
                            .error_name
                            .ok_or(unmarshal::Error::InvalidHeader)?
                            .to_owned(),
                        desc: msg.parse::<&dbus::String>().map(ToOwned::to_owned).ok(),
                    }),
                ),
                MessageType::Signal => {
                    let Fields {
                        path,
                        interface,
                        member,
                        sender,
                        ..
                    } = fields;

                    match interface.map(dbus::String::as_bytes) {
                        Some(b"org.kde.StatusNotifierItem") => {
                            match member.unwrap().as_bytes() {
                                b"NewIcon" => tasks
                                    .unbounded_send(Task::IconName(Tray::new(
                                        sender.unwrap(),
                                        path.unwrap(),
                                    )))
                                    .unwrap(),
                                _ => {}
                            };
                        }
                        Some(b"org.kde.StatusNotifierWatcher") => {
                            match member.unwrap().as_bytes() {
                                b"StatusNotifierItemRegistered" => {
                                    let service = msg.parse::<&dbus::String>().unwrap();
                                    if let Some(service) = Tray::try_from_string(service) {
                                        tasks.unbounded_send(Task::NewItem(service)).unwrap()
                                    }
                                }
                                b"StatusNotifierItemUnregistered" => {
                                    let service = msg.parse::<&dbus::String>().unwrap();
                                    if let Some(service) = Tray::try_from_string(service) {
                                        self.events.dispatch(TrayEvent::Unregistered(service)).await
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some(b"org.freedesktop.DBus") => match member.unwrap().as_bytes() {
                            b"NameOwnerChanged" => {
                                let dbus::multiple_match!(name, old, new): dbus::multiple_type!(
                                    &dbus::String,
                                    &dbus::String,
                                    &dbus::String,
                                ) = msg.parse().unwrap();
                                if name.as_bytes() == b"org.kde.StatusNotifierWatcher" {
                                    if old.is_empty() && !new.is_empty() {
                                        tasks.unbounded_send(Task::NewWatcher).unwrap();
                                    } else if new.is_empty() && !old.is_empty() {
                                        self.events.dispatch(TrayEvent::Disconnected).await;
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn sync(&mut self, timeout: Duration, tasks: &mut UnboundedSender<Task>) -> Result<()> {
        while !self.cookie.is_empty() {
            match compio::time::timeout(timeout, self.read_dispatch(tasks)).await {
                Ok(x) => x?,
                Err(_) => self.cookie.cancel_all(),
            }
        }
        Ok(())
    }

    async fn serve(&mut self, tasks: &mut UnboundedSender<Task>) {
        loop {
            self.read_dispatch(tasks).await.unwrap();
        }
    }

    async fn icon_name(&mut self, proxy: dbus::Proxy<'_>) -> Option<String> {
        let icon_name = self.get_property(proxy, "IconName").await.unwrap();

        let icon_name = icon_name.await.unwrap();
        let icon_name = unsafe {
            String::from_utf8_unchecked(
                icon_name
                    .as_ref()
                    .parse::<dbus::Variant<&dbus::String>>()
                    .unwrap()
                    .0
                    .to_vec(),
            )
        };
        Some(icon_name)
    }
    async fn tooltip(&mut self, proxy: dbus::Proxy<'_>) -> Option<String> {
        let tooltip = self.get_property(proxy, "ToolTip").await.unwrap();

        let tooltip = tooltip.await.inspect_err(|e| tracing::warn!("{e}")).ok()?;
        let tooltip = unsafe {
            String::from_utf8_unchecked(
                tooltip
                    .as_ref()
                    .parse::<dbus::Variant<Tooltip>>()
                    .unwrap()
                    .0
                    .content
                    .to_vec(),
            )
        };
        Some(tooltip)
    }

    async fn new_item(&mut self, service: Tray) -> Option<()> {
        let (name, path) = service.item();
        self.method_call_silent(
            DBUS,
            "AddMatch",
            format!(
                concat!(
                    "type='signal',",
                    "sender='{}',",
                    "interface='org.kde.StatusNotifierItem',",
                    "path='{}'"
                ),
                name, path
            )
            .as_str(),
        )
        .await
        .ok();
        let icon_name = self.icon_name(service.proxy()).await?;
        self.events
            .dispatch(TrayEvent::Registered { icon_name, service })
            .await;
        Some(())
    }

    async fn new_watcher(&mut self) {
        let registered = self
            .method_call(
                dbus::Proxy {
                    name: "org.kde.StatusNotifierWatcher".into(),
                    path: "/StatusNotifierWatcher".into(),
                    interface: "org.freedesktop.DBus.Properties".into(),
                },
                "Get",
                dbus::multiple_new!(
                    "org.kde.StatusNotifierWatcher",
                    "RegisteredStatusNotifierItems"
                ),
            )
            .await
            .unwrap();
        if let Ok(msg) = registered.await {
            let arr: dbus::Variant<ArrayIter<&dbus::String>> = msg.as_ref().parse().unwrap();
            for item in arr.0 {
                if let Some(service) = Tray::try_from_string(item.unwrap()) {
                    self.new_item(service).await;
                }
            }
        }
    }
}

#[derive(Debug, From)]
pub enum Event {
    Tray(TrayEvent),
}

#[derive(Debug)]
pub enum TrayEvent {
    Registered { service: Tray, icon_name: String },
    NewIcon { service: Tray, icon_name: String },
    Unregistered(Tray),
    Disconnected,
}

pub async fn new<D: Dispatcher>(dispatch: D) -> Option<(Daemon<D>, Proxy<D>)> {
    let connection = Connection::session(dispatch).await.ok()?;
    Some((
        Daemon {
            connection: connection.clone(),
        },
        Proxy { connection },
    ))
}

pub struct Daemon<D> {
    connection: Connection<D>,
}

impl<D: Dispatcher> Daemon<D> {
    async fn init(&mut self, tasks: &mut UnboundedSender<Task>) {
        self.connection
            .method_call_silent(
                DBUS,
                "AddMatch",
                concat!(
                    "type='signal',",
                    "sender='org.kde.StatusNotifierWatcher',",
                    "interface='org.kde.StatusNotifierWatcher',",
                    "member='StatusNotifierItemRegistered'",
                ),
            )
            .await
            .ok();
        self.connection
            .method_call_silent(
                DBUS,
                "AddMatch",
                concat!(
                    "type='signal',",
                    "sender='org.kde.StatusNotifierWatcher',",
                    "interface='org.kde.StatusNotifierWatcher',",
                    "member='StatusNotifierItemUnregistered'",
                ),
            )
            .await
            .ok();
        self.connection
            .method_call_silent(
                DBUS,
                "AddMatch",
                concat!(
                    "type='signal',",
                    "sender='org.freedesktop.DBus',",
                    "interface='org.freedesktop.DBus',",
                    "member='NameOwnerChanged',",
                    "arg0='org.kde.StatusNotifierWatcher'",
                ),
            )
            .await
            .unwrap();
        tasks.unbounded_send(Task::NewWatcher).unwrap();
    }
    pub async fn serve(mut self) {
        let (mut sender, mut receiver) = mpsc::unbounded();
        self.init(&mut sender).await;
        let mut connection = self.connection.clone();
        let daemon = connection.serve(&mut sender);
        let mut connection = self.connection.clone();
        let tasks = async {
            loop {
                receiver
                    .next()
                    .await
                    .unwrap()
                    .execute(&mut connection)
                    .await;
            }
        };
        std::future::join!(daemon, tasks).await;
    }
}

pub struct Proxy<D> {
    connection: Connection<D>,
}

impl<D: Dispatcher> Proxy<D> {
    pub async fn tray_tooltip(&mut self, service: Tray) -> Option<String> {
        self.connection.tooltip(service.proxy()).await
    }
    pub async fn tray_action(&mut self, service: Tray) {
        self.connection
            .method_call_silent(service.proxy(), "Activate", dbus::multiple_new!(0i32, 0i32))
            .await
            .unwrap();
    }
}

#[allow(dead_code)]
fn show_bytes(xs: &[u8]) -> impl Debug {
    fmt::from_fn(move |f| {
        Ok(for &x in xs {
            if x.is_ascii_graphic() {
                write!(f, "{}", x as char)?;
            } else {
                write!(f, "\\{x}")?;
            }
        })
    })
}

#[derive(Clone, Eq)]
pub struct Tray {
    data: Box<dbus::String>,
    split: usize,
}
impl Hash for Tray {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.data.hash(state);
    }
}
impl PartialEq for Tray {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}
impl Tray {
    fn try_from_string(service: &dbus::String) -> Option<Self> {
        let idx = service.iter().copied().position(|x| x == b'/')?;
        let service = Self {
            data: service.to_owned(),
            split: idx,
        };
        Some(service)
    }
    fn new(name: &dbus::String, path: &dbus::ObjectPath) -> Self {
        let mut inner: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(name.len() + path.len());
        unsafe { ptr::copy_nonoverlapping(name.as_ptr(), inner.as_mut_ptr().cast(), name.len()) };
        unsafe {
            ptr::copy_nonoverlapping(
                path.as_ptr(),
                inner.as_mut_ptr().add(name.len()).cast(),
                path.len(),
            )
        };
        Self {
            data: unsafe { inner.assume_init().into() },
            split: name.len(),
        }
    }
    fn item(&self) -> (&dbus::String, &dbus::ObjectPath) {
        let (name, path) = unsafe { self.data.as_bytes().split_at_unchecked(self.split) };
        (name.into(), path.into())
    }
    fn name(&self) -> &dbus::String {
        self.item().0
    }
    fn path(&self) -> &dbus::ObjectPath {
        self.item().1
    }
    fn proxy(&self) -> dbus::Proxy<'_> {
        dbus::Proxy {
            name: self.name(),
            path: self.path(),
            interface: "org.kde.StatusNotifierItem".into(),
        }
    }
}
impl Debug for Tray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service")
            .field("name", &self.name())
            .field("path", &self.path())
            .finish()
    }
}

struct Tooltip<'a> {
    content: &'a dbus::String,
}
impl<'a> SignatureProxy for Tooltip<'a> {
    type Proxy = dbus::struct_type!(
        &'a dbus::String,
        ArrayIter<'a, dbus::struct_type!(i32, i32, ArrayIter<'a, u8>)>,
        &'a dbus::String,
        &'a dbus::String,
    );
}
impl<'a> Unmarshal<'a> for Tooltip<'a> {
    fn unmarshal(r: &mut unmarshal::Reader<'a>) -> unmarshal::Result<Self> {
        let dbus::struct_match!(_, _, content, _): <Self as SignatureProxy>::Proxy = r.read()?;
        Ok(Self { content })
    }
}

mod cookie;
