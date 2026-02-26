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
use std::str::FromStr;
use tokio::fs::{create_dir_all, write};
use toml;
use xdg::BaseDirectories;
use zbus::Connection;

use crate::systemd::{EnableState, JobMode, SystemdUnit, daemon_reload};

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

enum HdmiCecBackend<'dbus> {
    Legacy {
        plasma_rc_unit: SystemdUnit<'dbus>,
        wakehook_unit: SystemdUnit<'dbus>,
    },
    Cecd(Config1Proxy<'dbus>),
}

pub(crate) struct HdmiCecControl<'dbus> {
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
                let path = home.join("cecd/config.d");
                create_dir_all(&path).await?;
                let path = path.join("99-steamos-manager.toml");
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
    use crate::enum_roundtrip;

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
}
