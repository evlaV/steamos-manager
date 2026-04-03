/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Error, Result, bail, ensure};
use async_trait::async_trait;
use cecd_proxy::{CecDevice1Proxy, Config1Proxy};
use linux_cec::{PhysicalAddress, VendorId};
use num_enum::TryFromPrimitive;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::ErrorKind;
use std::path::Path;
use std::str::FromStr;
use strum::{Display, EnumString, VariantNames};
use tokio::fs::{File, create_dir_all, remove_file, write};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_stream::StreamExt;
use toml;
use xdg::BaseDirectories;
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;

use crate::hardware::device_config;
use crate::manager::root::RootManagerProxy;
use crate::systemd::{EnableState, JobMode, SystemdUnit, daemon_reload};
use crate::{Service, path};

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

#[derive(Deserialize, Display, EnumString, VariantNames, PartialEq, Debug, Clone)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum HdmiCecHardware {
    CrosEc { port: u8 },
}

#[async_trait]
pub(crate) trait HdmiCecHwController: Send + Sync {
    async fn can_awaken(&self) -> Result<bool>;
    async fn get_awaken(&self) -> Result<bool>;
    async fn set_awaken(&self, _awaken: bool) -> Result<()>;
    async fn get_phys_addr(&self) -> Result<PhysicalAddress>;
    async fn set_phys_addr(&self, phys_addr: PhysicalAddress) -> Result<()>;
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
    pub vendor_id: Option<VendorId>,
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
            (device_match.friendly_name.clone(), device_match.oui)
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

pub(crate) async fn cec_hw_controller() -> Result<Box<dyn HdmiCecHwController>> {
    let config = device_config().await?;
    let Some(config) = config
        .as_ref()
        .and_then(|config| config.cec_hw.as_ref())
        .and_then(|config| config.hardware.as_ref())
    else {
        bail!("HDMI CEC hardware not configured");
    };
    let hw = match config {
        HdmiCecHardware::CrosEc { port } => Box::new(CrosEcHwController { port: *port }),
    };
    if !hw.can_awaken().await? {
        bail!("HDMI CEC hardware not supported");
    }
    Ok(hw)
}

struct CrosEcHwController {
    port: u8,
}

impl CrosEcHwController {
    const BASE: &str = "/sys/class/chromeos/cros_ec/";
    const WAKE_ENABLE: &str = "cec_wake_enable";
    const PHYS_ADDR: &str = "cec_phys_addr";
}

#[async_trait]
impl HdmiCecHwController for CrosEcHwController {
    async fn can_awaken(&self) -> Result<bool> {
        let wake_enable = BufReader::new(
            File::open(path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE))
                .await?,
        );
        let mut lines = wake_enable.lines();
        while let Some(line) = lines.next_line().await? {
            let Some((port, _)) = line.split_once(' ') else {
                continue;
            };
            let Ok(port) = port.parse::<u8>() else {
                continue;
            };
            if port != self.port {
                continue;
            }
            return Ok(true);
        }
        Ok(false)
    }

    async fn get_awaken(&self) -> Result<bool> {
        let wake_enable = BufReader::new(
            File::open(path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE))
                .await?,
        );
        let mut lines = wake_enable.lines();
        while let Some(line) = lines.next_line().await? {
            let Some((port, enable)) = line.split_once(' ') else {
                continue;
            };
            let Ok(port) = port.parse::<u8>() else {
                continue;
            };
            if port != self.port {
                continue;
            }
            return Ok(enable == "1");
        }
        bail!("Port not found");
    }

    async fn set_awaken(&self, awaken: bool) -> Result<()> {
        let line = format!("{} {}\n", self.port, if awaken { 1 } else { 0 });
        Ok(write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE),
            line,
        )
        .await?)
    }

    async fn get_phys_addr(&self) -> Result<PhysicalAddress> {
        let wake_enable = BufReader::new(
            File::open(path(CrosEcHwController::BASE).join(CrosEcHwController::PHYS_ADDR)).await?,
        );
        let mut lines = wake_enable.lines();
        while let Some(line) = lines.next_line().await? {
            let Some((port, phys_addr)) = line.split_once(' ') else {
                continue;
            };
            let Ok(port) = port.parse::<u8>() else {
                continue;
            };
            if port != self.port {
                continue;
            }
            let phys_addr = phys_addr.parse::<u16>()?;
            return Ok(PhysicalAddress::from(phys_addr));
        }
        bail!("Port not found");
    }

    async fn set_phys_addr(&self, phys_addr: PhysicalAddress) -> Result<()> {
        let line = format!("{} {}\n", self.port, u16::from(phys_addr));
        Ok(write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::PHYS_ADDR),
            line,
        )
        .await?)
    }
}

pub(crate) struct CecdService {
    manager: RootManagerProxy<'static>,
    device: CecDevice1Proxy<'static>,
}

impl CecdService {
    pub(crate) async fn new(
        system: &Connection,
        hdmi_cec: &HdmiCecControl<'_>,
    ) -> Result<CecdService> {
        let manager = RootManagerProxy::new(system).await?;
        ensure!(
            manager.hdmi_cec_can_awaken().await?,
            "No supported cec hardware backend found"
        );
        ensure!(
            matches!(&hdmi_cec.backend, HdmiCecBackend::Cecd(_)),
            "Not using cecd"
        );
        let object_manager = ObjectManagerProxy::new(
            &hdmi_cec.connection,
            "com.steampowered.CecDaemon1",
            "/com/steampowered/CecDaemon1",
        )
        .await?;

        let mut device = None;
        for (path, ifaces) in object_manager.get_managed_objects().await? {
            if !path
                .as_str()
                .starts_with("/com/steampowered/CecDaemon1/Devices")
            {
                continue;
            }
            if !ifaces.contains_key("com.steampowered.CecDaemon1.CecDevice1") {
                continue;
            }
            device = Some(
                CecDevice1Proxy::builder(&hdmi_cec.connection)
                    .path(path)?
                    .build()
                    .await?,
            );
        }

        let Some(device) = device else {
            bail!("No CEC device found");
        };

        Ok(CecdService { manager, device })
    }

    async fn reconfigure(&self) -> Result<()> {
        let phys_addr = self.device.physical_address().await?;
        self.manager.set_hdmi_cec_phys_addr(phys_addr).await?;
        Ok(())
    }
}

impl Service for CecdService {
    const NAME: &'static str = "cecd-listener";

    async fn run(&mut self) -> Result<()> {
        let mut phys_addr_changed = self.device.receive_physical_address_changed().await;
        loop {
            self.reconfigure().await?;

            let Some(_) = phys_addr_changed.next().await else {
                break;
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::collections::HashMap;
    use tokio::fs::{read_to_string, try_exists};
    use zbus::fdo::{self, ObjectManager};
    use zbus::{ObjectServer, interface};

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

    #[tokio::test]
    async fn test_cros_get_awaken() {
        let _h = testing::start();

        create_dir_all(path(CrosEcHwController::BASE))
            .await
            .unwrap();
        let cros_ec = CrosEcHwController { port: 0 };

        write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE),
            "0 1\n",
        )
        .await
        .unwrap();
        assert!(cros_ec.get_awaken().await.unwrap());

        write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE),
            "0 0\n",
        )
        .await
        .unwrap();
        assert!(!cros_ec.get_awaken().await.unwrap());
    }

    #[tokio::test]
    async fn test_cros_get_awaken_no_port() {
        let _h = testing::start();

        create_dir_all(path(CrosEcHwController::BASE))
            .await
            .unwrap();
        let cros_ec = CrosEcHwController { port: 1 };

        write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE),
            "0 1\n",
        )
        .await
        .unwrap();
        assert!(cros_ec.get_awaken().await.is_err());
    }

    #[tokio::test]
    async fn test_cros_set_awaken() {
        let _h = testing::start();

        create_dir_all(path(CrosEcHwController::BASE))
            .await
            .unwrap();
        let cros_ec = CrosEcHwController { port: 0 };

        cros_ec.set_awaken(true).await.unwrap();
        assert_eq!(
            read_to_string(path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE))
                .await
                .unwrap(),
            "0 1\n"
        );

        cros_ec.set_awaken(false).await.unwrap();
        assert_eq!(
            read_to_string(path(CrosEcHwController::BASE).join(CrosEcHwController::WAKE_ENABLE))
                .await
                .unwrap(),
            "0 0\n"
        );
    }

    #[tokio::test]
    async fn test_cros_get_phys_addr() {
        let _h = testing::start();

        create_dir_all(path(CrosEcHwController::BASE))
            .await
            .unwrap();
        let cros_ec = CrosEcHwController { port: 0 };

        write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::PHYS_ADDR),
            "0 4660\n",
        )
        .await
        .unwrap();
        assert_eq!(
            cros_ec.get_phys_addr().await.unwrap(),
            PhysicalAddress::from(0x1234)
        );
    }

    #[tokio::test]
    async fn test_cros_get_phys_addr_no_port() {
        let _h = testing::start();

        create_dir_all(path(CrosEcHwController::BASE))
            .await
            .unwrap();
        let cros_ec = CrosEcHwController { port: 1 };

        write(
            path(CrosEcHwController::BASE).join(CrosEcHwController::PHYS_ADDR),
            "0 4660\n",
        )
        .await
        .unwrap();
        assert!(cros_ec.get_phys_addr().await.is_err());
    }

    #[tokio::test]
    async fn test_cros_set_phys_addr() {
        let _h = testing::start();

        create_dir_all(path(CrosEcHwController::BASE))
            .await
            .unwrap();
        let cros_ec = CrosEcHwController { port: 0 };

        cros_ec
            .set_phys_addr(PhysicalAddress::from(0x1234))
            .await
            .unwrap();
        assert_eq!(
            read_to_string(path(CrosEcHwController::BASE).join(CrosEcHwController::PHYS_ADDR))
                .await
                .unwrap(),
            "0 4660\n"
        );
    }

    #[derive(Debug)]
    struct MockCecHw {
        can_awaken: bool,
        phys_addr: u16,
        awaken: bool,
    }

    #[interface(name = "com.steampowered.SteamOSManager1.RootManager")]
    impl MockCecHw {
        #[zbus(property)]
        fn hdmi_cec_can_awaken(&self) -> bool {
            self.can_awaken
        }

        #[zbus(property)]
        fn hdmi_cec_awaken(&self) -> fdo::Result<bool> {
            if !self.can_awaken {
                return Err(fdo::Error::Failed(String::new()));
            }
            Ok(self.awaken)
        }

        #[zbus(property)]
        fn set_hdmi_cec_awaken(&mut self, awaken: bool) -> fdo::Result<()> {
            if !self.can_awaken {
                return Err(fdo::Error::Failed(String::new()));
            }
            self.awaken = awaken;
            Ok(())
        }

        #[zbus(property)]
        fn hdmi_cec_phys_addr(&self) -> fdo::Result<u16> {
            if !self.can_awaken {
                return Err(fdo::Error::Failed(String::new()));
            }
            Ok(self.phys_addr)
        }

        #[zbus(property)]
        fn set_hdmi_cec_phys_addr(&mut self, phys_addr: u16) -> fdo::Result<()> {
            if !self.can_awaken {
                return Err(fdo::Error::Failed(String::new()));
            }
            self.phys_addr = phys_addr;
            Ok(())
        }
    }

    struct MockCecdConfig;

    #[interface(name = "com.steampowered.CecDaemon1.Config1")]
    impl MockCecdConfig {}

    struct MockCecdDevice {
        phys_addr: u16,
    }

    #[interface(name = "com.steampowered.CecDaemon1.CecDevice1")]
    impl MockCecdDevice {
        #[zbus(property)]
        fn physical_address(&self) -> u16 {
            self.phys_addr
        }
    }

    struct CecHwTest {
        _h: testing::TestHandle,
        object_server: ObjectServer,
        connection: Connection,
    }

    async fn setup_cec_hw_test(config: MockCecHw) -> Result<CecHwTest> {
        let mut h = testing::start();
        let connection = h.new_dbus().await.unwrap();
        let object_server = connection.object_server().clone();

        object_server
            .at("/com/steampowered/SteamOSManager1", config)
            .await?;
        object_server
            .at("/com/steampowered/CecDaemon1/Daemon", MockCecdConfig {})
            .await?;
        object_server
            .at("/com/steampowered/CecDaemon1", ObjectManager {})
            .await?;
        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await?;
        connection
            .request_name("com.steampowered.CecDaemon1")
            .await?;

        let connection = h.new_connection().await?;

        Ok(CecHwTest {
            _h: h,
            object_server,
            connection,
        })
    }

    #[tokio::test]
    async fn test_cecd_service_no_hw() {
        let test = setup_cec_hw_test(MockCecHw {
            can_awaken: false,
            phys_addr: 0xFFFF,
            awaken: false,
        })
        .await
        .unwrap();

        let backend = HdmiCecBackend::Cecd(Config1Proxy::new(&test.connection).await.unwrap());
        let service = CecdService::new(
            &test.connection,
            &HdmiCecControl {
                connection: test.connection.clone(),
                backend,
            },
        )
        .await;
        assert!(service.is_err());
    }

    #[tokio::test]
    async fn test_cecd_service_no_device() {
        let test = setup_cec_hw_test(MockCecHw {
            can_awaken: true,
            phys_addr: 0xFFFF,
            awaken: false,
        })
        .await
        .unwrap();

        let backend = HdmiCecBackend::Cecd(Config1Proxy::new(&test.connection).await.unwrap());
        let service = CecdService::new(
            &test.connection,
            &HdmiCecControl {
                connection: test.connection.clone(),
                backend,
            },
        )
        .await;
        assert!(service.is_err());
    }

    #[tokio::test]
    async fn test_cecd_service_device() {
        let test = setup_cec_hw_test(MockCecHw {
            can_awaken: true,
            phys_addr: 0xFFFF,
            awaken: false,
        })
        .await
        .unwrap();

        test.object_server
            .at(
                "/com/steampowered/CecDaemon1/Devices/Cec0",
                MockCecdDevice { phys_addr: 0x1234 },
            )
            .await
            .unwrap();

        let backend = HdmiCecBackend::Cecd(Config1Proxy::new(&test.connection).await.unwrap());
        let service = CecdService::new(
            &test.connection,
            &HdmiCecControl {
                connection: test.connection.clone(),
                backend,
            },
        )
        .await;
        service.unwrap();
    }

    #[tokio::test]
    async fn test_cecd_service_reconfigure() {
        let test = setup_cec_hw_test(MockCecHw {
            can_awaken: true,
            phys_addr: 0xFFFF,
            awaken: false,
        })
        .await
        .unwrap();

        test.object_server
            .at(
                "/com/steampowered/CecDaemon1/Devices/Cec0",
                MockCecdDevice { phys_addr: 0x1234 },
            )
            .await
            .unwrap();

        let backend = HdmiCecBackend::Cecd(Config1Proxy::new(&test.connection).await.unwrap());
        let service = CecdService::new(
            &test.connection,
            &HdmiCecControl {
                connection: test.connection.clone(),
                backend,
            },
        )
        .await
        .unwrap();

        let cec_hw = test
            .object_server
            .interface::<_, MockCecHw>("/com/steampowered/SteamOSManager1")
            .await
            .unwrap();

        assert_eq!(cec_hw.get().await.phys_addr, 0xFFFF);
        service.reconfigure().await.unwrap();
        assert_eq!(cec_hw.get().await.phys_addr, 0x1234);
    }
}
