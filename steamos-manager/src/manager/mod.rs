/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use async_trait::async_trait;
use serde::Deserialize;
use zbus::names::{OwnedBusName, OwnedWellKnownName};
use zbus::object_server::Interface;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, ObjectServer, fdo};

pub(crate) mod root;
pub(crate) mod user;

pub(crate) const MANAGER_PATH: &str = "/com/steampowered/SteamOSManager1";

pub(crate) trait RemoteOwner: Sized {
    type Context: Send + Sync;

    async fn new(
        destination: OwnedBusName,
        path: OwnedObjectPath,
        session: &Connection,
        system: &Connection,
        is_transient: bool,
        context: Self::Context,
    ) -> fdo::Result<Self>;
}

#[async_trait]
pub(crate) trait RemoteInterface {
    type Remote: Interface;
    type Owner: RemoteOwner;

    async fn unregister(
        _context: Option<&mut <<Self as RemoteInterface>::Owner as RemoteOwner>::Context>,
        object_server: &ObjectServer,
    ) -> zbus::Result<()> {
        object_server
            .remove::<Self::Remote, _>(MANAGER_PATH)
            .await
            .map(|_| ())
    }
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct RemoteInterfaceConfig {
    bus_name: OwnedWellKnownName,
    object_path: OwnedObjectPath,
}
