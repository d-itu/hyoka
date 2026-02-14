use std::{env, io::Write, path::PathBuf};

use compio::{
    BufResult,
    buf::IoBuf,
    io::{AsyncRead as _, AsyncReadExt, AsyncWrite as _},
    net::UnixStream,
};

use crate::{TinyString, mapping::Mapping};

fn xdg_runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(Into::into)
        .unwrap_or_else(|| format!("/run/user/{}", rustix::process::getuid()).into())
}

pub struct Context {
    /// hyprland instance signature
    his: PathBuf,
}

impl Context {
    pub fn new() -> Option<Self> {
        let mut dir = xdg_runtime_dir();
        dir.push("hypr");
        dir.push(env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?);
        let his = dir;
        Some(Self { his })
    }
    pub async fn controller(&self) -> Controller {
        Controller {
            stream: UnixStream::connect(self.his.join(".socket.sock"))
                .await
                .unwrap(),
        }
    }
    async fn listener(&self) -> Listener {
        Listener {
            stream: UnixStream::connect(self.his.join(".socket2.sock"))
                .await
                .unwrap(),
        }
    }
}

pub struct Listener {
    stream: UnixStream,
}

pub struct Controller {
    stream: UnixStream,
}

#[derive(Debug)]
pub enum Event {
    Workspace {
        id: usize,
    },
    CreateWorkspace {
        id: usize,
    },
    DestroyWorkspace {
        id: usize,
    },
    ActiveWindow {
        class: TinyString,
        title: TinyString,
    },
    Urgent {
        win: TinyString,
    },
}

impl Listener {
    pub async fn listen(mut self, mut dispatch: impl AsyncFnMut(Event)) {
        let mut buffer = Mapping::page().unwrap();

        fn parse_workspace(body: &[u8]) -> Option<usize> {
            let (id, _name) = body.split_once(|&x| x == b',')?;
            usize::from_ascii(id).ok()
        }

        fn parse_line(line: &[u8]) -> Option<Event> {
            let idx = line.iter().position(|&x| x == b'>')?;
            let event_type = unsafe { line.get_unchecked(..idx) };
            let event_body = unsafe { line.get_unchecked(idx + 2..) };
            match event_type {
                b"workspacev2" => Some(Event::Workspace {
                    id: parse_workspace(event_body)?,
                }),
                b"createworkspacev2" => Some(Event::CreateWorkspace {
                    id: parse_workspace(event_body)?,
                }),
                b"destroyworkspacev2" => Some(Event::DestroyWorkspace {
                    id: parse_workspace(event_body)?,
                }),
                b"activewindow" => {
                    let (class, title) = event_body.split_once(|&x| x == b',')?;
                    let [class, title] =
                        [class, title].map(|x| unsafe { str::from_utf8_unchecked(x) }.into());
                    Some(Event::ActiveWindow { class, title })
                }
                b"urgent" => {
                    let win = unsafe { str::from_utf8_unchecked(event_body) }.into();
                    Some(Event::Urgent { win })
                }
                _ => None,
            }
        }

        loop {
            let BufResult(result, buf) = self.stream.read(buffer).await;
            buffer = buf;
            let n = result.unwrap();
            let buf = unsafe { buffer.as_bytes_mut().get_unchecked(..n) };

            for line in buf.split(|&x| x == b'\n') {
                if let Some(e) = parse_line(line) {
                    dispatch(e).await;
                }
            }
        }
    }
}

pub enum Command {
    Workspace(u8),
}

pub enum Query {
    ActiveWindow,
    Clients,
}

#[derive(Debug)]
#[must_use]
pub enum Response {
    Raw(String),
}

impl Controller {
    /// similar to request, but requires no response.
    pub async fn command(mut self, cmd: Command) {
        let mut buf = Vec::with_capacity(64);
        match cmd {
            Command::Workspace(id) => {
                write!(&mut buf, "q/dispatch workspace {id}").unwrap();
            }
        }
        self.stream.write(buf).await.unwrap();
    }

    pub async fn raw_query(mut self, msg: impl IoBuf) -> String {
        self.stream.write(msg).await.unwrap();

        let buf = Vec::with_capacity(1024);
        let BufResult(_, buf) = self.stream.read_to_end(buf).await;
        unsafe { String::from_utf8_unchecked(buf) }
    }

    pub async fn query(self, req: Query) -> Response {
        let msg: &[u8] = match req {
            Query::ActiveWindow => b"activewindow",
            Query::Clients => b"clients",
        };
        let raw = self.raw_query(msg).await;
        Response::Raw(raw)
    }
}

pub struct Daemon {
    listener: Listener,
}

impl Daemon {
    pub async fn run(self, init: Controller, mut dispatch: impl AsyncFnMut(Event)) {
        let Self { listener } = self;

        let res = init
            .raw_query("[[BATCH]]workspaces;activeworkspace;activewindow")
            .await;
        let mut res = res.split("\n\n\n\n\n");
        if let Some(workspaces) = res.next() {
            for workspace in workspaces.split("\n\n") {
                if let Some(id) = parse_workspace_id(workspace) {
                    dispatch(Event::CreateWorkspace { id }).await;
                }
            }
        }
        if let Some(active_workspace) = res.next() {
            if let Some(id) = parse_workspace_id(active_workspace) {
                dispatch(Event::Workspace { id }).await;
            }
        }
        if let Some(active_window) = res.next() {
            let mut required = 2;
            let [mut class, mut title] = [TinyString::new(), TinyString::new()];
            for line in active_window.split("\n") {
                if line.starts_with("\tclass") {
                    if let Some(pos) = line.find(' ') {
                        class = line[pos + 1..].into();
                        required -= 1;
                    }
                } else if line.starts_with("\ttitle") {
                    if let Some(pos) = line.find(' ') {
                        title = line[pos + 1..].into();
                        required -= 1;
                    }
                }
                if required == 0 {
                    break;
                }
            }
            dispatch(Event::ActiveWindow { class, title }).await;
        }

        listener.listen(dispatch).await;
    }
}

pub async fn new() -> Option<(Daemon, Context)> {
    let context = Context::new()?;
    let listener = context.listener().await;

    Some((Daemon { listener }, context))
}

pub fn parse_workspace_id(data: &str) -> Option<usize> {
    let data = data.as_bytes();
    let span = data.get(13..)?;
    let pos = span.iter().position(|&x| x == b' ')?;
    usize::from_ascii(&span[..pos]).ok()
}
