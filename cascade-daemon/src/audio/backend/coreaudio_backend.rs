//! Backend implementation over `coreaudio-rs`, direct.
//!
//! Implements the specified device lifecycle call for call. See
//! `spec/CASCADE_AUDIO_RECEIVE_SPEC.md` §13.1a for the configure sequence itself.
//!
//! This is the macOS backend. Linux is served by `alsa_backend`; there is no portable
//! layer under either.
//!
//! ## Where each call comes from
//!
//! `coreaudio-rs` covers enumeration, naming, sample rate, physical format, hog mode and
//! AUHAL construction through `audio_unit::macos_helpers`. It is built on the objc2 crates
//! rather than `coreaudio-sys`, and it does not wrap the whole HAL — the properties it
//! leaves out are read here through `objc2-core-audio` directly, in the same shape its own
//! helpers use.

#![cfg(target_os = "macos")]
// The units expose more than the seam currently asks of them: `reconfigure`, `start_pair`
// and `stop_pair` implement the in-place period change §5.2 describes, but the daemon above
// still REBUILDS streams on a period change, so nothing calls them yet. Wiring that up is
// a change to the engine, not to this module. Until then the allow
// keeps genuine warnings from being buried.
#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::Arc;
use std::ptr::{null, null_mut, NonNull};

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers;
use coreaudio::audio_unit::{SampleFormat, Scope, StreamFormat};
use coreaudio::OSStatus;
use objc2_audio_toolbox::{
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentInstance,
    AudioComponentFindNext, AudioComponentInstanceDispose, AudioComponentInstanceNew,
    AudioOutputUnitStart, AudioOutputUnitStop, AudioUnitGetProperty, AudioUnitInitialize,
    AudioUnitRender, AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioOutputUnitProperty_SetInputCallback, kAudioUnitManufacturer_Apple,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
};
use objc2_core_audio::{
    AudioDeviceID, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener, AudioObjectSetPropertyData,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioHardwareNoError,
    kAudioHardwarePropertyPowerHint, kAudioObjectSystemObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{CFRetained, CFString};

/// A device handle.
///
/// An `AudioDeviceID` is a `u32` the HAL hands out; it is not stable across reboots or
/// replugs, which is why identity for persistence is the UID string rather than this.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Device {
    id: AudioDeviceID,
}

impl Device {
    /// The raw `AudioDeviceID`, for HAL calls that take one.
    pub fn native_id(self) -> AudioDeviceID { self.id }
}

// ── Enumeration ───────────────────────────────────────────────────────────────

/// Every device the HAL reports for playback.
///
/// Unfiltered, as the seam requires: the picker's policy lives in `audio/mod.rs`, and
/// device RESOLUTION deliberately searches the full list so a device named in an existing
/// config keeps resolving even if it would no longer be offered.
pub fn output_devices() -> Vec<Device> {
    devices_for(Scope::Output)
}

/// Every device the HAL reports for capture.
pub fn input_devices() -> Vec<Device> {
    devices_for(Scope::Input)
}

/// Every device that carries at least one stream in `scope`.
///
/// The filtering is done here, device by device, rather than by asking the HAL for a
/// scoped device list. `kAudioHardwarePropertyDevices` on the system object ignores the
/// scope in the property address and answers with every device on the machine whatever is
/// asked for, so a scoped query silently returns microphones among the outputs. A device's
/// own `kAudioDevicePropertyStreamConfiguration` is the property that actually knows the
/// direction, and `get_audio_device_supports_scope` reads it: true when any buffer in that
/// scope carries channels.
///
/// A device whose direction cannot be read is dropped from the list. It is one that has
/// gone away mid-enumeration or is answering nothing, and either way it is not something to
/// offer or open.
fn devices_for(scope: Scope) -> Vec<Device> {
    let ids = match macos_helpers::get_audio_device_ids() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("CoreAudio: enumerating devices failed: {e:?}");
            return Vec::new();
        }
    };
    ids.into_iter()
        .filter(|&id| macos_helpers::get_audio_device_supports_scope(id, scope).unwrap_or(false))
        .map(|id| Device { id })
        .collect()
}

// ── Identity ──────────────────────────────────────────────────────────────────

/// A device's name, guaranteed not to panic.
///
/// Degrades to a placeholder rather than failing: every caller is either building a UI list
/// or formatting an error about a device that has just vanished, and neither can usefully
/// propagate.
pub fn device_name(dev: &Device) -> String {
    macos_helpers::get_device_name(dev.id)
        .unwrap_or_else(|_| "<device disconnected>".to_string())
}

/// A stable per-device identifier, or `None` when the HAL will not supply one.
///
/// `kAudioDevicePropertyDeviceUID` — the persistent identity, unlike `AudioDeviceID`, which
/// is reassigned across reboots and replugs. `macos_helpers` has no accessor for it, so it
/// is read here in the same shape `get_device_name` uses for its own CFString property.
pub fn device_uid(dev: &Device) -> Option<String> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope:    kAudioObjectPropertyScopeGlobal,
        mElement:  kAudioObjectPropertyElementMain,
    };
    let mut uid: *const CFString = null();
    // In/out: the HAL writes the size it filled back through this pointer, so it must be a
    // mutable borrow — handing it a shared one would be writing through a `&`.
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    unsafe {
        let status = AudioObjectGetPropertyData(
            dev.id,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut uid).cast(),
        );
        if status != kAudioHardwareNoError as i32 { return None; }
        // The HAL hands back a +1 reference; CFRetained takes ownership of it, so the
        // string is released when this scope ends rather than leaked per call.
        let uid = NonNull::new(uid as *mut CFString)?;
        Some(CFRetained::from_raw(uid).to_string())
    }
}

/// The underlying `AudioDeviceID`, for HAL calls the backend does not wrap —
/// `kAudioDevicePropertyHogMode` in particular.
pub fn native_device_id(dev: &Device) -> Option<AudioDeviceID> {
    Some(dev.id)
}

// ── Which enumerated entries are real hardware ────────────────────────────────

/// Whether this entry is a real hardware endpoint.
///
/// Always true here. Unlike ALSA, whose enumeration surfaces plugin definitions, sound
/// servers and converting views of a card alongside the real endpoints, every device the
/// CoreAudio HAL reports is one — aggregates and virtual devices included, which are
/// legitimate choices rather than views of something else.
pub fn is_hardware_endpoint(_dev: &Device) -> bool { true }

#[cfg(test)]
mod enumeration_tests {
    use super::*;

    /// Every enumerated device must yield a name and a UID. A device that cannot produce
    /// either is one the picker could list but never resolve back to.
    #[test]
    #[ignore = "touches real audio hardware"]
    fn every_device_has_a_name_and_uid() {
        let all: Vec<Device> = output_devices().into_iter()
            .chain(input_devices())
            .collect();
        assert!(!all.is_empty(), "no audio devices found at all");
        for d in all {
            let name = device_name(&d);
            let uid  = device_uid(&d);
            println!("  {:>5}  {:?}  {}", d.native_id(), uid, name);
            assert_ne!(name, "<device disconnected>", "device {} has no name", d.native_id());
            assert!(uid.is_some(), "device {} ('{name}') has no UID", d.native_id());
        }
    }
}

// ── The audio unit ────────────────────────────────────────────────────────────

/// Element 0 — on an output unit the side the host feeds; on an input unit the side facing
/// the hardware.
const ELEM_OUTPUT: u32 = 0;
/// Element 1 — the input element.
const ELEM_INPUT: u32 = 1;

/// `kAudio_ParamError`. Returned from the capture callback when the HAL asks for more
/// frames than the unit was configured to allow.
const PARAM_ERROR: OSStatus = -50;

fn check(status: OSStatus, what: &str) -> anyhow::Result<()> {
    if status == 0 { Ok(()) } else { Err(anyhow::anyhow!("{what} failed: OSStatus {status}")) }
}

/// An owned AUHAL audio unit instance, over the raw C API.
///
/// `coreaudio-rs`'s `AudioUnit` is not used for the units themselves. It keeps its instance
/// pointer private with no accessor, and the capture path needs that pointer to call
/// `AudioUnitRender` from inside the callback. Its own `set_input_callback` cannot stand in:
/// it REALLOCATES the capture buffer on the real-time thread whenever the frame count
/// changes, reads `BufferFrameSize` from a different scope and element than the one Cascade
/// writes, and leaks the buffer's length and capacity by its own admission. The crate is
/// still used for everything around the units — enumeration, names, hog mode, the ASBD
/// conversion — where it is sound.
struct Unit {
    instance: AudioComponentInstance,
}

impl Unit {
    /// A fresh, unconfigured HAL output component instance.
    ///
    /// `kAudioUnitSubType_HALOutput` always, for capture as well as playback — the HAL unit
    /// is the one that can be bound to a chosen device. Never `DefaultOutput`, which follows
    /// the system default device rather than staying bound to the device asked for.
    fn new() -> anyhow::Result<Self> {
        let desc = AudioComponentDescription {
            componentType:         kAudioUnitType_Output,
            componentSubType:      kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags:        0,
            componentFlagsMask:    0,
        };
        let component = unsafe { AudioComponentFindNext(null_mut(), NonNull::from(&desc)) };
        if component.is_null() {
            anyhow::bail!("no HAL output audio component on this system");
        }
        let mut instance: AudioComponentInstance = null_mut();
        check(unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut instance)) },
              "AudioComponentInstanceNew")?;
        if instance.is_null() {
            anyhow::bail!("AudioComponentInstanceNew returned no instance");
        }
        Ok(Unit { instance })
    }

    fn set<T>(&self, id: u32, scope: u32, elem: u32, value: &T, what: &str)
        -> anyhow::Result<()>
    {
        check(unsafe {
            AudioUnitSetProperty(self.instance, id, scope, elem,
                                 value as *const T as *const c_void,
                                 std::mem::size_of::<T>() as u32)
        }, what)
    }

    fn get<T>(&self, id: u32, scope: u32, elem: u32) -> anyhow::Result<T> {
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        let mut size = std::mem::size_of::<T>() as u32;
        check(unsafe {
            AudioUnitGetProperty(self.instance, id, scope, elem,
                                 NonNull::new(value.as_mut_ptr()).unwrap().cast(),
                                 NonNull::from(&mut size))
        }, "AudioUnitGetProperty")?;
        Ok(unsafe { value.assume_init() })
    }

    fn initialize(&self) -> anyhow::Result<()> {
        check(unsafe { AudioUnitInitialize(self.instance) }, "AudioUnitInitialize")
    }

    /// Return the unit to the unconfigured state so its properties can be set again.
    ///
    /// `StreamFormat` and `BufferFrameSize` are rejected on an initialised unit, so this is
    /// what makes reconfiguration possible. The INSTANCE survives: it is not disposed and
    /// not recreated, which is the whole point — see `reconfigure`.
    fn uninitialize(&self) -> anyhow::Result<()> {
        check(unsafe { AudioUnitUninitialize(self.instance) }, "AudioUnitUninitialize")
    }

    fn start(&self, what: &str) -> anyhow::Result<()> {
        check(unsafe { AudioOutputUnitStart(self.instance) },
              &format!("AudioOutputUnitStart {what}"))
    }

    fn stop(&self, what: &str) -> anyhow::Result<()> {
        check(unsafe { AudioOutputUnitStop(self.instance) },
              &format!("AudioOutputUnitStop {what}"))
    }
}

impl Drop for Unit {
    /// Stop → uninitialise → dispose, the specified teardown order.
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.instance);
            AudioUnitUninitialize(self.instance);
            AudioComponentInstanceDispose(self.instance);
        }
    }
}

// ── Sample rate ───────────────────────────────────────────────────────────────

/// Ask the device for `rate`: set the nominal rate, check the status, continue.
///
/// Deliberately NOT `macos_helpers::set_device_sample_rate`, which differs from this in
/// three ways, each able to fail a device that works:
///
///   - it registers a rate listener and waits up to two seconds for the change to be
///     reported back;
///   - it turns that timeout into a hard `UnsupportedSampleRate` failure;
///   - it accepts a rate only against a DISCRETE advertised range (`min == max == rate`),
///     so a device advertising a continuous 44.1–192 kHz range is rejected outright,
///     before any wait, though it supports 48 kHz perfectly well.
///
/// It early-outs when the device already reports the target rate, so none of that runs in
/// the common case — but "usually skipped" is not the same as correct.
///
/// The rate change is asynchronous — the HAL returns success and the device reports the new
/// rate around 100 ms later — so a caller that reads it back immediately still sees the old
/// value. Waiting for it is what costs: a device whose clock is slaved to a network master
/// may never report the exact value, and a hard timeout then fails a device that works.
///
/// Set unconditionally rather than read-then-set: writing the value a device already holds
/// is a no-op in the HAL.
fn set_nominal_sample_rate(id: AudioDeviceID, rate: f64) -> anyhow::Result<()> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope:    kAudioObjectPropertyScopeGlobal,
        mElement:  kAudioObjectPropertyElementMain,
    };
    let rate: f64 = rate;
    check(unsafe {
        AudioObjectSetPropertyData(id, NonNull::from(&address), 0, null(),
                                   std::mem::size_of::<f64>() as u32,
                                   NonNull::from(&rate).cast())
    }, &format!("setting a {rate} Hz nominal sample rate"))
}

// ── Stream format ─────────────────────────────────────────────────────────────

/// The ASBD Cascade asks for on every unit: interleaved 32-bit float, one frame per packet.
///
/// f32 throughout, with no integer paths. The interleaving
/// is Cascade's own choice: the engine works in one flat `[f32]`, and an interleaved ASBD is what makes the callback's buffer
/// exactly that with no repacking. CoreAudio's canonical Mac format is non-interleaved
/// float, so this is a deliberate departure the HAL is asked for explicitly.
fn f32_asbd(channels: u32, rate: f64) -> AudioStreamBasicDescription {
    StreamFormat {
        sample_rate:   rate,
        sample_format: SampleFormat::F32,
        // IS_PACKED without IS_NON_INTERLEAVED is what makes it interleaved.
        flags:         LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
        channels,
    }.to_asbd()
}

// ── The shared configure sequence ─────────────────────────────────────────────

/// Which direction a unit is being configured for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir { Output, Input }

impl Dir {
    fn label(self) -> &'static str {
        match self { Dir::Input => "input", Dir::Output => "output" }
    }
}

/// Apply the six properties both units share, in the specified order
/// (CASCADE_AUDIO_RECEIVE_SPEC §13.1a).
///
/// The seventh — the callback — differs per direction and is applied by the caller, which
/// also owns the state the callback points at and so must control when it is installed.
///
/// The two units differ only in the `EnableIO` values and the `StreamFormat` address:
///
///   - an OUTPUT unit disables input element 1, enables output element 0, and takes its
///     format on scope Input / element 0 — the side the host feeds;
///   - an INPUT unit does the reverse, and takes its format on scope Output / element 1 —
///     the side the host reads.
///
/// Returns the unit's maximum slice: the largest number of frames one callback can carry.
fn configure(unit: &Unit, dev: &Device, dir: Dir, channels: u32, rate: f64, period: u32)
    -> anyhow::Result<u32>
{
    set_nominal_sample_rate(dev.id, rate)?;

    // 1, 2 — EnableIO. Same property, scope and element on both units; only the values swap.
    let (input_en, output_en): (u32, u32) = match dir {
        Dir::Output => (0, 1),
        Dir::Input  => (1, 0),
    };
    unit.set(kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input, ELEM_INPUT,
             &input_en, "EnableIO(input)")?;
    unit.set(kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Output, ELEM_OUTPUT,
             &output_en, "EnableIO(output)")?;

    // 3 — bind the unit to this device. Must come after EnableIO: the HAL rejects a device
    //     that cannot serve the enabled direction, and the enables are what say which.
    unit.set(kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, ELEM_OUTPUT,
             &dev.id, "CurrentDevice")?;

    // 4 — the format, on whichever side faces the host.
    let asbd = f32_asbd(channels, rate);
    let (fmt_scope, fmt_elem) = match dir {
        Dir::Output => (kAudioUnitScope_Input,  ELEM_OUTPUT),
        Dir::Input  => (kAudioUnitScope_Output, ELEM_INPUT),
    };
    unit.set(kAudioUnitProperty_StreamFormat, fmt_scope, fmt_elem, &asbd, "StreamFormat")?;

    // 5, 6 — the callback period. BOTH are required: MaximumFramesPerSlice only bounds the
    //        unit's internal working buffer, and BufferFrameSize is what actually requests
    //        the period. Setting only the first leaves the period unchanged.
    //        BufferFrameSize goes to scope Input / element 1 on BOTH units — counter-
    //        intuitive on a playback unit, but as specified.
    unit.set(kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, ELEM_OUTPUT,
             &period, "MaximumFramesPerSlice")?;
    unit.set(kAudioDevicePropertyBufferFrameSize, kAudioUnitScope_Input, ELEM_INPUT,
             &period, "BufferFrameSize")?;

    // A request is not a guarantee. When the HAL grants a LARGER period — a device whose
    // minimum buffer is above the one asked for — the maximum slice set above is smaller than
    // the callbacks the device will now deliver, and every one of them fails inside the unit
    // before reaching our callback: silence, with no error surfacing anywhere. So the slice
    // follows the grant upward. A SMALLER grant needs nothing: the slice already covers it.
    let slice = match unit_granted_period(unit) {
        Some(granted) if granted > period => {
            unit.set(kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global,
                     ELEM_OUTPUT, &granted, "MaximumFramesPerSlice (granted)")?;
            granted
        }
        _ => period,
    };
    Ok(slice)
}

/// Read back the period the HAL granted, from the address it was written to.
///
/// A request is not a guarantee; the point of reading it back is that a substitution is
/// visible rather than silent.
fn unit_granted_period(unit: &Unit) -> Option<u32> {
    unit.get::<u32>(kAudioDevicePropertyBufferFrameSize, kAudioUnitScope_Input, ELEM_INPUT).ok()
}

// ── Playback ──────────────────────────────────────────────────────────────────

/// What the render trampoline is handed back through `inputProcRefCon`.
struct RenderState {
    render:   Box<dyn FnMut(&mut [f32]) + Send>,
    channels: usize,
}

/// The C entry point CoreAudio calls on the real-time thread to fill a playback buffer.
///
/// Installed by hand rather than through `AudioUnit::set_render_callback`, which installs at
/// scope Input / element 0 and validates the caller's data type against
/// `output_stream_format()` — scope Output. On an output AUHAL unit the format Cascade sets
/// lives on scope INPUT, so that check reads the hardware side rather than the side that
/// was configured, and rejects a correctly configured unit.
///
/// Panics are caught rather than allowed to unwind: this frame is called from C, where
/// unwinding is undefined behaviour. `catch_unwind` costs nothing when nothing panics.
unsafe extern "C-unwind" fn render_trampoline(
    ref_con: NonNull<c_void>,
    _flags:  NonNull<AudioUnitRenderActionFlags>,
    _time:   NonNull<AudioTimeStamp>,
    _bus:    u32,
    frames:  u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let state = &mut *(ref_con.as_ptr() as *mut RenderState);
        if io_data.is_null() { return; }
        // Interleaved: the HAL presents a single buffer holding every channel.
        let buffer = (*io_data).mBuffers[0];
        if buffer.mData.is_null() { return; }
        // Trust the frame count only as far as the buffer the HAL actually sized; a short
        // buffer would otherwise be written past its end.
        let capacity = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
        let len = (frames as usize * state.channels).min(capacity);
        (state.render)(std::slice::from_raw_parts_mut(buffer.mData as *mut f32, len));
    }));
    // Nothing useful can be logged from a real-time thread mid-panic; the non-zero status
    // is what tells the HAL the render failed.
    if result.is_ok() { 0 } else { PARAM_ERROR }
}

/// A configured AUHAL output unit, prepared but not necessarily running.
pub struct OutputUnit {
    unit:     Unit,
    state:    *mut RenderState,
    device:   Device,
    channels: u32,
    rate:     f64,
    period:   u32,
}

// The unit and its callback state are owned exclusively by this struct; the raw pointer is
// a leaked Box that this type alone frees.
unsafe impl Send for OutputUnit {}

impl OutputUnit {
    /// Build and configure an output unit for `device`, prepared but NOT started.
    ///
    /// Starting is separate because reconfiguration requires both units prepared before
    /// either runs, then input started ahead of output — see §5.2 of the receive spec.
    pub fn open<R>(dev: &Device, channels: u32, rate: f64, period: u32, render: R)
        -> anyhow::Result<Self>
    where
        R: FnMut(&mut [f32]) + Send + 'static,
    {
        let unit = Unit::new()?;
        configure(&unit, dev, Dir::Output, channels, rate, period)?;

        let state = Box::into_raw(Box::new(RenderState {
            render:   Box::new(render),
            channels: channels as usize,
        }));
        let callback = AURenderCallbackStruct {
            inputProc:       Some(render_trampoline),
            inputProcRefCon: state as *mut c_void,
        };
        // From here `state` is owned by this function, so the remaining fallible steps run
        // together and their error path frees it.
        let finish = || -> anyhow::Result<()> {
            unit.set(kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Global, ELEM_OUTPUT,
                     &callback, "SetRenderCallback")?;
            unit.initialize()
        };
        if let Err(e) = finish() {
            unsafe { drop(Box::from_raw(state)) };
            return Err(e);
        }
        Ok(OutputUnit { unit, state, device: *dev, channels, rate, period })
    }

    /// Begin calling the render callback.
    pub fn start(&self) -> anyhow::Result<()> { self.unit.start("output") }

    /// Stop calling the render callback. The unit stays configured and can be restarted.
    pub fn stop(&self) -> anyhow::Result<()> { self.unit.stop("output") }

    /// The period the HAL actually granted.
    pub fn granted_period(&self) -> Option<u32> { unit_granted_period(&self.unit) }

    /// The period that was requested.
    pub fn requested_period(&self) -> u32 { self.period }

    /// The channel count the unit was configured with.
    pub fn channels(&self) -> u32 { self.channels }
}

impl Drop for OutputUnit {
    fn drop(&mut self) {
        // Tear the unit down before freeing the state the trampoline dereferences. `Unit`'s
        // own Drop would do this, but it runs AFTER this body — by which point the state
        // would already be gone.
        unsafe {
            AudioOutputUnitStop(self.unit.instance);
            AudioUnitUninitialize(self.unit.instance);
            drop(Box::from_raw(self.state));
        }
    }
}

// ── Capture ───────────────────────────────────────────────────────────────────

/// What the capture trampoline is handed back through `inputProcRefCon`.
///
/// Carries the unit's own instance pointer, because an input callback is only a
/// notification that frames are ready: the data has to be pulled with `AudioUnitRender`
/// from inside the callback, which needs the instance.
struct CaptureState {
    capture:  Box<dyn FnMut(&[f32]) + Send>,
    channels: usize,
    instance: AudioComponentInstance,
    /// Pre-allocated to the unit's maximum slice. Never resized, because the only place it
    /// could be resized from is the real-time thread.
    buffer:   Vec<f32>,
    /// Where an oversize callback is reported, and whether it already has been.
    reporter: Arc<FaultReporter>,
    oversize_reported: bool,
}

/// The C entry point CoreAudio calls on the real-time thread when captured frames are ready.
///
/// Unlike the render callback, this one is handed no buffer — `io_data` is null. The frames
/// are pulled with `AudioUnitRender` into a buffer allocated once at configure time.
///
/// If the HAL ever asks for more frames than that buffer holds, this returns an error rather
/// than growing it: the buffer is sized to the unit's maximum slice, which the HAL does not
/// exceed, and allocating on the audio thread to handle it if it did would be the worse
/// failure. It is reported as a fault — once per stream — rather than left as a bare status
/// the HAL swallows, since the capture would otherwise be silent with nothing said.
unsafe extern "C-unwind" fn capture_trampoline(
    ref_con: NonNull<c_void>,
    flags:   NonNull<AudioUnitRenderActionFlags>,
    time:    NonNull<AudioTimeStamp>,
    bus:     u32,
    frames:  u32,
    _io_data: *mut AudioBufferList,
) -> OSStatus {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let state = &mut *(ref_con.as_ptr() as *mut CaptureState);
        let wanted = frames as usize * state.channels;
        if wanted > state.buffer.len() {
            if !state.oversize_reported {
                // One allocation for the message, once per stream, and only on a path the
                // HAL's own contract says cannot run. try_lock: never wait on this thread.
                state.oversize_reported = state.reporter.try_report(StreamFault::Transient(
                    format!("capture callback carried {} frames, more than the {} the unit \
                             was built for — this input is silent until it is rebuilt",
                            frames, state.buffer.len() / state.channels.max(1))));
            }
            return PARAM_ERROR;
        }

        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: state.channels as u32,
                mDataByteSize:   (wanted * std::mem::size_of::<f32>()) as u32,
                mData:           state.buffer.as_mut_ptr() as *mut c_void,
            }],
        };
        let status = AudioUnitRender(state.instance, flags.as_ptr(), time, bus, frames,
                                     NonNull::from(&mut list));
        if status != 0 { return status; }

        // Read the length back from the list: AudioUnitRender may deliver fewer bytes than
        // were asked for, and the count it reports is the one that describes real frames.
        let got = list.mBuffers[0].mDataByteSize as usize / std::mem::size_of::<f32>();
        (state.capture)(&state.buffer[..got.min(wanted)]);
        0
    }));
    match result {
        Ok(status) => status,
        Err(_)     => PARAM_ERROR,
    }
}

/// A configured AUHAL input unit, prepared but not necessarily running.
pub struct InputUnit {
    unit:     Unit,
    state:    *mut CaptureState,
    device:   Device,
    channels: u32,
    rate:     f64,
    period:   u32,
}

unsafe impl Send for InputUnit {}

impl InputUnit {
    /// Build and configure an input unit for `device`, prepared but NOT started.
    ///
    /// `capture` is handed each block of interleaved f32 frames as they arrive; `reporter`
    /// is where a capture fault found on the audio thread is reported.
    pub fn open<C>(dev: &Device, channels: u32, rate: f64, period: u32, capture: C,
                   reporter: Arc<FaultReporter>)
        -> anyhow::Result<Self>
    where
        C: FnMut(&[f32]) + Send + 'static,
    {
        let unit = Unit::new()?;
        // Size the pull buffer from the unit's maximum slice — the requested period, or the
        // granted one when the HAL granted more — so no callback can carry more frames than
        // it holds.
        let frames = configure(&unit, dev, Dir::Input, channels, rate, period)?;
        let state = Box::into_raw(Box::new(CaptureState {
            capture:  Box::new(capture),
            channels: channels as usize,
            instance: unit.instance,
            buffer:   vec![0.0; frames as usize * channels as usize],
            reporter,
            oversize_reported: false,
        }));
        let callback = AURenderCallbackStruct {
            inputProc:       Some(capture_trampoline),
            inputProcRefCon: state as *mut c_void,
        };
        let finish = || -> anyhow::Result<()> {
            unit.set(kAudioOutputUnitProperty_SetInputCallback, kAudioUnitScope_Global,
                     ELEM_OUTPUT, &callback, "SetInputCallback")?;
            unit.initialize()
        };
        if let Err(e) = finish() {
            unsafe { drop(Box::from_raw(state)) };
            return Err(e);
        }
        Ok(InputUnit { unit, state, device: *dev, channels, rate, period })
    }

    /// Begin calling the capture callback.
    pub fn start(&self) -> anyhow::Result<()> { self.unit.start("input") }

    /// Stop calling the capture callback. The unit stays configured and can be restarted.
    pub fn stop(&self) -> anyhow::Result<()> { self.unit.stop("input") }

    /// The period the HAL actually granted.
    pub fn granted_period(&self) -> Option<u32> { unit_granted_period(&self.unit) }

    /// The period that was requested.
    pub fn requested_period(&self) -> u32 { self.period }

    /// The channel count the unit was configured with.
    pub fn channels(&self) -> u32 { self.channels }
}

impl Drop for InputUnit {
    fn drop(&mut self) {
        // As for OutputUnit: uninitialise before the state the trampoline dereferences goes
        // away, since `Unit`'s own Drop runs after this body.
        unsafe {
            AudioOutputUnitStop(self.unit.instance);
            AudioUnitUninitialize(self.unit.instance);
            drop(Box::from_raw(self.state));
        }
    }
}

// ── Starting a pair ───────────────────────────────────────────────────────────

/// Start a prepared input/output pair in the specified order: input first, then output.
///
/// Both units must already be prepared — opened or reconfigured — with NEITHER started.
/// Resuming input ahead of output makes the transient window fill-before-drain rather than
/// drain-without-fill.
///
/// A function rather than a note in a comment so the ordering cannot drift between the boot
/// path, the device-change handlers and the period-change path.
pub fn start_pair(input: &InputUnit, output: &OutputUnit) -> anyhow::Result<()> {
    input.start()?;
    output.start()
}

/// Stop a pair, output first — the reverse of `start_pair`.
///
/// Stopping the drain before the fill leaves the same asymmetry the right way round: input
/// keeps filling for the moment it takes output to come to rest, rather than output draining
/// a source that has already stopped.
pub fn stop_pair(input: &InputUnit, output: &OutputUnit) -> anyhow::Result<()> {
    let out = output.stop();
    let inp = input.stop();
    out.and(inp)
}

/// The device's current nominal sample rate, or `None` if it will not report one.
fn nominal_sample_rate(id: AudioDeviceID) -> Option<f64> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope:    kAudioObjectPropertyScopeGlobal,
        mElement:  kAudioObjectPropertyElementMain,
    };
    let mut rate: f64 = 0.0;
    let mut size = std::mem::size_of::<f64>() as u32;   // in/out — see device_uid
    let status = unsafe {
        AudioObjectGetPropertyData(id, NonNull::from(&address), 0, null(),
                                   NonNull::from(&mut size), NonNull::from(&mut rate).cast())
    };
    if status == 0 { Some(rate) } else { None }
}

// ── Device capability ─────────────────────────────────────────────────────────

/// How many channels the device carries in `dir`.
///
/// Summed across the buffers of `kAudioDevicePropertyStreamConfiguration`: a device can
/// present its channels as several buffers, and the total is what a stream can carry.
fn channel_count(id: AudioDeviceID, dir: Dir) -> anyhow::Result<u16> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope:    match dir { Dir::Input  => kAudioObjectPropertyScopeInput,
                               Dir::Output => kAudioObjectPropertyScopeOutput },
        mElement:  kAudioObjectPropertyElementMain,
    };
    let mut size = 0u32;
    check(unsafe {
        AudioObjectGetPropertyDataSize(id, NonNull::from(&address), 0, null(),
                                       NonNull::from(&mut size))
    }, "reading the stream configuration size")?;

    let mut bytes = vec![0u8; size as usize];
    check(unsafe {
        AudioObjectGetPropertyData(id, NonNull::from(&address), 0, null(),
                                   NonNull::from(&mut size),   // in/out — see device_uid
                                   NonNull::new(bytes.as_mut_ptr()).unwrap().cast())
    }, "reading the stream configuration")?;

    let list = bytes.as_ptr() as *const AudioBufferList;
    let total: u32 = unsafe {
        let n = (*list).mNumberBuffers as usize;
        std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n)
            .iter().map(|b| b.mNumberChannels).sum()
    };
    Ok(total.min(u16::MAX as u32) as u16)
}

/// Whether the device can run at `rate`.
///
/// Handles CONTINUOUS ranges as well as discrete points: a device advertising 44.1–192 kHz
/// supports 48 kHz, and rejecting it because `min != max` would be wrong. That is exactly
/// the mistake `macos_helpers::set_device_sample_rate` makes.
fn supports_rate(id: AudioDeviceID, rate: f64) -> bool {
    macos_helpers::get_available_sample_rates(id)
        .map(|rs| rs.iter().any(|r| r.mMinimum <= rate && rate <= r.mMaximum))
        .unwrap_or(false)
}

/// A negotiated, ready-to-open configuration.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    channels:    u16,
    sample_rate: u32,
    period:      u32,
}

impl Config {
    pub fn channels(&self) -> u16 { self.channels }
    pub fn sample_rate(&self) -> u32 { self.sample_rate }
}

/// Confirm the device can serve 48 kHz in this direction, and report its channel count.
///
/// There is nothing to negotiate. Cascade works in f32 at 48 kHz and asks the HAL for
/// exactly that; CoreAudio converts between the unit's stream format and the hardware's
/// without a format search, so the only questions are whether the device has channels in
/// this direction and whether it can run at the rate. A device that cannot is a
/// configuration failure, never a silent substitution.
pub fn find_config(device: &Device, dir: Dir, period_frames: usize)
    -> anyhow::Result<Config>
{
    let name = device_name(device);
    let channels = channel_count(device.id, dir)
        .map_err(|e| anyhow::anyhow!("cannot read the channel count of {} device '{name}': {e}",
                                     dir.label()))?;
    if channels == 0 {
        anyhow::bail!("{} device '{name}' has no {} channels", dir.label(), dir.label());
    }
    if !supports_rate(device.id, 48_000.0) {
        anyhow::bail!("{} device '{name}' does not support 48 kHz", dir.label());
    }
    Ok(Config { channels, sample_rate: 48_000, period: period_frames as u32 })
}

/// Whether this error means the device is held by something else.
///
/// Always false. CoreAudio devices are shared by default, so there is no busy state to
/// retry against; the retry-on-busy path exists for backends that open exclusively, ALSA in
/// particular. A device another process has hogged fails to open outright rather than
/// reporting busy, and the remedy there is the hog owner releasing it, not a retry.
pub fn is_device_busy(_err: &anyhow::Error) -> bool { false }

// ── Faults ────────────────────────────────────────────────────────────────────

/// A fault reported by a live stream, classified by what it means for the caller.
pub enum StreamFault {
    /// The hardware genuinely disappeared. Drive the device-lost path.
    DeviceLost,
    /// The stream is no longer valid but the DEVICE is still present — its sample rate
    /// changed underneath us. The remedy is a rebuild, not a teardown.
    Invalidated,
    /// Anything else: log it and keep the device.
    Transient(String),
}

/// A stream's fault callback, shared by the HAL property listeners and the capture callback.
///
/// The listeners run on a HAL thread and may wait for the lock; the capture callback runs on
/// the real-time thread and only ever try-locks.
pub struct FaultReporter {
    on_fault: parking_lot::Mutex<Box<dyn FnMut(StreamFault) + Send>>,
}

impl FaultReporter {
    fn new<F: FnMut(StreamFault) + Send + 'static>(on_fault: F) -> Arc<Self> {
        Arc::new(Self { on_fault: parking_lot::Mutex::new(Box::new(on_fault)) })
    }

    /// Report from a thread that may wait.
    fn report(&self, fault: StreamFault) {
        (self.on_fault.lock())(fault);
    }

    /// Report without waiting. False when the lock was busy and nothing was reported.
    fn try_report(&self, fault: StreamFault) -> bool {
        match self.on_fault.try_lock() {
            Some(mut f) => { (f)(fault); true }
            None => false,
        }
    }

    /// A reporter that discards everything, for the hardware tests.
    #[cfg(test)]
    fn ignore() -> Arc<Self> {
        Self::new(|_| {})
    }
}

/// What the HAL property listener is handed back.
struct FaultState {
    reporter: Arc<FaultReporter>,
    /// The rate this stream was opened at. A change away from it is an invalidation.
    rate:     f64,
    device:   AudioDeviceID,
}

/// The HAL's notification that a watched property changed.
///
/// An audio unit has no error callback of its own, so the two faults that matter are
/// watched directly:
///
///   - `DeviceIsAlive` going false is a genuine disconnect;
///   - the nominal sample rate moving off the one the stream was opened at means something
///     else reconfigured the device, and the remedy is a rebuild that re-asserts 48 kHz.
///
/// Runs on a HAL-owned thread, not the audio thread, so it may log and allocate. Panics are
/// still caught: it is called from C.
unsafe extern "C-unwind" fn fault_listener(
    _id:        AudioObjectID,
    n_addresses: u32,
    addresses:  NonNull<AudioObjectPropertyAddress>,
    client:     *mut c_void,
) -> OSStatus {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if client.is_null() { return; }
        let state = &mut *(client as *mut FaultState);
        let addrs = std::slice::from_raw_parts(addresses.as_ptr(), n_addresses as usize);
        for a in addrs {
            match a.mSelector {
                s if s == kAudioDevicePropertyDeviceIsAlive => {
                    let mut alive: u32 = 1;
                    let mut size = std::mem::size_of::<u32>() as u32;   // in/out — see device_uid
                    let st = AudioObjectGetPropertyData(
                        state.device, NonNull::from(a), 0, null(),
                        NonNull::from(&mut size), NonNull::from(&mut alive).cast());
                    // A device that has gone away often stops answering at all, which is
                    // itself the signal.
                    if st != 0 || alive == 0 { state.reporter.report(StreamFault::DeviceLost); }
                }
                s if s == kAudioDevicePropertyNominalSampleRate => {
                    if let Some(now) = nominal_sample_rate(state.device) {
                        if now != state.rate {
                            state.reporter.report(StreamFault::Invalidated);
                        }
                    }
                }
                _ => {}
            }
        }
    }));
    0
}

/// Registers the two fault listeners for a device and removes them on drop.
struct FaultWatch {
    device:   AudioDeviceID,
    state:    *mut FaultState,
    attached: bool,
}

/// The two properties worth watching on an open device.
const WATCHED: [u32; 2] = [kAudioDevicePropertyDeviceIsAlive,
                           kAudioDevicePropertyNominalSampleRate];

fn watched_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope:    kAudioObjectPropertyScopeGlobal,
        mElement:  kAudioObjectPropertyElementMain,
    }
}

// The state pointer is owned exclusively by this struct, and the HAL calls the listener on
// its own thread regardless of which thread holds the watch — so moving one between threads
// changes nothing about how it is used. This is what lets a retired stream be dropped on a
// dedicated thread rather than on the control loop.
unsafe impl Send for FaultWatch {}

impl FaultWatch {
    fn new(device: AudioDeviceID, rate: f64, reporter: Arc<FaultReporter>) -> Self {
        let state = Box::into_raw(Box::new(FaultState { reporter, rate, device }));
        for selector in WATCHED {
            let address = watched_address(selector);
            let status = unsafe {
                AudioObjectAddPropertyListener(device, NonNull::from(&address),
                                               Some(fault_listener), state as *mut c_void)
            };
            if status != 0 {
                // Not fatal. A device that will not report one of these is simply one whose
                // faults arrive later, through the device watcher, rather than not at all.
                tracing::debug!("CoreAudio: device {device} refused a listener for \
                                 {selector:#x} (OSStatus {status})");
            }
        }
        FaultWatch { device, state, attached: true }
    }

    /// Remove the listeners, leaving the state allocated.
    ///
    /// Called before the unit is stopped: each device's property listener is removed ahead
    /// of its `AudioOutputUnitStop`. A listener left armed
    /// across a teardown can report the rate change the teardown itself causes, which
    /// arrives as a fault on a stream that is already going away.
    ///
    /// Idempotent: `Drop` calls it again for a watch that was never detached explicitly.
    fn detach(&mut self) {
        if !self.attached { return; }
        for selector in WATCHED {
            let address = watched_address(selector);
            unsafe {
                AudioObjectRemovePropertyListener(self.device, NonNull::from(&address),
                                                  Some(fault_listener),
                                                  self.state as *mut c_void);
            }
        }
        self.attached = false;
    }
}

impl Drop for FaultWatch {
    fn drop(&mut self) {
        self.detach();
        // Only after every listener is removed can the state they point at go away.
        unsafe { drop(Box::from_raw(self.state)) };
    }
}

// ── The seam ──────────────────────────────────────────────────────────────────

/// An open stream in one direction, with its fault listeners attached.
///
/// Opaque above the backend, which is why it is a struct wrapping the direction rather than
/// a bare enum: callers hold one, hand it to `play` and `pause`, and drop it. `_watch` is
/// held only for its `Drop`, which removes the listeners before the state they point at
/// goes away.
///
/// The two directions are separate units on separate devices, so a stream is one or the
/// other, never a pair.
pub struct Stream {
    inner:  Direction,
    _watch: FaultWatch,
}

enum Direction {
    Output(OutputUnit),
    Input(InputUnit),
}

/// Open an output stream, prepared but NOT started. `play` starts it.
pub fn open_output<R, F>(device: &Device, cfg: &Config, render: R, on_fault: F)
    -> anyhow::Result<Stream>
where
    R: FnMut(&mut [f32]) + Send + 'static,
    F: FnMut(StreamFault) + Send + 'static,
{
    let rate = cfg.sample_rate as f64;
    let unit = OutputUnit::open(device, cfg.channels as u32, rate, cfg.period, render)?;
    Ok(Stream { inner: Direction::Output(unit),
                _watch: FaultWatch::new(device.id, rate, FaultReporter::new(on_fault)) })
}

/// Open an input stream, prepared but NOT started. `play` starts it.
pub fn open_input<C, F>(device: &Device, cfg: &Config, capture: C, on_fault: F)
    -> anyhow::Result<Stream>
where
    C: FnMut(&[f32]) + Send + 'static,
    F: FnMut(StreamFault) + Send + 'static,
{
    let rate = cfg.sample_rate as f64;
    let reporter = FaultReporter::new(on_fault);
    let unit = InputUnit::open(device, cfg.channels as u32, rate, cfg.period, capture,
                               Arc::clone(&reporter))?;
    Ok(Stream { inner: Direction::Input(unit),
                _watch: FaultWatch::new(device.id, rate, reporter) })
}

/// Start a prepared stream — `AudioOutputUnitStart`.
pub fn start(stream: &Stream) -> anyhow::Result<()> {
    match &stream.inner {
        Direction::Output(unit) => unit.start(),
        Direction::Input(unit)  => unit.start(),
    }
}

/// Stop a running stream — `AudioOutputUnitStop`. The unit stays configured and allocated;
/// this is the call made before the teardown settle, not a lighter "pause" that leaves the
/// callback running.
///
pub fn stop(stream: &Stream) -> anyhow::Result<()> {
    match &stream.inner {
        Direction::Output(unit) => unit.stop(),
        Direction::Input(unit)  => unit.stop(),
    }
}

/// Tell the HAL not to trade latency for power — `kAudioHardwarePropertyPowerHint` set to
/// `None` on the system object.
///
/// Written at the start of every audio start, so it is re-asserted on every rebuild rather
/// than set once at boot. The alternative value, `FavorSavingPower`,
/// lets the HAL apply optimisations that compromise latency, which is the opposite of what
/// this application wants.
///
/// Best-effort: a failure here costs latency margin, not correctness, so it is logged once
/// and execution continues.
pub fn set_power_hint() {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyPowerHint,
        mScope:    kAudioObjectPropertyScopeGlobal,
        mElement:  kAudioObjectPropertyElementMain,
    };
    let hint: u32 = 0;   // kAudioHardwarePowerHintNone
    let status = unsafe {
        AudioObjectSetPropertyData(kAudioObjectSystemObject as AudioObjectID,
                                   NonNull::from(&address), 0, null(),
                                   std::mem::size_of::<u32>() as u32,
                                   NonNull::from(&hint).cast())
    };
    if status != 0 {
        static WARNED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!("CoreAudio: power hint not accepted (OSStatus {status})");
        }
    }
}

/// Remove this stream's device fault listeners, without stopping or disposing it.
///
/// Called immediately before the stop that begins a teardown: each device's property
/// listener is removed ahead of its `AudioOutputUnitStop`. A
/// listener left armed across a teardown reports the rate change the teardown itself causes,
/// arriving as a fault on a stream that is already going away.
///
/// Not called on a freshly built stream — that one needs its listeners.
pub fn detach_faults(stream: &mut Stream) {
    stream._watch.detach();
}

/// The callback period the HAL granted this stream.
pub fn granted_period(stream: &Stream) -> Option<usize> {
    match &stream.inner {
        Direction::Output(unit) => unit.granted_period(),
        Direction::Input(unit)  => unit.granted_period(),
    }.map(|p| p as usize)
}

/// The period a trial open of `dev` must ask for, or `None` to skip the trial.
///
/// `kAudioDevicePropertyBufferFrameSize` belongs to the DEVICE, not the unit, so a trial open
/// that asked for any other size would move the period under a unit already running on the
/// same device — the other direction of one interface, say. Asking for the size the device
/// already has changes nothing. A device that will not report it is not probed at all.
pub fn probe_period(dev: &Device, _dir: Dir) -> Option<usize> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyBufferFrameSize,
        mScope:    kAudioObjectPropertyScopeGlobal,
        mElement:  kAudioObjectPropertyElementMain,
    };
    let mut frames: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(dev.id, NonNull::from(&address), 0, null(),
                                   NonNull::from(&mut size),   // in/out — see device_uid
                                   NonNull::from(&mut frames).cast())
    };
    (status == 0 && frames > 0).then_some(frames as usize)
}

/// The shortest period `dev` will run at, for the receive-buffer floor. Not learned here:
/// always `None`, so on macOS every buffer setting stays offered and no setpoint floor is
/// derived from the device.
pub fn smallest_period(_dev: &Device, _dir: Dir) -> anyhow::Result<Option<usize>> {
    Ok(None)
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHANNELS: u32 = 2;
    const RUN_MS:   u64 = 300;
    /// The three periods §13 uses. One period alone proves nothing — a device already
    /// sitting at 480 frames would pass without `BufferFrameSize` having any effect.
    /// Asking for each in turn, and getting each, is what shows the property sets the cadence.
    const PERIODS: [u32; 3] = [480, 120, 240];

    /// Serialises the hardware tests against each other.
    ///
    /// `kAudioDevicePropertyBufferFrameSize` belongs to the DEVICE, not the unit, so two
    /// units open on one device share a single period — the last request wins, and the
    /// other unit is simply handed it. That is the HAL enforcing the shared-period model
    /// §13 describes, not something to work around; but cargo runs tests in parallel
    /// threads, so without this a test asking for 480 gets whatever period a concurrent
    /// test happened to set.
    static HARDWARE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the hardware lock, ignoring poisoning — a panic in one test tells us nothing
    /// about whether the audio device is usable by the next.
    fn hardware() -> std::sync::MutexGuard<'static, ()> {
        HARDWARE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Poll until the device reports `want`, returning how long it took.
    ///
    /// Only ever used to set a test UP. A nominal-rate change lands asynchronously, so a
    /// test that moves a device and then does something else immediately is racing its own
    /// setup — two pending transitions, and the last to land wins. The backend itself never
    /// waits like this; that is the point.
    fn settle(id: AudioDeviceID, want: f64) -> Option<std::time::Duration> {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(2) {
            if nominal_sample_rate(id) == Some(want) { return Some(start.elapsed()); }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    fn default_device(input: bool) -> Device {
        let id = macos_helpers::get_default_device_id(input)
            .unwrap_or_else(|| panic!("no default {} device", if input { "input" } else { "output" }));
        Device { id }
    }

    /// Open the default output, run it, and confirm CoreAudio drives the callback at each
    /// period asked for.
    ///
    /// Writes silence, so it is safe to run with speakers up. Ignored by default because it
    /// opens real hardware: `cargo test -- --ignored coreaudio`.
    #[test]
    #[ignore = "touches real audio hardware"]
    fn drives_a_render_callback_at_each_requested_period() {
        let _hw = hardware();
        let dev = default_device(false);
        println!("output: {} ({:?})", device_name(&dev), device_uid(&dev));

        for period in PERIODS {
            let calls  = Arc::new(AtomicUsize::new(0));
            let frames = Arc::new(AtomicUsize::new(0));
            let (c, f) = (calls.clone(), frames.clone());

            let unit = OutputUnit::open(&dev, CHANNELS, 48_000.0, period,
                                            move |out: &mut [f32]| {
                c.fetch_add(1, Ordering::Relaxed);
                f.fetch_add(out.len(), Ordering::Relaxed);
                out.fill(0.0);
            }).expect("opening the output unit");

            let granted = unit.granted_period();
            unit.start().expect("starting the output unit");
            std::thread::sleep(std::time::Duration::from_millis(RUN_MS));
            unit.stop().expect("stopping the output unit");

            let (calls, frames) = (calls.load(Ordering::Relaxed), frames.load(Ordering::Relaxed));
            let per_call = if calls == 0 { 0 } else { frames / calls / CHANNELS as usize };
            println!("  render  {period:>3} → granted {granted:?}, {calls} calls, {per_call} frames each");

            assert_eq!(granted, Some(period),
                       "asked for a {period}-frame period, the HAL granted {granted:?}");
            assert!(calls > 0, "the render callback never fired at {period} frames");
            assert_eq!(per_call as u32, period,
                       "callbacks carried {per_call} frames at a granted period of {period}");
            let expected = RUN_MS as usize * 48 / period as usize;
            assert!(calls > expected / 2,
                    "{calls} callbacks at {period} frames, expected around {expected}");
        }
    }

    /// Open the default input, run it, and confirm frames are really being pulled through
    /// `AudioUnitRender` at each period asked for.
    ///
    /// Asserts on the SHAPE of what arrives, never on its content: whether there is signal
    /// depends on what is plugged in and on whether the process holds microphone permission,
    /// neither of which this is testing.
    #[test]
    #[ignore = "touches real audio hardware"]
    fn pulls_captured_frames_at_each_requested_period() {
        let _hw = hardware();
        let dev = default_device(true);
        println!("input: {} ({:?})", device_name(&dev), device_uid(&dev));

        for period in PERIODS {
            let calls  = Arc::new(AtomicUsize::new(0));
            let frames = Arc::new(AtomicUsize::new(0));
            let short  = Arc::new(AtomicUsize::new(0));
            let (c, f, s) = (calls.clone(), frames.clone(), short.clone());

            let unit = InputUnit::open(&dev, CHANNELS, 48_000.0, period,
                                           move |input: &[f32]| {
                c.fetch_add(1, Ordering::Relaxed);
                f.fetch_add(input.len(), Ordering::Relaxed);
                if input.is_empty() { s.fetch_add(1, Ordering::Relaxed); }
            }, FaultReporter::ignore()).expect("opening the input unit");

            let granted = unit.granted_period();
            unit.start().expect("starting the input unit");
            std::thread::sleep(std::time::Duration::from_millis(RUN_MS));
            unit.stop().expect("stopping the input unit");

            let (calls, frames) = (calls.load(Ordering::Relaxed), frames.load(Ordering::Relaxed));
            let per_call = if calls == 0 { 0 } else { frames / calls / CHANNELS as usize };
            println!("  capture {period:>3} → granted {granted:?}, {calls} calls, {per_call} frames each");

            assert_eq!(granted, Some(period),
                       "asked for a {period}-frame period, the HAL granted {granted:?}");
            assert!(calls > 0, "the capture callback never fired at {period} frames");
            assert_eq!(short.load(Ordering::Relaxed), 0, "some callbacks delivered no frames");
            assert_eq!(per_call as u32, period,
                       "callbacks carried {per_call} frames at a granted period of {period}");
            let expected = RUN_MS as usize * 48 / period as usize;
            assert!(calls > expected / 2,
                    "{calls} callbacks at {period} frames, expected around {expected}");
        }
    }

    /// Both units configured before either is started, then started input first.
    ///
    /// This is the ordering §5.2 requires, and the case that matters: an input and an output
    /// unit running at once against separate devices, each driving its own callback.
    #[test]
    #[ignore = "touches real audio hardware"]
    fn runs_an_input_and_an_output_unit_together() {
        let _hw = hardware();
        const PERIOD: u32 = 240;
        let (out_dev, in_dev) = (default_device(false), default_device(true));
        let rendered = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(AtomicUsize::new(0));
        let (r, c) = (rendered.clone(), captured.clone());

        // Prepare BOTH before starting EITHER.
        let output = OutputUnit::open(&out_dev, CHANNELS, 48_000.0, PERIOD,
                                          move |out: &mut [f32]| {
            r.fetch_add(out.len() / CHANNELS as usize, Ordering::Relaxed);
            out.fill(0.0);
        }).expect("opening the output unit");
        let input = InputUnit::open(&in_dev, CHANNELS, 48_000.0, PERIOD,
                                        move |inp: &[f32]| {
            c.fetch_add(inp.len() / CHANNELS as usize, Ordering::Relaxed);
        }, FaultReporter::ignore()).expect("opening the input unit");

        // Input first: it makes the transient fill-before-drain rather than drain-without-fill.
        input.start().expect("starting the input unit");
        output.start().expect("starting the output unit");
        std::thread::sleep(std::time::Duration::from_millis(RUN_MS));
        output.stop().expect("stopping the output unit");
        input.stop().expect("stopping the input unit");

        let (rendered, captured) = (rendered.load(Ordering::Relaxed), captured.load(Ordering::Relaxed));
        let expected = RUN_MS as usize * 48;
        println!("  duplex {PERIOD} → {rendered} frames out, {captured} frames in (~{expected} expected)");
        assert!(rendered > expected / 2, "only {rendered} frames rendered, expected ~{expected}");
        assert!(captured > expected / 2, "only {captured} frames captured, expected ~{expected}");
    }

    /// Prove the nominal-sample-rate write moves a device, and measure how long it takes.
    ///
    /// Every other test runs at 48 kHz on devices already there, so the rate path is a no-op
    /// in all of them. This one moves the device to a rate it is not at.
    ///
    /// The change is ASYNCHRONOUS: the HAL accepts the write and returns success, and the
    /// device reports the new rate some milliseconds later. Reading it back immediately
    /// still shows the old value. That is what the crate's rate listener is really for — the
    /// reason not to use it is its failure modes, not that the settle time is imaginary.
    /// The backend sets the rate and continues regardless, so this measures the delay
    /// rather than waiting on it.
    #[test]
    #[ignore = "touches real audio hardware"]
    fn sets_the_nominal_sample_rate_without_waiting() {
        let _hw = hardware();
        let dev = default_device(false);
        let original = nominal_sample_rate(dev.id).expect("device reports no sample rate");

        let rates = macos_helpers::get_available_sample_rates(dev.id)
            .expect("device lists no sample rates");
        // A rate the device really supports and is not already at. Discrete entries only:
        // a continuous range would need a value chosen from inside it, and the point here is
        // to move the device, not to explore range handling.
        let Some(target) = rates.iter()
            .filter(|r| r.mMinimum == r.mMaximum && r.mMinimum != original)
            .map(|r| r.mMinimum)
            .next()
        else {
            println!("  {} offers no second discrete rate — nothing to switch to",
                     device_name(&dev));
            return;
        };

        set_nominal_sample_rate(dev.id, target).expect("setting the target rate");
        let immediate = nominal_sample_rate(dev.id);
        let took = settle(dev.id, target);
        println!("  {}: {original} → {target} Hz — immediately after the write the device \
                  still reported {immediate:?}, settled after {took:?}",
                 device_name(&dev));

        // Restore before asserting, so a failure cannot leave the device moved.
        let restored = set_nominal_sample_rate(dev.id, original);
        let back = settle(dev.id, original);
        assert!(took.is_some(), "the device never reached {target} Hz");
        restored.expect("restoring the original rate");
        assert!(back.is_some(), "the device was left at {target} Hz");
    }

    /// Open a unit at a rate the device has NOT yet settled to.
    ///
    /// This is the production path, and the one the ~100 ms settle time puts in question:
    /// `configure` sets the nominal rate and then immediately sets a stream format at that
    /// rate and initialises, without waiting. If the asynchrony mattered, this is where it
    /// would show — a failed initialise, or a unit running at the old rate.
    #[test]
    #[ignore = "touches real audio hardware"]
    fn opens_a_unit_at_a_rate_the_device_has_not_settled_to() {
        let _hw = hardware();
        let dev = default_device(false);
        let original = nominal_sample_rate(dev.id).expect("device reports no sample rate");

        let rates = macos_helpers::get_available_sample_rates(dev.id)
            .expect("device lists no sample rates");
        let Some(other) = rates.iter()
            .filter(|r| r.mMinimum == r.mMaximum && r.mMinimum != 48_000.0)
            .map(|r| r.mMinimum)
            .next()
        else {
            println!("  {} offers no rate other than 48 kHz", device_name(&dev));
            return;
        };

        // Move the device away and let THAT change land, so what follows is a device
        // genuinely settled at the wrong rate rather than one with a transition still in
        // flight. Opening is what must not wait, not the setup.
        set_nominal_sample_rate(dev.id, other).expect("moving the device off 48 kHz");
        assert!(settle(dev.id, other).is_some(), "the device never reached {other} Hz");
        let before = nominal_sample_rate(dev.id);

        let frames = Arc::new(AtomicUsize::new(0));
        let f = frames.clone();
        let opened = OutputUnit::open(&dev, CHANNELS, 48_000.0, 240, move |out: &mut [f32]| {
            f.fetch_add(out.len() / CHANNELS as usize, Ordering::Relaxed);
            out.fill(0.0);
        });

        // Two windows: the first spans the rate transition, the second follows it. The
        // question is not whether the transition costs audio — it does — but whether the
        // unit recovers to full rate on its own afterwards.
        let outcome = opened.and_then(|unit| {
            unit.start()?;
            std::thread::sleep(std::time::Duration::from_millis(RUN_MS));
            let during = frames.load(Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(RUN_MS));
            let after = frames.load(Ordering::Relaxed) - during;
            unit.stop()?;
            Ok((during, after, nominal_sample_rate(dev.id)))
        });

        // Restore before asserting either way, and let it land — otherwise the next test
        // inherits a device mid-transition.
        let _ = set_nominal_sample_rate(dev.id, original);
        let _ = settle(dev.id, original);
        let (during, after, settled) =
            outcome.expect("opening a unit while the device was still at the old rate");

        let expected = RUN_MS as usize * 48;
        println!("  device was at {before:?} when the unit opened, ended at {settled:?}");
        println!("  first {RUN_MS} ms: {during} frames ({}% of {expected}) — spans the transition",
                 during * 100 / expected);
        println!("  next  {RUN_MS} ms: {after} frames ({}% of {expected}) — after it",
                 after * 100 / expected);

        assert!(after > expected * 9 / 10,
                "the unit did not recover to full rate: {after} frames in the second window, \
                 expected ~{expected}");
        assert_eq!(settled, Some(48_000.0),
                   "the device did not end up at 48 kHz");
    }
}
