//! Device manager — single owner of audio I/O device lifecycle.
//!
//! The model, in brief:
//!
//! - Engine state is BINARY per direction: running on a device, or none. There is no
//!   "lost" state — "missing" is the none state plus a config that still names a device,
//!   surfaced only in the UI as "(unavailable)".
//! - Config holds the persistent intent (last user-chosen name, or "" = none).
//! - A disconnected device commits to none immediately (no debounce — macOS CoreAudio
//!   doesn't re-list a device within any useful debounce window after a quick toggle, so a
//!   debounce only delayed recovery).
//! - Re-acquisition (a named device re-presenting) is the SAME path as a user selecting a
//!   device. It is driven by the OS device-change watcher (instant) with a slow poll as a
//!   safety net, and is only requested once a trial open off the main loop has succeeded —
//!   a present-but-unusable device must not keep interrupting the other direction.
//!
//! ## The hard constraint (do not regress)
//!
//! The main `select!` task is `!Send` (it owns device streams) and pinned to one worker;
//! awaiting anything inside an arm suspends the WHOLE task and stalls POKE forwarding —
//! this caused 45–77 ms latency spikes. Therefore device ENUMERATION and probing must
//! never run on the main loop. This module is split accordingly:
//!
//! - `Send`, runs detached / on `spawn_blocking`: enumeration, re-acquire timing,
//!   deciding what to do. Holds only `Arc<Mutex<String>>` live-name handles, the lost-flag
//!   `AtomicBool`, config, and channels — NEVER an engine reference.
//! - Cheap, synchronous, main-loop-owned: `rebuild_*` / `stop_*`, applied by the caller in
//!   response to a `DeviceAction` carrying an already-resolved `Device`.

use std::sync::{Arc, Mutex};
use crate::audio::Device;

/// Which I/O direction an action refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Input,
    Output,
}

/// An action the manager asks the MAIN LOOP to perform on the engines. These are the
/// cheap, synchronous operations — the manager has already done any enumeration/probing
/// off-thread and resolved the `Device`. The main loop applies these and performs the
/// post-rebuild settle/teardown bookkeeping.
pub enum DeviceAction {
    /// Build/replace the output stream on this resolved device.
    RebuildOutput { name: String, device: Device },
    /// Build/replace the input stream on this resolved device.
    RebuildInput { name: String, device: Device },
    /// Stop the output stream → `None` (no fallback).
    StopOutput,
    /// Stop the input stream → `None` (no fallback).
    StopInput,
}

impl std::fmt::Debug for DeviceAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceAction::RebuildOutput { name, .. } => write!(f, "RebuildOutput({name})"),
            DeviceAction::RebuildInput { name, .. } => write!(f, "RebuildInput({name})"),
            DeviceAction::StopOutput => write!(f, "StopOutput"),
            DeviceAction::StopInput => write!(f, "StopInput"),
        }
    }
}

/// Resolve a device by direction + persistent UID (preferred) + name (fallback), OFF the
/// async executor (synchronous CoreAudio enumeration). Callers must invoke this inside
/// `spawn_blocking` or a detached task — never directly on the main `select!` loop.
/// Returns the resolved handle or `None` if the device is not currently present.
///
/// Matching by UID first means a re-acquire finds the SAME physical device even if it was
/// renamed, and does not grab a different device that happens to share the name.
pub fn resolve_blocking(dir: Dir, uid: &str, name: &str) -> Option<Device> {
    if uid.is_empty() && name.is_empty() {
        return None;
    }
    crate::audio::resolve_device(matches!(dir, Dir::Input), uid, name).map(|r| r.device)
}

/// Default fallback cadence for parked re-acquire when there is NO device-change watcher
/// (e.g. the Linux poll fallback). With a watcher active the caller passes a much slower
/// value (the watcher nudge does the real, instant work and the poll is only a safety net).
/// Enumerating an input device trips the macOS mic-in-use indicator, which is why the
/// watcher path keeps this slow — a parked device would otherwise flash at this cadence.
pub const REACQUIRE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest an INPUT device that is present but refusing to open waits between trial
/// opens. See the backoff in `run_manager_task`.
pub const OPEN_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(300);

/// Per-direction lifecycle phase, driven entirely off the main loop by the manager task.
///
/// There is deliberately NO debounce state. CoreAudio does not re-list a device within a
/// couple of seconds of a quick unplug/replug, so a debounce would absorb no blips and only
/// delay recovery. Loss commits to `None` immediately and the device is re-acquired
/// promptly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// A stream is live. On a loss signal we go straight to `Parked` (if configured) or
    /// `Idle`, committing the stop immediately.
    Live,
    /// Committed to `None`; the configured device is absent. Re-check for its return at
    /// `REACQUIRE_INTERVAL`.
    Parked { last_check: std::time::Instant },
    /// No device configured (deliberate none). Idle — re-acquisition does not run.
    Idle,
}

/// What the manager task decided this tick for one direction.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do.
    Nothing,
    /// Commit the stop now (loss just happened) — tell the main loop to stop → `None`.
    CommitStop,
    /// Time to attempt (re-)acquisition: enumerate the configured name off-thread; if
    /// present and it opens, rebuild.
    TryAcquire,
}

/// Pure decision step for one direction.
///
/// - `loss_signalled`: the consumed device-fault flag (a stream just errored). Commits a
///   stop immediately — no debounce.
/// - `live_empty`: whether the engine currently reports no running device.
/// - `configured`: config name ("" = deliberate none).
/// - `now`: clock.
/// - `reacquire_interval`: how long to wait between parked re-acquire probes. The caller
///   passes a fast value when there's no watcher (poll is the mechanism) or a slow value
///   when a watcher is active (poll is just a safety net; the watcher nudge does the work).
///
/// Returns the next phase and the decision. No I/O — the caller performs any enumeration
/// or rebuild off-thread based on the `Decision`.
pub fn step(
    phase: &Phase,
    loss_signalled: bool,
    live_empty: bool,
    configured: &str,
    now: std::time::Instant,
    reacquire_interval: std::time::Duration,
) -> (Phase, Decision) {
    let has_config = !configured.is_empty();

    // Seed parked last_check far enough in the past that the first parked tick is due
    // immediately (prompt first probe after a loss / config change).
    let due_now = now.checked_sub(reacquire_interval).unwrap_or(now);

    // A fresh loss signal → commit the stop immediately, then park (if a device is still
    // configured, so we re-acquire when it returns) or go idle.
    if loss_signalled {
        if has_config {
            return (Phase::Parked { last_check: due_now }, Decision::CommitStop);
        } else {
            return (Phase::Idle, Decision::CommitStop);
        }
    }

    match phase {
        Phase::Live => {
            // Engine reports empty while we think we're live (e.g. a stop elsewhere) →
            // park (re-acquire) or go idle, matching config.
            if live_empty && has_config {
                (Phase::Parked { last_check: due_now }, Decision::Nothing)
            } else if live_empty {
                (Phase::Idle, Decision::Nothing)
            } else {
                (Phase::Live, Decision::Nothing)
            }
        }
        Phase::Parked { last_check } => {
            if !has_config {
                // User cleared the device while parked → stop re-acquiring.
                (Phase::Idle, Decision::Nothing)
            } else if !live_empty {
                // Device came back (rebuild applied) → live again.
                (Phase::Live, Decision::Nothing)
            } else if now.duration_since(*last_check) >= reacquire_interval {
                (Phase::Parked { last_check: now }, Decision::TryAcquire)
            } else {
                (Phase::Parked { last_check: *last_check }, Decision::Nothing)
            }
        }
        Phase::Idle => {
            if has_config && live_empty {
                // A device name appeared in config (user selected one) while idle →
                // begin acquiring promptly.
                (Phase::Parked { last_check: due_now }, Decision::Nothing)
            } else if !live_empty {
                (Phase::Live, Decision::Nothing)
            } else {
                (Phase::Idle, Decision::Nothing)
            }
        }
    }
}

/// Drive one direction's device lifecycle. This is the detached, `Send` task body shared
/// by input and output — it owns the `step` state machine, performs all enumeration
/// off-thread (`spawn_blocking`), and emits `DeviceAction`s to the main loop. It NEVER
/// touches an engine (that would make it `!Send` and risk blocking the main loop).
///
/// Inputs are the `Send` shared handles only:
/// - `dir`: which direction (selects which config field + action variants).
/// - `live_name`: the engine's published live-device name (`""` when not running).
/// - `lost_flag`: the device-fault flag, consumed (swapped to false) each tick.
/// - `cfg`: the shared config (for the configured device name).
/// - `act_tx`: channel to the main loop for the cheap engine ops.
/// - `device_gen`: a generation counter bumped by the device-change watcher (Stage 4).
///   When it changes while we are parked, we re-acquire immediately instead of waiting for
///   the slow poll — this is what makes watcher-driven reconnection instant.
/// - `watcher_active`: whether a native device-change watcher is installed. When true, the
///   periodic parked re-acquire poll is slowed right down (it exists only as a safety net
///   for events the watcher might miss); the watcher nudge is the primary mechanism, so
///   there is no frequent enumeration and thus no periodic mic-in-use flash. When false
///   (no watcher — e.g. Linux fallback), the poll runs at the normal faster cadence.
pub async fn run_manager_task(
    dir: Dir,
    live_name: Arc<Mutex<String>>,
    lost_flag: Arc<std::sync::atomic::AtomicBool>,
    cfg: Arc<std::sync::RwLock<crate::config::Config>>,
    act_tx: tokio::sync::mpsc::Sender<DeviceAction>,
    device_gen: Arc<std::sync::atomic::AtomicU64>,
    watcher_active: bool,
) {
    // Parked re-acquire cadence. With a watcher, the poll is only a slow safety net (the
    // watcher nudge does the real work) — keep it slow so it doesn't flash the mic. Without
    // a watcher, the poll IS the mechanism, so it runs faster.
    let reacquire_interval = if watcher_active {
        std::time::Duration::from_secs(30)
    } else {
        REACQUIRE_INTERVAL
    };
    // 500ms service tick. Real enumeration happens only on TryAcquire (parked re-check at
    // REACQUIRE_INTERVAL, or immediately when the watcher bumps device_gen). On loss we
    // commit the stop immediately (no debounce).
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut phase = Phase::Live;
    let mut seen_gen = device_gen.load(std::sync::atomic::Ordering::Relaxed);
    // Whether the configured device is currently present but refusing to open. Logged on
    // the transition into that state, not on every retry.
    let mut open_failing = false;
    // Backoff for an INPUT device that is present but keeps refusing to open. Every trial
    // open of an input opens the microphone, which lights the system's mic-in-use
    // indicator, so retrying on the plain poll flashed it every 30 s for as long as the device
    // stayed unusable. The wait doubles after each refusal, up to OPEN_RETRY_MAX. A change in
    // the device set (the watcher's nudge) retries at once, and an open that succeeds, or the
    // device going away, resets it. Output trial opens light nothing and keep the poll's pace.
    let mut open_backoff = reacquire_interval;
    let mut next_open_try: Option<std::time::Instant> = None;

    loop {
        tick.tick().await;
        let loss = lost_flag.swap(false, std::sync::atomic::Ordering::Relaxed);
        let live_empty = live_name.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
        let (configured, configured_uid) = {
            let c = cfg.read().unwrap_or_else(|e| e.into_inner());
            match dir {
                Dir::Input => (c.audio.input_device.clone(), c.audio.input_device_uid.clone()),
                Dir::Output => (c.audio.output_device.clone(), c.audio.output_device_uid.clone()),
            }
        };
        let now = std::time::Instant::now();

        // Watcher nudge: if the OS device set changed since we last looked and we're parked
        // (absent but configured), force a prompt re-acquire rather than waiting out the
        // interval. We detect "parked" via live_empty + a configured name; the `step`
        // machine still owns the actual transition.
        let cur_gen = device_gen.load(std::sync::atomic::Ordering::Relaxed);
        let watcher_nudge = cur_gen != seen_gen;
        seen_gen = cur_gen;

        let (next, decision) = step(&phase, loss, live_empty, &configured, now, reacquire_interval);
        phase = next;

        // If the watcher fired and we're sitting parked, upgrade a Nothing decision to a
        // TryAcquire so reconnection is immediate. (Only meaningful while parked: if we're
        // Live or Idle there's nothing to re-acquire.)
        let decision = if watcher_nudge
            && matches!(phase, Phase::Parked { .. })
            && decision == Decision::Nothing
        {
            Decision::TryAcquire
        } else {
            decision
        };

        match decision {
            Decision::Nothing => {}
            Decision::CommitStop => {
                let action = match dir {
                    Dir::Input => DeviceAction::StopInput,
                    Dir::Output => DeviceAction::StopOutput,
                };
                let _ = act_tx.send(action).await;
            }
            // Enumerate off-thread; if the configured device is present AND opens, rebuild.
            //
            // The trial open is what keeps a device that is present but unusable — held
            // exclusively by another application, say — from costing anything. The rebuild
            // stops both units, so asking for it on every retry interrupted the healthy
            // direction each time for a rebuild that was going to fail.
            Decision::TryAcquire => {
                if dir == Dir::Input && !watcher_nudge
                    && next_open_try.is_some_and(|t| now < t)
                {
                    continue;
                }
                let name = configured.clone();
                let d2 = name.clone();
                let u2 = configured_uid.clone();
                let found = tokio::task::spawn_blocking(move || {
                    resolve_blocking(dir, &u2, &d2).map(|device| {
                        let opened = crate::audio::probe_open(matches!(dir, Dir::Input), &device)
                            .map_err(|e| e.to_string());
                        (device, opened)
                    })
                }).await.ok().flatten();
                match found {
                    Some((device, Ok(()))) => {
                        open_failing = false;
                        open_backoff = reacquire_interval;
                        next_open_try = None;
                        let action = match dir {
                            Dir::Input => DeviceAction::RebuildInput { name, device },
                            Dir::Output => DeviceAction::RebuildOutput { name, device },
                        };
                        let _ = act_tx.send(action).await;
                    }
                    Some((_, Err(e))) => {
                        if dir == Dir::Input {
                            next_open_try = Some(now + open_backoff);
                            open_backoff = (open_backoff * 2).min(OPEN_RETRY_MAX);
                        }
                        if !open_failing {
                            open_failing = true;
                            tracing::warn!("{} device '{}' is present but will not open ({}) \
                                            — leaving the other direction untouched and \
                                            retrying", match dir { Dir::Input => "Input",
                                                                    Dir::Output => "Output" },
                                           name, e);
                        }
                    }
                    // Not present: nothing to try, and not an open failure.
                    None => {
                        open_failing = false;
                        open_backoff = reacquire_interval;
                        next_open_try = None;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Tests use the default REACQUIRE_INTERVAL; a thin wrapper keeps call sites short.
    fn step_t(phase: &Phase, loss: bool, live_empty: bool, cfg: &str, now: std::time::Instant)
        -> (Phase, Decision) {
        step(phase, loss, live_empty, cfg, now, REACQUIRE_INTERVAL)
    }

    #[test]
    fn loss_commits_stop_immediately_and_parks_when_configured() {
        let now = Instant::now();
        let (phase, dec) = step_t(&Phase::Live, true, false, "Dev X", now);
        // No debounce — stop now, and park so we re-acquire when it returns.
        assert_eq!(dec, Decision::CommitStop);
        assert!(matches!(phase, Phase::Parked { .. }));
    }

    #[test]
    fn loss_goes_idle_when_no_config() {
        let now = Instant::now();
        let (phase, dec) = step_t(&Phase::Live, true, false, "", now);
        assert_eq!(dec, Decision::CommitStop);
        assert!(matches!(phase, Phase::Idle));
    }

    #[test]
    fn parked_after_loss_tries_acquire_promptly() {
        // After a loss commits to Parked, last_check is set in the past so the very first
        // parked tick probes (prompt reconnection) rather than waiting a full interval.
        let now = Instant::now();
        let (phase, _) = step_t(&Phase::Live, true, false, "Dev X", now);
        // Next tick, shortly after: should already be due to try acquiring.
        let (phase, dec) = step_t(&phase, false, true, "Dev X", now + Duration::from_millis(10));
        assert_eq!(dec, Decision::TryAcquire);
        assert!(matches!(phase, Phase::Parked { .. }));
    }

    #[test]
    fn parked_tries_acquire_on_interval() {
        let t0 = Instant::now();
        let phase = Phase::Parked { last_check: t0 };
        // Before interval: nothing.
        let (_, dec) = step_t(&phase, false, true, "Dev X", t0 + Duration::from_millis(100));
        assert_eq!(dec, Decision::Nothing);
        // After interval: try acquire.
        let (phase, dec) = step_t(&phase, false, true, "Dev X", t0 + REACQUIRE_INTERVAL);
        assert_eq!(dec, Decision::TryAcquire);
        assert!(matches!(phase, Phase::Parked { .. }));
    }

    #[test]
    fn parked_returns_to_live_when_device_back() {
        let t0 = Instant::now();
        let phase = Phase::Parked { last_check: t0 };
        // Rebuild applied elsewhere → live_empty=false.
        let (phase, dec) = step_t(&phase, false, false, "Dev X", t0 + Duration::from_millis(100));
        assert!(matches!(phase, Phase::Live));
        assert_eq!(dec, Decision::Nothing);
    }

    #[test]
    fn parked_goes_idle_if_user_clears_config() {
        let t0 = Instant::now();
        let phase = Phase::Parked { last_check: t0 };
        let (phase, dec) = step_t(&phase, false, true, "", t0 + Duration::from_millis(100));
        assert!(matches!(phase, Phase::Idle));
        assert_eq!(dec, Decision::Nothing);
    }

    #[test]
    fn idle_parks_when_config_appears() {
        let t0 = Instant::now();
        let (phase, _) = step_t(&Phase::Idle, false, true, "Dev X", t0);
        assert!(matches!(phase, Phase::Parked { .. }));
    }
}
