/*
 * Copyright © 2025 Collabora Ltd.
 * Copyright © 2025 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, ensure};
#[cfg(test)]
use input_linux::InputEvent;
use input_linux::{EventKind, EventTime, Key, KeyEvent, KeyState, SynchronizeEvent};
#[cfg(not(test))]
use input_linux::{InputId, UInputHandle};
#[cfg(not(test))]
use nix::fcntl::{FcntlArg, OFlag, fcntl};
#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(not(test))]
use std::fs::OpenOptions;
#[cfg(not(test))]
use std::os::fd::OwnedFd;
use std::time::SystemTime;
use tracing::warn;

pub(crate) struct UInputDevice {
    #[cfg(not(test))]
    handle: UInputHandle<OwnedFd>,
    #[cfg(test)]
    queue: VecDeque<InputEvent>,
    #[cfg(test)]
    keybits: HashSet<Key>,
    name: String,
    open: bool,
}

impl UInputDevice {
    #[cfg(not(test))]
    pub(crate) fn new() -> Result<UInputDevice> {
        let fd = OpenOptions::new()
            .write(true)
            .create(false)
            .open("/dev/uinput")?
            .into();

        let mut flags = OFlag::from_bits_retain(fcntl(&fd, FcntlArg::F_GETFL)?);
        flags.set(OFlag::O_NONBLOCK, true);
        fcntl(&fd, FcntlArg::F_SETFL(flags))?;

        Ok(UInputDevice {
            handle: UInputHandle::new(fd),
            name: String::new(),
            open: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn new() -> Result<UInputDevice> {
        Ok(UInputDevice {
            queue: VecDeque::new(),
            keybits: HashSet::new(),
            name: String::new(),
            open: false,
        })
    }

    pub(crate) fn set_name(&mut self, name: String) -> Result<()> {
        ensure!(!self.open, "Cannot change name after opening");
        self.name = name;
        Ok(())
    }

    #[cfg(not(test))]
    pub(crate) fn open(&mut self, keybits: &[Key]) -> Result<()> {
        ensure!(!self.open, "Cannot reopen uinput handle");

        self.handle.set_evbit(EventKind::Key)?;
        for key in keybits.iter().copied() {
            self.handle.set_keybit(key)?;
        }

        let input_id = InputId {
            bustype: input_linux::sys::BUS_VIRTUAL,
            vendor: 0x28DE,
            product: 0,
            version: 0,
        };
        self.handle
            .create(&input_id, self.name.as_bytes(), 0, &[])?;
        self.open = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn open(&mut self, keybits: &[Key]) -> Result<()> {
        ensure!(!self.open, "Cannot reopen uinput handle");
        self.open = true;
        self.keybits = HashSet::from_iter(keybits.into_iter().copied());
        Ok(())
    }

    fn system_time() -> Result<EventTime> {
        let duration = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
        Ok(EventTime::new(
            duration.as_secs().try_into()?,
            duration.subsec_micros().into(),
        ))
    }

    fn send_key_event(&mut self, key: Key, value: KeyState) -> Result<()> {
        let tv = UInputDevice::system_time().unwrap_or_else(|err| {
            warn!("System time error: {err}");
            EventTime::default()
        });

        let ev = KeyEvent::new(tv, key, value);
        let syn = SynchronizeEvent::report(tv);
        #[cfg(not(test))]
        self.handle.write(&[*ev.as_ref(), *syn.as_ref()])?;
        #[cfg(test)]
        {
            ensure!(self.keybits.contains(&key), "Key not in keybits");
            self.queue.extend(&[*ev.as_ref(), *syn.as_ref()]);
        }
        Ok(())
    }

    pub(crate) fn key_down(&mut self, key: Key) -> Result<()> {
        self.send_key_event(key, KeyState::PRESSED)
    }

    pub(crate) fn key_up(&mut self, key: Key) -> Result<()> {
        self.send_key_event(key, KeyState::RELEASED)
    }

    pub(crate) fn key_press(&mut self, key: Key) -> Result<()> {
        self.send_key_event(key, KeyState::PRESSED)?;
        self.send_key_event(key, KeyState::RELEASED)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn expect_sync(&mut self) -> Result<()> {
        let event = self.queue.pop_front().unwrap();
        ensure!(
            event.kind == EventKind::Synchronize,
            "event.kind is {:?}",
            event.kind
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn expect_key(&mut self, key: Key, state: KeyState) -> Result<()> {
        let event = self.queue.pop_front().unwrap();
        ensure!(
            event.kind == EventKind::Key,
            "event.kind is {:?}",
            event.kind
        );
        ensure!(event.code == key as u16, "event.code is {}", event.code);
        ensure!(event.value == state.value, "event.value is {}", event.value);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn expect_empty(&mut self) -> Result<()> {
        ensure!(self.queue.is_empty(), "queue not empty");
        Ok(())
    }
}
