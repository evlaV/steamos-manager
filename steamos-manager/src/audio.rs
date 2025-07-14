/*
 * Copyright © 2025 Collabora Ltd.
 * Copyright © 2025 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use num_enum::TryFromPrimitive;
use strum::{Display, EnumString};
use tracing::warn;

#[cfg(not(test))]
use crate::process::{run_script, script_output};
#[cfg(not(test))]
use serde_json::Value;
#[cfg(not(test))]
use tokio::fs::try_exists;

#[cfg(not(test))]
const MONO_KEY: &str = "node.features.audio.mono";

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[repr(u32)]
pub enum Mode {
    Mono,
    Stereo,
}

pub(crate) struct AudioManager {
    mode: Option<Mode>,
}

impl AudioManager {
    pub async fn new() -> AudioManager {
        let mut manager = AudioManager { mode: None };
        let _ = manager
            .load_values()
            .await
            .inspect_err(|e| warn!("Failed to load audio configuration: {e}"));
        manager
    }

    #[cfg(test)]
    pub async fn is_supported() -> Result<bool> {
        Ok(true)
    }

    #[cfg(not(test))]
    pub async fn is_supported() -> Result<bool> {
        if try_exists("/usr/bin/wpctl").await? {
            // wpctl exists, now check if node.features.audio.mono is patched in also
            let output = script_output("pw-dump", &["sm-settings"]).await?;
            let object: Value = serde_json::from_str(&output)?;
            // Now check for our key
            if let Some(array) = object.as_array() {
                for obj in array {
                    let Some(metadata) = obj.get("metadata").and_then(|m| m.as_array()) else {
                        continue;
                    };
                    for entry in metadata {
                        let Some(key) = entry.get("key").and_then(|k| k.as_str()) else {
                            continue;
                        };
                        if key == MONO_KEY {
                            return Ok(true);
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    pub fn mode(&self) -> Option<Mode> {
        self.mode
    }

    pub async fn set_mode(&mut self, mode: Mode) -> Result<()> {
        if self.mode == Some(mode) {
            return Ok(());
        }

        #[cfg(not(test))]
        {
            let mono = match mode {
                Mode::Mono => "true",
                Mode::Stereo => "false",
            };

            run_script("/usr/bin/wpctl", &["settings", MONO_KEY, mono]).await?;
        }

        self.mode = Some(mode);
        Ok(())
    }

    #[cfg(not(test))]
    async fn load_values(&mut self) -> Result<()> {
        let output = script_output("/usr/bin/wpctl", &["settings", MONO_KEY]).await?;

        match output.trim() {
            "Value: true" => self.mode = Some(Mode::Mono),
            "Value: false" => self.mode = Some(Mode::Stereo),
            _ => warn!("Unable to get audio mode from wpctl output"),
        }

        Ok(())
    }

    #[cfg(test)]
    async fn load_values(&mut self) -> Result<()> {
        // Just start in stereo mode in tests.
        self.mode = Some(Mode::Stereo);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_set_mode() {
        let mut manager = AudioManager::new().await;
        manager.mode = Some(Mode::Mono);
        manager.set_mode(Mode::Stereo).await.unwrap();
        assert_eq!(manager.mode, Some(Mode::Stereo));
        assert_eq!(manager.mode(), Some(Mode::Stereo));

        manager.set_mode(Mode::Mono).await.unwrap();
        assert_eq!(manager.mode, Some(Mode::Mono));
        assert_eq!(manager.mode(), Some(Mode::Mono));
    }
}
