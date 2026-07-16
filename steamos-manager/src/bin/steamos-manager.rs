/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use steamos_manager::daemon;
use steamos_manager::hardware::DeviceConfig;

#[derive(Parser)]
struct Args {
    /// Run the root manager daemon
    #[arg(short, long)]
    root: bool,

    /// Force the device config. This bypasses DMI matching, so use at your own risk.
    #[arg(long)]
    device_config: Option<PathBuf>,
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(path) = args.device_config {
        DeviceConfig::load_from_path(&path).await?;
    }

    if args.root {
        daemon::root().await
    } else {
        daemon::user().await
    }
}
