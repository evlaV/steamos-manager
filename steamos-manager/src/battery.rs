/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2026 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow, bail, ensure};
use nix::errno::Errno;
use serde::Deserialize;
use std::path::PathBuf;
use strum::{Display, EnumString, VariantNames};
use tokio::fs::{read_dir, read_to_string, try_exists};
use tokio::io::ErrorKind;
use tokio::sync::oneshot;

use crate::hardware::device_config;
use crate::path;
use crate::power::find_hwmon;
use crate::sysfs::{SysfsWritten, parse_sysfs_choice, sysfs_queued_write};

#[cfg(not(test))]
const SB_PATHS: &[&str] = &[
    "/sys/bus/platform/drivers/acpi-battery/PNP0C0A:00/firmware_node/power_supply",
    "/sys/bus/acpi/drivers/battery/PNP0C0A:00/power_supply",
];
#[cfg(test)]
const SB_PATHS: &[&str] = &["power_supply", "power_supply_legacy"];
pub const BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT: i32 = 10;
const SB_LIMIT_PATH: &str = "charge_control_end_threshold";

#[derive(Deserialize, Display, EnumString, VariantNames, PartialEq, Debug, Clone, Default)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub(crate) enum BatteryChargeLimitMethod {
    #[default]
    AcpiSb,
    HwmonAttribute {
        hwmon: String,
        attribute: String,
    },
}

async fn find_acpi_sb_battery() -> Result<PathBuf> {
    for base in SB_PATHS {
        let mut dir = match read_dir(path(base)).await {
            Ok(dir) => dir,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = dir.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let path = entry.path();
            match read_to_string(path.join("type")).await {
                Ok(s) if s.trim() == "Battery" => (),
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => continue,
            };
            return Ok(path);
        }
    }
    bail!("Battery not found");
}

async fn find_charge_limit_path() -> Result<PathBuf> {
    let config = device_config().await?;
    let config = config
        .as_ref()
        .and_then(|config| config.battery_charge_limit.as_ref())
        .ok_or(anyhow!("No battery charge limit configured"))?;

    match &config.method {
        BatteryChargeLimitMethod::AcpiSb => {
            let path = find_acpi_sb_battery().await?.join(SB_LIMIT_PATH);
            if try_exists(&path).await? {
                return Ok(path);
            }
        }
        BatteryChargeLimitMethod::HwmonAttribute { hwmon, attribute } => {
            let base = find_hwmon(hwmon.as_str()).await?;
            let path = base.join(attribute);
            if try_exists(&path).await? {
                return Ok(path);
            }
        }
    }
    bail!("Battery not found");
}

async fn find_charge_type_path() -> Result<PathBuf> {
    let path = find_acpi_sb_battery().await?.join("charge_types");
    if !try_exists(&path).await? {
        bail!("No charge types found");
    }
    Ok(path)
}

fn parse_max_charge_level(result: std::io::Result<String>) -> Result<i32> {
    match result {
        Ok(s) => s
            .trim()
            .parse()
            .map_err(|e| anyhow!("Error parsing value: {e}")),
        // asus-wmi may return ENODATA until userspace writes a value
        Err(e) if e.raw_os_error().map(Errno::from_raw) == Some(Errno::ENODATA) => Ok(-1),
        Err(e) => Err(anyhow!("Error reading sysfs: {e}")),
    }
}

pub(crate) async fn get_max_charge_level() -> Result<i32> {
    let path = find_charge_limit_path().await?;
    parse_max_charge_level(read_to_string(path).await)
}

pub(crate) async fn set_max_charge_level(limit: i32) -> Result<oneshot::Receiver<SysfsWritten>> {
    ensure!((0..=100).contains(&limit), "Invalid limit");
    let data = limit.to_string();
    let path = find_charge_limit_path().await?;
    sysfs_queued_write(path, data.as_bytes().to_owned()).await
}

pub(crate) async fn available_charge_types() -> Result<Vec<String>> {
    let path = find_charge_type_path().await?;
    Ok(parse_sysfs_choice(path).await?.0)
}

pub(crate) async fn get_active_charge_type() -> Result<String> {
    let path = find_charge_type_path().await?;
    parse_sysfs_choice(path)
        .await?
        .1
        .ok_or(anyhow!("No charging type found"))
}

pub(crate) async fn set_charge_type(ty: &str) -> Result<oneshot::Receiver<SysfsWritten>> {
    ensure!(
        available_charge_types()
            .await?
            .iter()
            .any(|avail| avail == ty),
        "Invalid charge type"
    );
    let path = find_charge_type_path().await?;
    sysfs_queued_write(path, ty.as_bytes().to_owned()).await
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::hardware::{BatteryChargeLimitConfig, DeviceConfig};
    use crate::power::HWMON_PREFIX;
    use crate::testing;
    use tokio::fs::{create_dir_all, write};

    pub async fn create_nodes() -> Result<()> {
        let path = path(SB_PATHS[0]).join("BAT1");
        create_dir_all(&path).await?;
        write(path.join("type"), b"Battery\n").await?;
        write(path.join("charge_types"), b"Standard [Long_Life]\n").await?;
        Ok(())
    }

    #[tokio::test]
    async fn read_max_charge_level_acpi_sb() {
        let handle = testing::start();

        let config = DeviceConfig {
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::AcpiSb,
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;

        let base = path(SB_PATHS[0]).join("BAT1");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("type"), "Battery\n").await.expect("write");

        write(base.join(SB_LIMIT_PATH), "10\n")
            .await
            .expect("write");

        assert_eq!(
            find_charge_limit_path().await.unwrap(),
            path(SB_PATHS[0]).join("BAT1/charge_control_end_threshold")
        );

        assert_eq!(get_max_charge_level().await.unwrap(), 10);

        write(base.join(SB_LIMIT_PATH), "99\n")
            .await
            .expect("write");

        assert_eq!(get_max_charge_level().await.unwrap(), 99);

        assert!(set_max_charge_level(101).await.is_err());
        assert!(set_max_charge_level(-1).await.is_err());
    }

    #[tokio::test]
    async fn read_max_charge_level_acpi_sb_legacy_path() {
        let handle = testing::start();

        let config = DeviceConfig {
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::AcpiSb,
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;

        let base = path(SB_PATHS[1]).join("BAT1");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("type"), "Battery\n").await.expect("write");

        write(base.join(SB_LIMIT_PATH), "80\n")
            .await
            .expect("write");

        assert_eq!(
            find_charge_limit_path().await.unwrap(),
            path(SB_PATHS[1]).join("BAT1/charge_control_end_threshold")
        );

        assert_eq!(get_max_charge_level().await.unwrap(), 80);
    }

    #[test]
    fn parse_max_charge_level_enodata_is_unknown() {
        let err = std::io::Error::from_raw_os_error(Errno::ENODATA as i32);
        assert_eq!(parse_max_charge_level(Err(err)).unwrap(), -1);
    }

    #[tokio::test]
    async fn read_max_charge_level_hwmmon() {
        let handle = testing::start();

        let config = DeviceConfig {
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::HwmonAttribute {
                    hwmon: String::from("steamdeck_hwmon"),
                    attribute: String::from("max_battery_charge_level"),
                },
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;

        let base = path(HWMON_PREFIX).join("hwmon6");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("name"), "steamdeck_hwmon\n")
            .await
            .expect("write");

        write(base.join("max_battery_charge_level"), "10\n")
            .await
            .expect("write");

        assert_eq!(
            find_charge_limit_path().await.unwrap(),
            path(HWMON_PREFIX).join("hwmon6/max_battery_charge_level")
        );

        assert_eq!(get_max_charge_level().await.unwrap(), 10);

        write(base.join("max_battery_charge_level"), "99\n")
            .await
            .expect("write");

        assert_eq!(get_max_charge_level().await.unwrap(), 99);

        assert!(set_max_charge_level(101).await.is_err());
        assert!(set_max_charge_level(-1).await.is_err());
    }

    #[tokio::test]
    async fn read_available_charge_types() {
        let _handle = testing::start();

        create_nodes().await.unwrap();
        assert_eq!(
            available_charge_types().await.unwrap(),
            &["Standard", "Long_Life"]
        );
    }

    #[tokio::test]
    async fn read_charge_type() {
        let _handle = testing::start();

        create_nodes().await.unwrap();
        assert_eq!(get_active_charge_type().await.unwrap(), "Long_Life");
    }

    #[tokio::test]
    async fn set_invalid_charge_type() {
        let _handle = testing::start();

        create_nodes().await.unwrap();
        assert!(set_charge_type("Nothing").await.is_err());
    }
}
