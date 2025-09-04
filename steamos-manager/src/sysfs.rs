/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, bail, ensure, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{read_dir, read_to_string};
use tokio::sync::{oneshot, Mutex, Notify, OnceCell};
use tracing::{error, trace};

use crate::write_synced;
use crate::Service;

static SYSFS_WRITER: OnceCell<Arc<SysfsWriterQueue>> = OnceCell::const_new();

#[derive(Debug)]
pub(crate) enum SysfsWritten {
    Written(Result<()>),
    Superseded,
}

type SysfsQueue = (Vec<u8>, oneshot::Sender<SysfsWritten>);
type SysfsQueueMap = HashMap<PathBuf, SysfsQueue>;

#[derive(Debug)]
struct SysfsWriterQueue {
    values: Mutex<SysfsQueueMap>,
    notify: Notify,
}

impl SysfsWriterQueue {
    fn new() -> SysfsWriterQueue {
        SysfsWriterQueue {
            values: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }

    pub(crate) async fn send(
        &self,
        path: PathBuf,
        contents: Vec<u8>,
    ) -> oneshot::Receiver<SysfsWritten> {
        let (tx, rx) = oneshot::channel();
        if let Some((_, old_tx)) = self.values.lock().await.insert(path, (contents, tx)) {
            let _ = old_tx.send(SysfsWritten::Superseded);
        }
        self.notify.notify_one();
        rx
    }

    async fn recv(&self) -> Option<(PathBuf, Vec<u8>, oneshot::Sender<SysfsWritten>)> {
        // Take an arbitrary file from the map
        self.notify.notified().await;
        let mut values = self.values.lock().await;
        if let Some(path) = values.keys().next().cloned() {
            values
                .remove_entry(&path)
                .map(|(path, (contents, tx))| (path, contents, tx))
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub(crate) struct SysfsWriterService {
    queue: Arc<SysfsWriterQueue>,
}

impl SysfsWriterService {
    pub fn init() -> Result<SysfsWriterService> {
        ensure!(!SYSFS_WRITER.initialized(), "sysfs writer already active");
        let queue = Arc::new(SysfsWriterQueue::new());
        SYSFS_WRITER.set(queue.clone())?;
        Ok(SysfsWriterService { queue })
    }
}

impl Service for SysfsWriterService {
    const NAME: &'static str = "sysfs-writer";

    async fn run(&mut self) -> Result<()> {
        loop {
            let Some((path, contents, tx)) = self.queue.recv().await else {
                continue;
            };
            trace!(
                "Writing bytes \"{}\" to {}",
                str::from_utf8(&contents).unwrap_or(format!("{contents:?}").as_str()),
                path.display()
            );
            let res = write_synced(path, &contents)
                .await
                .inspect_err(|message| error!("Error writing to sysfs file: {message}"));
            let _ = tx.send(SysfsWritten::Written(res));
        }
    }
}

pub(crate) async fn find_sysdir(prefix: impl AsRef<Path>, expected: &str) -> Result<PathBuf> {
    let mut dir = read_dir(prefix.as_ref()).await?;
    loop {
        let base = match dir.next_entry().await? {
            Some(entry) => entry.path(),
            None => bail!("prefix not found"),
        };
        let file_name = base.join("name");
        let name = read_to_string(file_name.as_path())
            .await?
            .trim()
            .to_string();
        if name == expected {
            return Ok(base);
        }
    }
}

pub(crate) async fn sysfs_queued_write(
    path: PathBuf,
    data: Vec<u8>,
) -> Result<oneshot::Receiver<SysfsWritten>> {
    Ok(SYSFS_WRITER
        .get()
        .ok_or(anyhow!("sysfs writer not running"))?
        .send(path, data)
        .await)
}
