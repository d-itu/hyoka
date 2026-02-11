use std::{
    cell::OnceCell,
    ffi::CStr,
    fmt::Write as _,
    mem::MaybeUninit,
    os::fd::{AsFd, OwnedFd},
};

use rustix::fs::{Mode, OFlags};

use crate::TinyString;

#[derive(Debug, Clone, Copy)]
pub struct BatteryInfo {
    status: ChargingStatus,
    power_now: u32,
    energy_now: u32,
    energy_full: u32,
}

#[derive(Default, Debug, PartialEq, Clone, Copy)]
pub enum ChargingStatus {
    Charging,
    Full,
    #[default]
    Other,
}
impl ChargingStatus {
    pub fn from_bytes(s: &[u8]) -> Self {
        if s.starts_with(b"Charging") {
            return Self::Charging;
        }
        if s.starts_with(b"Full") {
            return Self::Full;
        }
        Self::Other
    }
}

pub struct Battery {
    fd: OwnedFd,
    energy_full: OnceCell<u32>,
}
impl Battery {
    pub fn new() -> Option<Self> {
        let fd =
            rustix::fs::open(c"/sys/class/power_supply", OFlags::empty(), Mode::empty()).ok()?;
        let mut buf = [MaybeUninit::uninit(); 1024];
        let mut dir = rustix::fs::RawDir::new(&fd, &mut buf);
        while let Some(entry) = dir.next() {
            let entry = entry.unwrap();

            // skip . and ..
            if unsafe { *(entry.file_name().as_ptr() as *const u8) } == b'.' {
                continue;
            }
            let name = entry.file_name();
            let device = rustix::fs::openat(&fd, name, OFlags::empty(), Mode::empty()).unwrap();
            let r#type =
                rustix::fs::openat(&device, c"type", OFlags::empty(), Mode::empty()).unwrap();
            let mut buf = [MaybeUninit::uninit(); 32];
            let (r#type, _) = rustix::io::read(r#type, &mut buf).unwrap();
            if r#type.starts_with(b"Battery") {
                return Some(Self {
                    fd: device,
                    energy_full: OnceCell::new(),
                });
            }
        }
        None
    }
    pub fn capacity(&self) -> u8 {
        Attr::get(&self.fd, c"capacity")
    }
    pub fn status(&self) -> ChargingStatus {
        Attr::get(&self.fd, c"status")
    }
    pub fn power_now(&self) -> u32 {
        Attr::get(&self.fd, c"power_now")
    }
    pub fn energy_now(&self) -> u32 {
        Attr::get(&self.fd, c"energy_now")
    }
    pub fn energy_full(&self) -> u32 {
        *self
            .energy_full
            .get_or_init(|| Attr::get(&self.fd, c"energy_full"))
    }
    pub fn info(&self) -> BatteryInfo {
        BatteryInfo {
            status: self.status(),
            power_now: self.power_now(),
            energy_now: self.energy_now(),
            energy_full: self.energy_full(),
        }
    }
}

impl BatteryInfo {
    fn charing(&self) -> bool {
        self.status == ChargingStatus::Charging
    }
    fn percentage(&self) -> u32 {
        self.energy_now * 10 / (self.energy_full / 1000)
    }
    fn energy_remaining(&self) -> u32 {
        if self.charing() {
            self.energy_full - self.energy_now
        } else {
            self.energy_now
        }
    }
    fn minutes_remaining(&self) -> u32 {
        if self.power_now == 0 {
            0
        } else {
            self.energy_remaining() * 6 / (self.power_now / 10)
        }
    }
    pub fn tooltip(&self) -> TinyString {
        let power = self.power_now / 1_000_000;
        let power_frac = (self.power_now - power * 1_000_000) / 10_000;
        let percentage = self.percentage();
        let cap = percentage / 100 + if percentage % 100 > 50 { 1 } else { 0 };
        let h = self.minutes_remaining() / 60;
        let m = self.minutes_remaining() % 60;
        let mut result = TinyString::new();
        write!(&mut result, "{cap}% {power}.{power_frac:0>2}W {h}h{m}").unwrap();
        result
    }
}

pub struct Backlight(OwnedFd);

impl Backlight {
    pub fn new() -> Option<Self> {
        let fd = rustix::fs::open(c"/sys/class/backlight", OFlags::empty(), Mode::empty()).ok()?;
        let mut buf = [MaybeUninit::uninit(); 1024];
        let mut dir = rustix::fs::RawDir::new(&fd, &mut buf);
        while let Some(entry) = dir.next() {
            let entry = entry.unwrap();

            // skip . and ..
            if unsafe { *(entry.file_name().as_ptr() as *const u8) } == b'.' {
                continue;
            }
            let name = entry.file_name();
            let device = rustix::fs::openat(&fd, name, OFlags::empty(), Mode::empty()).unwrap();
            return Some(Self(device));
        }
        None
    }
    pub fn brightness(&self) -> u32 {
        u32::get(&self.0, c"brightness")
    }
    pub fn max_brightness(&self) -> u32 {
        u32::get(&self.0, c"max_brightness")
    }
    pub fn percentage(&self) -> u32 {
        self.brightness() * 100 / self.max_brightness()
    }
}

trait Attr {
    fn get(dev: impl AsFd, path: &CStr) -> Self;
}

impl Attr for u32 {
    fn get(dev: impl AsFd, path: &CStr) -> Self {
        let mut buf = [MaybeUninit::uninit(); 1024];
        let value = rustix::fs::openat(dev, path, OFlags::empty(), Mode::empty()).unwrap();
        let (value, _) = rustix::io::read(value, &mut buf).unwrap();
        let value = u32::from_ascii(unsafe { value.get_unchecked(..value.len() - 1) }).unwrap();
        value
    }
}

impl Attr for u8 {
    fn get(dev: impl AsFd, path: &CStr) -> Self {
        let mut buf = [MaybeUninit::uninit(); 1024];
        let value = rustix::fs::openat(dev, path, OFlags::empty(), Mode::empty()).unwrap();
        let (value, _) = rustix::io::read(value, &mut buf).unwrap();
        let value = u8::from_ascii(unsafe { value.get_unchecked(..value.len() - 1) }).unwrap();
        value
    }
}

impl Attr for ChargingStatus {
    fn get(dev: impl AsFd, path: &CStr) -> Self {
        let mut buf = [MaybeUninit::uninit(); 1024];
        let value = rustix::fs::openat(dev, path, OFlags::empty(), Mode::empty()).unwrap();
        let (value, _) = rustix::io::read(value, &mut buf).unwrap();
        let value = ChargingStatus::from_bytes(unsafe { value.get_unchecked(..value.len() - 1) });
        value
    }
}
