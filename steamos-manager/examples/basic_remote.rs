/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */
use anyhow::Result;
use zbus::connection::Builder;
use zbus::interface;

/* Place the following text in a file called
 * /usr/share/steamos-manager/remotes.d/basic_remote.toml
 * for this example:

[BatteryChargeLimit1]
bus_name = "com.steampowered.ExampleRemote"
object_path = "/com/steampowered/ExampleRemote"

*/

struct BatteryChargeLimit1 {
    limit: i32,
}

#[interface(name = "com.steampowered.SteamOSManager1.BatteryChargeLimit1")]
impl BatteryChargeLimit1 {
    #[zbus(property)]
    async fn max_charge_level(&self) -> i32 {
        println!("max_charge_level: {}", self.limit);
        self.limit
    }

    #[zbus(property)]
    async fn set_max_charge_level(&mut self, limit: i32) {
        println!("set_max_charge_level: {limit}");
        self.limit = limit;
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn suggested_minimum_limit(&self) -> i32 {
        10
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _connection = Builder::system()?
        .name("com.steampowered.ExampleRemote")?
        .serve_at(
            "/com/steampowered/ExampleRemote",
            BatteryChargeLimit1 { limit: 10 },
        )?
        .build()
        .await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
