//! Worker-pool scheduler — per-channel serial queues over one shared pool. Serves Linux
//! and Windows; macOS uses GCD (`macos.rs`).
//!
//! Structural mirror of the macOS backend. There, `dispatch_queue_create` makes a serial
//! queue and `dispatch_set_target_queue` points it at the global concurrent queue, so a
//! serial queue carries ORDERING while the shared pool carries PARALLELISM and the thread
//! count stays constant however many channels are active. This module reproduces that:
//!
//!   * `SerialQueue` — a task deque plus a `scheduled` flag. No thread of its own.
//!     Admitting at most one handle into the pool at a time is what serialises a
//!     channel: its tasks can never run concurrently with each other.
//!   * `pool()` — one process-wide set of worker threads draining a shared injector.
//!     Every scheduler instance targets it, matching macOS where every `GcdScheduler`'s
//!     queues target the same process-wide global queue.
//!
//! CASCADE_AUDIO_RECEIVE_SPEC §2 describes decode as per-channel queues running
//! "subject to the underlying thread pool's own scheduling", which is this shape.
//!
//! ── Priority ──────────────────────────────────────────────────────────────────
//!
//! Workers run in the same elevated-but-not-real-time class as the network receive
//! thread and the tokio workers, via `set_qos_user_initiated()` — `nice -10` on Linux,
//! `QOS_CLASS_USER_INITIATED` on macOS, MMCSS "Audio" on Windows (NOT "Pro Audio", which
//! belongs to the WASAPI I/O thread alone). CASCADE_AUDIO_SEND_SPEC §2 requires
//! `USER_INITIATED` "or the platform equivalent … at a comparable priority" for the
//! encode/transmit stage.
//!
//! Workers are deliberately NOT SCHED_FIFO. Decode and the receive loop must be peers:
//! raising decode above the receive thread lets decode preempt it mid-receive, costing
//! more in receive latency than it gains in decode. The
//! only thread that takes a real-time class is the audio device callback, elevated by
//! `elevate_audio_callback_thread()` at its first invocation — on macOS CoreAudio applies
//! a time-constraint policy to that thread itself; on Windows the WASAPI backend registers
//! its own I/O thread with MMCSS. Only Linux needs `elevate_audio_callback_thread`.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crossbeam_channel::{unbounded, Sender};

use super::{ChannelQueue, Scheduler};

type Task = Box<dyn FnOnce() + Send + 'static>;

/// Worker stack. Opus decode and the resampler are shallow and allocate on the heap; the
/// 2 MiB default is reserve we never touch, and it multiplies by the worker count.
const WORKER_STACK: usize = 512 * 1024;

/// SCHED_FIFO priority for the device callback, the only real-time thread in the process.
/// Below the 50 that threaded ALSA interrupt handlers conventionally take, so the interrupt
/// that wakes the callback can always preempt it.
#[cfg(target_os = "linux")]
const AUDIO_RT_PRIORITY: libc::c_int = 40;

/// Worker-count bounds. The floor keeps two channels genuinely parallel on a single-core
/// board; the ceiling reflects decode being memory-bound rather than compute-bound, so
/// past this point workers contend instead of helping. macOS's shared pool sits in the
/// same region — 6 threads for a 64-channel run on an M4.
const MIN_WORKERS: usize = 2;
const MAX_WORKERS: usize = 8;

// ── Work item ─────────────────────────────────────────────────────────────────

/// What a worker can be handed. `Serial` is a channel queue with at least one task
/// pending; `Parallel` is one chunk of an `encode_apply` fan-out, which has no ordering
/// requirement and so needs no queue.
enum Job {
    Serial(Arc<SerialQueue>),
    Parallel(Task),
}

// ── SerialQueue ───────────────────────────────────────────────────────────────

pub struct SerialQueue {
    tasks:     Mutex<VecDeque<Task>>,
    /// True while a handle to this queue is in flight in the pool. It is the serialisation
    /// mechanism, not a hint: exactly one handle exists at a time, so at most one worker
    /// can be inside `run_one` for this queue.
    scheduled: AtomicBool,
}

impl SerialQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks:     Mutex::new(VecDeque::with_capacity(8)),
            scheduled: AtomicBool::new(false),
        })
    }

    fn enqueue(self: &Arc<Self>, f: Task) {
        {
            let mut q = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back(f);
        }
        self.wake();
    }

    /// Put a handle into the pool if one is not already in flight.
    fn wake(self: &Arc<Self>) {
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            if pool().inject.send(Job::Serial(Arc::clone(self))).is_err() {
                // Only reachable if every worker is gone, which cannot happen while the
                // process lives. Undo the claim rather than leaving the queue permanently
                // marked in-flight, which would silence the channel for good.
                self.scheduled.store(false, Ordering::Release);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Run exactly one task, then re-arm. Running one at a time (rather than draining) is
    /// what lets a busy channel yield to others on the same worker.
    fn run_one(self: &Arc<Self>) {
        let task = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        if let Some(t) = task {
            // Contain a panic to this one task. Without it the panic unwinds out of the
            // worker and kills it, and every queue that worker would later have served
            // stops draining. Decode is bounds-safe today; this is a guard, not a fix.
            if std::panic::catch_unwind(AssertUnwindSafe(t)).is_err() {
                tracing::debug!("scheduler: task panicked (contained — worker continues)");
            }
        }

        // Re-arm. The ordering below closes a race: a producer that pushes between the
        // emptiness test and the `scheduled` clear would see `scheduled == true`, skip
        // injecting, and leave its task stranded. Clearing first and re-testing after
        // means either the producer sees a cleared flag and injects, or we see its task
        // and inject ourselves.
        if !self.is_empty() {
            if pool().inject.send(Job::Serial(Arc::clone(self))).is_err() {
                self.scheduled.store(false, Ordering::Release);
            }
            return;                                   // stays scheduled — handle in flight
        }
        self.scheduled.store(false, Ordering::Release);
        if !self.is_empty() {
            self.wake();
        }
    }
}

/// Handle handed to callers: a serial queue over the shared pool. It owns no thread.
pub struct ThreadQueue {
    inner: Arc<SerialQueue>,
}

impl ThreadQueue {
    pub fn new(_label: String) -> Self {
        Self { inner: SerialQueue::new() }
    }
}

impl ChannelQueue for ThreadQueue {
    fn dispatch(&self, f: Task) {
        self.inner.enqueue(f);
    }
}

// ── Shared worker pool ────────────────────────────────────────────────────────

struct Pool {
    inject:  Sender<Job>,
    workers: usize,
}

/// Workers, sized from the machine. One core's worth is left for the device callback and
/// the network receive thread, which are not pool work. `available_parallelism` honours
/// cgroup CPU quotas, so a container-limited deployment sizes to its quota rather than to
/// the host.
fn worker_count() -> usize {
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    ncpu.saturating_sub(1).clamp(MIN_WORKERS, MAX_WORKERS)
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = worker_count();
        let (inject, rx) = unbounded::<Job>();
        for i in 0..workers {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("cascade.pool.{i}"))
                .stack_size(WORKER_STACK)
                .spawn(move || {
                    // Same class as the receive thread and the tokio workers — see the
                    // module header on why this is not SCHED_FIFO.
                    crate::audio::encode::set_qos_user_initiated();
                    // Flush denormals to zero on this worker.
                    //
                    // The resampler runs here, and a silent channel decodes to values in the
                    // denormal range. On x86 a denormal operand is handled in microcode and
                    // costs far more than normal arithmetic, and a 64-tap FIR multiplies 64
                    // of them per output sample. With FTZ and DAZ set the CPU substitutes
                    // zero. ARM64 handles denormal arithmetic in hardware, with no such assist,
                    // and is unaffected either way.
                    //
                    // The substitution changes a sample by less than 1.2e-38, which is below
                    // -760 dBFS. Set CASCADE_FTZ=0 to disable it and measure the difference.
                    if std::env::var("CASCADE_FTZ").map(|v| v != "0").unwrap_or(true) {
                        crate::audio::zita::set_flush_to_zero(true);
                    }
                    while let Ok(job) = rx.recv() {
                        match job {
                            Job::Serial(q) => q.run_one(),
                            Job::Parallel(t) => {
                                let _ = std::panic::catch_unwind(AssertUnwindSafe(t));
                            }
                        }
                    }
                })
                .expect("failed to spawn scheduler worker");
        }
        tracing::info!(
            "audio worker pool: {} threads ({} logical CPUs, {} KiB stacks)",
            workers,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            WORKER_STACK / 1024);
        Pool { inject, workers }
    })
}

// ── ThreadPoolScheduler ───────────────────────────────────────────────────────

pub struct ThreadPoolScheduler {
    enc_queues: Vec<Arc<ThreadQueue>>,
    dec_queues: Vec<Arc<ThreadQueue>>,
}

impl ThreadPoolScheduler {
    /// Creates queues only. Threads belong to the process-wide pool and are spawned once,
    /// on first dispatch, however many schedulers exist.
    pub fn new(n_encode: usize, n_decode: usize) -> Self {
        let enc_queues = (0..n_encode)
            .map(|i| Arc::new(ThreadQueue::new(format!("cascade.enc.{i}"))))
            .collect();
        let dec_queues = (0..n_decode)
            .map(|i| Arc::new(ThreadQueue::new(format!("cascade.dec.{i}"))))
            .collect();
        Self { enc_queues, dec_queues }
    }
}

/// The `encode_apply` context pointer, made sendable for the duration of the call.
///
/// SAFETY: this is `dispatch_apply_f`'s contract, which the trait mirrors. The context is
/// read-shared by every iteration, the iterations are disjoint by index, and the call
/// blocks until all of them have finished — so the pointee outlives every use and no
/// iteration outlives the borrow.
struct ApplyCtx(*mut std::ffi::c_void);
unsafe impl Send for ApplyCtx {}
unsafe impl Sync for ApplyCtx {}

impl Scheduler for ThreadPoolScheduler {
    /// Parallel fan-out across the shared pool, blocking until every iteration completes.
    ///
    /// CASCADE_AUDIO_SEND_SPEC §2 requires this stage to be concurrent — "multiple
    /// channels' encoding and transmission genuinely run in parallel across cores" — so a
    /// sequential loop here is a behavioural difference, not just a slower one.
    ///
    /// The calling thread takes a chunk itself, as `dispatch_apply` does. That is what
    /// guarantees forward progress when every worker is already busy: the fan-out can
    /// always complete on the caller alone.
    fn encode_apply(&self, iterations: usize,
                    context: *mut std::ffi::c_void,
                    work:    unsafe extern "C" fn(*mut std::ffi::c_void, usize)) {
        if iterations == 0 { return; }

        let p = pool();
        let chunks = iterations.min(p.workers + 1);
        if chunks <= 1 {
            for i in 0..iterations { unsafe { work(context, i); } }
            return;
        }

        // Block-partition: contiguous index ranges, for locality over load-balance. The
        // per-iteration cost here is one channel's Opus encode, which is uniform enough
        // that striping would buy nothing.
        let bound = |c: usize| c * iterations / chunks;

        let ctx  = Arc::new(ApplyCtx(context));
        let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
        let outstanding = chunks - 1;

        for c in 1..chunks {
            let (lo, hi) = (bound(c), bound(c + 1));
            let ctx       = Arc::clone(&ctx);
            let gate_task = Arc::clone(&gate);
            let sent = p.inject.send(Job::Parallel(Box::new(move || {
                for i in lo..hi { unsafe { work(ctx.0, i); } }
                let (m, cv) = &*gate_task;
                *m.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                cv.notify_one();
            })));
            if sent.is_err() {
                // Workers gone: run this chunk inline so the barrier below still clears.
                for i in lo..hi { unsafe { work(context, i); } }
                let (m, cv) = &*gate;
                *m.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                cv.notify_one();
            }
        }

        // Caller's own chunk.
        for i in bound(0)..bound(1) { unsafe { work(context, i); } }

        let (m, cv) = &*gate;
        let mut done = m.lock().unwrap_or_else(|e| e.into_inner());
        while *done < outstanding {
            done = cv.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn encode_queue(&self, channel: usize) -> Arc<dyn ChannelQueue> {
        self.enc_queues[channel % self.enc_queues.len()].clone()
    }
    fn decode_queue(&self, channel: usize) -> Arc<dyn ChannelQueue> {
        self.dec_queues[channel % self.dec_queues.len()].clone()
    }
}

// ── Audio-callback elevation ──────────────────────────────────────────────────

/// Put the CALLING thread into SCHED_FIFO, once per thread. Called at the top of the
/// device render and capture callbacks.
///
/// The priority sits below the 50 that threaded ALSA interrupt handlers conventionally
/// take, so the interrupt that wakes this callback can always preempt it. It is the only
/// real-time thread in the process; the kernel's own RT throttle
/// (`/proc/sys/kernel/sched_rt_runtime_us`, 95% by default) bounds a runaway.
///
/// Best-effort. A refusal leaves the thread at normal priority and warns once — audio
/// still runs, it just slips under load, and the servo reads that slip as ordinary
/// load-induced drift. That is a hard fault to attribute, so it must not be silent.
/// Linux only, deliberately. On Windows the audio thread is the WASAPI backend's own I/O
/// thread, which registers itself with MMCSS "Pro Audio" for its whole lifetime and reverts
/// on teardown (`audio/backend/wasapi_backend.rs`); applying a second policy from here would
/// leave two owners of one thread's scheduling. On macOS CoreAudio applies a time-constraint
/// policy to its own I/O thread.
#[cfg(target_os = "linux")]
pub fn elevate_audio_callback_thread() {

    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    // Per-thread, not once-per-process: a device rebuild gives the backend a NEW callback thread
    // that needs elevating again.
    if DONE.with(|d| d.replace(true)) { return; }

    unsafe {
        let param = libc::sched_param { sched_priority: AUDIO_RT_PRIORITY };
        if libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) != 0 {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "SCHED_FIFO refused for the audio callback — it runs at normal priority \
                     and will slip under load. Grant CAP_SYS_NICE and rtprio (systemd: \
                     AmbientCapabilities=CAP_SYS_NICE, LimitRTPRIO=95; or \
                     /etc/security/limits.d/audio.conf → @audio - rtprio 95)."
                );
            }
        } else {
            tracing::debug!("audio callback thread → SCHED_FIFO {}", AUDIO_RT_PRIORITY);
        }
    }
}

/// Check the worker pool and the thread privileges it needs, and return the result as text.
///
/// Reached by `cascade --selftest`. It exists because the pool's concurrency properties are
/// covered by unit tests that are `#[cfg]`-gated to Linux and so cannot run on the machine
/// that builds the binary, and because whether `nice` and `SCHED_FIFO` are actually granted
/// is a property of the machine, not of the build.
#[cfg(target_os = "linux")]
pub fn self_test() -> String {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let mut r = String::new();
    let _ = writeln!(r, "scheduler pool — configuration and privileges on this machine\n");

    let _ = writeln!(r, "  workers            {} (from {} logical CPUs, clamped to {}..={})",
                     worker_count(),
                     std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
                     MIN_WORKERS, MAX_WORKERS);
    let _ = writeln!(r, "  worker stack       {} KiB", WORKER_STACK / 1024);
    let _ = writeln!(r, "  callback priority  SCHED_FIFO {AUDIO_RT_PRIORITY}");

    // nice: raised on a spawned thread and read back, because nice is per-thread on Linux
    // and the value that matters is the one the audio threads actually get.
    let nice = std::thread::spawn(|| {
        crate::audio::encode::set_qos_user_initiated();
        // SAFETY: getpriority on the calling thread; -1 is a valid nice value, so errno
        // would be needed to distinguish an error, and a failure here is not important
        // enough to warrant it.
        unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
    }).join().unwrap_or(999);
    let _ = writeln!(r, "  nice achieved      {nice}{}",
                     if nice <= -10 { "  (granted)" } else { "  (NOT granted — needs CAP_SYS_NICE)" });

    // SCHED_FIFO: attempted on a throwaway thread so the answer is known without audio
    // running, since the real elevation happens inside the device callback.
    let fifo_err = std::thread::spawn(|| unsafe {
        let param = libc::sched_param { sched_priority: AUDIO_RT_PRIORITY };
        let rc = libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param);
        if rc == 0 { None } else { Some(rc) }
    }).join().unwrap_or(Some(-1));
    match fifo_err {
        None => { let _ = writeln!(r, "  SCHED_FIFO {AUDIO_RT_PRIORITY}     granted"); }
        Some(rc) => { let _ = writeln!(r, "  SCHED_FIFO {AUDIO_RT_PRIORITY}     REFUSED (errno {rc}) — the audio \
callback will run at normal priority and slip under load"); }
    }

    // The queue accepts every task; a producer that outruns the workers grows the backlog
    // rather than losing work. Each check below therefore expects an exact total.
    let _ = writeln!(r, "\n  concurrency checks:");

    // Never two tasks from one queue at once, over many small batches.
    let q = ThreadQueue::new("selftest".into());
    let inside = Arc::new(AtomicUsize::new(0));
    let seen   = Arc::new(AtomicUsize::new(0));
    let done   = Arc::new(AtomicUsize::new(0));
    let batches = 20usize;
    let per = 32usize;
    for _ in 0..batches {
        for _ in 0..per {
            let (i, sn, d) = (Arc::clone(&inside), Arc::clone(&seen), Arc::clone(&done));
            q.dispatch(Box::new(move || {
                let n = i.fetch_add(1, Ordering::SeqCst) + 1;
                sn.fetch_max(n, Ordering::SeqCst);
                std::thread::yield_now();
                i.fetch_sub(1, Ordering::SeqCst);
                d.fetch_add(1, Ordering::SeqCst);
            }));
        }
        // Pace the batches so the wake/re-arm path in `run_one` is exercised repeatedly
        // rather than the whole run being one uninterrupted drain.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while done.load(Ordering::SeqCst) < batches * per && std::time::Instant::now() < dl {
        std::thread::yield_now();
    }
    let ran = done.load(Ordering::SeqCst);
    let overlap = seen.load(Ordering::SeqCst);
    let _ = writeln!(r, "    one queue never runs two tasks at once      {} (max concurrent {overlap}, {ran}/{} ran)",
                     if overlap == 1 { "PASS" } else { "FAIL" }, batches * per);

    // No task is stranded: everything dispatched must run.
    let q2 = ThreadQueue::new("selftest2".into());
    let ran2 = Arc::new(AtomicUsize::new(0));
    for _ in 0..10 {
        for _ in 0..per {
            let c = Arc::clone(&ran2);
            q2.dispatch(Box::new(move || { c.fetch_add(1, Ordering::SeqCst); }));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while ran2.load(Ordering::SeqCst) < 10 * per && std::time::Instant::now() < dl {
        std::thread::yield_now();
    }
    let n2 = ran2.load(Ordering::SeqCst);
    let _ = writeln!(r, "    no task is stranded                        {} ({n2}/{})",
                     if n2 == 10 * per { "PASS" } else { "FAIL" }, 10 * per);

    // FIFO within a queue.
    let q3 = ThreadQueue::new("selftest3".into());
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fin = Arc::new(AtomicUsize::new(0));
    let n3 = 200usize;
    for i in 0..n3 {
        let (o, f) = (Arc::clone(&order), Arc::clone(&fin));
        q3.dispatch(Box::new(move || {
            o.lock().unwrap_or_else(|e| e.into_inner()).push(i);
            f.fetch_add(1, Ordering::SeqCst);
        }));
    }
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while fin.load(Ordering::SeqCst) < n3 && std::time::Instant::now() < dl {
        std::thread::yield_now();
    }
    let got = order.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let _ = writeln!(r, "    tasks run in submission order              {}",
                     if got == (0..n3).collect::<Vec<_>>() { "PASS" } else { "FAIL" });
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn worker_count_is_bounded_on_every_machine() {
        let n = worker_count();
        assert!((MIN_WORKERS..=MAX_WORKERS).contains(&n), "worker_count {n} out of bounds");
    }

    /// A queue's tasks must never overlap, however many are pending and however many
    /// workers exist. Each task asserts it is the only one running for its queue.
    #[test]
    fn one_queue_never_runs_two_tasks_at_once() {
        let q = ThreadQueue::new("t".into());
        let inside = Arc::new(AtomicUsize::new(0));
        let seen   = Arc::new(AtomicUsize::new(0));
        let done   = Arc::new(AtomicUsize::new(0));

        for _ in 0..500 {
            let (i, s, d) = (Arc::clone(&inside), Arc::clone(&seen), Arc::clone(&done));
            q.dispatch(Box::new(move || {
                let n = i.fetch_add(1, Ordering::SeqCst) + 1;
                s.fetch_max(n, Ordering::SeqCst);
                std::thread::yield_now();
                i.fetch_sub(1, Ordering::SeqCst);
                d.fetch_add(1, Ordering::SeqCst);
            }));
        }
        while done.load(Ordering::SeqCst) < 500 { std::thread::yield_now(); }
        assert_eq!(seen.load(Ordering::SeqCst), 1, "queue ran tasks concurrently");
    }

    /// Every task enqueued must run — the re-arm race in `run_one` losing a wake-up would
    /// strand the tail of a queue. Producers push from several threads to widen the window.
    #[test]
    fn no_task_is_stranded_under_concurrent_dispatch() {
        let q = Arc::new(ThreadQueue::new("t".into()));
        let count = Arc::new(AtomicUsize::new(0));
        let producers: Vec<_> = (0..4).map(|_| {
            let (q, c) = (Arc::clone(&q), Arc::clone(&count));
            std::thread::spawn(move || {
                for _ in 0..250 {
                    let c = Arc::clone(&c);
                    q.dispatch(Box::new(move || { c.fetch_add(1, Ordering::SeqCst); }));
                }
            })
        }).collect();
        for p in producers { p.join().unwrap(); }
        while !q.inner.is_empty() || q.inner.scheduled.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        assert_eq!(count.load(Ordering::SeqCst), 4 * 250, "tasks were lost");
    }

    /// Ordering within a queue is FIFO.
    #[test]
    fn tasks_run_in_submission_order() {
        let q = ThreadQueue::new("t".into());
        let log = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        for n in 0..200usize {
            let (l, d) = (Arc::clone(&log), Arc::clone(&done));
            q.dispatch(Box::new(move || {
                l.lock().unwrap().push(n);
                d.fetch_add(1, Ordering::SeqCst);
            }));
        }
        while done.load(Ordering::SeqCst) < 200 { std::thread::yield_now(); }
        let l = log.lock().unwrap();
        assert!(l.windows(2).all(|w| w[0] < w[1]), "queue reordered tasks");
    }

    /// encode_apply must cover every index exactly once and block until all are done.
    #[test]
    fn encode_apply_covers_every_index_once() {
        static HITS: OnceLock<Vec<AtomicUsize>> = OnceLock::new();
        const N: usize = 97;   // prime, so chunk bounds do not divide evenly
        HITS.get_or_init(|| (0..N).map(|_| AtomicUsize::new(0)).collect());

        unsafe extern "C" fn work(_ctx: *mut std::ffi::c_void, i: usize) {
            HITS.get().unwrap()[i].fetch_add(1, Ordering::SeqCst);
        }

        let s = ThreadPoolScheduler::new(1, 1);
        s.encode_apply(N, std::ptr::null_mut(), work);

        for (i, h) in HITS.get().unwrap().iter().enumerate() {
            assert_eq!(h.load(Ordering::SeqCst), 1, "index {i} not covered exactly once");
        }
    }
}
