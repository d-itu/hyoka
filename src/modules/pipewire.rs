use std::{mem, os::fd::BorrowedFd};

use compio::net::PollFd;
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedSender},
};
use pipewire::{
    context::ContextBox,
    device::{Device, DeviceListener},
    main_loop::MainLoopBox,
    metadata::{Metadata, MetadataListener},
    node::{self, NodeListener},
    permissions::PermissionFlags,
    proxy::ProxyT,
    registry::{GlobalObject, Registry},
    spa::{
        param::ParamType,
        pod::deserialize::PodDeserializer,
        sys::{
            SPA_PARAM_ROUTE_description, SPA_PROP_channelVolumes, SPA_PROP_mute,
            spa_loop_control_iterate,
        },
        utils::{Id, dict::DictRef},
    },
    types::ObjectType,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::TinyString;

enum Message {
    Done,
    GlobalRemove(u32),
    MetadataDefaullt(u32),
    Node {
        id: u32,
        device_id: u32,
    },
    NodeDriver {
        input_id: u32,
        device_id: u32,
        driver_id: u32,
    },
    NodeInfo {
        id: u32,
        name: Option<String>,
        display: Option<TinyString>,
    },
    DeviceRoute {
        node_id: u32,
        desc: TinyString,
    },
    DefaultSink(String),
    Props {
        id: u32,
        volume: Option<f32>,
        mute: Option<bool>,
    },
}

fn bind<T: ProxyT>(registry: &Registry, ty: ObjectType, id: u32) -> Result<T, pipewire::Error> {
    registry.bind(&GlobalObject {
        id,
        permissions: PermissionFlags::empty(),
        type_: ty,
        version: 0,
        props: None as Option<DictRef>,
    })
}

fn dispatch_global(global: &GlobalObject<&DictRef>) -> Option<Message> {
    Some(match global.type_ {
        ObjectType::Metadata if global.props?.get("metadata.name")? == "default" => {
            Message::MetadataDefaullt(global.id)
        }
        ObjectType::Node => {
            let props = global.props?;
            let device_id: u32 = props.get("device.id")?.parse().ok()?;
            let media_class = props.get("media.class")?;
            if media_class != "Audio/Sink" {
                None?
            }
            Message::Node {
                id: global.id,
                device_id,
            }
        }
        _ => {
            return None;
        }
    })
}

#[derive(Debug)]
pub enum Event {
    DefaultChanged {
        display: TinyString,
        route: TinyString,
    },
    Volume(f32),
    Mute(bool),
    // Props {
    //     volume: Option<f32>,
    //     mute: Option<bool>,
    // },
}

pub async fn run(mut dispatch: impl AsyncFnMut(Event)) -> Option<()> {
    pipewire::init();

    let main_loop = MainLoopBox::new(None).ok()?;
    let context = ContextBox::new(main_loop.loop_(), None).ok()?;
    let core = context.connect(None).ok()?;
    let registry = core.get_registry().ok()?;
    core.sync(0).unwrap();

    let (sender, mut receiver) = mpsc::unbounded();

    let _listener = registry
        .add_listener_local()
        .global({
            let sender = sender.clone();
            move |global| {
                if let Some(msg) = dispatch_global(global) {
                    sender.unbounded_send(msg).unwrap();
                }
            }
        })
        .global_remove({
            let sender = sender.clone();
            move |id| {
                sender.unbounded_send(Message::GlobalRemove(id)).unwrap();
            }
        })
        .register();

    let _core_listener = core
        .add_listener_local()
        .done({
            let sender = sender.clone();
            move |_, _| {
                sender.unbounded_send(Message::Done).unwrap();
            }
        })
        .register();

    let lo = main_loop.loop_();
    let fd: BorrowedFd<'static> = unsafe { mem::transmute(lo.fd()) };
    let fd = PollFd::new(fd).unwrap();

    let daemon = async {
        loop {
            fd.read_ready().await.unwrap();
            unsafe {
                lo.enter();
                spa_loop_control_iterate(lo.as_raw().control, 0);
                lo.leave();
            }
        }
    };
    let consumer = async {
        let mut devices = FxHashMap::default();
        let mut nodes = FxHashMap::default();
        let mut _metadata;
        let mut _metadata_listener;
        let mut metadata_default = None;
        let mut default_sink = None as Option<DefaultNode>;
        let mut done = false;

        struct DefaultNode {
            id: u32,
            name: String,
        }

        let mut waiting = FxHashSet::default();

        loop {
            match receiver.next().await.unwrap() {
                Message::Done => {
                    done = true;
                }
                Message::GlobalRemove(id) => {
                    devices.remove(&id);
                    nodes.remove(&id);
                }
                Message::MetadataDefaullt(id) => {
                    metadata_default = Some(id);
                }
                Message::Node { id, device_id } => {
                    waiting.insert(id);
                    devices.entry(device_id).or_insert_with({
                        let registry = registry.as_ref();
                        let sender = sender.clone();
                        move || {
                            let object =
                                bind::<Device>(registry, ObjectType::Device, device_id).unwrap();
                            object.subscribe_params(&[ParamType::Route]);
                            let listener = object
                                .add_listener_local()
                                .param({
                                    let sender = sender.clone();
                                    move |_seq, _type, _index, _next, value| {
                                        if let Some(value) = value {
                                            for prop in value.as_object().unwrap().props() {
                                                #[allow(non_upper_case_globals)]
                                                match prop.key() {
                                                    Id(SPA_PARAM_ROUTE_description) => {
                                                        let s = prop.value().as_bytes();
                                                        let s = &s[8..s.len() - 1];
                                                        let s =
                                                            unsafe { str::from_utf8_unchecked(s) };
                                                        sender
                                                            .unbounded_send(Message::DeviceRoute {
                                                                node_id: id,
                                                                desc: s.into(),
                                                            })
                                                            .unwrap();
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                })
                                .register();
                            DeviceInfo {
                                _object: object,
                                _listener: listener,
                            }
                        }
                    });

                    nodes.insert(
                        id,
                        Node::bind(id, device_id, None, &registry, sender.clone()),
                    );
                }
                Message::NodeDriver {
                    input_id: node_id,
                    driver_id,
                    device_id,
                } => {
                    waiting.insert(driver_id);
                    nodes.insert(
                        driver_id,
                        Node::bind(
                            driver_id,
                            device_id,
                            Some(node_id),
                            &registry,
                            sender.clone(),
                        ),
                    );
                }
                Message::NodeInfo { id, name, display } => {
                    waiting.remove(&id);
                    if waiting.is_empty() && done {
                        if let Some(x) = metadata_default {
                            (_metadata, _metadata_listener) = bind_metadata(&registry, x, &sender);
                        }
                    }
                    nodes.get_mut(&id).map(|node| {
                        if let Some(x) = name {
                            node.info.name = x;
                        }
                        if let Some(x) = display {
                            node.info.display = x;
                        }
                    });
                }
                Message::DeviceRoute { node_id, desc } => {
                    nodes.get_mut(&node_id).map(|x| x.info.route = desc);
                }
                Message::DefaultSink(name) => {
                    if default_sink.as_ref().map(|x| &x.name) == Some(&name) {
                        continue;
                    }
                    if let Some((&id, Node { info, .. })) =
                        nodes.iter().find(|(_, node)| node.info.name == name)
                    {
                        dispatch(Event::DefaultChanged {
                            display: info.display.clone(),
                            route: info.route.clone(),
                        })
                        .await;
                        if let Some(x) = info.volume {
                            dispatch(Event::Volume(x)).await;
                        }
                        if let Some(x) = info.mute {
                            dispatch(Event::Mute(x)).await;
                        }
                        default_sink = Some(DefaultNode { id, name });
                    }
                }
                Message::Props { id, volume, mute } => {
                    let input = get_input(&nodes, id);
                    let is_default_sink = default_sink.as_ref().map(|x| x.id) == Some(input);
                    let Node { info, .. } = nodes.get_mut(&input).unwrap();
                    if let Some(x) = volume {
                        if info.volume.replace(x) != volume && is_default_sink {
                            dispatch(Event::Volume(x)).await;
                        }
                    }
                    if let Some(x) = mute {
                        if info.mute.replace(x) != mute && is_default_sink {
                            dispatch(Event::Mute(x)).await;
                        }
                    }
                }
            }
        }
    };

    std::future::join!(daemon, consumer).await;
    Some(())
}

fn get_input(nodes: &FxHashMap<u32, Node>, id: u32) -> u32 {
    if let Some(id) = nodes.get(&id).unwrap().info.input {
        get_input(nodes, id)
    } else {
        id
    }
}

struct Node {
    _object: node::Node,
    _listener: NodeListener,
    info: NodeInfo,
}

#[derive(Debug)]
struct NodeInfo {
    input: Option<u32>,
    name: String,
    display: TinyString,
    volume: Option<f32>,
    mute: Option<bool>,
    route: TinyString,
}

impl Node {
    fn bind(
        id: u32,
        device_id: u32,
        node: Option<u32>,
        registry: &Registry,
        sender: UnboundedSender<Message>,
    ) -> Self {
        let object: node::Node = bind(registry, ObjectType::Node, id).unwrap();
        object.subscribe_params(&[ParamType::Props]);
        let listener = object
            .add_listener_local()
            .info({
                let sender = sender.clone();
                move |info| {
                    if let Some(props) = info.props() {
                        if let Some(driver_id) = props.get("node.driver-id") {
                            let driver_id: u32 = driver_id.parse().unwrap();
                            sender
                                .unbounded_send(Message::NodeDriver {
                                    input_id: id,
                                    device_id,
                                    driver_id,
                                })
                                .unwrap();
                        }
                        sender
                            .unbounded_send(Message::NodeInfo {
                                id,
                                name: props.get("node.name").map(Into::into),
                                display: props
                                    .get("media.name")
                                    .or_else(|| props.get("alsa.card_name"))
                                    .map(Into::into),
                            })
                            .unwrap();
                    }
                }
            })
            .param({
                let sender = sender.clone();
                move |_seq, _type, _index, _next, value| {
                    if let Some(value) = value {
                        let mut volume = None;
                        let mut mute = None;
                        for prop in value.as_object().unwrap().props() {
                            #[allow(non_upper_case_globals)]
                            match prop.key() {
                                Id(SPA_PROP_channelVolumes) => {
                                    if let Ok((_, values)) =
                                        PodDeserializer::deserialize_from::<Vec<f32>>(
                                            prop.value().as_bytes(),
                                        )
                                    {
                                        let n = values.len();
                                        let sum: f32 =
                                            values.iter().map(|x| x.powf(1.0 / 3.0)).sum();
                                        volume = Some(sum / n as f32);
                                    }
                                }
                                Id(SPA_PROP_mute) => {
                                    if let Ok(x) = prop.value().get_bool() {
                                        mute = Some(x);
                                    }
                                }
                                _ => {}
                            }
                        }
                        sender
                            .unbounded_send(Message::Props { id, volume, mute })
                            .unwrap();
                    }
                }
            })
            .register();
        Self {
            _object: object,
            _listener: listener,
            info: NodeInfo {
                input: node,
                name: String::default(),
                display: Default::default(),
                route: Default::default(),
                mute: None,
                volume: None,
            },
        }
    }
}

#[must_use]
fn bind_metadata(
    registry: &Registry,
    id: u32,
    sender: &UnboundedSender<Message>,
) -> (Metadata, MetadataListener) {
    let metadata: Metadata = bind(&registry, ObjectType::Metadata, id).unwrap();
    let listener = metadata
        .add_listener_local()
        .property({
            let sender = sender.clone();
            move |_subject, key, ty, value| {
                match (key, ty, value) {
                    (Some("default.audio.sink"), Some("Spa:String:JSON"), Some(value)) => {
                        fn parse_json(x: &str) -> &str {
                            let bytes = x.as_bytes();
                            let kv = &bytes[1..bytes.len() - 1];
                            let (_k, v) = kv.split_once(|&x| x == b':').unwrap();
                            let v = &v[1..v.len() - 1];
                            unsafe { str::from_utf8_unchecked(v) }
                        }
                        sender
                            .unbounded_send(Message::DefaultSink(parse_json(value).into()))
                            .unwrap();
                    }
                    _ => {}
                }
                0
            }
        })
        .register();
    (metadata, listener)
}

struct DeviceInfo {
    _object: Device,
    _listener: DeviceListener,
}
