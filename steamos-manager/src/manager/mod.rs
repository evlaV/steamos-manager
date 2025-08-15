/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use serde::Deserialize;
use zbus::names::OwnedWellKnownName;
use zbus::object_server::Interface;
use zbus::zvariant::OwnedObjectPath;

pub(crate) mod root;
pub(crate) mod user;

pub(crate) trait RemoteInterface {
    type Remote: Interface;
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct RemoteInterfaceConfig {
    bus_name: OwnedWellKnownName,
    object_path: OwnedObjectPath,
}
