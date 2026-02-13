#![allow(dead_code)]

use iced::{Executor, Program, futures::StreamExt};
use iced_program::Instance;

pub fn run<P>(program: P)
where
    P: Program,
{
    let (mut sender, mut receiver) = futures::channel::mpsc::channel(1);
    let executor = P::Executor::new().unwrap();
    let (mut instance, task) = Instance::new(program);

    let sink = sender.clone();
    let tasks = async {
        let stream = iced_runtime::task::into_stream(task).unwrap();
        stream.map(Ok).forward(sink).await.unwrap();
    };

    #[allow(unused_variables)]
    let execute = async {
        while let Some(action) = receiver.next().await {
            match action {
                iced_runtime::Action::Output(msg) => {
                    let task = instance.update(msg);
                    let stream = iced_runtime::task::into_stream(task).unwrap();
                    stream.map(Ok).forward(&mut sender).await.unwrap();
                }
                iced_runtime::Action::LoadFont { bytes, channel } => {
                    eprintln!("load font");
                }
                iced_runtime::Action::Widget(operation) => {
                    eprintln!("operation");
                }
                iced_runtime::Action::Clipboard(action) => {
                    eprintln!("clipboard");
                }
                iced_runtime::Action::Window(action) => {
                    unimplemented!("don't use window action")
                }
                iced_runtime::Action::System(action) => {
                    eprintln!("system");
                }
                iced_runtime::Action::Image(action) => {
                    eprintln!("image");
                }
                iced_runtime::Action::Event { window, event } => {
                    eprintln!("event");
                }
                iced_runtime::Action::Tick => {
                    eprintln!("tick");
                }
                iced_runtime::Action::Reload => {
                    eprintln!("reload");
                }
                iced_runtime::Action::Exit => {
                    eprintln!("exit");
                }
            }
        }
    };

    executor.block_on(std::future::join!(tasks, execute));

    // let x = async {
    //     let task = receiver.next();
    // };
}
