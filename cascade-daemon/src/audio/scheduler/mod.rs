//! Audio scheduler abstraction — platform-specific per-channel serial queues.
//!
//! Both platforms have the same shape: per-channel SERIAL queues carry ordering, a
//! shared worker pool carries parallelism, and the thread count does not scale with the
//! number of queues.
//!
//! On macOS: libdispatch GCD — serial queues targeting the global concurrent queue.
//! On Linux: serial queues over one process-wide worker pool (libdispatch exists there
//! but lacks the `_pthread_workqueue` kernel support that gives GCD its guarantees).
//!
//! Callers use the Scheduler trait; platform selection is compile-time.

use std::sync::Arc;

/// Opaque per-channel work queue.
/// Guarantees serial execution — tasks submitted for one channel never run
/// concurrently with each other.
pub trait ChannelQueue: Send + Sync {
    /// Dispatch a closure to run on this channel's serial queue.
    /// Non-blocking — returns immediately.
    fn dispatch(&self, f: Box<dyn FnOnce() + Send + 'static>);
}

/// Creates per-channel serial queues appropriate for this platform.
pub trait Scheduler: Send + Sync {
    fn encode_queue(&self, channel: usize) -> Arc<dyn ChannelQueue>;
    /// Run `work(context, i)` for every i in 0..iterations, blocking until all have
    /// completed.
    ///
    /// The iterations run CONCURRENTLY on both platforms — macOS across a private
    /// concurrent GCD queue, Linux across the shared worker pool — as
    /// CASCADE_AUDIO_SEND_SPEC §2 requires of the encode/transmit stage. On both, the
    /// calling thread takes a share of the work itself, so the fan-out still completes
    /// when every worker is already busy.
    fn encode_apply(&self, iterations: usize,
                    context: *mut std::ffi::c_void,
                    work:    unsafe extern "C" fn(*mut std::ffi::c_void, usize));

    fn decode_queue(&self, channel: usize) -> Arc<dyn ChannelQueue>;
}

// ── Platform selection ─────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub mod macos;

/// The worker-pool scheduler. Pure std apart from the audio-callback elevation, which is
/// platform-specific inside — so it is a portable pool, not a Linux implementation.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod pool;

/// Create the platform-appropriate scheduler.
#[cfg(target_os = "macos")]
pub fn make_scheduler(n_encode: usize, n_decode: usize) -> Arc<dyn Scheduler> {
    Arc::new(macos::GcdScheduler::new(n_encode, n_decode))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn make_scheduler(n_encode: usize, n_decode: usize) -> Arc<dyn Scheduler> {
    Arc::new(pool::ThreadPoolScheduler::new(n_encode, n_decode))
}

// Named platforms only, deliberately. The pool is portable but its audio-callback
// elevation is not, so an unsupported target must fail HERE — with a message saying what
// to add — rather than silently taking a branch whose priority handling does nothing.
// Adding a platform means naming it above and deciding how its audio callback thread gets
// a real-time class — `pool::elevate_audio_callback_thread` on Linux, MMCSS inside the
// backend on Windows, the OS itself on macOS.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!(
    "cascade: no scheduler for this target. Supported: macOS (GCD), Linux and Windows \
     (worker pool). Add a scheduler implementation in src/audio/scheduler/ and name it in \
     make_scheduler."
);
