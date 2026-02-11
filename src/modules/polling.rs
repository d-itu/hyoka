use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use derive_more::From;
use futures::{StreamExt as _, channel::mpsc::Receiver};

use crate::modules::{
    clock::Clock,
    fs::{Battery, BatteryInfo},
};

#[derive(From, Debug)]
pub enum Event {
    Clock(Clock),
    Battery(BatteryInfo),
}

pub enum Signal {
    Battery(Rc<Battery>),
    BatteryStop,
}

pub async fn run(signals: &mut Receiver<Signal>, mut dispatch: impl AsyncFnMut(Event)) {
    let interval = Duration::from_secs(1);
    let mut timer = compio::time::interval_at(Instant::now() + interval, interval);
    let battery = Rc::new(RefCell::new(None));
    let bat = battery.clone();
    std::future::join!(
        async {
            loop {
                match signals.next().await.unwrap() {
                    Signal::Battery(x) => {
                        battery.replace(Some(x));
                    }
                    Signal::BatteryStop => {
                        battery.replace(None);
                    }
                }
            }
        },
        async {
            loop {
                timer.tick().await;
                dispatch(Clock::now().into()).await;

                if let Some(bat) = bat.borrow().as_ref() {
                    dispatch(bat.info().into()).await;
                }
            }
        },
    )
    .await;
}
