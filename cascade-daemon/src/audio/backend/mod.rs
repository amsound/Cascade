//! Audio backend — the device layer, behind one compile-time-selected implementation.
//!
//! Everything above this module works in terms of `backend::Device` and `backend::Stream`
//! and never names a host toolkit. Which implementation supplies them is chosen here, the
//! same way `audio/scheduler/` chooses its scheduler.
//!
//! The seam exists so each platform gets the backend that expresses the specified device
//! behaviour directly, rather than whatever a portable abstraction happens to offer. Where a backend cannot express something the device layer needs, that is a
//! reason to change the backend, not to change what the layer asks for.
//!
//! ## What belongs here
//!
//! Backend-specific knowledge: how devices are enumerated and named, what a stable
//! identifier is, which enumerated entries are real hardware endpoints, how a native
//! device handle is reached for platform APIs the backend does not wrap, and how a stream
//! is opened, started, stopped and watched for faults.
//!
//! ## What does not
//!
//! Policy. `audio/mod.rs` decides which devices to OFFER and how a configured device is
//! resolved; this module only reports what exists and what each entry is.

// ── Implementation selection ──────────────────────────────────────────────────
//
// Named implementations only, deliberately. An unsupported target must fail HERE, with a
// message saying what to add, rather than silently taking a branch that cannot work.

/// The direct ALSA backend: `hw:` devices only, rate strictness asserted rather than
/// inferred, and the period COUNT chosen for a wall-clock margin.
#[cfg(target_os = "linux")]
mod alsa_backend;

/// The direct WASAPI backend, exclusive mode only.
#[cfg(target_os = "windows")]
mod wasapi_backend;

/// The direct CoreAudio backend: the units, their configure sequence and their lifecycle as
/// specified (CASCADE_AUDIO_RECEIVE_SPEC §13).
#[cfg(target_os = "macos")]
mod coreaudio_backend;

#[cfg(target_os = "linux")]
pub use alsa_backend::{
    Device,
    Stream,
    input_devices,
    output_devices,
    device_name,
    device_uid,
    is_hardware_endpoint,
    granted_period,
    probe_period,
    smallest_period,
    Dir,
    StreamFault,
    find_config,
    open_output,
    open_input,
    start,
    stop,
    detach_faults,
    set_power_hint,
    is_device_busy,
};

#[cfg(target_os = "windows")]
pub use wasapi_backend::{
    Device,
    Stream,
    input_devices,
    output_devices,
    device_name,
    device_uid,
    is_hardware_endpoint,
    granted_period,
    probe_period,
    smallest_period,
    Dir,
    StreamFault,
    find_config,
    open_output,
    open_input,
    start,
    stop,
    detach_faults,
    set_power_hint,
    is_device_busy,
};

#[cfg(target_os = "macos")]
pub use coreaudio_backend::{
    Device,
    Stream,
    input_devices,
    output_devices,
    device_name,
    device_uid,
    is_hardware_endpoint,
    granted_period,
    probe_period,
    smallest_period,
    Dir,
    StreamFault,
    find_config,
    open_output,
    open_input,
    start,
    stop,
    detach_faults,
    set_power_hint,
    is_device_busy,
    native_device_id,
};

// An unsupported target fails HERE rather than silently taking a branch that cannot work.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!(
    "no audio backend for this target. macOS is served by CoreAudio, Linux by ALSA and \
     Windows by WASAPI, all directly. Add a backend module here; do not reach for a \
     portable layer to fill the gap."
);
