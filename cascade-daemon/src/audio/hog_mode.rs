//! Exclusive output-device access — CoreAudio "hog mode".
//!
//! Exclusive access to the OUTPUT device is claimed via
//! `AudioObjectSetPropertyData(kAudioDevicePropertyHogMode)`. Taking a device out
//! of the shared mix engine gives the app a more direct path to the hardware, lowering
//! output latency. Deliberately OUTPUT-ONLY: the input device stays shared, so other
//! applications can still capture.
//!
//! Semantics of the property (a `pid_t`):
//!   * read  → the owning process's PID, or `-1` when the device is not hogged
//!   * write → our own PID claims it; `-1` releases it
//!
//! Ownership is process-wide and OS-enforced: while we hold it, other applications cannot
//! open the device. That makes RELEASE mandatory — on disable, on device change, and at
//! shutdown — or the device stays locked for everyone else until this process exits. Both
//! `release()` and `Drop` handle that.
//!
//! Failure to claim is NOT fatal: another app may already hold the device, or it may be a
//! device that does not support hogging. We log and continue in shared mode, because playing
//! shared audio is always better than not playing at all.
//!
//! All of the above is macOS-only. Off macOS these functions track the setting but perform
//! no claim — see the note above the non-macOS implementations.

#[cfg(target_os = "macos")]
use std::sync::Mutex;

/// The device we currently hold hog mode on, so we can release exactly that device even if
/// the configured device has since changed. `None` = we hold nothing.
#[cfg(target_os = "macos")]
static HOGGED: Mutex<Option<u32>> = Mutex::new(None);

#[cfg(target_os = "macos")]
mod imp {
    use super::HOGGED;
    use tracing::{debug, info, warn};

    use std::ptr::{null, NonNull};

    use objc2_core_audio::{
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectSetPropertyData, kAudioDevicePropertyHogMode,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    };

    /// Not-hogged sentinel, and the value written to release.
    const NOT_HOGGED: i32 = -1;

    /// Hog mode is a whole-device property, so global scope and the main element.
    fn addr() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyHogMode,
            mScope:    kAudioObjectPropertyScopeGlobal,
            mElement:  kAudioObjectPropertyElementMain,
        }
    }

    /// Read the current owner PID, or None if the property is unsupported/unreadable.
    fn owner(device: AudioObjectID) -> Option<i32> {
        let a = addr();
        let mut pid: i32 = NOT_HOGGED;
        let size = std::mem::size_of::<i32>() as u32;
        let st = unsafe {
            AudioObjectGetPropertyData(device, NonNull::from(&a), 0, null(),
                                       NonNull::from(&size), NonNull::from(&mut pid).cast())
        };
        if st == 0 { Some(pid) } else { None }
    }

    /// Claim exclusive access to `device`. Returns true only if we hold it afterwards.
    /// Idempotent: re-claiming a device we already hold is a no-op returning true.
    pub fn claim(device: AudioObjectID) -> bool {
        let mut held = HOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if *held == Some(device) {
            return true;   // already ours
        }
        // Release a previously-held DIFFERENT device first (device switch).
        if let Some(prev) = *held {
            release_inner(prev);
            *held = None;
        }
        let me = std::process::id() as i32;
        match owner(device) {
            None => {
                debug!("hog mode: device {} does not report hog mode — staying shared", device);
                return false;
            }
            Some(pid) if pid == me => {
                // Already ours at the OS level (e.g. re-entry after a stream rebuild).
                *held = Some(device);
                return true;
            }
            Some(pid) if pid != NOT_HOGGED => {
                warn!("hog mode: device {} already exclusively held by PID {} — staying shared",
                      device, pid);
                return false;
            }
            Some(_) => {}   // NOT_HOGGED — free to claim
        }
        let a = addr();
        let st = unsafe {
            AudioObjectSetPropertyData(device, NonNull::from(&a), 0, null(),
                                       std::mem::size_of::<i32>() as u32,
                                       NonNull::from(&me).cast())
        };
        if st != 0 {
            warn!("hog mode: claim failed on device {} (OSStatus {}) — staying shared", device, st);
            return false;
        }
        // Verify the OS actually granted it rather than trusting the write.
        match owner(device) {
            Some(pid) if pid == me => {
                *held = Some(device);
                info!("hog mode: exclusive access acquired on output device {} \
                       (other apps cannot open it until released)", device);
                true
            }
            other => {
                warn!("hog mode: claim on device {} not granted (owner now {:?}) — staying shared",
                      device, other);
                false
            }
        }
    }

    /// Write the release sentinel. Does not touch HOGGED (callers manage that).
    fn release_inner(device: AudioObjectID) {
        let a = addr();
        let val = NOT_HOGGED;
        let st = unsafe {
            AudioObjectSetPropertyData(device, NonNull::from(&a), 0, null(),
                                       std::mem::size_of::<i32>() as u32,
                                       NonNull::from(&val).cast())
        };
        if st == 0 {
            info!("hog mode: released exclusive access on device {}", device);
        } else {
            warn!("hog mode: release failed on device {} (OSStatus {}) — \
                   the device may stay locked until this process exits", device, st);
        }
    }

    /// Release whatever device we hold, if any. Idempotent and safe to call unconditionally.
    pub fn release() {
        let mut held = HOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dev) = held.take() {
            release_inner(dev);
        }
    }

    /// True while we hold exclusive access to some device.
    pub fn is_active() -> bool {
        HOGGED.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

/// Claim exclusive (hog) access to an OUTPUT device. Returns true if held afterwards.
/// Non-fatal on failure — the caller continues in shared mode.
/// Whether exclusive output is CONFIGURED — independent of whether it is currently held.
///
/// A rebuild releases the claim with the old unit and re-claims it with the new one, so
/// "released right now" must not be read as "the user turned it off". This records the
/// setting so the rebuild path can re-apply it without touching the config lock.
///
/// Deliberately an atomic and not a config read: the rebuild macros already hold a
/// `RwLock<Config>` read guard for their whole body, and taking a second read on the same
/// thread deadlocks against any writer that queues between the two — std's lock is fair, so
/// the second read waits behind the writer while the writer waits on the first guard.
static EXCLUSIVE_WANTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Record whether exclusive output is configured. Set wherever the setting is applied.
pub fn set_wanted(on: bool) {
    EXCLUSIVE_WANTED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether exclusive output is configured, for the rebuild path's re-claim.
pub fn wanted() -> bool {
    EXCLUSIVE_WANTED.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(target_os = "macos")]
pub fn claim_output(device: &crate::audio::Device) -> bool {
    match crate::audio::backend::native_device_id(device) {
        Some(id) => imp::claim(id),
        None => false,
    }
}

/// Release any exclusive access we hold. MUST be called on disable, device change, and
/// shutdown — otherwise the device stays locked to other applications.
#[cfg(target_os = "macos")]
pub fn release() { imp::release() }

/// True while we currently hold exclusive access.
#[cfg(target_os = "macos")]
pub fn is_active() -> bool { imp::is_active() }

// ── Off macOS: exclusivity is a property of the device, not a mode ─────────────
//
// macOS shares a device between clients by default, and hog mode is an explicit claim
// that takes it exclusively. ALSA has no equivalent claim: `hw:` and `plughw:` opens are
// already exclusive — first opener wins, anything else gets EBUSY — while `default` and
// `dmix` are shared by design. Device enumeration exposes both kinds, so which of the two
// applies depends on the device the user selected.
//
// There is therefore no claim to make and nothing to release. These functions only record
// whether the setting is enabled, which is what `is_active` reports. The UI shows it as a
// state rather than a switch.
#[cfg(not(target_os = "macos"))]
static OUTPUT_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(target_os = "macos"))]
pub fn claim_output(_device: &crate::audio::Device) -> bool {
    // Nothing to claim: a `hw:`/`plughw:` device is already exclusive, and a shared one
    // (`default`, `dmix`) cannot be made exclusive. Returns true either way, so this
    // reports the setting rather than the device's actual exclusivity.
    OUTPUT_HELD.store(true, std::sync::atomic::Ordering::Relaxed);
    true
}

#[cfg(not(target_os = "macos"))]
pub fn release() { OUTPUT_HELD.store(false, std::sync::atomic::Ordering::Relaxed); }

#[cfg(not(target_os = "macos"))]
pub fn is_active() -> bool { OUTPUT_HELD.load(std::sync::atomic::Ordering::Relaxed) }

/// True when exclusivity is a property of opening the device rather than a request the
/// user makes. The UI uses this to present it as a state instead of a control.
pub fn is_inherent() -> bool { !cfg!(target_os = "macos") }
