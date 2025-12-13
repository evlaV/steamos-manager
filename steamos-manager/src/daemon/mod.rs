/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow, ensure};
use nix::time::{ClockId, clock_gettime};
use serde::{Deserialize, Serialize};
use std::env;
use std::fmt::Debug;
use std::path::PathBuf;
use tokio::net::UnixDatagram;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use zbus::connection::Connection;
use zbus::fdo::ObjectManager;

use crate::Service;
use crate::daemon::config::{read_config, read_state, write_state};

mod config;
pub(crate) mod root;
pub(crate) mod user;

pub use root::daemon as root;
pub use user::daemon as user;

pub(crate) trait DaemonContext: Sized {
    type State: for<'a> Deserialize<'a> + Serialize + Default + Debug;
    type Config: for<'a> Deserialize<'a> + Default + Debug;
    type Command: Send + Debug;

    fn state_path(&self) -> Result<PathBuf> {
        let config_path = self.user_config_path()?;
        Ok(config_path.join("state.toml"))
    }

    fn user_config_path(&self) -> Result<PathBuf>;
    fn system_config_path(&self) -> Result<PathBuf>;
    fn state(&self) -> &Self::State;

    async fn start(
        &mut self,
        state: Self::State,
        config: Self::Config,
        daemon: &mut Daemon<Self>,
    ) -> Result<()>;

    async fn reload(&mut self, config: Self::Config, daemon: &mut Daemon<Self>) -> Result<()>;

    async fn handle_command(&mut self, cmd: Self::Command, daemon: &mut Daemon<Self>)
    -> Result<()>;
}

pub(crate) struct Daemon<C: DaemonContext> {
    services: JoinSet<Result<()>>,
    token: CancellationToken,
    connection: Connection,
    channel: Receiver<DaemonCommand<C::Command>>,
    notify_socket: NotifySocket,
}

#[derive(Debug)]
pub(crate) enum DaemonCommand<T: Debug> {
    ContextCommand(T),
    ReadConfig,
    WriteState,
}

#[derive(Debug, Default)]
struct NotifySocket {
    socket: Option<UnixDatagram>,
}

impl NotifySocket {
    async fn setup_socket(&mut self) -> Result<()> {
        if self.socket.is_some() {
            return Ok(());
        }
        let Some(notify_socket) = env::var_os("NOTIFY_SOCKET") else {
            return Ok(());
        };
        let socket = UnixDatagram::unbound()?;
        socket.connect(notify_socket)?;
        self.socket = Some(socket);
        Ok(())
    }

    async fn notify(&mut self, message: &str) {
        if let Err(e) = self.setup_socket().await {
            warn!("Couldn't set up systemd notify socket: {e}");
            return;
        }
        let Some(ref socket) = self.socket else {
            return;
        };
        trace!("Sending message to systemd: {message}");
        if let Err(e) = socket.send(message.as_bytes()).await {
            warn!("Couldn't notify systemd: {e}");
        }
    }
}

impl<C: DaemonContext> Daemon<C> {
    pub(crate) async fn new(
        connection: Connection,
        channel: Receiver<DaemonCommand<C::Command>>,
    ) -> Result<Daemon<C>> {
        let services = JoinSet::new();
        let token = CancellationToken::new();

        let daemon = Daemon {
            services,
            token,
            connection,
            channel,
            notify_socket: NotifySocket::default(),
        };

        Ok(daemon)
    }

    pub(crate) fn add_service<S: Service + 'static>(&mut self, service: S) -> CancellationToken {
        let token = self.token.child_token();
        let moved_token = token.clone();
        self.services
            .spawn(async move { service.start(moved_token).await });
        token
    }

    pub(crate) fn get_connection(&self) -> Connection {
        self.connection.clone()
    }

    pub(crate) async fn run(&mut self, mut context: C) -> Result<()> {
        ensure!(
            !self.services.is_empty(),
            "Can't run a daemon with no services attached."
        );

        let state = read_state(&context).await?;
        let config = read_config(&context).await?;
        debug!("Starting daemon with state: {state:#?}, config: {config:#?}");
        context.start(state, config, self).await?;

        let object_server = self.connection.object_server().clone();
        self.services.spawn(async move {
            object_server.at("/", ObjectManager {}).await?;

            // Tell systemd we're done loading
            let mut notify_socket = NotifySocket::default();
            notify_socket.notify("READY=1\n").await;
            Ok(())
        });

        let mut res = loop {
            let mut sigterm = signal(SignalKind::terminate())?;
            let mut sigquit = signal(SignalKind::quit())?;
            let mut sighup = signal(SignalKind::hangup())?;

            let res = tokio::select! {
                e = self.services.join_next() => match e.unwrap() {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.into())
                },
                _ = tokio::signal::ctrl_c() => {
                    info!("Got SIGINT, shutting down");
                    break Ok(());
                }
                e = sigterm.recv() => match e {
                    Some(()) => {
                        info!("Got SIGTERM, shutting down");
                        break Ok(());
                    }
                    None => Err(anyhow!("SIGTERM pipe broke")),
                },
                e = sighup.recv() => match e {
                    Some(()) => {
                        match clock_gettime(ClockId::CLOCK_MONOTONIC) {
                            Ok(timestamp) => {
                                let timestamp = timestamp.tv_sec() * 1_000_000 +
                                    timestamp.tv_nsec() / 1_000;
                                let notifies = format!("RELOADING=1\nMONOTONIC_USEC={timestamp}\n");
                                self.notify_socket.notify(notifies.as_str()).await;
                            }
                            Err(e) => warn!("Failed to notify systemd: {e}"),
                        }
                        let res = match read_config(&context).await {
                            Ok(config) =>
                                context.reload(config, self).await,
                            Err(error) => {
                                error!("Failed to load configuration: {error}");
                                Ok(())
                            }
                        };
                        self.notify_socket.notify("READY=1\n").await;
                        res
                    }
                    None => Err(anyhow!("SIGHUP pipe broke")),
                },
                msg = self.channel.recv() => match msg {
                    Some(msg) => {
                        self.handle_message(msg, &mut context).await
                    }
                    None => Err(anyhow!("All senders have been closed")),
                },
                _ = sigquit.recv() => Err(anyhow!("Got SIGQUIT")),
            }
            .inspect_err(|e| error!("Encountered error running: {e}"));
            if res.is_err() {
                break res;
            }
        };
        self.token.cancel();

        info!("Shutting down");

        while let Some(service_res) = self.services.join_next().await {
            res = match service_res {
                Ok(Err(e)) => Err(e),
                Err(e) => Err(e.into()),
                _ => continue,
            };
        }

        res.inspect_err(|e| error!("Encountered error: {e}"))
    }

    async fn handle_message(
        &mut self,
        cmd: DaemonCommand<C::Command>,
        context: &mut C,
    ) -> Result<()> {
        match cmd {
            DaemonCommand::ContextCommand(cmd) => context.handle_command(cmd, self).await,
            DaemonCommand::ReadConfig => match read_config(context).await {
                Ok(config) => context.reload(config, self).await,
                Err(error) => {
                    error!("Failed to load configuration: {error}");
                    Ok(())
                }
            },
            DaemonCommand::WriteState => write_state(context).await,
        }
    }
}

// Rust doesn't support a good way to simplify this type yet
// See <https://github.com/rust-lang/rust/issues/8995>
#[allow(clippy::type_complexity)]
pub(crate) fn channel<C: DaemonContext>() -> (
    Sender<DaemonCommand<C::Command>>,
    Receiver<DaemonCommand<C::Command>>,
) {
    mpsc::channel(10)
}
