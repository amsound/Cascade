//! Linux — the direct ALSA backend.
//!
//! This is the Linux backend. macOS is served by `coreaudio_backend`; there is no portable
//! layer under either.
//!
//! ## Why direct rather than through a portable layer
//!
//! Three things the device layer needs cannot be expressed through a portable layer:
//!
//!   * **Rate strictness as an assertion.** ALSA will silently resample unless told not
//!     to. `set_rate_resample(false)` plus `ValueOr::Exact` makes "the backend provides
//!     the requested topology or configuration fails" a property of the open call rather
//!     than a consequence of which devices the picker happened to offer.
//!   * **The period COUNT.** ALSA's ring is `period_size × periods`, and the second half
//!     of that pair has no CoreAudio equivalent, so the spec is silent on it. The
//!     conventional value is 2, which leaves a 2.5 ms margin at a 2.5 ms period. See
//!     `PERIOD_MARGIN_MS`.
//!   * **The I/O loop itself.** ALSA has no callback model; the loop is ours, and so is
//!     what happens on an XRUN.
//!
//! ## Enumeration is structural, not filtered
//!
//! Devices are built by walking the card/device control API, so every entry is a real
//! `hw:` endpoint by construction. The plugin names that a hint-based enumeration would
//! surface — `plughw:`, `default:`, `dmix:`, `pulse`, `jack`, the rate converters — are
//! never constructed, so there is nothing to filter out. That matters because those all
//! resample underneath a configuration that reported success, which is the one thing the
//! no-resampling rule forbids.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};

// ── Tunables ──────────────────────────────────────────────────────────────────

/// The wall-clock margin the device ring must hold, which is what sets the period COUNT.
///
/// ALSA's ring is `period_size × periods`. The conventional count is 2, which leaves the
/// margin between the loop returning and the hardware running dry at exactly one period —
/// 2.5 ms at a 2.5 ms period, on a general-purpose kernel. The quantity that actually protects against
/// a scheduling stall is TIME, not a count of periods, so the count is derived from this
/// instead (`PLATFORM_LINUX_ALSA.md` O1(c)).
///
/// At a 10 ms period this yields 2 periods — the conventional ring — and it only diverges
/// where the margin is genuinely too thin to defend.
const PERIOD_MARGIN_MS: f64 = 20.0;

/// Never fewer than two periods: one being filled while one is being played is the minimum
/// that can run at all.
const MIN_PERIODS: u32 = 2;

/// How long the I/O loop blocks in `wait()` before looking at the command state again.
/// Bounded so a stop is acted on promptly even if the device has gone quiet.
const WAIT_TIMEOUT_MS: u32 = 200;

// ── Devices ───────────────────────────────────────────────────────────────────

/// One ALSA hardware endpoint.
///
/// `card`/`dev` are the numeric indices used to OPEN the device, because they are
/// unambiguous at runtime. `card_id` is the driver's short identifier (`PCH`, `USB`), used
/// for the stable uid, because indices renumber when cards are added or removed and a
/// saved device selection has to survive that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    card: i32,
    dev: u32,
    card_id: String,
    card_name: String,
    pcm_name: String,
}

impl Device {
    /// The ALSA PCM name to open. Always `hw:` — never `plughw:`, which would insert the
    /// conversion layer this backend exists to avoid.
    fn pcm_id(&self) -> String {
        format!("hw:{},{}", self.card, self.dev)
    }
}

fn enumerate(dir: Direction) -> Vec<Device> {
    let mut out = Vec::new();
    for card in alsa::card::Iter::new().flatten() {
        let ctl = match alsa::ctl::Ctl::from_card(&card, false) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let info = match ctl.card_info() {
            Ok(i) => i,
            Err(_) => continue,
        };
        let card_id = info.get_id().unwrap_or_default().to_string();
        let card_name = info.get_name().unwrap_or_default().to_string();

        for dev in alsa::ctl::DeviceIter::new(&ctl) {
            // A card advertises playback and capture devices separately; asking for the
            // wrong direction is how a capture-only card is kept out of the output list.
            let pcm_name = match ctl.pcm_info(dev as u32, 0, dir) {
                Ok(pi) => pi.get_name().unwrap_or_default().to_string(),
                Err(_) => continue,
            };
            out.push(Device {
                card: card.get_index(),
                dev: dev as u32,
                card_id: card_id.clone(),
                card_name: card_name.clone(),
                pcm_name,
            });
        }
    }
    out
}

pub fn output_devices() -> Vec<Device> {
    enumerate(Direction::Playback)
}

pub fn input_devices() -> Vec<Device> {
    enumerate(Direction::Capture)
}

/// What the user is shown: the card and the PCM, with the id that identifies it.
pub fn device_name(dev: &Device) -> String {
    if dev.pcm_name.is_empty() || dev.pcm_name == dev.card_name {
        format!("{} (hw:{},{})", dev.card_name, dev.card, dev.dev)
    } else {
        format!("{}: {} (hw:{},{})", dev.card_name, dev.pcm_name, dev.card, dev.dev)
    }
}

/// The stable identifier a configuration stores.
///
/// Keyed on the card's driver ID rather than its index: indices renumber when cards come
/// and go, and a saved selection that silently retargets a different card on reboot is
/// worse than one that fails to resolve.
pub fn device_uid(dev: &Device) -> Option<String> {
    if dev.card_id.is_empty() {
        None
    } else {
        Some(format!("hw:{},{}", dev.card_id, dev.dev))
    }
}

/// Every enumerated entry is a real endpoint here — see the module header. Nothing that
/// is not one is ever constructed.
pub fn is_hardware_endpoint(_dev: &Device) -> bool {
    true
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
    fn alsa(self) -> Direction {
        match self {
            Dir::Input => Direction::Capture,
            Dir::Output => Direction::Playback,
        }
    }
}

/// The device sample formats accepted, in descending preference.
///
/// f32 is what the pipeline works in. `hw:` devices commonly refuse it and offer only
/// integer formats, so those are accepted and converted at this boundary — a BIT-DEPTH
/// change, never a rate or channel-count one, so it does not engage the no-resampling
/// rule (`PLATFORM_LINUX_ALSA.md` O3(b)).
///
/// The 24-bit cases are genuinely distinct and must be keyed off what the device reports,
/// never inferred from "24-bit":
///
///   * `S243LE` is packed into three bytes.
///   * `S24LE` sits in a four-byte container, RIGHT-justified — the low 24 bits carry the
///     sample, so its full-scale divisor is 2^23, not 2^31.
///   * A device presenting 24 bits LEFT-justified in four bytes reports `S32LE`, and
///     scales by 2^31 like any other 32-bit sample. It needs no separate case; the low
///     eight bits are simply zero.
///
/// Confusing right- with left-justified is a 48 dB error, not a subtle one, which is why
/// the divisor travels with the format rather than being derived from a bit count.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Fmt {
    F32,
    S32,
    S24In4,
    S24In3,
    S16,
}

impl Fmt {
    fn alsa(self) -> Format {
        match self {
            Fmt::F32 => Format::float(),
            Fmt::S32 => Format::s32(),
            Fmt::S24In4 => Format::S24LE,
            Fmt::S24In3 => Format::S243LE,
            Fmt::S16 => Format::s16(),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Fmt::F32 => "F32",
            Fmt::S32 => "S32_LE",
            Fmt::S24In4 => "S24_LE (24-in-32, right-justified)",
            Fmt::S24In3 => "S24_3LE (packed)",
            Fmt::S16 => "S16_LE",
        }
    }
}

const FORMAT_PREFERENCE: [Fmt; 5] = [Fmt::F32, Fmt::S32, Fmt::S24In4, Fmt::S24In3, Fmt::S16];

/// A negotiated, ready-to-open configuration. Opaque above the backend: callers ask for
/// 48 kHz at a period and get back whatever the device agreed to, which they inspect only
/// through the accessors.
#[derive(Clone, Debug)]
pub struct Config {
    channels: u16,
    sample_rate: u32,
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
/// context. The rebuild paths downcast to it to tell Cascade's own stream still holding an
/// exclusive device from a genuine third-party conflict.
#[derive(Debug)]
pub struct DeviceBusy;

impl std::fmt::Display for DeviceBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("device is busy")
    }
}
impl std::error::Error for DeviceBusy {}

/// True when this error chain carries the backend's device-busy signal.
pub fn is_device_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<DeviceBusy>().is_some())
}

fn busy_hint() -> &'static str {
    " — `hw:` devices are opened exclusively, so this usually means it is already held: \
     either by another application (commonly PipeWire or PulseAudio; try stopping or \
     masking it), or by Cascade's own stream on the same device mid-rebuild"
}

/// Open the PCM, attaching the busy marker when the device is held.
fn open_pcm(device: &Device, dir: Dir) -> anyhow::Result<PCM> {
    let id = device.pcm_id();
    PCM::new(&id, dir.alsa(), false).map_err(|e| {
        let msg = format!(
            "cannot open {} device '{}': {}{}",
            dir.label(),
            device_name(device),
            e,
            if e.errno() == libc::EBUSY { busy_hint() } else { "" }
        );
        if e.errno() == libc::EBUSY {
            anyhow::Error::new(DeviceBusy).context(e.to_string()).context(msg)
        } else {
            anyhow::Error::new(e).context(msg)
        }
    })
}

/// The period count that gives `PERIOD_MARGIN_MS` of ring, never below `MIN_PERIODS`.
fn periods_for(period_frames: usize, rate: u32) -> u32 {
    let period_ms = period_frames as f64 * 1000.0 / rate as f64;
    if period_ms <= 0.0 {
        return MIN_PERIODS;
    }
    let want = (PERIOD_MARGIN_MS / period_ms).ceil() as u32;
    want.max(MIN_PERIODS)
}

/// Apply the hardware parameters, in the order ALSA requires, and return what was granted.
///
/// The rate is asserted twice over: `set_rate_resample(false)` removes ALSA's automatic
/// conversion, and `ValueOr::Exact` refuses a near miss. Either alone is insufficient —
/// without the first, an exact request is satisfied by converting underneath.
fn apply_hw(
    pcm: &PCM,
    fmt: Fmt,
    channels: u32,
    rate: u32,
    period_frames: usize,
) -> anyhow::Result<(u32, usize, u32)> {
    let hw = HwParams::any(pcm)?;
    hw.set_rate_resample(false)?;
    hw.set_access(Access::RWInterleaved)?;
    hw.set_format(fmt.alsa())?;
    hw.set_channels(channels)?;
    hw.set_rate(rate, ValueOr::Nearest)?;
    // Asserted after the fact rather than with ValueOr::Exact: some drivers refuse an
    // exact request they would otherwise satisfy, and the check below is what actually
    // guarantees the rate. A device that lands anywhere but 48 kHz fails configuration.
    let got_rate = hw.get_rate()?;
    anyhow::ensure!(
        got_rate == rate,
        "device would run at {got_rate} Hz, not {rate} Hz — Cascade does not resample"
    );

    hw.set_period_size(period_frames as i64, ValueOr::Nearest)?;
    let periods = periods_for(period_frames, rate);
    hw.set_periods(periods, ValueOr::Nearest)?;
    pcm.hw_params(&hw)?;

    let granted_period = hw.get_period_size()? as usize;
    let granted_periods = hw.get_periods()?;
    let granted_channels = hw.get_channels()?;
    Ok((granted_channels, granted_period, granted_periods))
}

/// Software parameters: wake at one period, and never auto-start.
///
/// `start_threshold` is set to the whole ring so playback cannot begin on the first write.
/// The spec requires both endpoints configured before either runs and capture started
/// before playback, so starting is an explicit act — see `start`.
fn apply_sw(pcm: &PCM, period: usize, periods: u32) -> anyhow::Result<()> {
    let sw = pcm.sw_params_current()?;
    let buffer = period as i64 * periods as i64;
    sw.set_avail_min(period as i64)?;
    sw.set_start_threshold(buffer)?;
    sw.set_stop_threshold(buffer)?;
    pcm.sw_params(&sw)?;
    Ok(())
}

/// Negotiate 48 kHz at the requested period, preferring f32.
///
/// The two directions negotiate independently because they are genuinely different
/// devices — a card can be integer-only for playback and f32 for capture, or the two
/// halves can be different cards entirely.
///
/// This opens the device to ask, then closes it. `hw:` devices are exclusive, so the open
/// in `open_output`/`open_input` is a second one; the window between them is why
/// `is_device_busy` exists.
pub fn find_config(device: &Device, dir: Dir, period_frames: usize) -> anyhow::Result<Config> {
    let pcm = open_pcm(device, dir)?;

    // Channel count is whatever the device has. Cascade routes to what it is given; the
    // rule is that the topology must not be silently CHANGED, not that it must be asked
    // for in advance.
    let channels = {
        let hw = HwParams::any(&pcm)?;
        hw.set_rate_resample(false)?;
        hw.set_access(Access::RWInterleaved)?;
        hw.get_channels_max()?
    };

    let mut last_err: Option<anyhow::Error> = None;
    for fmt in FORMAT_PREFERENCE {
        match apply_hw(&pcm, fmt, channels, 48_000, period_frames) {
            Ok((got_channels, got_period, got_periods)) => {
                apply_sw(&pcm, got_period, got_periods)?;
                tracing::debug!(
                    "ALSA {} '{}': {} ch, 48 kHz, {}, period {} × {} periods ({:.1} ms ring)",
                    dir.label(),
                    device_name(device),
                    got_channels,
                    fmt.name(),
                    got_period,
                    got_periods,
                    got_period as f64 * got_periods as f64 / 48.0
                );
                return Ok(Config {
                    channels: got_channels as u16,
                    sample_rate: 48_000,
                    period: got_period,
                    fmt,
                });
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(anyhow::anyhow!(
        "{} device '{}' does not support 48 kHz in any accepted format ({}) — \
         this device cannot be used without resampling, which Cascade does not do{}",
        dir.label(),
        device_name(device),
        FORMAT_PREFERENCE.iter().map(|f| f.name()).collect::<Vec<_>>().join(", "),
        last_err.map(|e| format!(": {e}")).unwrap_or_default()
    ))
}

// ── Streams ───────────────────────────────────────────────────────────────────

/// What the I/O thread should be doing. The thread owns the PCM; every transition is a
/// state change here plus a notify, so the caller never touches ALSA from another thread.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Cmd {
    /// Configured and prepared, not moving samples. The state `open_*` returns in.
    Prepared,
    Running,
    Stopped,
    /// Terminal: unwind and exit.
    Dead,
}

struct Shared {
    cmd: Mutex<Cmd>,
    wake: Condvar,
    /// Set once the thread has observed `Dead`, so `Drop` can join without racing.
    faults_detached: AtomicBool,
}

/// A configured stream and the thread that runs it.
///
/// ALSA has no callback registration, so the stream IS a thread. It parks in `Prepared`
/// until `start`, which is what lets the caller configure both endpoints before either
/// runs — the ordering the spec requires.
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

/// The callback period the backend actually granted, in frames.
pub fn granted_period(stream: &Stream) -> Option<usize> {
    Some(stream.period)
}

/// The period a trial open of a device asks for. Any size will do: hardware parameters
/// belong to the PCM stream being opened, so a trial touches no other open stream — not even
/// the other direction of the same card.
pub fn probe_period(_dev: &Device, _dir: Dir) -> Option<usize> {
    Some(crate::audio::encode::IO_BUF_CAP_FRAMES)
}

/// The shortest period the device will run at, in frames: the period it grants for
/// `audio::SMALLEST_PERIOD_REQUEST`.
///
/// This is `find_config`'s own negotiation at that request — the same hardware parameters a
/// stream opened at it would get, so the answer is the period such a stream runs at. It
/// opens the device and closes it again, so the device must not be held by a stream.
pub fn smallest_period(device: &Device, dir: Dir) -> anyhow::Result<Option<usize>> {
    find_config(device, dir, crate::audio::SMALLEST_PERIOD_REQUEST).map(|cfg| Some(cfg.period))
}

/// Start a prepared stream.
pub fn start(stream: &Stream) -> anyhow::Result<()> {
    stream.set(Cmd::Running);
    Ok(())
}

/// Stop a running stream without closing the device.
pub fn stop(stream: &Stream) -> anyhow::Result<()> {
    stream.set(Cmd::Stopped);
    Ok(())
}

/// Remove this stream's fault reporting. The callback lives in the thread, so this only
/// has to stop it being called; the thread itself goes with the stream.
pub fn detach_faults(stream: &mut Stream) {
    stream.shared.faults_detached.store(true, Ordering::Relaxed);
}

/// Tell the host not to trade latency for power. ALSA exposes no such hint — the CoreAudio
/// `kAudioHardwarePropertyPowerHint` has no counterpart here.
pub fn set_power_hint() {}

/// Classify an ALSA error into the caller's fault taxonomy.
fn classify(e: &alsa::Error) -> StreamFault {
    match e.errno() {
        // The card was unplugged or the driver detached.
        libc::ENODEV | libc::ENXIO | libc::ENOENT => StreamFault::DeviceLost,
        // The stream is no longer usable but the device is still there.
        libc::EBADFD | libc::ESTRPIPE => StreamFault::Invalidated,
        _ => StreamFault::Transient(e.to_string()),
    }
}

/// Convert between the device's format and the pipeline's f32, one interleaved block.
///
/// The divisors are per-format constants rather than a function of a bit count, because
/// `S24In4` is right-justified and so does NOT scale by its container width. See `Fmt`.
mod convert {
    pub const S32: f32 = 2_147_483_648.0;
    pub const S24: f32 = 8_388_608.0;
    pub const S16: f32 = 32_768.0;

    /// Read three-byte packed little-endian signed samples into f32.
    ///
    /// Writes into a slice rather than a Vec, so the capture path converts into its
    /// preallocated buffer with no allocation on the audio thread. Converts as many samples
    /// as both sides hold and returns that count.
    pub fn s24_3_to_f32(src: &[u8], dst: &mut [f32]) -> usize {
        let mut n = 0;
        for (d, c) in dst.iter_mut().zip(src.chunks_exact(3)) {
            // Sign-extend by placing the 24 bits at the TOP of an i32 and shifting back.
            let v = ((c[0] as i32) << 8) | ((c[1] as i32) << 16) | ((c[2] as i32) << 24);
            *d = (v >> 8) as f32 / S24;
            n += 1;
        }
        n
    }

    /// Write f32 into three-byte packed little-endian signed samples.
    pub fn f32_to_s24_3(src: &[f32], dst: &mut Vec<u8>) {
        dst.clear();
        for &s in src {
            let v = (s.clamp(-1.0, 1.0) * (S24 - 1.0)) as i32;
            dst.push((v & 0xff) as u8);
            dst.push(((v >> 8) & 0xff) as u8);
            dst.push(((v >> 16) & 0xff) as u8);
        }
    }
}

/// The stream's IO handle, built once and kept for the life of the stream.
///
/// `PCM::io_f32`/`io_i32`/`io_i16` verify the sample format against the device's current
/// hardware parameters, and that read allocates and frees an ALSA parameter struct. Taking
/// the handle once keeps that off the period loop, where it ran on the real-time thread
/// every period — a heap allocation per period, 400 a second at a 2.5 ms period.
///
/// A PCM allows exactly one IO at a time, and no hardware-parameter call while it exists
/// (`snd_pcm_hw_params`, `snd_pcm_hw_free`), so it is taken after `apply_hw` and
/// `apply_sw`, and everything the loop does afterwards — prepare, drop, recover, wait,
/// avail_update, state — is allowed alongside it.
///
/// The variant follows the negotiated format, and the format decides the scaling: `I32`
/// carries S32 and 24-in-32 alike, which differ only in their divisor (see `Fmt`).
enum StreamIo<'a> {
    F32(alsa::pcm::IO<'a, f32>),
    I32(alsa::pcm::IO<'a, i32>),
    I16(alsa::pcm::IO<'a, i16>),
    /// Packed 24-bit: written and read as raw bytes, which needs no format check.
    Bytes(alsa::pcm::IO<'a, u8>),
}

impl<'a> StreamIo<'a> {
    fn new(pcm: &'a PCM, fmt: Fmt) -> Result<Self, alsa::Error> {
        Ok(match fmt {
            Fmt::F32 => StreamIo::F32(pcm.io_f32()?),
            Fmt::S32 => StreamIo::I32(pcm.io_i32()?),
            // 24-in-32 is S24_LE on the device, not S32_LE: `io_i32` checks for S32 and
            // refuses it. Same i32 buffer, right-justified samples.
            Fmt::S24In4 => StreamIo::I32(pcm.io_i32_s24()?),
            Fmt::S24In3 => StreamIo::Bytes(pcm.io_bytes()),
            Fmt::S16 => StreamIo::I16(pcm.io_i16()?),
        })
    }
}

/// Open an output stream, PREPARED BUT NOT STARTED.
///
/// `render` is handed an f32 buffer to fill, whatever the device's own format is; the
/// integer conversions happen here. Every buffer is allocated once, before the thread
/// starts — nothing on the audio thread allocates.
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

enum Io {
    Render(Box<dyn FnMut(&mut [f32]) + Send>),
    Capture(Box<dyn FnMut(&[f32]) + Send>),
}

fn spawn(
    device: &Device,
    dir: Dir,
    cfg: &Config,
    mut io: Io,
    mut on_fault: Box<dyn FnMut(StreamFault) + Send>,
) -> anyhow::Result<Stream> {
    // Configure on THIS thread so a failure is reported to the caller rather than
    // disappearing into a worker.
    let pcm = open_pcm(device, dir)?;
    let (channels, period, periods) =
        apply_hw(&pcm, cfg.fmt, cfg.channels as u32, cfg.sample_rate, cfg.period)?;
    apply_sw(&pcm, period, periods)?;
    pcm.prepare()?;
    // Prove the stream's IO handle can be taken, here where a failure is the caller's error.
    // The I/O thread takes the real one (it owns the PCM from this point), and a refusal
    // there would leave the caller holding a stream that cannot move a sample.
    drop(StreamIo::new(&pcm, cfg.fmt)?);

    let shared = Arc::new(Shared {
        cmd: Mutex::new(Cmd::Prepared),
        wake: Condvar::new(),
        faults_detached: AtomicBool::new(false),
    });

    let fmt = cfg.fmt;
    let frames = period;
    let chans = channels as usize;
    let samples = frames * chans;
    let label = device_name(device);
    let thread_shared = Arc::clone(&shared);

    let handle = std::thread::Builder::new()
        .name(format!("cascade-alsa-{}", dir.label()))
        .spawn(move || {
            // SCHED_FIFO, the same policy the decode and encode callback threads get.
            // Best-effort: a refusal leaves the thread at normal priority and warns once —
            // audio still runs, it just XRUNs more readily under load.
            crate::audio::scheduler::pool::elevate_audio_callback_thread();

            let mut f32_buf: Vec<f32> = vec![0.0; samples];
            let mut i32_buf: Vec<i32> = vec![0; samples];
            let mut i16_buf: Vec<i16> = vec![0; samples];
            let mut u8_buf: Vec<u8> = vec![0; samples * 3];

            // Taken once, for the life of the stream — see `StreamIo`. A format the device
            // agreed to during configuration cannot be refused here, so this failing means
            // the device changed under us: report it and let the stream end rather than
            // running a loop that cannot move a sample.
            let io_handle = match StreamIo::new(&pcm, fmt) {
                Ok(io) => io,
                Err(e) => {
                    if !thread_shared.faults_detached.load(Ordering::Relaxed) {
                        on_fault(classify(&e));
                    }
                    return;
                }
            };

            let mut report = |fault: StreamFault| {
                if !thread_shared.faults_detached.load(Ordering::Relaxed) {
                    on_fault(fault);
                }
            };

            let mut running = false;
            // Whether the current fault has been reported. One failing device produces the
            // same error on every attempt, so it is reported on the transition into failure
            // and re-armed by the next period that actually moves.
            let mut fault_reported = false;
            loop {
                // ── Command state ─────────────────────────────────────────────
                {
                    let mut g = thread_shared.cmd.lock().unwrap();
                    loop {
                        match *g {
                            Cmd::Dead => return,
                            Cmd::Running => {
                                if !running {
                                    running = true;
                                    fault_reported = false;
                                    if let Err(e) = begin(&pcm, dir, &io_handle, periods,
                                                          samples, &mut f32_buf,
                                                          &mut i32_buf, &mut i16_buf,
                                                          &mut u8_buf) {
                                        report(classify(&e));
                                        running = false;
                                        *g = Cmd::Stopped;
                                        continue;
                                    }
                                }
                                break;
                            }
                            Cmd::Prepared | Cmd::Stopped => {
                                if running {
                                    running = false;
                                    // `drop` is ALSA's stop-without-drain: discard what is
                                    // queued and halt. `prepare` puts it back in a state
                                    // that can be started again without reopening, which
                                    // is what makes stop/start reusable across a rebuild.
                                    let _ = pcm.drop();
                                    let _ = pcm.prepare();
                                }
                                g = thread_shared.wake.wait(g).unwrap();
                            }
                        }
                    }
                }

                // ── One period ────────────────────────────────────────────────
                let failed = match pump(
                    &pcm, dir, fmt, &io_handle, frames, chans, &mut io, &mut f32_buf,
                    &mut i32_buf, &mut i16_buf, &mut u8_buf,
                ) {
                    Ok(moved) => {
                        if moved { fault_reported = false; }
                        None
                    }
                    // An XRUN is recoverable and expected under load; a failed recovery is
                    // a fault like any other.
                    Err(e) => match e.errno() {
                        libc::EPIPE => {
                            tracing::debug!("ALSA {} '{}': XRUN, recovering", dir.label(), label);
                            pcm.recover(e.errno(), true).err()
                        }
                        libc::EAGAIN | libc::EINTR => None,
                        _ => Some(e),
                    },
                };
                // Anything else is reported once and the loop keeps going, because tearing
                // the device down is a destructive response to a transient — but it PAUSES
                // before the next attempt. A card that has gone away fails `wait` at once, so
                // retrying immediately spun this thread flat out at SCHED_FIFO, reporting the
                // same fault every iteration, until the main loop got round to stopping it.
                if let Some(e) = failed {
                    if !fault_reported {
                        fault_reported = true;
                        report(classify(&e));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(WAIT_TIMEOUT_MS as u64));
                }
            }
        })?;

    Ok(Stream { shared, handle: Some(handle), period })
}

/// Bring a prepared PCM into the running state.
///
/// Playback is pre-filled with silence to the start threshold before starting, so the
/// first real period has somewhere to land and the device does not XRUN on the first
/// cycle. Capture just starts.
#[allow(clippy::too_many_arguments)]
fn begin(pcm: &PCM, dir: Dir, io: &StreamIo, periods: u32, samples: usize,
         f32_buf: &mut [f32], i32_buf: &mut Vec<i32>, i16_buf: &mut Vec<i16>,
         u8_buf: &mut Vec<u8>) -> Result<(), alsa::Error> {
    if dir == Dir::Output {
        // Silence, not audio: the render callback has not run at this point. Filling the
        // ring before starting is what stops the device XRUNing on its first cycle.
        //
        // Written through the stream's own IO handle and its scratch buffers: a second IO
        // is not allowed (`StreamIo`), and the period count and sample width are already
        // known from the configuration, so nothing here reads the hardware parameters back.
        match io {
            StreamIo::F32(io) => {
                f32_buf[..samples].fill(0.0);
                for _ in 0..periods {
                    if io.writei(&f32_buf[..samples]).is_err() { break }
                }
            }
            StreamIo::I32(io) => {
                i32_buf.clear();
                i32_buf.resize(samples, 0);
                for _ in 0..periods {
                    if io.writei(i32_buf).is_err() { break }
                }
            }
            StreamIo::I16(io) => {
                i16_buf.clear();
                i16_buf.resize(samples, 0);
                for _ in 0..periods {
                    if io.writei(i16_buf).is_err() { break }
                }
            }
            StreamIo::Bytes(io) => {
                u8_buf.clear();
                u8_buf.resize(samples * 3, 0);
                for _ in 0..periods {
                    if io.writei(u8_buf).is_err() { break }
                }
            }
        }
    }
    match pcm.state() {
        State::Running => Ok(()),
        _ => pcm.start(),
    }
}

/// Move exactly one period, converting at the device boundary.
///
/// Returns whether a period actually moved: `Ok(false)` is a wait that timed out or found
/// less than a period available, which is not evidence either way about the device's health.
#[allow(clippy::too_many_arguments)]
fn pump(
    pcm: &PCM,
    dir: Dir,
    fmt: Fmt,
    io_handle: &StreamIo,
    frames: usize,
    chans: usize,
    io: &mut Io,
    f32_buf: &mut [f32],
    i32_buf: &mut Vec<i32>,
    i16_buf: &mut Vec<i16>,
    u8_buf: &mut Vec<u8>,
) -> Result<bool, alsa::Error> {
    // Block until a period's worth is available in the direction we care about, with a
    // bounded timeout so a stop command is never waited on indefinitely.
    if !pcm.wait(Some(WAIT_TIMEOUT_MS))? {
        return Ok(false);
    }
    if (pcm.avail_update()? as usize) < frames {
        return Ok(false);
    }
    let samples = frames * chans;

    match (dir, io) {
        (Dir::Output, Io::Render(render)) => {
            render(&mut f32_buf[..samples]);
            // The scaling follows the FORMAT; the handle follows the sample type, and one
            // handle serves both 32-bit integer formats (see `StreamIo`).
            match (io_handle, fmt) {
                (StreamIo::F32(io), _) => {
                    io.writei(&f32_buf[..samples])?;
                }
                (StreamIo::I32(io), Fmt::S24In4) => {
                    i32_buf.clear();
                    // Right-justified: the sample occupies the LOW 24 bits, so it scales
                    // by 2^23 and is not shifted up into the container.
                    i32_buf.extend(f32_buf[..samples].iter().map(|&s| {
                        (s.clamp(-1.0, 1.0) * (convert::S24 - 1.0)) as i32
                    }));
                    io.writei(i32_buf)?;
                }
                (StreamIo::I32(io), _) => {
                    i32_buf.clear();
                    i32_buf.extend(f32_buf[..samples].iter().map(|&s| {
                        (s.clamp(-1.0, 1.0) * (convert::S32 - 1.0)) as i32
                    }));
                    io.writei(i32_buf)?;
                }
                (StreamIo::I16(io), _) => {
                    i16_buf.clear();
                    i16_buf.extend(f32_buf[..samples].iter().map(|&s| {
                        (s.clamp(-1.0, 1.0) * (convert::S16 - 1.0)) as i16
                    }));
                    io.writei(i16_buf)?;
                }
                (StreamIo::Bytes(io), _) => {
                    convert::f32_to_s24_3(&f32_buf[..samples], u8_buf);
                    io.writei(u8_buf)?;
                }
            }
        }
        (Dir::Input, Io::Capture(capture)) => {
            match (io_handle, fmt) {
                (StreamIo::F32(io), _) => {
                    io.readi(&mut f32_buf[..samples])?;
                }
                (StreamIo::I32(io), Fmt::S24In4) => {
                    i32_buf.resize(samples, 0);
                    io.readi(i32_buf)?;
                    for (d, &s) in f32_buf.iter_mut().zip(i32_buf.iter()) {
                        *d = s as f32 / convert::S24;
                    }
                }
                (StreamIo::I32(io), _) => {
                    i32_buf.resize(samples, 0);
                    io.readi(i32_buf)?;
                    for (d, &s) in f32_buf.iter_mut().zip(i32_buf.iter()) {
                        *d = s as f32 / convert::S32;
                    }
                }
                (StreamIo::I16(io), _) => {
                    i16_buf.resize(samples, 0);
                    io.readi(i16_buf)?;
                    for (d, &s) in f32_buf.iter_mut().zip(i16_buf.iter()) {
                        *d = s as f32 / convert::S16;
                    }
                }
                (StreamIo::Bytes(io), _) => {
                    u8_buf.resize(samples * 3, 0);
                    io.readi(u8_buf)?;
                    convert::s24_3_to_f32(u8_buf, &mut f32_buf[..samples]);
                }
            }
            capture(&f32_buf[..samples]);
        }
        _ => unreachable!("stream direction and io callback are set together"),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O1(c): the period COUNT is derived so the ring holds a fixed wall-clock margin,
    /// rather than being pinned at the conventional 2. The 10 ms case coincides with that
    /// ring; the small periods are where they diverge, which is the point.
    #[test]
    fn period_count_holds_a_fixed_time_margin() {
        assert_eq!(periods_for(480, 48_000), 2); //  10 ms period → 20 ms ring
        assert_eq!(periods_for(240, 48_000), 4); //   5 ms period → 20 ms ring
        assert_eq!(periods_for(120, 48_000), 8); // 2.5 ms period → 20 ms ring
    }

    /// Never fewer than two, however long the period is.
    #[test]
    fn period_count_never_drops_below_two() {
        assert_eq!(periods_for(4800, 48_000), 2); // 100 ms period, already over the margin
        assert_eq!(periods_for(0, 48_000), MIN_PERIODS);
    }

    /// Packed 24-bit is sign-extended through the top of an i32, not masked — a negative
    /// sample read as unsigned is a full-scale error, not a small one.
    #[test]
    fn packed_24_bit_round_trips_through_zero_and_the_extremes() {
        let mut bytes = Vec::new();
        let src = [0.0f32, 0.5, -0.5, 0.999, -0.999];
        convert::f32_to_s24_3(&src, &mut bytes);
        assert_eq!(bytes.len(), src.len() * 3);
        let mut back = vec![0.0f32; src.len()];
        assert_eq!(convert::s24_3_to_f32(&bytes, &mut back), src.len());
        for (a, b) in src.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1.0 / 8_388_608.0 * 2.0, "{a} vs {b}");
        }
    }

    /// Negative samples must come back negative.
    #[test]
    fn packed_24_bit_sign_extends() {
        let mut bytes = Vec::new();
        convert::f32_to_s24_3(&[-1.0], &mut bytes);
        let mut back = [0.0f32; 1];
        convert::s24_3_to_f32(&bytes, &mut back);
        assert!(back[0] < -0.99, "expected near -1.0, got {}", back[0]);
    }
}
