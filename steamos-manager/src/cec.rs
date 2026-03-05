/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Error, Result, bail};
use cecd_proxy::Config1Proxy;
use num_enum::TryFromPrimitive;
use serde::Serialize;
use std::fmt;
use std::io::ErrorKind;
use std::path::Path;
use std::str::FromStr;
use tokio::fs::{create_dir_all, remove_file, write};
use toml;
use xdg::BaseDirectories;
use zbus::Connection;

use crate::hardware::device_config;
use crate::systemd::{EnableState, JobMode, SystemdUnit, daemon_reload};

const CECD_CONFIG_DIR: &str = "cecd/config.d";
const CECD_RUNTIME_CONFIG: &str = "99-steamos-manager.toml";
const CECD_SYSTEM_CONFIG: &str = "00-steamos-manager.toml";

#[derive(PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[repr(u32)]
pub enum HdmiCecState {
    Disabled = 0,
    ControlOnly = 1,
    ControlAndWake = 2,
}

impl FromStr for HdmiCecState {
    type Err = Error;
    fn from_str(input: &str) -> Result<HdmiCecState, Self::Err> {
        Ok(match input.to_lowercase().as_str() {
            "disable" | "disabled" | "off" => HdmiCecState::Disabled,
            "control-only" | "controlonly" => HdmiCecState::ControlOnly,
            "control-wake" | "control-and-wake" | "controlandwake" => HdmiCecState::ControlAndWake,
            v => bail!("No enum match for value {v}"),
        })
    }
}

impl fmt::Display for HdmiCecState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HdmiCecState::Disabled => write!(f, "Disabled"),
            HdmiCecState::ControlOnly => write!(f, "ControlOnly"),
            HdmiCecState::ControlAndWake => write!(f, "ControlAndWake"),
        }
    }
}

impl HdmiCecState {
    #[must_use]
    pub fn to_human_readable(&self) -> &'static str {
        match self {
            HdmiCecState::Disabled => "disabled",
            HdmiCecState::ControlOnly => "control-only",
            HdmiCecState::ControlAndWake => "control-and-wake",
        }
    }
}

#[derive(Serialize, Clone, Debug, Default)]
struct CecdConfigFragment {
    pub wake_tv: bool,
    pub uinput: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
struct CecdSystemFragment {
    pub osd_name: Option<String>,
    pub vendor_id: Option<String>, // TODO: Make this a VendorId once linux-cec 0.2 is tagged
}

enum HdmiCecBackend<'dbus> {
    Legacy {
        plasma_rc_unit: SystemdUnit<'dbus>,
        wakehook_unit: SystemdUnit<'dbus>,
    },
    Cecd(Config1Proxy<'dbus>),
}

pub struct HdmiCecControl<'dbus> {
    backend: HdmiCecBackend<'dbus>,
    connection: Connection,
}

impl<'dbus> HdmiCecControl<'dbus> {
    pub async fn new(connection: &Connection) -> Result<HdmiCecControl<'dbus>> {
        let backend = if let Ok(proxy) = Config1Proxy::new(connection).await
            && proxy.wake_tv().await.is_ok()
        {
            // Prefer cecd if available
            HdmiCecBackend::Cecd(proxy)
        } else {
            HdmiCecBackend::Legacy {
                plasma_rc_unit: SystemdUnit::new(connection, "plasma-remotecontrollers.service")
                    .await?,
                wakehook_unit: SystemdUnit::new(connection, "wakehook.service").await?,
            }
        };
        Ok(HdmiCecControl {
            backend,
            connection: connection.clone(),
        })
    }

    pub async fn configure_cecd(path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref().join(CECD_CONFIG_DIR);
        // XXX: If we ever get async combinators, cleaning this up would be nice
        let (osd_name, vendor_id) = if let Some(device_config) = device_config().await?
            && let Some(device_match) = device_config.device_match().await?
            && (device_match.friendly_name.is_some() || device_match.oui.is_some())
        {
            (device_match.friendly_name.clone(), device_match.oui.clone())
        } else {
            if let Err(err) = remove_file(path.join(CECD_SYSTEM_CONFIG)).await
                && err.kind() != ErrorKind::NotFound
            {
                return Err(err.into());
            }
            return Ok(());
        };
        create_dir_all(&path).await?;
        let path = path.join(CECD_SYSTEM_CONFIG);
        let fragment = CecdSystemFragment {
            osd_name,
            vendor_id,
        };
        let fragment = toml::to_string(&fragment)?;
        write(path, fragment.as_bytes()).await?;
        Ok(())
    }

    pub async fn get_enabled_state(&self) -> Result<HdmiCecState> {
        Ok(match &self.backend {
            HdmiCecBackend::Legacy {
                plasma_rc_unit,
                wakehook_unit,
            } => {
                if !matches!(
                    plasma_rc_unit.enabled().await?,
                    EnableState::Enabled | EnableState::Static
                ) {
                    HdmiCecState::Disabled
                } else if matches!(
                    wakehook_unit.enabled().await?,
                    EnableState::Enabled | EnableState::Static
                ) {
                    HdmiCecState::ControlAndWake
                } else {
                    HdmiCecState::ControlOnly
                }
            }
            HdmiCecBackend::Cecd(proxy) => {
                let wake = proxy.wake_tv().await?;
                let control = proxy.uinput().await?;
                match (control, wake) {
                    (true, true) => HdmiCecState::ControlAndWake,
                    (true, false) => HdmiCecState::ControlOnly,
                    (false, _) => HdmiCecState::Disabled,
                }
            }
        })
    }

    pub async fn set_enabled_state(&self, state: HdmiCecState) -> Result<()> {
        match &self.backend {
            HdmiCecBackend::Legacy {
                plasma_rc_unit,
                wakehook_unit,
            } => match state {
                HdmiCecState::Disabled => {
                    plasma_rc_unit.mask().await?;
                    plasma_rc_unit.stop(JobMode::Fail).await?;
                    wakehook_unit.mask().await?;
                    wakehook_unit.stop(JobMode::Fail).await?;
                    daemon_reload(&self.connection).await?;
                }
                HdmiCecState::ControlOnly => {
                    wakehook_unit.mask().await?;
                    wakehook_unit.stop(JobMode::Fail).await?;
                    plasma_rc_unit.unmask().await?;
                    daemon_reload(&self.connection).await?;
                    plasma_rc_unit.start(JobMode::Fail).await?;
                }
                HdmiCecState::ControlAndWake => {
                    plasma_rc_unit.unmask().await?;
                    wakehook_unit.unmask().await?;
                    daemon_reload(&self.connection).await?;
                    plasma_rc_unit.start(JobMode::Fail).await?;
                    wakehook_unit.start(JobMode::Fail).await?;
                }
            },
            HdmiCecBackend::Cecd(proxy) => {
                let fragment = CecdConfigFragment {
                    wake_tv: state == HdmiCecState::ControlAndWake,
                    uinput: state != HdmiCecState::Disabled,
                };
                let Some(home) = BaseDirectories::new().get_config_home() else {
                    bail!("No home directory found");
                };
                let fragment = toml::to_string(&fragment)?;
                let path = home.join(CECD_CONFIG_DIR);
                create_dir_all(&path).await?;
                let path = path.join(CECD_RUNTIME_CONFIG);
                write(path, fragment.as_bytes()).await?;
                proxy.reload().await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::collections::HashMap;
    use tokio::fs::{read_to_string, try_exists};

    use crate::hardware::SteamDeckVariant;
    use crate::hardware::test::fake_model;
    use crate::{enum_roundtrip, path, testing};

    #[test]
    fn hdmi_cec_state_roundtrip() {
        enum_roundtrip!(HdmiCecState {
            0: u32 = Disabled,
            1: u32 = ControlOnly,
            2: u32 = ControlAndWake,
            "Disabled": str = Disabled,
            "ControlOnly": str = ControlOnly,
            "ControlAndWake": str = ControlAndWake,
        });
        assert_eq!(
            HdmiCecState::from_str("control-only").unwrap(),
            HdmiCecState::ControlOnly
        );
        assert_eq!(
            HdmiCecState::from_str("control-and-wake").unwrap(),
            HdmiCecState::ControlAndWake
        );
        assert_eq!(HdmiCecState::Disabled.to_human_readable(), "disabled");
        assert_eq!(
            HdmiCecState::ControlOnly.to_human_readable(),
            "control-only"
        );
        assert_eq!(
            HdmiCecState::ControlAndWake.to_human_readable(),
            "control-and-wake"
        );
        assert!(HdmiCecState::try_from(3).is_err());
        assert!(HdmiCecState::from_str("working").is_err());
    }

    #[tokio::test]
    async fn test_system_config_none() {
        let _h = testing::start();
        let home = path("cecd");
        HdmiCecControl::configure_cecd(&home).await.unwrap();
        assert!(!try_exists(home.join("cecd/config.d")).await.unwrap());
    }

    #[tokio::test]
    async fn test_system_config_steam_deck() {
        let _h = testing::start();
        fake_model(SteamDeckVariant::Jupiter).await.unwrap();

        let home = path("cecd");
        HdmiCecControl::configure_cecd(&home).await.unwrap();

        let path = home.join(CECD_CONFIG_DIR).join(CECD_SYSTEM_CONFIG);
        let config = read_to_string(path).await.unwrap();
        let config = toml::from_str::<HashMap<String, String>>(config.as_str()).unwrap();
        assert_eq!(config.get("osd_name").unwrap(), "Steam Deck");
        assert_eq!(config.get("vendor_id").unwrap(), "e0-31-9e");
    }
}
