use std::{mem::MaybeUninit, os::fd::OwnedFd};

use rustix::fs::{Mode, OFlags};

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
}
