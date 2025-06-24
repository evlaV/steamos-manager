/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{ensure, Result};
use clap::Parser;
use tokio::fs::read_link;
use zbus::fdo::DBusProxy;
use zbus::Connection;

use steamos_manager::daemon;

#[derive(Parser)]
struct Args {
    /// Run the root manager daemon
    #[arg(short, long)]
    root: bool,

    #[arg(long, exclusive(true), hide(true))]
    validate_bus_owner: Option<u32>,
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(pid) = args.validate_bus_owner {
        let connection = Connection::session().await?;
        let dbus = DBusProxy::new(&connection).await?;
        ensure!(
            dbus.get_connection_unix_process_id("com.steampowered.SteamOSManager1".try_into()?)
                .await?
                == pid,
            "Given pid does not match bus name"
        );
        let their_exe = read_link(format!("/proc/{pid}/exe")).await?;
        let our_exe = read_link(format!("/proc/self/exe")).await?;
        ensure!(
            their_exe == our_exe,
            "Bus name is not owned by steamos-manager"
        );
        return Ok(());
    }

    if args.root {
        daemon::root().await
    } else {
        daemon::user().await
    }
}
