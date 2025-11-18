/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

#[cfg(not(test))]
use anyhow::anyhow;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, Sender};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::subscriber::set_global_default;
use tracing::{error, info};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter, Registry};
#[cfg(not(test))]
use xdg::BaseDirectories;
use zbus::connection::{Builder, Connection};
use zbus::fdo::PeerProxy;

use crate::daemon::{channel, Daemon, DaemonCommand, DaemonContext};
use crate::job::{JobManager, JobManagerService};
use crate::manager::user::create_interfaces;
use crate::path;
use crate::power::TdpManagerService;
use crate::session::SessionManagerState;
use crate::udev::UdevMonitor;

#[derive(Copy, Clone, Default, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct UserConfig {
    pub services: UserServicesConfig,
}

#[derive(Copy, Clone, Default, Deserialize, Debug)]
pub(crate) struct UserServicesConfig {}

#[derive(Clone, Default, Deserialize, Serialize, Debug)]
#[serde(default)]
pub(crate) struct UserState {
    pub services: UserServicesState,
    pub session_manager: SessionManagerState,
}

#[derive(Clone, Default, Deserialize, Serialize, Debug)]
pub(crate) struct UserServicesState {}

#[derive(Debug)]
pub(crate) enum UserCommand {
    SetSessionManagerState(SessionManagerState),
    GetSessionManagerState(oneshot::Sender<SessionManagerState>),
}

pub(crate) struct UserContext {
    session: Connection,
    state: UserState,
    channel: Sender<Command>,
}

impl DaemonContext for UserContext {
    type State = UserState;
    type Config = UserConfig;
    type Command = UserCommand;

    #[cfg(not(test))]
    fn user_config_path(&self) -> Result<PathBuf> {
        let xdg_base = BaseDirectories::new();
        xdg_base
            .get_config_file("steamos-manager")
            .ok_or(anyhow!("No config directory found"))
    }

    #[cfg(test)]
    fn user_config_path(&self) -> Result<PathBuf> {
        Ok(path("steamos-manager"))
    }

    fn system_config_path(&self) -> Result<PathBuf> {
        Ok(path("/usr/share/steamos-manager/user.d"))
    }

    fn state(&self) -> &UserState {
        &self.state
    }

    async fn start(
        &mut self,
        state: UserState,
        _config: UserConfig,
        daemon: &mut Daemon<UserContext>,
    ) -> Result<()> {
        self.state = state;

        let udev = UdevMonitor::init(&self.session).await?;
        daemon.add_service(udev);

        Ok(())
    }

    async fn reload(
        &mut self,
        _config: UserConfig,
        _daemon: &mut Daemon<UserContext>,
    ) -> Result<()> {
        // Nothing to do yet
        Ok(())
    }

    async fn handle_command(
        &mut self,
        cmd: Self::Command,
        _daemon: &mut Daemon<UserContext>,
    ) -> Result<()> {
        match cmd {
            UserCommand::SetSessionManagerState(state) => {
                self.state.session_manager = state;
                self.channel.send(DaemonCommand::WriteState).await?;
            }
            UserCommand::GetSessionManagerState(sender) => {
                let _ = sender.send(self.state.session_manager.clone());
            }
        }
        Ok(())
    }
}

pub(crate) type Command = DaemonCommand<UserCommand>;

async fn create_connections() -> Result<(Connection, Connection)> {
    let system = Connection::system().await?;
    // Ensure the root daemon is running, or start it if it's not
    for _ in 0..5 {
        let peer = PeerProxy::new(&system, "com.steampowered.SteamOSManager1", "/").await?;
        tokio::select! {
            r = peer.ping() => {
                if let Err(e) = r {
                    error!("Couldn't ping root daemon: {e}");
                } else {
                    break;
                }
            },
            _ = sleep(Duration::from_secs(1)) => (),
        }
    }
    let connection = Builder::session()?
        .name("com.steampowered.SteamOSManager1")?
        .build()
        .await?;

    Ok((connection, system))
}

pub async fn daemon() -> Result<()> {
    // This daemon is responsible for creating a dbus api that steam client can use to do various OS
    // level things. It implements com.steampowered.SteamOSManager1.Manager interface

    let stdout_log = fmt::layer();
    let subscriber = Registry::default()
        .with(stdout_log)
        .with(EnvFilter::from_default_env());
    set_global_default(subscriber)?;
    let (tx, rx) = channel::<UserContext>();

    let (session, system) = match create_connections().await {
        Ok(c) => c,
        Err(e) => {
            error!("Error connecting to DBus: {}", e);
            bail!(e);
        }
    };

    let (jm_tx, jm_rx) = unbounded_channel();
    let job_manager = JobManager::new(session.clone()).await?;
    let jm_service = JobManagerService::new(job_manager, jm_rx, system.clone());

    let (tdp_tx, tdp_rx) = unbounded_channel();
    let tdp_service = TdpManagerService::new(tdp_rx, &system, &session).await;
    let tdp_tx = if tdp_service.is_ok() {
        Some(tdp_tx)
    } else {
        None
    };

    let (signal_relay_service, session_manager_service, screenreader_setup_service) =
        create_interfaces(session.clone(), system.clone(), tx.clone(), jm_tx, tdp_tx).await?;

    let mut daemon = Daemon::new(session.clone(), rx).await?;
    let context = UserContext {
        session,
        state: UserState::default(),
        channel: tx,
    };

    daemon.add_service(signal_relay_service);
    if let Some(service) = screenreader_setup_service {
        daemon.add_service(service);
    }
    if let Some(service) = session_manager_service {
        daemon.add_service(service);
    }
    daemon.add_service(jm_service);
    if let Ok(tdp_service) = tdp_service {
        daemon.add_service(tdp_service);
    } else if let Err(e) = tdp_service {
        info!("TdpManagerService not available: {e}");
    }

    daemon.run(context).await
}
