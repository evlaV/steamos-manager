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
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use tokio::sync::mpsc::{unbounded_channel, Sender};
use tokio::sync::oneshot;
use tracing::subscriber::set_global_default;
use tracing::{error, info};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter, Registry};
#[cfg(not(test))]
use xdg::BaseDirectories;
use zbus::connection::{Builder, Connection};
use zbus::AuthMechanism;

use crate::daemon::{channel, Daemon, DaemonCommand, DaemonContext};
use crate::job::{JobManager, JobManagerService};
use crate::manager::root::HandleContextProxy;
use crate::manager::user::{create_interfaces, SignalRelayService};
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

async fn create_connections(
    channel: Sender<Command>,
) -> Result<(
    Connection,
    Connection,
    Connection,
    JobManagerService,
    Result<TdpManagerService>,
    SignalRelayService,
)> {
    let system = Connection::system().await?;
    let connection = Builder::session()?
        .name("com.steampowered.SteamOSManager1")?
        .build()
        .await?;

    let fd = HandleContextProxy::new(&system).await?.get_handle().await?;

    let stream = UnixStream::from(OwnedFd::from(fd));
    let stream = tokio::net::UnixStream::from_std(stream)?;

    let root = Builder::unix_stream(stream)
        .auth_mechanism(AuthMechanism::Anonymous)
        .build()
        .await?;

    let (jm_tx, rx) = unbounded_channel();
    let job_manager = JobManager::new(connection.clone()).await?;
    let jm_service = JobManagerService::new(job_manager, rx, root.clone());

    let (tdp_tx, rx) = unbounded_channel();
    let tdp_service = TdpManagerService::new(rx, &root, &connection).await;
    let tdp_tx = if tdp_service.is_ok() {
        Some(tdp_tx)
    } else {
        None
    };

    let signal_relay_service = create_interfaces(
        connection.clone(),
        system.clone(),
        root.clone(),
        channel,
        jm_tx,
        tdp_tx,
    )
    .await?;

    Ok((
        connection,
        system,
        root,
        jm_service,
        tdp_service,
        signal_relay_service,
    ))
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

    let (session, _system, _root, mirror_service, tdp_service, signal_relay_service) =
        match create_connections(tx.clone()).await {
            Ok(c) => c,
            Err(e) => {
                error!("Error connecting to DBus: {}", e);
                bail!(e);
            }
        };

    let mut daemon = Daemon::new(session.clone(), rx).await?;
    let context = UserContext {
        session,
        state: UserState::default(),
        channel: tx,
    };

    daemon.add_service(signal_relay_service);
    daemon.add_service(mirror_service);
    if let Ok(tdp_service) = tdp_service {
        daemon.add_service(tdp_service);
    } else if let Err(e) = tdp_service {
        info!("TdpManagerService not available: {e}");
    }

    daemon.run(context).await
}
