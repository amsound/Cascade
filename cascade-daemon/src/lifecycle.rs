//! One place that decides the daemon is stopping, and why.
//!
//! Stopping is requested from several places — an interrupt, a terminate signal, a Windows
//! session ending, a tray or menu-bar item — and every one of them must do the same work: release the exclusive device claim, flush the
//! debounced config, and only then let the process go. Routing them through one request
//! keeps that from being reimplemented per caller, and keeps a caller from skipping half.
//!
//! `request()` is callable from ANY thread, including one with no tokio runtime attached —
//! a Win32 window procedure, or an AppKit menu action. It does not block and never runs
//! the shutdown itself; the main select loop owns that.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use tokio::sync::Notify;

/// What the process should do once it has shut down cleanly.
///
/// Quit is the only outcome. NOTHING RESTARTS ON ANY TARGET: a port or interface change
/// rebinds the audio socket in place, a rename re-derives each peer's wire token through
/// `AddRemote`, devices rebuild on the hot path, the HTTP listener rebinds itself, and a
/// port that is busy at startup is taken by the probe when it frees.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Stop.
    Quit,
}

const NONE: u8 = 0;
const QUIT: u8 = 1;

static REASON: AtomicU8 = AtomicU8::new(NONE);
static SIGNAL: OnceLock<Notify> = OnceLock::new();

fn signal() -> &'static Notify {
    SIGNAL.get_or_init(Notify::new)
}

/// Ask the process to stop. Returns immediately; safe to call from any thread, more than
/// once, and before anything is waiting. The first request wins; later ones are ignored.
pub fn request(exit: Exit) {
    let code = match exit { Exit::Quit => QUIT };
    if REASON
        .compare_exchange(NONE, code, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    // notify_one, not notify_waiters: it stores a permit when nothing is waiting yet, so a
    // request racing startup is still delivered rather than dropped.
    signal().notify_one();
}

/// Resolves when a stop has been requested, with the reason.
pub async fn requested() -> Exit {
    signal().notified().await;
    Exit::Quit
}

/// True once a stop has been requested. For code that cannot await and needs to tell a
/// deliberate shutdown from a fault — the receive loop, whose socket is closed under it.
pub fn is_stopping() -> bool {
    REASON.load(Ordering::SeqCst) != NONE
}
