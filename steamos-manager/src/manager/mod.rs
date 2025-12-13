/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use serde::Deserialize;
use zbus::names::{BusName, OwnedWellKnownName};
use zbus::object_server::Interface;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{Connection, fdo};

pub(crate) mod root;
pub(crate) mod user;

pub(crate) trait RemoteOwner: Sized {
    async fn new<'a, 'b>(
        destination: &BusName<'a>,
        path: ObjectPath<'b>,
        session: &Connection,
        system: &Connection,
        is_transient: bool,
    ) -> fdo::Result<Self>;
}

pub(crate) trait RemoteInterface {
    type Remote: Interface;
    type Owner: RemoteOwner;
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct RemoteInterfaceConfig {
    bus_name: OwnedWellKnownName,
    object_path: OwnedObjectPath,
}
