//! macOS scheduler — libdispatch GCD.
//!
//! Configuration:
//!   - QoS class: QOS_CLASS_USER_INITIATED (0x19), rel_pri 0. CoreAudio's own audio-IO
//!     queues run USER_INTERACTIVE (0x21); ours deliberately do not.
//!   - Queue type: serial (DISPATCH_QUEUE_SERIAL = NULL)
//!   - Dispatch: dispatch_async_f (function-pointer variant)
//!   - One serial queue per encode channel and per decode channel
//!   - All queues target the shared GCD thread pool
//!   - NO SCHED_FIFO or thread-priority elevation
//!
//! All queues are created via GcdQueue::new (USER_INITIATED), with no priority elevation.
//! Labels are descriptive for debugging only.

use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};

static TRAMPOLINE_CALLS: AtomicU64 = AtomicU64::new(0);
// Count of decode tasks whose closure panicked and was contained by catch_unwind in
// the trampoline (would otherwise have been UB across the extern "C" boundary). Should
// stay 0; a non-zero value means a decode task panicked — investigate that channel.
static TRAMPOLINE_PANICS: AtomicU64 = AtomicU64::new(0);

use std::os::raw::{c_char, c_void};
use std::sync::Arc;

use super::{ChannelQueue, Scheduler};

// ── Minimal libdispatch FFI ────────────────────────────────────────────────────
//
// 5 functions — everything we need, nothing we don't.
// All types are opaque pointers; libdispatch manages their memory.

type DispatchObject     = *mut c_void;
type DispatchQueue      = DispatchObject;
type DispatchQueueAttr  = DispatchObject;
type DispatchFunction   = unsafe extern "C" fn(*mut c_void);

/// QOS_CLASS_USER_INITIATED = 0x19
const QOS_CLASS_USER_INITIATED:   u32 = 0x19;

/// DISPATCH_QUEUE_SERIAL = NULL
const DISPATCH_QUEUE_SERIAL: DispatchQueueAttr = std::ptr::null_mut();

#[link(name = "System")]
extern "C" {
    fn dispatch_queue_create(
        label: *const c_char,
        attr:  DispatchQueueAttr,
    ) -> DispatchQueue;

    fn dispatch_queue_attr_make_with_qos_class(
        attr: DispatchQueueAttr, qos_class: u32, relative_priority: i32,
    ) -> DispatchQueueAttr;

    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> DispatchQueue;

    /// Set the target queue for a dispatch queue.
    /// Serial queues targeting a concurrent queue share the concurrent pool's threads
    /// rather than getting dedicated threads. Plain serial queues each get a dedicated
    /// GCD thread under load; serial queues targeting a shared concurrent queue do not,
    /// so thread count stops scaling with channel count.
    fn dispatch_set_target_queue(object: DispatchObject, queue: DispatchQueue);

    fn dispatch_async_f(queue: DispatchQueue, context: *mut c_void, work: DispatchFunction);

    fn dispatch_apply_f(
        iterations: usize,
        queue:      DispatchQueue,
        context:    *mut std::ffi::c_void,
        work:       unsafe extern "C" fn(*mut std::ffi::c_void, usize),
    );

    fn dispatch_release(object: DispatchObject);
}

// ── Trampoline ────────────────────────────────────────────────────────────────
//
// Converts a boxed Rust closure into a C function pointer + context pointer.
// Box::into_raw gives ownership to GCD; Box::from_raw reclaims it when done.
// One allocation per dispatch call — ~50ns, negligible at 50fps per channel.

unsafe extern "C" fn trampoline(ctx: *mut c_void) {
    TRAMPOLINE_CALLS.fetch_add(1, Ordering::Relaxed);
    // NO thread-priority elevation here, deliberately. Decode workers run at plain
    // USER_INITIATED (0x19) GCD priority with no SCHED_FIFO. Elevating them puts decode
    // above the select-loop thread, which is then preempted mid-receive; that inflates
    // measured receive time by more than the decode gains.
    let f = Box::from_raw(ctx as *mut Box<dyn FnOnce() + Send + 'static>);
    // Contain any panic HERE. f() is the decode closure; this trampoline is called by
    // GCD across an `extern "C"` boundary, so a panic unwinding out of it would be
    // undefined behaviour. catch_unwind turns a panic into a logged, contained failure
    // of this one task (the channel keeps running) instead of UB. It is a guard: the
    // decode path has no known panic.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f()));
    if r.is_err() {
        TRAMPOLINE_PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

// ── GcdQueue ──────────────────────────────────────────────────────────────────

pub struct GcdQueue {
    queue: DispatchQueue,
}

// SAFETY: dispatch_queue_t is an ObjC object pointer managed by GCD.
// GCD guarantees thread-safety on the queue itself; we only ever call
// dispatch_async_f (which is documented as safe from any thread).
unsafe impl Send for GcdQueue {}
unsafe impl Sync for GcdQueue {}

impl GcdQueue {
    /// Create a serial queue for DECODE — targets USER_INITIATED global queue.
    pub fn new(label: &str) -> Self {
        Self::new_with_qos(label, QOS_CLASS_USER_INITIATED)
    }

    fn new_with_qos(label: &str, qos: u32) -> Self {
        let c_label = CString::new(label).unwrap_or_default();
        let queue = unsafe {
            // Create plain serial queue (NULL attr = serial, guaranteed).
            let q = dispatch_queue_create(c_label.as_ptr(), DISPATCH_QUEUE_SERIAL);
            if q.is_null() {
                tracing::error!("GcdQueue: dispatch_queue_create returned NULL for '{}'", label);
                return Self { queue: q };
            }
            // Target the global concurrent queue at the requested QoS class: serial queues
            // for per-channel ordering, a shared concurrent pool for threads.
            //
            // Without this, each serial queue under sustained load gets a dedicated GCD
            // thread and thread count scales with channel count. With it, all serial queues
            // share the global pool and thread count stays constant regardless of how many
            // channels are active — a 64-channel run on an M4 uses 6 threads on
            // com.apple.root.user-initiated-qos.
            let target = dispatch_get_global_queue(qos as isize, 0);
            dispatch_set_target_queue(q, target);
            tracing::debug!("GcdQueue[{} qos={:#x} target-pool]: {:p}", label, qos, q);
            q
        };
        Self { queue }
    }
}

impl Drop for GcdQueue {
    fn drop(&mut self) {
        unsafe { dispatch_release(self.queue); }
    }
}

impl ChannelQueue for GcdQueue {
    fn dispatch(&self, f: Box<dyn FnOnce() + Send + 'static>) {
        // Double-box so the outer Box<Box<...>> has a stable pointer size for FFI.
        let ctx = Box::into_raw(Box::new(f)) as *mut c_void;
        unsafe {
            dispatch_async_f(self.queue, ctx, trampoline);
        }
    }
}

// ── GcdScheduler ─────────────────────────────────────────────────────────────

// SAFETY: DispatchQueue (*mut c_void) is safe to send/share — GCD queues
// are thread-safe reference-counted objects managed by the OS.
unsafe impl Send for GcdScheduler {}
unsafe impl Sync for GcdScheduler {}

pub struct GcdScheduler {
    /// Private concurrent queue for dispatch_apply_f, rather than the global queue.
    enc_concurrent: DispatchQueue,
    enc_queues: Vec<Arc<GcdQueue>>,
    dec_queues: Vec<Arc<GcdQueue>>,
}

impl GcdScheduler {
    /// Create one USER_INITIATED serial queue per encode channel and per decode channel,
    /// all dispatching to the shared GCD pool.
    pub fn new(n_encode: usize, n_decode: usize) -> Self {
        let enc_queues = (0..n_encode)
            .map(|i| Arc::new(GcdQueue::new(&format!("cascade.enc.{i}"))))
            .collect();
        let dec_queues = (0..n_decode)
            .map(|i| Arc::new(GcdQueue::new(&format!("cascade.dec.{i}"))))
            .collect();
        // Create a private concurrent queue targeting USER_INITIATED (0x19).
        // USER_INITIATED (0x19) for encode as well as decode — never 0x21, which would put
        // encode above the select loop. dispatch_queue_attr_make_with_qos_class with a
        // non-NULL attr_in produces a CONCURRENT queue, not a serial one.
        let enc_concurrent = unsafe {
            let attr = dispatch_queue_attr_make_with_qos_class(
                std::ptr::null_mut() as *mut _,
                QOS_CLASS_USER_INITIATED, 0);
            // Pass the QoS attr as the queue attr — this creates a CONCURRENT queue
            // at USER_INITIATED priority (non-NULL attr_in = concurrent).
            let q = dispatch_queue_create(
                b"cascade.enc.concurrent\0".as_ptr() as *const _,
                attr);
            assert!(!q.is_null(), "failed to create enc_concurrent queue");
            tracing::debug!("GcdScheduler: enc_concurrent queue (USER_INITIATED 0x19, concurrent): {:?}", q);
            q
        };
        Self { enc_concurrent, enc_queues, dec_queues }
    }
}

impl Scheduler for GcdScheduler {
    fn encode_apply(&self, iterations: usize,
                    context: *mut std::ffi::c_void,
                    work:    unsafe extern "C" fn(*mut std::ffi::c_void, usize)) {
        // Use the private concurrent encode queue instead of the global queue.
        // A private concurrent queue takes the _dispatch_apply_redirect_invoke path,
        // which has better thread-cache locality than _dispatch_apply_invoke
        // (global queue direct). Verified from Instruments profile comparison.
        unsafe { dispatch_apply_f(iterations, self.enc_concurrent, context, work); }
    }

    fn encode_queue(&self, channel: usize) -> Arc<dyn ChannelQueue> {
        self.enc_queues[channel % self.enc_queues.len()].clone()
    }
    fn decode_queue(&self, channel: usize) -> Arc<dyn ChannelQueue> {
        self.dec_queues[channel % self.dec_queues.len()].clone()
    }
}
