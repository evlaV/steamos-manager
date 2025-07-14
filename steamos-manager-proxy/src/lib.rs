/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

#![allow(clippy::module_name_repetitions)]

// Re-export relevant proxies

// Deprecated interface
mod manager;
pub use crate::manager::ManagerProxy;

// Optional interfaces
mod ambient_light_sensor1;
mod audio1;
mod battery_charge_limit1;
mod cpu_boost1;
mod cpu_scaling1;
mod factory_reset1;
mod fan_control1;
mod gpu_performance_level1;
mod gpu_power_profile1;
mod hdmi_cec1;
mod low_power_mode1;
mod manager2;
mod performance_profile1;
mod screenreader0;
mod session_management1;
mod storage1;
mod tdp_limit1;
mod update_bios1;
mod update_dock1;
mod wifi_debug1;
mod wifi_debug_dump1;
mod wifi_power_management1;
pub use crate::ambient_light_sensor1::AmbientLightSensor1Proxy;
pub use crate::audio1::Audio1Proxy;
pub use crate::battery_charge_limit1::BatteryChargeLimit1Proxy;
pub use crate::cpu_boost1::CpuBoost1Proxy;
pub use crate::cpu_scaling1::CpuScaling1Proxy;
pub use crate::factory_reset1::FactoryReset1Proxy;
pub use crate::fan_control1::FanControl1Proxy;
pub use crate::gpu_performance_level1::GpuPerformanceLevel1Proxy;
pub use crate::gpu_power_profile1::GpuPowerProfile1Proxy;
pub use crate::hdmi_cec1::HdmiCec1Proxy;
pub use crate::low_power_mode1::LowPowerMode1Proxy;
pub use crate::manager2::Manager2Proxy;
pub use crate::performance_profile1::PerformanceProfile1Proxy;
pub use crate::screenreader0::ScreenReader0Proxy;
pub use crate::session_management1::SessionManagement1Proxy;
pub use crate::storage1::Storage1Proxy;
pub use crate::tdp_limit1::TdpLimit1Proxy;
pub use crate::update_bios1::UpdateBios1Proxy;
pub use crate::update_dock1::UpdateDock1Proxy;
pub use crate::wifi_debug1::WifiDebug1Proxy;
pub use crate::wifi_debug_dump1::WifiDebugDump1Proxy;
pub use crate::wifi_power_management1::WifiPowerManagement1Proxy;

// Sub-interfaces
mod job1;
mod job_manager1;
mod udev_events1;
pub use crate::job1::Job1Proxy;
pub use crate::job_manager1::JobManager1Proxy;
pub use crate::udev_events1::UdevEvents1Proxy;
