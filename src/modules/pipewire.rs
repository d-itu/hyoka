use std::{cell::RefCell, mem, ops::Add, os::fd::BorrowedFd, rc::Rc};

use compio::net::PollFd;
use pipewire::{
    context::ContextRc,
    main_loop::MainLoopRc,
    metadata::{Metadata, MetadataListener},
    node::{Node, NodeListener},
    permissions::PermissionFlags,
    proxy::ProxyT as _,
    registry::{GlobalObject, Listener, RegistryRc},
    spa::{
        param::ParamType,
        pod::{Value, ValueArray, deserialize::PodDeserializer},
        sys::{SPA_PROP_channelVolumes, SPA_PROP_mute, spa_loop_control_iterate},
        utils::{Id, dict::DictRef},
    },
};
use serde::Deserialize;

#[derive(Default, Clone, Copy)]
struct Info {
    volume: Option<f32>,
    mute: Option<bool>,
}

#[derive(Clone, Default)]
struct Client {
    sinks: Rc<RefCell<Vec<NodeInfo>>>,
    default: Rc<RefCell<Option<(Metadata, MetadataListener)>>>,
    default_sink_name: Rc<RefCell<Option<String>>>,
    default_sink: Rc<RefCell<Option<(Node, NodeListener)>>>,
    info: Rc<RefCell<Info>>,
}

impl Client {
    fn default_sink_change(&self, registry: &RegistryRc) -> Option<()> {
        let id = self
            .sinks
            .borrow()
            .iter()
            .find(|x| x.name == *self.default_sink_name.borrow())?
            .id;

        let sink: Node = registry
            .bind(&GlobalObject::<DictRef> {
                id,
                permissions: PermissionFlags::empty(),
                type_: pipewire::types::ObjectType::Node,
                version: 0,
                props: None,
            })
            .ok()?;

        sink.subscribe_params(&[ParamType::Props]);
        let listener = sink
            .add_listener_local()
            .info(|_info| {})
            .param({
                let client = self.clone();
                move |_seq, _type, _index, _next, value| {
                    if let Some(value) = value {
                        if let Ok(x) = value.as_object() {
                            let mut info = client.info.borrow_mut();
                            for prop in x.props() {
                                #[allow(non_upper_case_globals)]
                                match prop.key() {
                                    Id(SPA_PROP_channelVolumes) => {
                                        if let Ok((
                                            _,
                                            Value::ValueArray(ValueArray::Float(values)),
                                        )) = PodDeserializer::deserialize_from::<Value>(
                                            prop.value().as_bytes(),
                                        ) {
                                            let n = values.len();
                                            let sum = values
                                                .iter()
                                                .map(|x| x.powf(1.0 / 3.0))
                                                .fold(0.0, Add::add);
                                            let volume = sum / n as f32;
                                            info.volume = Some(volume);
                                        }
                                    }
                                    Id(SPA_PROP_mute) => {
                                        if let Ok(mute) = prop.value().get_bool() {
                                            info.mute = Some(mute);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            })
            .register();
        *self.default_sink.borrow_mut() = Some((sink, listener));
        Some(())
    }
}

#[derive(Debug)]
struct NodeInfo {
    id: u32,
    name: Option<String>,
}

pub struct Daemon {
    main_loop: MainLoopRc,
    client: Client,
    _listener: Listener,
}

#[derive(Deserialize)]
struct NodeName<'a> {
    name: &'a str,
}

impl Daemon {
    pub fn new() -> Result<Self, pipewire::Error> {
        pipewire::init();

        let main_loop = MainLoopRc::new(None)?;
        let context = ContextRc::new(&main_loop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;

        let client = Client::default();

        let listener = registry
            .add_listener_local()
            .global({
                let registry = registry.clone();
                let client = client.clone();
                move |global| match (&global.type_, global.props) {
                    (pipewire::types::ObjectType::Node, Some(props))
                        if props.get("media.class") == Some("Audio/Sink") =>
                    {
                        client.sinks.borrow_mut().push(NodeInfo {
                            id: global.id,
                            name: props.get("node.name").map(Into::into),
                        });
                    }
                    (pipewire::types::ObjectType::Metadata, Some(props))
                        if props.get("metadata.name") == Some("default") =>
                    {
                        let metadata: Metadata = registry.bind(global).unwrap();
                        let client = client.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property({
                                let client = client.clone();
                                let registry = registry.clone();
                                move |_subject, key, r#type, value| {
                                    match (key, r#type, value) {
                                        (
                                            Some("default.audio.sink"),
                                            Some("Spa:String:JSON"),
                                            value,
                                        ) => {
                                            *client.default_sink_name.borrow_mut() = value
                                                .and_then(|x| {
                                                    match serde_json::de::from_slice::<NodeName>(
                                                        x.as_bytes(),
                                                    ) {
                                                        Ok(x) => Some(x.name.to_string()),
                                                        Err(_) => None,
                                                    }
                                                });
                                            client.default_sink_change(&registry);
                                        }
                                        _ => {}
                                    }
                                    0
                                }
                            })
                            .register();
                        *client.default.borrow_mut() = Some((metadata, listener));
                    }
                    _ => {}
                }
            })
            .global_remove({
                let client = client.clone();
                move |id| {
                    let mut sinks = client.sinks.borrow_mut();
                    if let Some(idx) = sinks.iter().position(|x| x.id == id) {
                        sinks.swap_remove(idx);
                    }
                    let mut sink = client.default_sink.borrow_mut();
                    if let Some((node, _)) = &*sink {
                        if node.upcast_ref().id() == id {
                            *sink = None;
                        }
                    }
                }
            })
            .register();
        Ok(Self {
            main_loop,
            client,
            _listener: listener,
        })
    }
    pub async fn listen(&self, mut dispatch: impl AsyncFnMut(Event)) {
        let mut cached_info = Info::default();
        let lo = self.main_loop.loop_();
        let fd: BorrowedFd<'static> = unsafe { mem::transmute(lo.fd()) };
        let fd = PollFd::new(fd).unwrap();

        loop {
            fd.read_ready().await.unwrap();
            unsafe {
                lo.enter();
                spa_loop_control_iterate(lo.as_raw().control, 0);
                lo.leave();
            }
            let info = *self.client.info.borrow();
            if let Some(x) = info.volume
                && info.volume != cached_info.volume
            {
                dispatch(Event::Volume(x)).await;
            }
            if let Some(x) = info.mute
                && info.mute != cached_info.mute
            {
                dispatch(Event::Mute(x)).await;
            }
            cached_info = info;
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Volume(f32),
    Mute(bool),
}
