//! Windows — the direct WASAPI backend, exclusive mode.
//!
//! This is the Windows backend. macOS is served by `coreaudio_backend` and Linux by
//! `alsa_backend`; there is no portable layer under any of them.
//!
//! ## Exclusive mode is the policy, not a preference
//!
//! `autoconvert` exists only on WASAPI's SHARED stream modes. Exclusive mode cannot
//! convert, so "the backend provides the requested topology or configuration fails" is
//! enforced by the type rather than by remembering to pass `false`. Shared mode would
//! route through the system mixer, resample to whatever the Sound control panel says and
//! cap the channel count at the mixer format — all three invisible from the API, and all
//! three forbidden. **There is no fallback to shared mode.** A device that will not open
//! exclusively fails configuration and says why.
//!
//! ## COM
//!
//! Every thread that touches WASAPI initialises COM into the multithreaded apartment
//! first. COM objects are apartment-bound, so a `Device` is never carried across threads:
//! `Device` here holds the endpoint's ID STRING, and the I/O thread re-resolves it after
//! its own `initialize_mta`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use wasapi::{
    Direction as WDirection, DeviceCollection, DeviceEnumerator, DeviceState, SampleType,
    StreamMode, WaveFormat,
};

// ── Tunables ──────────────────────────────────────────────────────────────────

/// Byte alignment used ONLY after a device has refused an unaligned period.
///
/// Some drivers require the period to land on a byte boundary and return
/// `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` otherwise. Forcing this alignment up front is the
/// wrong trade: it rounds the period away from what was asked for even on devices that
/// would have accepted it — 120 frames (2.5 ms) at 2ch/32-bit is 960 bytes, which is not a
/// multiple of 128, so a blanket alignment turns it into 144 frames (3.0 ms). The sync
/// mechanism's period tables are exact-match on 120/240/480, so a substituted period is not
/// a cosmetic difference.
///
/// So: ask for the period we want, and only align if the device actually objects.
const PERIOD_ALIGN_BYTES: u32 = 128;

/// How a stream learns that the device wants servicing.
///
/// EVENTS is the only mode used unless a driver refuses it. The driver signals a kernel
/// event once per period and the thread blocks on it, so it wakes exactly when the audio is
/// due and never otherwise.
///
/// POLLING is the fallback for drivers that implement exclusive mode but not its
/// event-driven form — nothing signals us, so the thread has to ask repeatedly whether the
/// device is ready. Dante Virtual Soundcard's WDM endpoints are one: they accept the
/// format for exclusive mode, refuse `Initialize` with the event flag at every period
/// including their own, and accept the same period polled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Timing {
    Events,
    Polling,
}

/// Fraction of a period between polls. A quarter bounds the added jitter at 2.5 ms on a
/// 10 ms period for ~400 wakeups a second, each a single cheap query, on a thread already
/// running at MMCSS "Audio".
const POLL_DIVISOR: i64 = 4;

/// How long the event loop waits before looking at the command state again. Bounded so a
/// stop is acted on promptly even if the device stops signalling.
const EVENT_TIMEOUT_MS: u32 = 200;

/// 100-nanosecond units per second — WASAPI expresses every duration in these.
const HNS_PER_SEC: i64 = 10_000_000;

// ── Devices ───────────────────────────────────────────────────────────────────

/// One WASAPI endpoint.
///
/// Holds the endpoint ID rather than the COM object: `IMMDevice` is apartment-bound, so
/// carrying one to the I/O thread would be wrong. The ID is also exactly what a saved
/// configuration should store, so `device_uid` is free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    id: String,
    name: String,
}

/// Initialise COM for this thread. Safe to call repeatedly — a second call on a thread
/// already in the MTA returns `S_FALSE` rather than failing.
fn com() {
    let _ = wasapi::initialize_mta();
}

fn collection(dir: WDirection) -> anyhow::Result<DeviceCollection> {
    com();
    let enumerator = DeviceEnumerator::new()
        .map_err(|e| anyhow::anyhow!("cannot create the WASAPI device enumerator: {e}"))?;
    enumerator
        .get_device_collection(&dir)
        .map_err(|e| anyhow::anyhow!("cannot enumerate WASAPI devices: {e}"))
}

fn enumerate(dir: WDirection) -> Vec<Device> {
    let Ok(coll) = collection(dir) else {
        return Vec::new();
    };
    let Ok(n) = coll.get_nbr_devices() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..n {
        let Ok(dev) = coll.get_device_at_index(i) else { continue };
        // Unplugged-but-remembered endpoints are enumerated by Windows and would otherwise
        // appear in the picker as selectable devices that cannot open.
        if !matches!(dev.get_state(), Ok(DeviceState::Active)) {
            continue;
        }
        let (Ok(id), Ok(name)) = (dev.get_id(), dev.get_friendlyname()) else { continue };
        out.push(Device { id, name });
    }
    out
}

pub fn output_devices() -> Vec<Device> {
    enumerate(WDirection::Render)
}

pub fn input_devices() -> Vec<Device> {
    enumerate(WDirection::Capture)
}

pub fn device_name(dev: &Device) -> String {
    dev.name.clone()
}

/// The endpoint ID — already the stable identifier Windows itself uses, so a saved
/// selection survives reboots and renaming.
pub fn device_uid(dev: &Device) -> Option<String> {
    Some(dev.id.clone())
}

/// Every enumerated entry is a real endpoint, filtered to `DeviceState::Active` above.
pub fn is_hardware_endpoint(_dev: &Device) -> bool {
    true
}

/// Re-resolve the COM object for this endpoint on the CURRENT thread.
fn resolve(dev: &Device) -> anyhow::Result<wasapi::Device> {
    com();
    let enumerator = DeviceEnumerator::new()
        .map_err(|e| anyhow::anyhow!("cannot create the WASAPI device enumerator: {e}"))?;
    enumerator
        .get_device(&dev.id)
        .map_err(|e| anyhow::anyhow!("cannot open device '{}': {e}", dev.name))
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Which direction a device is being opened for.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Dir {
    Input,
    Output,
}

impl Dir {
    fn label(self) -> &'static str {
        match self {
            Dir::Input => "input",
            Dir::Output => "output",
        }
    }
    fn wasapi(self) -> WDirection {
        match self {
            Dir::Input => WDirection::Capture,
            Dir::Output => WDirection::Render,
        }
    }
}

/// The device sample formats accepted, in descending preference.
///
/// f32 is what the pipeline works in. Exclusive-mode endpoints frequently refuse it and
/// offer only integer formats, so those are accepted and converted at this boundary — a
/// BIT-DEPTH change, never a rate or channel-count one, so it does not engage the
/// no-resampling rule (`PLATFORM_WINDOWS_WASAPI.md` O1(b)).
///
/// **The two 24-bit cases are not interchangeable.**
///
///   * `S24In4` is 24 valid bits in a 32-bit container. `WAVEFORMATEXTENSIBLE` specifies
///     the valid bits as the MOST significant ones with the unused low bits zero, so this
///     scales by 2^31 exactly like `S32` — it needs no separate divisor, only its own
///     `WaveFormat`. This is the form WASAPI exclusive devices most commonly use.
///   * `S24In3` is packed into three bytes and scales by 2^23.
///
/// The divisor travels with the format rather than being derived from a bit count, because
/// getting the justification wrong is a 48 dB error, not a subtle one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Fmt {
    F32,
    S32,
    S24In4,
    S24In3,
    S16,
}

impl Fmt {
    /// `(storebits, validbits, sample type)`.
    fn shape(self) -> (usize, usize, SampleType) {
        match self {
            Fmt::F32 => (32, 32, SampleType::Float),
            Fmt::S32 => (32, 32, SampleType::Int),
            Fmt::S24In4 => (32, 24, SampleType::Int),
            Fmt::S24In3 => (24, 24, SampleType::Int),
            Fmt::S16 => (16, 16, SampleType::Int),
        }
    }
    fn bytes_per_sample(self) -> usize {
        self.shape().0 / 8
    }
    fn name(self) -> &'static str {
        match self {
            Fmt::F32 => "F32",
            Fmt::S32 => "S32",
            Fmt::S24In4 => "S24-in-32 (valid bits left-justified)",
            Fmt::S24In3 => "S24 packed",
            Fmt::S16 => "S16",
        }
    }
    fn wave_format(self, rate: u32, channels: u16) -> WaveFormat {
        let (store, valid, ty) = self.shape();
        WaveFormat::new(store, valid, &ty, rate as usize, channels as usize, None)
    }
}

const FORMAT_PREFERENCE: [Fmt; 5] = [Fmt::F32, Fmt::S32, Fmt::S24In4, Fmt::S24In3, Fmt::S16];

/// A negotiated, ready-to-open configuration. Opaque above the backend.
#[derive(Clone, Debug)]
pub struct Config {
    channels: u16,
    sample_rate: u32,
    /// The aligned device period, in 100 ns units — what `EventsExclusive` is given.
    period_hns: i64,
    /// The period in frames, which is what the layer above reasons in.
    period: usize,
    fmt: Fmt,
}

impl Config {
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// A fault reported by a live stream, classified by what it means for the caller.
pub enum StreamFault {
    /// The hardware genuinely disappeared. Drive the device-lost path.
    DeviceLost,
    /// The stream is no longer valid but the DEVICE is still present. The remedy is a
    /// rebuild, not a teardown.
    Invalidated,
    /// Anything else: log it and keep the device.
    Transient(String),
}

/// The device-busy marker, attached as an anyhow SOURCE so it survives being wrapped in
/// context. An exclusive-mode client owns the endpoint, so the rebuild paths downcast to
/// this to tell Cascade's own stream still holding the device from a third-party conflict.
#[derive(Debug)]
pub struct DeviceBusy;

impl std::fmt::Display for DeviceBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("device is busy")
    }
}
impl std::error::Error for DeviceBusy {}

pub fn is_device_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<DeviceBusy>().is_some())
}

/// `AUDCLNT_E_DEVICE_IN_USE` — an exclusive-mode client already owns this endpoint.
const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
/// `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED` — exclusive mode is disabled for the endpoint in
/// the Sound control panel.
const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889_0014_u32 as i32;
/// `AUDCLNT_E_DEVICE_INVALIDATED` — the endpoint went away, or its format changed.
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;

/// The HRESULT carried by a wasapi error, if it carries one.
fn hresult_of(e: &wasapi::WasapiError) -> Option<i32> {
    // The crate wraps windows-core errors; the code is only reachable through the message
    // for some variants, so match the typed case and fall back to None rather than parsing.
    match e {
        wasapi::WasapiError::Windows(w) => Some(w.code().0),
        _ => None,
    }
}

fn open_error(dev: &Device, dir: Dir, e: wasapi::WasapiError) -> anyhow::Error {
    let code = hresult_of(&e);
    let name = &dev.name;
    match code {
        Some(AUDCLNT_E_DEVICE_IN_USE) => anyhow::Error::new(DeviceBusy).context(format!(
            "cannot open {} device '{name}' exclusively: it is already held — either by \
             another application, or by Cascade's own stream on the same endpoint \
             mid-rebuild",
            dir.label()
        )),
        Some(AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED) => anyhow::anyhow!(
            "{} device '{name}': exclusive mode is disabled for this endpoint. Enable \
             \"Allow applications to take exclusive control of this device\" in Sound → \
             Device properties → Advanced. Cascade does not fall back to shared mode, \
             because shared mode resamples.",
            dir.label()
        ),
        _ => anyhow::anyhow!("cannot open {} device '{name}': {e}", dir.label()),
    }
}

/// Negotiate 48 kHz at the requested period, preferring f32, in EXCLUSIVE mode.
///
/// The two directions negotiate independently because they are genuinely different
/// endpoints, and a card can be integer-only for render and f32 for capture.
pub fn find_config(device: &Device, dir: Dir, period_frames: usize) -> anyhow::Result<Config> {
    let dev = resolve(device)?;
    let client = dev
        .get_iaudioclient()
        .map_err(|e| open_error(device, dir, e))?;

    // Channel count is whatever the endpoint's own mix format declares. Exclusive mode does
    // not cap it at the mixer's, which is precisely why shared mode is unusable here.
    let channels = dev
        .get_device_format()
        .map_err(|e| anyhow::anyhow!("cannot read the device format for '{}': {e}", device.name))?
        .get_nchannels();

    let desired_hns = period_frames as i64 * HNS_PER_SEC / 48_000;

    let mut last: Option<String> = None;
    for fmt in FORMAT_PREFERENCE {
        let want = fmt.wave_format(48_000, channels);
        // Refuses rather than substituting — the assertion the no-resampling rule needs.
        let granted = match client.is_supported_exclusive_with_quirks(&want) {
            Ok(g) => g,
            Err(e) => {
                last = Some(format!("{}: {e}", fmt.name()));
                continue;
            }
        };
        // The quirks helper may hand back a format that differs from the request. Rate and
        // channel count are topology and must be exact; bit depth is allowed to move,
        // because that is the whole point of the preference list.
        if granted.get_samplespersec() != 48_000 || granted.get_nchannels() != channels {
            last = Some(format!(
                "{}: device offered {} Hz / {} ch instead of 48000 Hz / {} ch",
                fmt.name(),
                granted.get_samplespersec(),
                granted.get_nchannels(),
                channels
            ));
            continue;
        }
        // `None` clamps to the device's own minimum period and nothing more — no forced
        // byte alignment. If the device turns out to need it, `initialize_exclusive`
        // handles that on the refusal rather than pre-emptively.
        let period_hns = client
            .calculate_aligned_period_near(desired_hns, None, &granted)
            .map_err(|e| {
                anyhow::anyhow!("cannot align the period for '{}': {e}", device.name)
            })?;
        let period = (period_hns * 48_000 / HNS_PER_SEC) as usize;
        tracing::debug!(
            "WASAPI {} '{}': {} ch, 48 kHz, {}, period {} frames ({:.2} ms, aligned)",
            dir.label(),
            device.name,
            channels,
            fmt.name(),
            period,
            period as f64 / 48.0
        );
        return Ok(Config { channels, sample_rate: 48_000, period_hns, period, fmt });
    }

    Err(anyhow::anyhow!(
        "{} device '{}' does not support 48 kHz in exclusive mode in any accepted format \
         ({}) — Cascade does not fall back to shared mode, because shared mode resamples{}",
        dir.label(),
        device.name,
        FORMAT_PREFERENCE.iter().map(|f| f.name()).collect::<Vec<_>>().join(", "),
        last.map(|e| format!(". Last refusal — {e}")).unwrap_or_default()
    ))
}


/// Initialise an exclusive client at `period_hns`, aligning only if the device refuses.
///
/// The alignment dance is Microsoft's documented recovery: on
/// `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` the client cannot be reused, so a fresh one is taken
/// from the device and initialised at a period the driver will accept. Doing this on
/// refusal rather than up front is what lets a device that accepts 120 frames actually get
/// 120 frames.
///
/// A sub-millisecond sleep that does not depend on the system timer tick.
///
/// `thread::sleep` honours the scheduler tick, 15.6 ms by default on Windows, so a 2.5 ms
/// request can take 15.6 ms. Polling a 10 ms audio period on that would be late on every
/// cycle — continuous dropouts, and nothing in the log would point at the timer. A
/// high-resolution waitable timer (Windows 10 1803+) is scheduled by the kernel rather than
/// rounded to the tick.
///
/// This is a WAIT, not a spin: the thread is descheduled until the timer fires, so it costs
/// wakeups but no CPU.
struct HrTimer(windows::Win32::Foundation::HANDLE);

impl HrTimer {
    /// `None` when a high-resolution timer is unavailable — the caller must then not poll,
    /// because the alternative wait is the one that cannot keep time.
    fn new() -> Option<Self> {
        use windows::Win32::System::Threading::{
            CreateWaitableTimerExW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
        };
        // SAFETY: a null name and no security attributes are valid; the returned handle is
        // owned by this struct and closed exactly once in Drop.
        unsafe {
            CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                                   TIMER_ALL_ACCESS.0)
                .ok()
                .map(HrTimer)
        }
    }

    /// Block for `hns` (100 ns units). Returns false if the wait could not be armed, which
    /// the caller treats as a lost period.
    fn wait(&self, hns: i64) -> bool {
        use windows::Win32::System::Threading::{SetWaitableTimer, WaitForSingleObject};
        // Negative = relative time, which is what a delay is.
        let due = -hns;
        // SAFETY: `due` outlives the call; the handle is valid for the life of self.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_err() {
                // A failed arm must still yield the thread. Returning at once would have
                // every caller loop straight back round — a hot spin on a real-time thread.
                std::thread::sleep(std::time::Duration::from_millis(1));
                return false;
            }
            // Bounded, so a timer that never signals cannot hold this thread past the point
            // where it would notice a stop request.
            let ms = u32::try_from(hns / 10_000).unwrap_or(0).saturating_add(50);
            WaitForSingleObject(self.0, ms);
        }
        true
    }
}

impl Drop for HrTimer {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateWaitableTimerExW and is closed once.
        unsafe { let _ = windows::Win32::Foundation::CloseHandle(self.0); }
    }
}

/// Open an exclusive client, trying the periods the device will actually accept.
///
/// THE FORMAT NEVER MOVES. Sample rate, channel count and bit depth were agreed by
/// `find_config` and are passed through untouched — nothing here can change what the audio
/// sounds like. Only the SIZE OF EACH CALLBACK moves, and a callback that is not a whole
/// number of wire frames costs nothing in quality: the capture accumulator drains exact
/// frames and carries the remainder (`encode.rs`), so every sample is used once, in order,
/// unmodified. What a mismatched period does cost is determinism — callback and frame
/// boundaries stop coinciding and re-align only every LCM(period, frame) samples.
///
/// Three candidates, in order:
///
///   1. **The requested period** — one callback per wire frame, which is the point.
///   2. **A byte-aligned period**, when the device says the request is misaligned.
///   3. **The device's own default, then its minimum**, from `GetDevicePeriod`.
///
/// (3) exists for drivers that accept a format in `IsFormatSupported` and then refuse to
/// initialise at a period they never advertised — Dante Virtual Soundcard's WDM endpoints,
/// for one, return `E_INVALIDARG` rather than the documented alignment error.
/// Asking the device what it wants is more honest than insisting on what we want.
///
/// Returns the client and the period it was actually initialised at; the caller uses that
/// rather than what it asked for.
fn initialize_exclusive(
    dev: &wasapi::Device,
    device: &Device,
    dir: Dir,
    fmt: &WaveFormat,
    period_hns: i64,
    // False for a trial initialise (`trial_initialize`), which opens the device only to see
    // what it will accept. Without this every device is announced twice: once tried, once
    // opened for real.
    announce: bool,
) -> anyhow::Result<(wasapi::AudioClient, i64, Timing)> {
    let mut attempts: Vec<(i64, &'static str)> = vec![(period_hns, "requested")];
    let mut first_err: Option<wasapi::WasapiError> = None;
    let mut i = 0;

    while i < attempts.len() {
        let (p, why) = attempts[i];
        // A refused exclusive client is dead and cannot be reused, so each attempt gets a
        // fresh one — and the previous must be dropped first, because an exclusive client
        // owns the endpoint.
        let mut client = dev.get_iaudioclient().map_err(|e| open_error(device, dir, e))?;
        match client.initialize_client(fmt, &dir.wasapi(), &StreamMode::EventsExclusive {
            period_hns: p,
        }) {
            Ok(()) => {
                if i > 0 {
                    if announce {
                        tracing::warn!("WASAPI {} '{}': {}-frame period refused — using the {} \
                                        period of {} frames",
                                       dir.label(), device.name,
                                       period_hns * 48_000 / HNS_PER_SEC,
                                       why, p * 48_000 / HNS_PER_SEC);
                    }
                }
                return Ok((client, p, Timing::Events));
            }
            Err(e) => {
                if first_err.is_none() { first_err = Some(e); }
                drop(client);
                // Queue the fallbacks once, after the first refusal — asking the device for
                // its own periods needs a client, and there is no point before a failure.
                if i == 0 {
                    if let Ok(probe) = dev.get_iaudioclient() {
                        if let Ok(a) = probe.calculate_aligned_period_near(
                            period_hns, Some(PERIOD_ALIGN_BYTES), fmt)
                        {
                            if a != period_hns { attempts.push((a, "byte-aligned")); }
                        }
                        if let Ok((default_hns, min_hns)) = probe.get_device_period() {
                            if default_hns != period_hns {
                                attempts.push((default_hns, "default"));
                            }
                            if min_hns != default_hns && min_hns != period_hns {
                                attempts.push((min_hns, "minimum"));
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }

    // Every candidate refused. Say WHICH were tried, at WARN — without this the failure
    // path is indistinguishable from having tried only the requested period, and the
    // question "is it the period or is it exclusive mode?" cannot be answered from a log.
    //
    // Reaching here having tried the device's OWN default and minimum is the answer to that
    // question: the period was never the problem.
    // FALLBACK: exclusive mode with POLLED timing.
    //
    // `EventsExclusive` needs the driver to implement exclusive mode WITH event callbacks.
    // A WDM virtual driver that implements exclusive polling only would refuse every period
    // exactly as seen here, with E_INVALIDARG and no policy error — so this distinguishes
    // "cannot do exclusive" from "cannot do event-driven exclusive", which no amount of
    // period or format negotiation can.
    //
    // Tried only after every event-driven period has failed, so a device that supports
    // events never lands here and nothing that works today changes.
    //
    // Polling is strictly worse — the thread wakes on a timer and asks whether the device is
    // ready, rather than being told — so it is a last resort before "this device does not
    // work at all", not a preference.
    for (p, why) in attempts.iter() {
        let Ok(mut polled) = dev.get_iaudioclient() else { break };
        match polled.initialize_client(fmt, &dir.wasapi(), &StreamMode::PollingExclusive {
            buffer_duration_hns: *p,
            period_hns: *p,
        }) {
            Ok(()) => {
                if announce {
                    tracing::warn!("WASAPI {} '{}': event-driven exclusive refused — polling \
                                    at {} frames",
                                   dir.label(), device.name, p * 48_000 / HNS_PER_SEC);
                }
                return Ok((polled, *p, Timing::Polling));
            }
            Err(e) => {
                tracing::debug!("WASAPI {} '{}': polled exclusive also refused at the {} \
                                 period: {e}", dir.label(), device.name, why);
            }
        }
    }

    let tried = attempts.iter()
        .map(|(p, why)| format!("{} ({} frames)", why, p * 48_000 / HNS_PER_SEC))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!("WASAPI {} '{}': exclusive initialise refused at every period — {}",
                   dir.label(), device.name, tried);
    Err(open_error(device, dir, first_err.expect("at least one attempt was made")))
}

// ── Streams ───────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Cmd {
    Prepared,
    Running,
    Stopped,
    Dead,
}

struct Shared {
    cmd: Mutex<Cmd>,
    wake: Condvar,
    faults_detached: AtomicBool,
    /// Last `IAudioSessionEvents::OnSessionDisconnected` reason, as a `dis::*` code, or
    /// `dis::NONE`. Written from a WASAPI-owned notification thread, read by the I/O loop.
    ///
    /// An atomic rather than a channel because the callback must return immediately —
    /// Microsoft's contract for this interface forbids blocking, and releasing the session
    /// control from inside the callback deadlocks. Storing one byte satisfies both.
    disconnect: std::sync::atomic::AtomicU8,
}

/// A configured stream and the thread that runs it.
///
/// WASAPI has no callback registration: an event handle is signalled and we wait on it, so
/// the stream IS a thread. It parks in `Prepared` until `start`, which is what lets the
/// caller configure both endpoints before either runs — the ordering §5.2 requires.
///
/// All COM work happens inside that thread, including resolving the device, because COM
/// objects are apartment-bound.
pub struct Stream {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
    period: usize,
}

impl Stream {
    fn set(&self, c: Cmd) {
        let mut g = self.shared.cmd.lock().unwrap();
        if *g != Cmd::Dead {
            *g = c;
        }
        self.shared.wake.notify_all();
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.set(Cmd::Dead);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn granted_period(stream: &Stream) -> Option<usize> {
    Some(stream.period)
}

/// The period a trial open of a device asks for. Any size will do: the period belongs to the
/// endpoint client being opened, and capture and render are separate endpoints, so a trial
/// touches no other open stream.
pub fn probe_period(_dev: &Device, _dir: Dir) -> Option<usize> {
    Some(crate::audio::encode::IO_BUF_CAP_FRAMES)
}

/// The shortest period the device will run at, in frames: the period it grants for
/// `audio::SMALLEST_PERIOD_REQUEST`.
///
/// Learned by initialising an exclusive client at that request and releasing it, because a
/// driver can accept a format and still refuse or replace the period it is asked for (see
/// `initialize_exclusive`): only an initialise shows the period a stream would run at. The
/// endpoint must not be held by a stream when this is called.
pub fn smallest_period(device: &Device, dir: Dir) -> anyhow::Result<Option<usize>> {
    let cfg = find_config(device, dir, crate::audio::SMALLEST_PERIOD_REQUEST)?;
    trial_initialize(device, dir, &cfg).map(Some)
}

/// Initialise an exclusive client for `cfg` and release it again, returning the period the
/// device granted, in frames.
///
/// The client is dropped before this returns: an exclusive client owns the endpoint, so it
/// cannot be held while anything else opens it.
fn trial_initialize(device: &Device, dir: Dir, cfg: &Config) -> anyhow::Result<usize> {
    let dev = resolve(device)?;
    let fmt = cfg.fmt.wave_format(cfg.sample_rate, cfg.channels);
    let (client, _actual_hns, _timing) =
        initialize_exclusive(&dev, device, dir, &fmt, cfg.period_hns, false)?;
    // The size the device GRANTED, which is not necessarily the one asked for:
    // `calculate_aligned_period_near` rounds to the device's alignment, the driver may round
    // again, and `initialize_exclusive` may have fallen back to another period entirely.
    let frames = client
        .get_buffer_size()
        .map_err(|e| anyhow::anyhow!("cannot read the WASAPI buffer size: {e}"))?;
    Ok(frames as usize)
}

pub fn start(stream: &Stream) -> anyhow::Result<()> {
    stream.set(Cmd::Running);
    Ok(())
}

pub fn stop(stream: &Stream) -> anyhow::Result<()> {
    stream.set(Cmd::Stopped);
    Ok(())
}

pub fn detach_faults(stream: &mut Stream) {
    stream.shared.faults_detached.store(true, Ordering::Relaxed);
}

/// Tell the host not to trade latency for power. WASAPI exposes no such hint — the
/// CoreAudio `kAudioHardwarePropertyPowerHint` has no counterpart. Thread scheduling is
/// handled instead by MMCSS, applied inside the I/O thread.
pub fn set_power_hint() {}

/// `OnSessionDisconnected` reasons, as codes small enough for one atomic.
mod dis {
    pub const NONE: u8 = 0;
    pub const DEVICE_REMOVAL: u8 = 1;
    pub const SERVER_SHUTDOWN: u8 = 2;
    pub const FORMAT_CHANGED: u8 = 3;
    pub const SESSION_LOGOFF: u8 = 4;
    pub const SESSION_DISCONNECTED: u8 = 5;
    pub const EXCLUSIVE_OVERRIDE: u8 = 6;
    pub const UNKNOWN: u8 = 7;

    pub fn code(r: &wasapi::DisconnectReason) -> u8 {
        use wasapi::DisconnectReason as R;
        match r {
            R::DeviceRemoval        => DEVICE_REMOVAL,
            R::ServerShutdown       => SERVER_SHUTDOWN,
            R::FormatChanged        => FORMAT_CHANGED,
            R::SessionLogoff        => SESSION_LOGOFF,
            R::SessionDisconnected  => SESSION_DISCONNECTED,
            R::ExclusiveModeOverride => EXCLUSIVE_OVERRIDE,
            _                       => UNKNOWN,
        }
    }

    /// What the reason means for the caller. This is the whole point of the callback:
    /// `AUDCLNT_E_DEVICE_INVALIDATED` alone cannot distinguish these, so without it every
    /// cause below got one remedy — a rebuild — including the one case where a rebuild is
    /// certain to fail because the hardware is gone.
    pub fn fault(code: u8) -> Option<super::StreamFault> {
        use super::StreamFault as SF;
        Some(match code {
            NONE => return None,
            // The hardware is gone. A rebuild would re-run the config path against a device
            // that no longer exists; the device-lost teardown is the correct response.
            DEVICE_REMOVAL => SF::DeviceLost,
            // The device is present and its format moved under the exclusive stream. The
            // rebuild re-asserts 48 kHz, which is what pulls an externally-changed rate
            // back — the case the Invalidated path was written for.
            FORMAT_CHANGED => SF::Invalidated,
            // audiosrv restarted. The endpoint returns on its own, so a rebuild is right;
            // it may take a moment, and the retry path already handles a failed attempt.
            SERVER_SHUTDOWN => SF::Invalidated,
            // Another application took the endpoint. Rebuilding cannot win it back, so say
            // so plainly rather than looping on an open that will keep failing with
            // AUDCLNT_E_DEVICE_IN_USE. Cascade opens exclusive-only (D4), so this arrives
            // when something else claims exclusive access, not when we displace a shared one.
            EXCLUSIVE_OVERRIDE =>
                SF::Transient("another application took exclusive control of this device".into()),
            // Terminal-services events. Not a hardware fault and not something to tear a
            // device down over.
            SESSION_LOGOFF =>
                SF::Transient("the Windows session logged off".into()),
            SESSION_DISCONNECTED =>
                SF::Transient("the Windows session was disconnected".into()),
            // Anything the SDK adds later: keep the pre-callback behaviour.
            _ => SF::Invalidated,
        })
    }
}

/// Classify a WASAPI error, refined by any disconnect reason the session callback recorded.
///
/// The reason is strictly better information than the HRESULT: a removal, a format change
/// and a stolen endpoint all surface as `AUDCLNT_E_DEVICE_INVALIDATED` on the next call.
/// When a reason is present it wins; the HRESULT mapping stays as the fallback for faults
/// that arrive with no disconnect at all.
fn classify(e: &wasapi::WasapiError, shared: &Shared) -> StreamFault {
    if let Some(f) = dis::fault(shared.disconnect.load(Ordering::Relaxed)) {
        return f;
    }
    match hresult_of(e) {
        // The endpoint went away, or its format changed under an exclusive stream. Both are
        // a rebuild rather than a teardown: the rebuild re-runs the config path, which
        // re-asserts 48 kHz exclusive.
        Some(AUDCLNT_E_DEVICE_INVALIDATED) => StreamFault::Invalidated,
        Some(AUDCLNT_E_DEVICE_IN_USE) => StreamFault::DeviceLost,
        _ => StreamFault::Transient(e.to_string()),
    }
}

enum Io {
    Render(Box<dyn FnMut(&mut [f32]) + Send>),
    Capture(Box<dyn FnMut(&[f32]) + Send>),
}

/// Open an output stream, PREPARED BUT NOT STARTED.
pub fn open_output<R, F>(
    device: &Device,
    cfg: &Config,
    render: R,
    on_fault: F,
) -> anyhow::Result<Stream>
where
    R: FnMut(&mut [f32]) + Send + 'static,
    F: FnMut(StreamFault) + Send + 'static,
{
    spawn(device, Dir::Output, cfg, Io::Render(Box::new(render)), Box::new(on_fault))
}

/// Open an input stream, PREPARED BUT NOT STARTED. See `open_output`.
pub fn open_input<C, F>(
    device: &Device,
    cfg: &Config,
    capture: C,
    on_fault: F,
) -> anyhow::Result<Stream>
where
    C: FnMut(&[f32]) + Send + 'static,
    F: FnMut(StreamFault) + Send + 'static,
{
    spawn(device, Dir::Input, cfg, Io::Capture(Box::new(capture)), Box::new(on_fault))
}

/// Everything the I/O thread needs to build its own COM objects.
struct Setup {
    device: Device,
    dir: Dir,
    cfg: Config,
}

fn spawn(
    device: &Device,
    dir: Dir,
    cfg: &Config,
    mut io: Io,
    mut on_fault: Box<dyn FnMut(StreamFault) + Send>,
) -> anyhow::Result<Stream> {
    // Prove the configuration opens BEFORE returning a stream, so a failure is reported to
    // the caller rather than disappearing into a worker. The trial client is released before
    // the I/O thread opens its own.
    //
    // The period reported through `granted_period` is the one this trial was granted, not
    // the request: reporting the request would make `audio::verify_period` compare a value
    // against itself and never warn — defeating the one check that exists to make a
    // substituted period visible.
    let granted_frames = trial_initialize(device, dir, cfg)?;

    let shared = Arc::new(Shared {
        cmd: Mutex::new(Cmd::Prepared),
        wake: Condvar::new(),
        faults_detached: AtomicBool::new(false),
        disconnect: std::sync::atomic::AtomicU8::new(dis::NONE),
    });
    let thread_shared = Arc::clone(&shared);
    let setup = Setup { device: device.clone(), dir, cfg: cfg.clone() };
    if granted_frames != cfg.period {
        tracing::debug!(
            "WASAPI {} '{}': asked for a {}-frame period, device granted {}",
            dir.label(), device.name, cfg.period, granted_frames);
    }

    // The I/O thread opens its own client, and that open can fail even though the probe
    // above succeeded — another application can take the endpoint exclusively in between.
    // So the thread reports its setup result here, and this does not return a stream until
    // it has: a failed setup is the caller's error, with the busy marker intact, rather than
    // a fault reported from a thread that has already exited and a stream that is silently
    // dead. Everything after setup is reported through `on_fault` as before.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
    let handle = std::thread::Builder::new()
        .name(format!("cascade-wasapi-{}", dir.label()))
        .spawn(move || {
            let _mmcss = mmcss::ProAudio::acquire();
            let mut report = |fault: StreamFault| {
                if !thread_shared.faults_detached.load(Ordering::Relaxed) {
                    on_fault(fault);
                }
            };
            // `run` returns an error only from setup: once running it reports faults and
            // returns Ok when the stream is closed.
            if let Err(e) = run(&setup, &thread_shared, &mut io, &mut report, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Stream { shared, handle: Some(handle), period: granted_frames }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => {
            let _ = handle.join();
            Err(anyhow::anyhow!("the WASAPI {} thread for '{}' ended during setup",
                                dir.label(), device.name))
        }
    }
}

/// The I/O thread body. Builds its own COM objects, signals `ready` once they are all in
/// place, then parks until started. Returns an error only from that setup.
fn run(
    setup: &Setup,
    shared: &Arc<Shared>,
    io: &mut Io,
    report: &mut dyn FnMut(StreamFault),
    ready: &std::sync::mpsc::SyncSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let dev = resolve(&setup.device)?;
    let fmt = setup.cfg.fmt.wave_format(setup.cfg.sample_rate, setup.cfg.channels);
    let (client, actual_hns, timing) =
        initialize_exclusive(&dev, &setup.device, setup.dir, &fmt, setup.cfg.period_hns, true)?;

    // Session disconnect notification. Held for the life of this function, and dropped
    // here — on the I/O thread, which is in the MTA because initialize_exclusive put it
    // there — because `EventRegistration::drop` calls UnregisterAudioSessionNotification
    // and that needs COM live on the dropping thread.
    //
    // Best-effort: a stream that cannot register still runs, it just falls back to
    // classifying faults from the HRESULT alone, which is the pre-callback behaviour.
    shared.disconnect.store(dis::NONE, Ordering::Relaxed);
    let _session_reg = match client.get_audiosessioncontrol() {
        Ok(control) => {
            let sink = Arc::clone(shared);
            let mut cbs = wasapi::EventCallbacks::new();
            // Returns immediately: one relaxed store, no allocation, no logging, and it
            // never touches the session control it was registered on. The I/O loop below
            // does the reporting.
            cbs.set_disconnected_callback(move |reason| {
                sink.disconnect.store(dis::code(&reason), Ordering::Relaxed);
            });
            match control.register_session_notification(cbs) {
                Ok(reg) => Some(reg),
                Err(e) => {
                    tracing::debug!("WASAPI {} '{}': no session disconnect notification \
                                     ({e}) — faults classify from the HRESULT alone",
                                    setup.dir.label(), setup.device.name);
                    None
                }
            }
        }
        Err(e) => {
            tracing::debug!("WASAPI {} '{}': no session control ({e}) — faults classify \
                             from the HRESULT alone", setup.dir.label(), setup.device.name);
            None
        }
    };

    // A polled client has no event handle — asking for one fails — so this is the mode's
    // handle, not the stream's: exactly one of `event` and `poll_timer` is Some.
    let event = match timing {
        Timing::Events => Some(client
            .set_get_eventhandle()
            .map_err(|e| anyhow::anyhow!("cannot get the WASAPI event handle: {e}"))?),
        Timing::Polling => None,
    };
    let poll_timer = match timing {
        Timing::Events => None,
        Timing::Polling => Some(HrTimer::new().ok_or_else(|| anyhow::anyhow!(
            "'{}' needs polled timing, but a high-resolution timer is unavailable on this \
             system. Polling on the ordinary scheduler tick would be late on every period.",
            setup.device.name))?),
    };
    // PHASE-LOCKED POLLING.
    //
    // A free-running poll notices the device up to one interval late, and that lateness
    // lands directly on send timing as jitter. A device is regular, though — the same
    // interval every time — so once two readies have been seen the next is predictable:
    // sleep in ONE long wait until just before it is due, then poll finely.
    //
    // Fewer wakeups than a fixed poll AND finer granularity, because almost all of the wait
    // is a single sleep and only the last fraction of a millisecond is spent asking.
    //
    // `coarse_hns` is the fallback cadence before the interval is known, and after a miss.
    let coarse_hns = (actual_hns / POLL_DIVISOR).max(1);
    /// Fine cadence once the device is nearly due: 0.25 ms.
    const FINE_HNS: i64 = 2_500;
    /// Wake this long before the predicted ready and switch to the fine cadence.
    const GUARD: std::time::Duration = std::time::Duration::from_micros(500);
    let mut last_ready: Option<std::time::Instant> = None;
    let mut interval: Option<std::time::Duration> = None;
    /// How often the short-read count is reported, at most.
    const PARTIAL_LOG_SECS: u64 = 10;
    // Capture reads that could not wait for a whole period, and when that was last
    // reported. Per stream, and only ever touched by this thread.
    let mut partial_reads: u64 = 0;
    let mut last_partial_log = std::time::Instant::now();
    let frames = client
        .get_buffer_size()
        .map_err(|e| anyhow::anyhow!("cannot read the WASAPI buffer size: {e}"))?
        as usize;

    let chans = setup.cfg.channels as usize;
    let samples = frames * chans;
    let bytes = samples * setup.cfg.fmt.bytes_per_sample();
    let mut f32_buf: Vec<f32> = vec![0.0; samples];
    let mut byte_buf: Vec<u8> = vec![0; bytes];

    let render_client = match setup.dir {
        Dir::Output => Some(
            client
                .get_audiorenderclient()
                .map_err(|e| anyhow::anyhow!("cannot get the render client: {e}"))?,
        ),
        Dir::Input => None,
    };
    let capture_client = match setup.dir {
        Dir::Input => Some(
            client
                .get_audiocaptureclient()
                .map_err(|e| anyhow::anyhow!("cannot get the capture client: {e}"))?,
        ),
        Dir::Output => None,
    };

    // Setup is complete: from here on nothing returns an error.
    let _ = ready.send(Ok(()));

    let mut running = false;
    // Per-run, and only ever touched by this thread — the latch it guards is shared, the
    // "have I said so yet" is not.
    let mut disconnect_reported = false;
    loop {
        {
            let mut g = shared.cmd.lock().unwrap();
            loop {
                match *g {
                    Cmd::Dead => {
                        if running {
                            let _ = client.stop_stream();
                        }
                        return Ok(());
                    }
                    Cmd::Running => {
                        if !running {
                            // Pre-fill one buffer of silence so the first event has
                            // somewhere to land and the device does not glitch on the
                            // first cycle. The render callback has not run at this point.
                            if let Some(rc) = &render_client {
                                byte_buf.iter_mut().for_each(|b| *b = 0);
                                let _ = rc.write_to_device(frames, &byte_buf, None);
                            }
                            if let Err(e) = client.start_stream() {
                                report(classify(&e, shared));
                                *g = Cmd::Stopped;
                                continue;
                            }
                            running = true;
                        }
                        break;
                    }
                    Cmd::Prepared | Cmd::Stopped => {
                        if running {
                            running = false;
                            let _ = client.stop_stream();
                        }
                        g = shared.wake.wait(g).unwrap();
                    }
                }
            }
        }

        // A disconnect the session callback recorded, reported once per run.
        //
        // Acted on HERE rather than left to the next failing call because a removed
        // endpoint stops signalling its event: the loop would otherwise sit on the 200 ms
        // timeout and only learn of the removal when something else prodded it. This bounds
        // the delay at one timeout.
        //
        // The reason is LATCHED, not consumed. One disconnect surfaces twice — here, and
        // again as an HRESULT on the next call that touches the dead endpoint — and
        // classify() needs it still present to render that second one as the same precise
        // fault rather than a coarser `Invalidated`. Reporting one fault twice is something
        // the caller already absorbs (the lost flag is a store; the rebuild path suppresses
        // its own echo); reporting two DIFFERENT faults for one event is not. `run()`
        // clears the latch on entry, so a rebuilt stream starts clean.
        if !disconnect_reported {
            if let Some(fault) = dis::fault(shared.disconnect.load(Ordering::Relaxed)) {
                disconnect_reported = true;
                report(fault);
            }
        }

        // One period per event. A timeout is not an error: it is how a stopped device is
        // noticed so the command state can be re-read.
        // WAIT FOR THE DEVICE. Exactly one of these runs, decided once at open.
        //
        // Either way a timeout is not an error: it is how a stopped device is noticed so the
        // command state above can be re-read.
        match (&event, &poll_timer) {
            // Event-driven: the driver signals once per period and this blocks until it
            // does. Unchanged, and the only path any device that supports events takes.
            (Some(ev), _) => {
                if ev.wait_for_event(EVENT_TIMEOUT_MS).is_err() {
                    continue;
                }
            }
            // Polled: nothing signals us, so ask on a high-resolution timer until the device
            // has a period's worth of room (render) or of audio (capture). Bounded by the
            // same timeout as the event path, so a stopped device still falls through to
            // re-read the command state rather than waiting here forever.
            (None, Some(timer)) => {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(EVENT_TIMEOUT_MS as u64);
                // Sleep out the predictable part in one wait, if the device's rhythm is
                // known and the next ready is not imminent.
                if let Some(due) = interval.and_then(|i| last_ready.map(|t| t + i)) {
                    let now = std::time::Instant::now();
                    if due > now + GUARD {
                        let nap = (due - GUARD) - now;
                        if !timer.wait((nap.as_nanos() / 100) as i64) { continue; }
                    }
                }
                let mut ready = false;
                while std::time::Instant::now() < deadline {
                    // A WHOLE PERIOD, both directions: room to write one, or one waiting to
                    // be read. One read per period means the per-read work — convert,
                    // deinterleave, meter, check each channel for a full frame — runs once
                    // per period rather than once per fragment, and audio leaves on the
                    // device's own cadence instead of whenever a fragment happened to land.
                    //
                    // Capture reads `GetCurrentPadding`, the count of valid unread frames on
                    // a capture client. NOT `GetNextPacketSize`: that is a SHARED-mode
                    // concept — shared capture is delivered as discrete packets — and on an
                    // exclusive client it does not report the buffer, so the readiness test
                    // never became true and the capture loop timed out forever without ever
                    // reading a sample.
                    ready = match setup.dir {
                        Dir::Output => client
                            .get_available_space_in_frames()
                            .map(|space| space as usize >= frames)
                            .unwrap_or(false),
                        Dir::Input => client
                            .get_current_padding()
                            .map(|padding| padding as usize >= frames)
                            .unwrap_or(false),
                    };
                    if ready { break; }
                    // Fine once the rhythm is known — the long wait above has already been
                    // taken, so this only covers the last fraction of a millisecond.
                    let step = if interval.is_some() { FINE_HNS } else { coarse_hns };
                    // A failed arm is a lost period, not a reason to spin.
                    if !timer.wait(step) { break; }
                }
                // A capture device that never reports a whole period would otherwise never
                // be read at all, so take what is there rather than stall. The read handles
                // a short read and the encoder's accumulator carries the remainder; the
                // count says how often the whole-period wait did not hold, which is the
                // measure of whether this device delivers fragments.
                if !ready && setup.dir == Dir::Input {
                    if client.get_current_padding().map(|p| p > 0).unwrap_or(false) {
                        ready = true;
                        partial_reads += 1;
                        let now = std::time::Instant::now();
                        if now.duration_since(last_partial_log)
                            >= std::time::Duration::from_secs(PARTIAL_LOG_SECS)
                        {
                            last_partial_log = now;
                            tracing::debug!("WASAPI input '{}': {} short read(s) in the last \
                                             {}s — this device delivers less than a whole \
                                             {}-frame period at a time",
                                            setup.device.name, partial_reads,
                                            PARTIAL_LOG_SECS, frames);
                            partial_reads = 0;
                        }
                    }
                }
                if !ready {
                    // Missed: the rhythm estimate is stale, so drop back to coarse polling
                    // and re-learn rather than chasing a prediction that no longer holds.
                    interval = None;
                    last_ready = None;
                    continue;
                }
                // Learn the device's rhythm from the gap between readies. A short average
                // tracks a drifting device without chasing a single late wake.
                let now = std::time::Instant::now();
                if let Some(prev) = last_ready {
                    let gap = now - prev;
                    // Ignore absurd gaps — a rebuild or a stall, not the device's rhythm.
                    if gap < std::time::Duration::from_millis(EVENT_TIMEOUT_MS as u64 / 2) {
                        interval = Some(match interval {
                            Some(e) => (e * 3 + gap) / 4,
                            None => gap,
                        });
                    } else {
                        interval = None;
                    }
                }
                last_ready = Some(now);
            }
            // Cannot occur: the open sets exactly one of them.
            (None, None) => continue,
        }

        let r = match (setup.dir, &mut *io) {
            (Dir::Output, Io::Render(render)) => {
                render(&mut f32_buf[..samples]);
                encode(setup.cfg.fmt, &f32_buf[..samples], &mut byte_buf);
                render_client
                    .as_ref()
                    .map(|rc| rc.write_to_device(frames, &byte_buf, None))
                    .unwrap_or(Ok(()))
            }
            (Dir::Input, Io::Capture(capture)) => {
                match capture_client.as_ref().map(|cc| cc.read_from_device(&mut byte_buf)) {
                    Some(Ok((got, _info))) => {
                        let n = (got as usize * chans).min(samples);
                        decode(setup.cfg.fmt, &byte_buf[..n * setup.cfg.fmt.bytes_per_sample()],
                               &mut f32_buf[..n]);
                        capture(&f32_buf[..n]);
                        Ok(())
                    }
                    Some(Err(e)) => Err(e),
                    None => Ok(()),
                }
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            report(classify(&e, shared));
        }
    }
}

// ── Format conversion at the device boundary ──────────────────────────────────

/// Full-scale divisors. `S24In4` uses the 32-bit divisor because its valid bits are
/// left-justified in the container — see `Fmt`.
const FS_32: f32 = 2_147_483_648.0;
const FS_24: f32 = 8_388_608.0;
const FS_16: f32 = 32_768.0;

/// f32 → the device's format, interleaved, into a byte buffer.
fn encode(fmt: Fmt, src: &[f32], dst: &mut [u8]) {
    match fmt {
        Fmt::F32 => {
            for (s, c) in src.iter().zip(dst.chunks_exact_mut(4)) {
                c.copy_from_slice(&s.to_le_bytes());
            }
        }
        Fmt::S32 | Fmt::S24In4 => {
            for (s, c) in src.iter().zip(dst.chunks_exact_mut(4)) {
                let v = (s.clamp(-1.0, 1.0) * (FS_32 - 1.0)) as i32;
                // S24In4 keeps the same scaling; the low 8 bits simply carry no
                // information, which is what "24 valid bits, left-justified" means.
                c.copy_from_slice(&v.to_le_bytes());
            }
        }
        Fmt::S24In3 => {
            for (s, c) in src.iter().zip(dst.chunks_exact_mut(3)) {
                let v = (s.clamp(-1.0, 1.0) * (FS_24 - 1.0)) as i32;
                c[0] = (v & 0xff) as u8;
                c[1] = ((v >> 8) & 0xff) as u8;
                c[2] = ((v >> 16) & 0xff) as u8;
            }
        }
        Fmt::S16 => {
            for (s, c) in src.iter().zip(dst.chunks_exact_mut(2)) {
                let v = (s.clamp(-1.0, 1.0) * (FS_16 - 1.0)) as i16;
                c.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

/// The device's format → f32, interleaved.
fn decode(fmt: Fmt, src: &[u8], dst: &mut [f32]) {
    match fmt {
        Fmt::F32 => {
            for (d, c) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *d = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            }
        }
        Fmt::S32 | Fmt::S24In4 => {
            for (d, c) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *d = i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / FS_32;
            }
        }
        Fmt::S24In3 => {
            for (d, c) in dst.iter_mut().zip(src.chunks_exact(3)) {
                // Sign-extend by placing the 24 bits at the TOP of an i32 and shifting
                // back: masking instead would read every negative sample as a large
                // positive one.
                let v = ((c[0] as i32) << 8) | ((c[1] as i32) << 16) | ((c[2] as i32) << 24);
                *d = (v >> 8) as f32 / FS_24;
            }
        }
        Fmt::S16 => {
            for (d, c) in dst.iter_mut().zip(src.chunks_exact(2)) {
                *d = i16::from_le_bytes([c[0], c[1]]) as f32 / FS_16;
            }
        }
    }
}

// ── MMCSS ─────────────────────────────────────────────────────────────────────

/// Multimedia Class Scheduler Service registration for the I/O thread.
///
/// The Windows counterpart of `SCHED_FIFO` on Linux and the CoreAudio workgroup on macOS —
/// but unlike `SCHED_FIFO` it needs no capability or privilege, so a refusal is genuinely
/// unexpected rather than an ordinary unprivileged-run case.
mod mmcss {
    use windows::core::w;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{
        AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    };

    /// Holds the registration for the lifetime of the I/O thread and reverts it on drop.
    pub struct ProAudio(Option<HANDLE>);

    impl ProAudio {
        pub fn acquire() -> Self {
            let mut task_index: u32 = 0;
            match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
                Ok(h) => ProAudio(Some(h)),
                Err(e) => {
                    tracing::warn!(
                        "MMCSS refused for the audio I/O thread ({e}) — it runs at normal \
                         priority. Audio still runs, it just slips under load, and the servo \
                         reads that slip as ordinary drift."
                    );
                    ProAudio(None)
                }
            }
        }
    }

    impl Drop for ProAudio {
        fn drop(&mut self) {
            if let Some(h) = self.0.take() {
                let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packed 24-bit must sign-extend, not mask: a negative sample read as unsigned is a
    /// full-scale error rather than a small one.
    #[test]
    fn packed_24_bit_round_trips_and_sign_extends() {
        let src = [0.0f32, 0.5, -0.5, -1.0, 0.999];
        let mut bytes = vec![0u8; src.len() * 3];
        encode(Fmt::S24In3, &src, &mut bytes);
        let mut back = vec![0.0f32; src.len()];
        decode(Fmt::S24In3, &bytes, &mut back);
        for (a, b) in src.iter().zip(back.iter()) {
            assert!((a - b).abs() < 2.0 / FS_24, "{a} vs {b}");
        }
        assert!(back[3] < -0.99, "expected near -1.0, got {}", back[3]);
    }

    /// 24-in-32 shares the 32-bit divisor because its valid bits are left-justified.
    /// Scaling it by 2^23 instead would be a 48 dB error.
    #[test]
    fn s24_in_32_uses_the_32_bit_scaling() {
        let src = [0.5f32, -0.5];
        let mut a = vec![0u8; src.len() * 4];
        let mut b = vec![0u8; src.len() * 4];
        encode(Fmt::S24In4, &src, &mut a);
        encode(Fmt::S32, &src, &mut b);
        assert_eq!(a, b);
    }

    /// Bytes per sample must match the container, not the valid bits.
    #[test]
    fn container_width_drives_the_byte_count() {
        assert_eq!(Fmt::F32.bytes_per_sample(), 4);
        assert_eq!(Fmt::S32.bytes_per_sample(), 4);
        assert_eq!(Fmt::S24In4.bytes_per_sample(), 4);
        assert_eq!(Fmt::S24In3.bytes_per_sample(), 3);
        assert_eq!(Fmt::S16.bytes_per_sample(), 2);
    }
}
