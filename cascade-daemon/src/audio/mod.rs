pub mod ebu_tone;
pub mod spsc;
pub mod zita;
pub mod channel_sync;
pub mod engine;
pub mod encode;
pub mod activity;
pub mod device_manager;
pub mod device_watcher;
pub mod hog_mode;

pub use engine::AudioEngine;
pub use encode::CaptureEngine;
pub mod routing;
pub mod pool;
pub mod scheduler;
pub mod backend;

pub use backend::{Device, Stream, device_name, device_uid};

/// The pause between stopping both audio units and disposing them, on every rebuild
/// (CASCADE_AUDIO_RECEIVE_SPEC §5.2, inside `stop_audio`).
///
/// Both units are stopped, this elapses, and only then are they torn down. It gives
/// in-flight callbacks time to return before the instances they were called on go away.
pub const UNIT_STOP_SETTLE: std::time::Duration = std::time::Duration::from_millis(20);

/// The additional pause between disposing the units and rebuilding them on a DEVICE change
/// (CASCADE_AUDIO_RECEIVE_SPEC §5.2).
///
/// A device change sets the hardware's nominal sample rate, and that write is
/// asynchronous: the HAL returns success and the device reports the new rate roughly
/// 100 ms later. Initialising a unit before it lands costs audio — measured at about a
/// third of the expected frames over the first 300 ms, recovering unaided afterwards. The
/// pause spends that window with everything stopped instead, so playback resumes at the
/// settled rate.
///
/// The callback-period path does not carry this: it changes no device and no rate.
pub const DEVICE_RATE_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

/// The shortest callback period Cascade ever requests, in frames (2.5 ms): the receive half
/// for a 5 ms buffer, and the send half for 2.5 ms frames.
///
/// What a device grants for this request is the shortest period it will run at, which is
/// what `backend::smallest_period` reports. Only the ALSA and WASAPI backends ask for it.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub const SMALLEST_PERIOD_REQUEST: usize = 120;

// ── Device callback limits ────────────────────────────────────────────────────
//
// The input and output devices share one callback request, so what limits either limits
// both. Each device's shortest callback period is learned when it is assigned, and the
// longer of the two — the shortest period BOTH can run — sets three things:
//
//   * the shortest callback Cascade requests (`shortest_request`),
//   * the shortest outgoing frame size offered and sent (`shortest_frame`),
//   * the receive-buffer floor (`channel_sync::period_floor_samples`).
//
// On macOS neither device is asked, both values stay 0, and none of the three moves.

/// Shortest callback period of the assigned output and input devices, in frames. 0 when
/// not learned: always on macOS, when the probe failed, and while that direction has no
/// device.
static SHORTEST_OUTPUT_PERIOD: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static SHORTEST_INPUT_PERIOD: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn shortest_period_cell(input: bool) -> &'static std::sync::atomic::AtomicUsize {
    if input { &SHORTEST_INPUT_PERIOD } else { &SHORTEST_OUTPUT_PERIOD }
}

/// Ask `device` for its shortest callback period (`backend::smallest_period`) and record it
/// for its direction. Called when a device is assigned, while nothing holds it: the
/// backends that learn it open the device.
///
/// A device that cannot be asked records 0, so it imposes no limit.
pub fn learn_shortest_period(device: &Device, input: bool) {
    let dir = if input { backend::Dir::Input } else { backend::Dir::Output };
    let label = if input { "Input" } else { "Output" };
    let period = match backend::smallest_period(device, dir) {
        Ok(Some(period)) => {
            tracing::info!("{label} '{}': shortest callback period {} frames ({:.1} ms)",
                           device_name(device), period, period as f32 / 48.0);
            period
        }
        Ok(None) => 0,
        Err(e) => {
            tracing::info!("{label} '{}': shortest callback period not learned ({e}) — no \
                            limit applied for this device", device_name(device));
            0
        }
    };
    shortest_period_cell(input).store(period, std::sync::atomic::Ordering::Relaxed);
}

/// Forget a direction's shortest period — its device was stopped or lost.
pub fn forget_shortest_period(input: bool) {
    shortest_period_cell(input).store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Holds a direction's learned period only if its stream is actually built: a device that
/// never opened must limit nothing.
///
/// The period is learned before the stream is built, and building has many failure points
/// after that — the device can be busy, refuse the format, or fail to start. Dropping this
/// without `keep` clears the period again, so every one of them is covered.
pub struct LearnedPeriod {
    input: bool,
    armed: bool,
}

impl LearnedPeriod {
    /// Ask `device` for its shortest period (`learn_shortest_period`) and hold the answer
    /// until the caller either keeps it or drops this.
    pub fn learn(device: &Device, input: bool) -> Self {
        learn_shortest_period(device, input);
        LearnedPeriod { input, armed: true }
    }

    /// The stream is built: keep what was learned.
    pub fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for LearnedPeriod {
    fn drop(&mut self) {
        if self.armed {
            forget_shortest_period(self.input);
        }
    }
}

/// The shortest callback period both assigned devices can run, in frames: the longer of the
/// two devices' shortest periods. 0 when neither is known.
pub fn agreed_period() -> usize {
    use std::sync::atomic::Ordering::Relaxed;
    SHORTEST_OUTPUT_PERIOD.load(Relaxed).max(SHORTEST_INPUT_PERIOD.load(Relaxed))
}

/// The shortest outgoing frame size, in samples, for a callback period: the smallest frame
/// size at least as long as the period, so a capture callback never holds more than one
/// frame. 120 (every size) for a period of 0; 960 (20 ms, always offered) when no frame
/// size is that long.
pub fn shortest_frame_for(period: usize) -> usize {
    [120usize, 240, 480].into_iter().find(|&f| f >= period).unwrap_or(960)
}

/// `shortest_frame_for` the agreed period. Every outgoing frame size is raised to at least
/// this, and the web UI offers no shorter one.
pub fn shortest_frame() -> usize {
    shortest_frame_for(agreed_period())
}

/// The shortest callback Cascade requests, in frames: `shortest_frame`, capped at the
/// longest request (`encode::IO_BUF_CAP_FRAMES`). Never below the agreed period unless
/// that is longer than every request.
pub fn shortest_request() -> usize {
    shortest_frame().min(encode::IO_BUF_CAP_FRAMES)
}

/// Whether to offer this device in the picker.
///
/// Policy, not capability: the backend reports what each enumerated entry IS
/// (`backend::is_hardware_endpoint`), and this decides what the user is shown. On Linux
/// that means real hardware endpoints only — converting or shared views of a card would
/// silently resample. Everything enumerated is offered on CoreAudio.
///
/// This gates the PICKER ONLY. `resolve_device` deliberately searches the full
/// enumeration, so a device named in an existing config keeps resolving even if it would
/// no longer be offered.
pub fn is_selectable_device(dev: &Device) -> bool {
    backend::is_hardware_endpoint(dev)
}

/// Device names for the picker, output direction. Filtered by `is_selectable_device`.
pub fn selectable_output_devices() -> Vec<String> {
    backend::output_devices().into_iter()
        .filter(is_selectable_device)
        .map(|d| backend::device_name(&d))
        .collect()
}

/// Device names for the picker, input direction. Filtered by `is_selectable_device`.
pub fn selectable_input_devices() -> Vec<String> {
    backend::input_devices().into_iter()
        .filter(is_selectable_device)
        .map(|d| backend::device_name(&d))
        .collect()
}

/// A device resolved for one direction, together with its CURRENT identity — so the caller
/// can persist a freshly-learned UID, or a new name for a device that was renamed.
pub struct ResolvedDevice {
    pub device: Device,
    pub uid:    String,
    pub name:   String,
}

/// Resolve the configured device for one direction, preferring the PERSISTENT UID over the
/// name. Order:
///   1. `uid` non-empty and present → that device (authoritative; its name may have changed,
///      and the returned `name` reflects the new one so config can be refreshed).
///   2. otherwise match by `name` → returns its UID so the caller can learn it.
/// `None` when neither resolves (device absent) or nothing is configured.
///
/// This is the single place device identity is decided, so name-vs-UID precedence cannot
/// drift between the boot path, the hot device-change handlers, and the re-acquire loop.
/// Synchronous CoreAudio enumeration — call inside `spawn_blocking`, never on the select loop.
pub fn resolve_device(input: bool, uid: &str, name: &str) -> Option<ResolvedDevice> {
    let candidates: Vec<Device> = if input {
        backend::input_devices()
    } else {
        backend::output_devices()
    };
    let describe = |d: &Device| ResolvedDevice {
        device: d.clone(),
        uid:    device_uid(d).unwrap_or_default(),
        name:   device_name(d),
    };
    if !uid.is_empty() {
        if let Some(d) = candidates.iter()
            .find(|d| device_uid(d).as_deref() == Some(uid)) {
            return Some(describe(d));
        }
    }
    // Empty means "no device", not "the system default" — there is no default-device
    // selection in Cascade, by design.
    if name.is_empty() { return None; }
    // Prefer a selectable (real hardware) match before any other. Names are unique on both
    // platforms — `device_name` appends the pcm id on Linux precisely so they are — so this
    // normally finds the same device either way. It matters for a config written before
    // that suffix existed, or by hand: a bare ALSA description still matches several
    // devices, and without this preference the winner is whichever enumerated first, which
    // can be a converting variant (`plughw:`, `default:`) that would silently resample.
    candidates.iter()
        .find(|d| device_name(d) == name && is_selectable_device(d))
        .or_else(|| candidates.iter().find(|d| device_name(d) == name))
        .map(describe)
}

/// Open `device` in one direction and close it again, without starting it, to learn whether
/// it will open at all.
///
/// A device can be PRESENT and still refuse to open — another application holds it
/// exclusively, or its driver refuses the format. Re-acquiring it goes through a rebuild that
/// stops BOTH audio units, so without this check every retry interrupted the healthy
/// direction for about half a second just to fail again.
///
/// The trial asks for the period `backend::probe_period` says is safe. On CoreAudio that is
/// the device's current buffer size, because the period is a property of the device and a
/// trial asking for another size would move it under a unit already running there; when the
/// device will not report its size the trial is skipped and the open is assumed to work.
///
/// Synchronous device I/O — call off the select loop.
pub fn probe_open(input: bool, device: &Device) -> anyhow::Result<()> {
    let dir = if input { backend::Dir::Input } else { backend::Dir::Output };
    let Some(period) = backend::probe_period(device, dir) else { return Ok(()) };
    let cfg = backend::find_config(device, dir, period)?;
    let stream = if input {
        backend::open_input(device, &cfg, |_: &[f32]| {}, |_| {})?
    } else {
        backend::open_output(device, &cfg, |out: &mut [f32]| out.fill(0.0), |_| {})?
    };
    drop(stream);
    Ok(())
}

/// Whether a device honours the callback period it is asked for.
///
/// A device can differ from a request in two ways that must not be confused:
///
///   - it ROUNDS — grants a nearby aligned size (240 → 224), and a different request gets a
///     different answer. It honours requests; it just cannot hit them exactly.
///   - it SUBSTITUTES — grants its own fixed size whatever is asked. Dante Virtual
///     Soundcard's WDM endpoints grant 512 for 120, 240 and 480 alike.
///
/// Only the second makes a rebuild for a new period pointless. They are told apart by
/// asking twice: the same answer to two DIFFERENT requests is a device choosing its own
/// period. A single answer proves nothing, so a device starts out assumed to honour
/// requests, and substituting devices cost exactly one extra rebuild before they are known.
///
/// Requests are only ever 120/240/480 (capped), at least a factor of two apart, so two
/// distinct requests cannot both round onto the same aligned size and be misread.
#[derive(Default)]
pub struct PeriodTracker {
    requested: std::sync::atomic::AtomicUsize,
    granted:   std::sync::atomic::AtomicUsize,
    fixed:     std::sync::atomic::AtomicBool,
}

impl PeriodTracker {
    /// Record a build: the period asked for and the one granted.
    pub fn record(&self, requested: usize, granted: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        let prev_requested = self.requested.load(Relaxed);
        let prev_granted = self.granted.load(Relaxed);
        if prev_requested != 0 && prev_requested != requested {
            self.fixed.store(granted == prev_granted, Relaxed);
        }
        self.requested.store(requested, Relaxed);
        self.granted.store(granted, Relaxed);
    }

    /// Forget everything. Called when the device changes or stops — what one device does
    /// says nothing about the next.
    pub fn reset(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.requested.store(0, Relaxed);
        self.granted.store(0, Relaxed);
        self.fixed.store(false, Relaxed);
    }

    /// True once the device has shown it grants its own period regardless of the request.
    pub fn is_fixed(&self) -> bool {
        self.fixed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod shortest_frame_tests {
    use super::shortest_frame_for;

    #[test]
    fn every_frame_size_is_offered_without_a_limit() {
        assert_eq!(shortest_frame_for(0), 120);
        assert_eq!(shortest_frame_for(120), 120);
    }

    /// No frame shorter than the callback, so one callback never carries two frames.
    #[test]
    fn frames_shorter_than_the_callback_are_not_offered() {
        assert_eq!(shortest_frame_for(128), 240);
        assert_eq!(shortest_frame_for(144), 240);
        assert_eq!(shortest_frame_for(256), 480);
        assert_eq!(shortest_frame_for(480), 480);
        assert_eq!(shortest_frame_for(512), 960);   // a 512-frame device: 20 ms only
    }

    #[test]
    fn twenty_milliseconds_is_always_offered() {
        assert_eq!(shortest_frame_for(1024), 960);
        assert_eq!(shortest_frame_for(4096), 960);
    }
}

#[cfg(test)]
mod smallest_request_tests {
    /// No buffer setting asks for a shorter period than the smallest request.
    #[test]
    fn no_buffer_setting_requests_a_shorter_period() {
        for buffer_ms in 0u32..=10_000 {
            assert!(super::channel_sync::receive_half_for_buffer(buffer_ms)
                    >= super::SMALLEST_PERIOD_REQUEST, "buffer {buffer_ms} ms");
        }
        assert_eq!(super::channel_sync::receive_half_for_buffer(5), super::SMALLEST_PERIOD_REQUEST);
    }
}

#[cfg(test)]
mod period_tracker_tests {
    use super::PeriodTracker;

    /// DVS-style: the same size back for every request.
    #[test]
    fn a_device_granting_its_own_period_is_fixed_after_two_requests() {
        let t = PeriodTracker::default();
        t.record(480, 512);
        assert!(!t.is_fixed(), "one answer proves nothing");
        t.record(240, 512);
        assert!(t.is_fixed());
    }

    /// Realtek-style: rounds to an aligned size, but a different request moves the answer.
    #[test]
    fn a_device_that_rounds_is_not_fixed() {
        let t = PeriodTracker::default();
        t.record(480, 480);
        t.record(240, 224);
        assert!(!t.is_fixed());
        t.record(480, 480);
        assert!(!t.is_fixed(), "and it must be allowed back to the original period");
    }

    #[test]
    fn repeating_the_same_request_is_no_new_information() {
        let t = PeriodTracker::default();
        t.record(480, 512);
        t.record(480, 512);
        assert!(!t.is_fixed());
    }

    #[test]
    fn reset_forgets_a_previous_device() {
        let t = PeriodTracker::default();
        t.record(480, 512);
        t.record(240, 512);
        assert!(t.is_fixed());
        t.reset();
        assert!(!t.is_fixed());
        t.record(240, 240);
        assert!(!t.is_fixed());
    }
}

/// Read back the callback period the backend actually granted, and report it.
///
/// Returns the EFFECTIVE period: the granted size when it differs from the request, the
/// request otherwise. Never fails.
///
/// A substituted PERIOD is a cadence and latency difference, not a topology substitution,
/// so it does not engage Cascade's no-resampling policy — hence the warning rather than an
/// error. The RATE half of that policy is absolute and is enforced up front by
/// `backend::find_config`; nothing here relaxes it.
///
/// The read-back is worth doing because the period is genuinely not guaranteed off
/// CoreAudio:
///
///   - ALSA rounds to the nearest supported period and reports success — it requests
///     but does not guarantee a specific callback size.
///   - ASIO uses ONE buffer size for the whole driver and, if a stream already exists,
///     returns that size and discards the request outright. Since we deliberately prepare
///     both units before starting either, the second one always inherits the first's.
///   - ASIO drivers also commonly expose power-of-two sizes only, and none of 120/240/480
///     is a power of two.
///
/// So the value of this call is that a substitution is VISIBLE rather than silent. A
/// backend that cannot report its period at all is allowed through with one warning, for
/// the same reason.
pub fn verify_period(stream: &Stream, requested: usize, what: &str) -> usize {
    match backend::granted_period(stream) {
        Some(actual) if actual == requested => requested,
        Some(actual) => {
            tracing::warn!("{what}: requested a {requested}-frame callback period ({:.2} ms at \
                            48 kHz), backend granted {actual} ({:.2} ms) — continuing at the \
                            granted size.",
                           requested as f32 / 48.0, actual as f32 / 48.0);
            actual
        }
        None => {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!("{what}: this backend cannot report its callback period, so the \
                                requested {requested} frames is UNVERIFIED. If audio is wrong, \
                                suspect a silently substituted period first.");
            }
            requested
        }
    }
}
