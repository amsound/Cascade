//! Debounced, non-blocking config persistence.
//!
//! The in-memory `Arc<RwLock<Config>>` is the single source of truth. This module
//! persists it to disk LAZILY so that:
//!   - callers never block on disk I/O (important on slow storage, e.g. a Pi SD card);
//!   - a burst of rapid edits (e.g. dragging across a routing matrix) coalesces into
//!     ONE disk write instead of dozens;
//!   - the write always reflects the LATEST in-memory state (the writer snapshots
//!     `cfg_arc` at write time, so a coalesced burst can never persist a stale
//!     intermediate);
//!   - the file never goes backwards: every write — debounced or `flush_now` — takes its
//!     snapshot and renames the file while holding one lock, so a later write always
//!     carries a snapshot at least as new as any earlier one.
//!
//! Model: everything READS and MUTATES `cfg_arc` synchronously and immediately.
//! Nothing reads the file at runtime — `Config::load` runs once at startup and never
//! again — so the file is purely a background snapshot, never an input.
//!
//! API:
//!   - `request()`    — non-blocking; schedules a debounced write. Call after any
//!                      mutation of `cfg_arc`.
//!   - `flush_now()`  — synchronous; writes immediately and reports whether it worked. Use
//!                      on a settings save and on shutdown so no recent change is lost to
//!                      the debounce window.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Notify;

use crate::config::Config;

/// Quiet period after the last `request()` before the debounced write fires.
/// Long enough to coalesce a fast drag across a matrix; short enough that a crash
/// loses at most this much. 500ms is a good balance for interactive editing.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone)]
pub struct ConfigSaver {
    cfg:    Arc<RwLock<Config>>,
    path:   PathBuf,
    notify: Arc<Notify>,
    /// Held from snapshot to rename by every write. See the module doc.
    write_lock: Arc<std::sync::Mutex<()>>,
}

/// Snapshot the in-memory config and write it, under the saver's write lock.
fn write_latest(cfg: &RwLock<Config>, path: &std::path::Path,
                write_lock: &std::sync::Mutex<()>) -> anyhow::Result<()> {
    let _held = write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let snapshot = match cfg.read() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    };
    snapshot.save(path)
}

impl ConfigSaver {
    /// Create the saver and spawn its background debounce task.
    pub fn new(cfg: Arc<RwLock<Config>>, path: PathBuf) -> Self {
        let saver = Self {
            cfg, path,
            notify: Arc::new(Notify::new()),
            write_lock: Arc::new(std::sync::Mutex::new(())),
        };
        saver.spawn_task();
        saver
    }

    /// Non-blocking: schedule a debounced write. Safe to call as often as you like.
    pub fn request(&self) {
        self.notify.notify_one();
    }

    /// Synchronous: write the current in-memory config to disk right now.
    /// Use on shutdown and on a settings save so the debounce window can't drop a change.
    ///
    /// BLOCKS the calling thread for the whole write — including waiting out a debounced
    /// write already in progress. Some callers are async request handlers, so this blocks a
    /// runtime worker thread rather than yielding.
    ///
    /// Returns what the write did, so a caller that told someone the settings were saved can
    /// tell them the truth. The reason is logged here either way.
    pub fn flush_now(&self) -> anyhow::Result<()> {
        write_latest(&self.cfg, &self.path, &self.write_lock).map_err(|e| {
            tracing::warn!("config flush_now failed: {}", e);
            e
        })
    }

    fn spawn_task(&self) {
        let cfg    = Arc::clone(&self.cfg);
        let path   = self.path.clone();
        let notify = Arc::clone(&self.notify);
        let write_lock = Arc::clone(&self.write_lock);
        tokio::spawn(async move {
            loop {
                // Wait for the first change request.
                notify.notified().await;
                // Coalesce: keep resetting the timer while changes keep arriving.
                // Each new request within the window pushes the write later, so a
                // continuous drag results in a single write after it settles.
                loop {
                    tokio::select! {
                        _ = notify.notified() => { continue; }      // more edits → wait again
                        _ = tokio::time::sleep(DEBOUNCE) => { break; } // quiet → write
                    }
                }
                // Snapshot the LATEST in-memory state and write it via spawn_blocking, so
                // a slow disk — or a flush_now holding the write lock — can't stall other
                // tasks. The snapshot is taken under the lock, not before it.
                let (c, p, l) = (Arc::clone(&cfg), path.clone(), Arc::clone(&write_lock));
                let res = tokio::task::spawn_blocking(move || write_latest(&c, &p, &l)).await;
                match res {
                    Ok(Ok(()))   => tracing::debug!("config persisted (debounced)"),
                    Ok(Err(e))   => tracing::warn!("config save failed: {}", e),
                    Err(e)       => tracing::warn!("config save task join error: {}", e),
                }
            }
        });
    }
}
