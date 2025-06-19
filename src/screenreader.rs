/*
 * Copyright © 2025 Collabora Ltd.
 * Copyright © 2025 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use ::sysinfo::System;
use anyhow::{anyhow, bail, ensure, Result};
use gio::{prelude::SettingsExt, Settings};
use input_linux::Key;
use lazy_static::lazy_static;
use nix::sys::signal;
use nix::unistd::Pid;
use num_enum::TryFromPrimitive;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use strum::{Display, EnumString};
use tokio::fs::{read_to_string, write};
use tracing::{debug, error, info, trace, warn};
#[cfg(not(test))]
use xdg::BaseDirectories;
use zbus::Connection;

#[cfg(test)]
use crate::path;
use crate::systemd::SystemdUnit;
use crate::uinput::UInputDevice;

#[cfg(test)]
const TEST_ORCA_SETTINGS: &str = "data/test-orca-settings.conf";
#[cfg(test)]
const ORCA_SETTINGS: &str = "orca-settings.conf";

#[cfg(not(test))]
const ORCA_SETTINGS: &str = "orca/user-settings.conf";
const PITCH_SETTING: &str = "average-pitch";
const RATE_SETTING: &str = "rate";
const VOLUME_SETTING: &str = "gain";
const ENABLE_SETTING: &str = "enableSpeech";

const A11Y_SETTING: &str = "org.gnome.desktop.a11y.applications";
const SCREEN_READER_SETTING: &str = "screen-reader-enabled";
const KEYBOARD_NAME: &str = "steamos-manager";

const PITCH_DEFAULT: f64 = 5.0;
const RATE_DEFAULT: f64 = 50.0;
const VOLUME_DEFAULT: f64 = 10.0;

lazy_static! {
    static ref VALID_SETTINGS: HashMap<&'static str, RangeInclusive<f64>> = HashMap::from_iter([
        (PITCH_SETTING, (0.0..=10.0)),
        (RATE_SETTING, (0.0..=100.0)),
        (VOLUME_SETTING, (0.0..=10.0)),
    ]);
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[repr(u32)]
pub enum ScreenReaderMode {
    Browse = 0,
    Focus = 1,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[repr(u32)]
pub enum ScreenReaderAction {
    StopTalking = 0,
    ReadNextWord = 1,
    ReadPreviousWord = 2,
    ReadNextItem = 3,
    ReadPreviousItem = 4,
    MoveToNextLandmark = 5,
    MoveToPreviousLandmark = 6,
    MoveToNextHeading = 7,
    MoveToPreviousHeading = 8,
    ToggleMode = 9,
}

pub(crate) struct OrcaManager<'dbus> {
    orca_unit: SystemdUnit<'dbus>,
    rate: f64,
    pitch: f64,
    volume: f64,
    enabled: bool,
    mode: ScreenReaderMode,
    keyboard: UInputDevice,
}

impl<'dbus> OrcaManager<'dbus> {
    pub async fn new(connection: &Connection) -> Result<OrcaManager<'dbus>> {
        let mut manager = OrcaManager {
            orca_unit: SystemdUnit::new(connection.clone(), "orca.service").await?,
            rate: RATE_DEFAULT,
            pitch: PITCH_DEFAULT,
            volume: VOLUME_DEFAULT,
            enabled: true,
            // Always start in browse mode for now, since we have no storage to remember this property
            mode: ScreenReaderMode::Browse,
            keyboard: UInputDevice::new()?,
        };
        let _ = manager
            .load_values()
            .await
            .inspect_err(|e| warn!("Failed to load orca configuration: {e}"));
        let a11ysettings = Settings::new(A11Y_SETTING);
        manager.enabled = a11ysettings.boolean(SCREEN_READER_SETTING);
        manager.keyboard.set_name(KEYBOARD_NAME.to_string())?;
        manager.keyboard.open(&[
            Key::A,
            Key::H,
            Key::M,
            Key::Insert,
            Key::LeftCtrl,
            Key::LeftShift,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Up,
        ])?;

        Ok(manager)
    }

    #[cfg(not(test))]
    fn settings_path(&self) -> Result<PathBuf> {
        let xdg_base = BaseDirectories::new();
        Ok(xdg_base
            .get_data_home()
            .ok_or(anyhow!("No XDG_DATA_HOME found"))?
            .join(ORCA_SETTINGS))
    }

    #[cfg(test)]
    fn settings_path(&self) -> Result<PathBuf> {
        Ok(path(ORCA_SETTINGS))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub async fn set_enabled(&mut self, enable: bool) -> Result<()> {
        if enable != self.enabled {
            #[cfg(not(test))]
            {
                let a11ysettings = Settings::new(A11Y_SETTING);
                a11ysettings
                    .set_boolean(SCREEN_READER_SETTING, enable)
                    .map_err(|e| anyhow!("Unable to set screen reader enabled gsetting, {e}"))?;
            }
            if let Err(e) = self.set_orca_enabled(enable).await {
                match e.downcast_ref::<std::io::Error>() {
                    Some(e) if e.kind() == ErrorKind::NotFound => (),
                    _ => return Err(e),
                }
            }
        }
        if enable {
            self.restart_orca().await?;
        } else {
            self.stop_orca().await?;
        }
        self.enabled = enable;
        Ok(())
    }

    pub fn pitch(&self) -> f64 {
        self.pitch
    }

    pub async fn set_pitch(&mut self, pitch: f64) -> Result<()> {
        trace!("set_pitch called with {pitch}");

        self.set_orca_option(PITCH_SETTING, pitch).await?;
        self.pitch = pitch;
        self.reload_orca().await?;
        Ok(())
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub async fn set_rate(&mut self, rate: f64) -> Result<()> {
        trace!("set_rate called with {rate}");

        self.set_orca_option(RATE_SETTING, rate).await?;
        self.rate = rate;
        self.reload_orca().await?;
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume
    }

    pub async fn set_volume(&mut self, volume: f64) -> Result<()> {
        trace!("set_volume called with {volume}");

        self.set_orca_option(VOLUME_SETTING, volume).await?;
        self.volume = volume;
        self.reload_orca().await?;
        Ok(())
    }

    pub fn mode(&self) -> ScreenReaderMode {
        self.mode
    }

    pub async fn set_mode(&mut self, mode: ScreenReaderMode) -> Result<()> {
        if self.mode == mode {
            return Ok(());
        }

        // Use insert+A twice to switch to focus mode sticky
        // Use insert+A three times to switch to browse mode sticky
        match mode {
            ScreenReaderMode::Focus => {
                self.keyboard.key_down(Key::Insert)?;
                self.keyboard.key_press(Key::A)?;
                self.keyboard.key_press(Key::A)?;
                self.keyboard.key_up(Key::Insert)?;
            }
            ScreenReaderMode::Browse => {
                self.keyboard.key_down(Key::Insert)?;
                self.keyboard.key_press(Key::A)?;
                self.keyboard.key_press(Key::A)?;
                self.keyboard.key_press(Key::A)?;
                self.keyboard.key_up(Key::Insert)?;
            }
        }
        self.mode = mode;

        Ok(())
    }

    pub async fn trigger_action(
        &mut self,
        action: ScreenReaderAction,
        _timestamp: u64,
    ) -> Result<()> {
        // TODO: Maybe filter events if the timestamp is too old?
        match action {
            ScreenReaderAction::StopTalking => {
                // TODO: Use dbus method to stop orca from speaking instead once that's in a release/steamos package.
                let pid = self.get_orca_pid()?;
                signal::kill(pid, signal::Signal::SIGUSR2)?;
            }
            ScreenReaderAction::ReadNextWord => {
                self.keyboard.key_down(Key::LeftCtrl)?;
                self.keyboard.key_press(Key::Right)?;
                self.keyboard.key_up(Key::LeftCtrl)?;
            }
            ScreenReaderAction::ReadPreviousWord => {
                self.keyboard.key_down(Key::LeftCtrl)?;
                self.keyboard.key_press(Key::Left)?;
                self.keyboard.key_up(Key::LeftCtrl)?;
            }
            ScreenReaderAction::ReadNextItem => {
                self.keyboard.key_press(Key::Down)?;
            }
            ScreenReaderAction::ReadPreviousItem => {
                self.keyboard.key_press(Key::Up)?;
            }
            ScreenReaderAction::MoveToNextLandmark => {
                self.keyboard.key_press(Key::M)?;
            }
            ScreenReaderAction::MoveToPreviousLandmark => {
                self.keyboard.key_down(Key::LeftShift)?;
                self.keyboard.key_press(Key::M)?;
                self.keyboard.key_up(Key::LeftShift)?;
            }
            ScreenReaderAction::MoveToNextHeading => {
                self.keyboard.key_press(Key::H)?;
            }
            ScreenReaderAction::MoveToPreviousHeading => {
                self.keyboard.key_down(Key::LeftShift)?;
                self.keyboard.key_press(Key::H)?;
                self.keyboard.key_up(Key::LeftShift)?;
            }
            ScreenReaderAction::ToggleMode => {
                self.keyboard.key_down(Key::Insert)?;
                self.keyboard.key_press(Key::A)?;
                self.keyboard.key_up(Key::Insert)?;
                // TODO: I guess we should emit that the mode changed here...
                match self.mode {
                    ScreenReaderMode::Browse => {
                        self.mode = ScreenReaderMode::Focus;
                    }
                    ScreenReaderMode::Focus => {
                        self.mode = ScreenReaderMode::Browse;
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    async fn reload_orca(&self) -> Result<()> {
        Ok(())
    }

    #[cfg(not(test))]
    async fn reload_orca(&self) -> Result<()> {
        // TODO: Use dbus api to tell orca to reload settings once dbus api is packaged.
        let pid = self.get_orca_pid()?;
        signal::kill(pid, signal::Signal::SIGUSR1)?;
        Ok(())
    }

    fn get_orca_pid(&self) -> Result<Pid> {
        let mut system = System::new();
        system.refresh_all();

        let mut p = system.processes_by_exact_name("orca".as_ref());

        let pid = p.next().ok_or(anyhow!("No orca process found"))?;
        Ok(Pid::from_raw(pid.pid().as_u32().try_into()?))
    }

    async fn set_orca_enabled(&mut self, enabled: bool) -> Result<()> {
        // Change json file
        let data = read_to_string(self.settings_path()?).await?;
        let mut json: Value = serde_json::from_str(&data)?;

        let general = json
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf json is not an object"))?
            .entry("general")
            .or_insert(Value::Object(Map::new()));
        general
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf general is not an object"))?
            .insert(ENABLE_SETTING.to_string(), Value::Bool(enabled));

        let data = serde_json::to_string_pretty(&json)?;
        Ok(write(self.settings_path()?, data.as_bytes()).await?)
    }

    async fn load_values(&mut self) -> Result<()> {
        debug!("Loading orca values from user-settings.conf");
        let data = read_to_string(self.settings_path()?).await?;
        let json: Value = serde_json::from_str(&data)?;

        let Some(default_voice) = json
            .get("profiles")
            .and_then(|profiles| profiles.get("default"))
            .and_then(|default_profile| default_profile.get("voices"))
            .and_then(|voices| voices.get("default"))
        else {
            warn!("Orca user-settings.conf missing default voice");
            self.pitch = PITCH_DEFAULT;
            self.rate = RATE_DEFAULT;
            self.volume = VOLUME_DEFAULT;
            return Ok(());
        };
        if let Some(pitch) = default_voice.get(PITCH_SETTING) {
            self.pitch = pitch.as_f64().unwrap_or_else(|| {
                error!("Unable to convert orca pitch setting to float value");
                PITCH_DEFAULT
            });
        } else {
            warn!("Unable to load default pitch from orca user-settings.conf");
            self.pitch = PITCH_DEFAULT;
        }

        if let Some(rate) = default_voice.get(RATE_SETTING) {
            self.rate = rate.as_f64().unwrap_or_else(|| {
                error!("Unable to convert orca rate setting to float value");
                RATE_DEFAULT
            });
        } else {
            warn!("Unable to load default voice rate from orca user-settings.conf");
        }

        if let Some(volume) = default_voice.get(VOLUME_SETTING) {
            self.volume = volume.as_f64().unwrap_or_else(|| {
                error!("Unable to convert orca volume value to float value");
                VOLUME_DEFAULT
            });
        } else {
            warn!("Unable to load default voice volume from orca user-settings.conf");
        }
        info!(
            "Done loading orca user-settings.conf, values: Rate: {}, Pitch: {}, Volume: {}",
            self.rate, self.pitch, self.volume
        );

        Ok(())
    }

    async fn set_orca_option(&self, option: &str, value: f64) -> Result<()> {
        if let Some(range) = VALID_SETTINGS.get(option) {
            ensure!(
                range.contains(&value),
                "orca option {option} value {value} out of range"
            );
        } else {
            bail!("Invalid orca option {option}");
        }
        let data = read_to_string(self.settings_path()?).await?;
        let mut json: Value = serde_json::from_str(&data)?;

        let profiles = json
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf json is not an object"))?
            .entry("profiles")
            .or_insert(Value::Object(Map::new()));
        let default_profile = profiles
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf profiles is not an object"))?
            .entry("default")
            .or_insert(Value::Object(Map::new()));
        let voices = default_profile
            .as_object_mut()
            .ok_or(anyhow!(
                "orca user-settings.conf default profile is not an object"
            ))?
            .entry("voices")
            .or_insert(Value::Object(Map::new()));
        let default_voice = voices
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf voices is not an object"))?
            .entry("default")
            .or_insert(Value::Object(Map::new()));
        default_voice
            .as_object_mut()
            .ok_or(anyhow!(
                "orca user-settings.conf default voice is not an object"
            ))?
            .insert(option.to_string(), value.into());

        let data = serde_json::to_string_pretty(&json)?;
        Ok(write(self.settings_path()?, data.as_bytes()).await?)
    }

    async fn restart_orca(&self) -> Result<()> {
        trace!("Restarting orca...");
        self.orca_unit.restart().await
    }

    async fn stop_orca(&self) -> Result<()> {
        trace!("Stopping orca...");
        self.orca_unit.stop().await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::systemd::test::MockUnit;
    use crate::testing;
    use input_linux::{Key, KeyState};
    use std::time::Duration;
    use tokio::fs::{copy, remove_file};
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_enable_disable() {
        let mut h = testing::start();
        let mut unit = MockUnit::default();
        unit.active = String::from("inactive");
        unit.unit_file = String::from("disabled");
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1/unit/orca_2eservice", unit)
            .await
            .expect("at");

        sleep(Duration::from_millis(10)).await;

        let unit = SystemdUnit::new(connection.clone(), "orca.service")
            .await
            .expect("unit");

        let mut manager = OrcaManager::new(&connection)
            .await
            .expect("OrcaManager::new");
        manager.set_enabled(true).await.unwrap();
        assert_eq!(manager.enabled(), true);
        assert_eq!(unit.active().await.unwrap(), true);

        manager.set_enabled(false).await.unwrap();
        assert_eq!(manager.enabled(), false);
        assert_eq!(unit.active().await.unwrap(), false);

        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        manager.load_values().await.unwrap();

        manager.set_enabled(true).await.unwrap();
        assert_eq!(manager.enabled(), true);
        assert_eq!(unit.active().await.unwrap(), true);

        manager.set_enabled(false).await.unwrap();
        assert_eq!(manager.enabled(), false);
        assert_eq!(unit.active().await.unwrap(), false);
    }

    #[tokio::test]
    async fn test_pitch() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        let set_result = manager.set_pitch(5.0).await;
        assert!(set_result.is_ok());
        assert_eq!(manager.pitch(), 5.0);

        let too_low_result = manager.set_pitch(-1.0).await;
        assert!(too_low_result.is_err());
        assert_eq!(manager.pitch(), 5.0);

        let too_high_result = manager.set_pitch(12.0).await;
        assert!(too_high_result.is_err());
        assert_eq!(manager.pitch(), 5.0);

        remove_file(h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let nofile_result = manager.set_pitch(7.0).await;
        assert_eq!(manager.pitch(), 5.0);
        assert!(nofile_result.is_err());
    }

    #[tokio::test]
    async fn test_rate() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        let set_result = manager.set_rate(5.0).await;
        assert!(set_result.is_ok());
        assert_eq!(manager.rate(), 5.0);

        let too_low_result = manager.set_rate(-1.0).await;
        assert!(too_low_result.is_err());
        assert_eq!(manager.rate(), 5.0);

        let too_high_result = manager.set_rate(101.0).await;
        assert!(too_high_result.is_err());
        assert_eq!(manager.rate(), 5.0);

        remove_file(h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let nofile_result = manager.set_rate(7.0).await;
        assert_eq!(manager.rate(), 5.0);
        assert!(nofile_result.is_err());
    }

    #[tokio::test]
    async fn test_volume() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        let set_result = manager.set_volume(5.0).await;
        assert!(set_result.is_ok());
        assert_eq!(manager.volume(), 5.0);

        let too_low_result = manager.set_volume(-1.0).await;
        assert!(too_low_result.is_err());
        assert_eq!(manager.volume(), 5.0);

        let too_high_result = manager.set_volume(12.0).await;
        assert!(too_high_result.is_err());
        assert_eq!(manager.volume(), 5.0);

        remove_file(h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let nofile_result = manager.set_volume(7.0).await;
        assert_eq!(manager.volume(), 5.0);
        assert!(nofile_result.is_err());
    }

    #[tokio::test]
    async fn test_read_next_word() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::ReadNextWord, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::LeftCtrl, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Right, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Right, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::LeftCtrl, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_read_previous_word() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::ReadPreviousWord, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::LeftCtrl, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Left, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Left, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::LeftCtrl, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_read_next_item() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::ReadNextItem, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::Down, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Down, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_read_previous_item() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::ReadPreviousItem, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::Up, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Up, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_move_to_next_landmark() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::MoveToNextLandmark, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::M, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::M, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_move_to_previous_landmark() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::MoveToPreviousLandmark, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::LeftShift, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::M, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::M, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::LeftShift, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_move_to_next_heading() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::MoveToNextHeading, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::H, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::H, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_move_to_previous_heading() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager
            .trigger_action(ScreenReaderAction::MoveToPreviousHeading, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::LeftShift, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::H, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::H, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::LeftShift, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();
    }

    #[tokio::test]
    async fn test_toggle_mode_to_focus() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager.mode = ScreenReaderMode::Browse;
        manager
            .trigger_action(ScreenReaderAction::ToggleMode, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();

        assert_eq!(manager.mode, ScreenReaderMode::Focus);
    }

    #[tokio::test]
    async fn test_toggle_mode_to_browse() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager.mode = ScreenReaderMode::Focus;
        manager
            .trigger_action(ScreenReaderAction::ToggleMode, 0)
            .await
            .unwrap();

        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();

        assert_eq!(manager.mode, ScreenReaderMode::Browse);
    }

    #[tokio::test]
    async fn test_set_mode_to_focus() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager.mode = ScreenReaderMode::Browse;
        manager.set_mode(ScreenReaderMode::Focus).await.unwrap();

        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();

        assert_eq!(manager.mode, ScreenReaderMode::Focus);
    }

    #[tokio::test]
    async fn test_set_mode_to_browse() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager.mode = ScreenReaderMode::Focus;
        manager.set_mode(ScreenReaderMode::Browse).await.unwrap();

        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::PRESSED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::A, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager
            .keyboard
            .expect_key(Key::Insert, KeyState::RELEASED)
            .unwrap();
        manager.keyboard.expect_sync().unwrap();
        manager.keyboard.expect_empty().unwrap();

        assert_eq!(manager.mode, ScreenReaderMode::Browse);
    }

    #[tokio::test]
    async fn test_set_mode_same() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        manager.mode = ScreenReaderMode::Browse;
        manager.set_mode(ScreenReaderMode::Browse).await.unwrap();
        manager.keyboard.expect_empty().unwrap();
        assert_eq!(manager.mode, ScreenReaderMode::Browse);
    }
}
