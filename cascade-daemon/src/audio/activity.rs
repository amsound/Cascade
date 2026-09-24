//! Process activity assertion —
//! `[[NSProcessInfo processInfo] beginActivityWithOptions:reason:]`.
//!
//! WHY: without it macOS throttles the process — App Nap and, more importantly, timer
//! coalescing — when the system gets busy, descheduling our threads for milliseconds at a
//! time. The throttle freezes the THREAD; it is not lock contention, and a pure CPU
//! busy-loop probe with no locks, allocations or syscalls stalls the same way.
//!
//! The call is guarded with `respondsToSelector:` for OS-version safety, and passes
//! options = 0xFF0010C000, which decodes exactly (zero leftover bits) to:
//!   NSActivityLatencyCritical             (0xFF00000000)  ← disables timer coalescing
//!   NSActivityIdleSystemSleepDisabled     (0x00100000)
//!   NSActivitySuddenTerminationDisabled   (0x00004000)
//!   NSActivityAutomaticTerminationDisabled(0x00008000)
//! The returned token is retained for the process lifetime and never ended.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    // The documented NSActivityOptions bits.
    const NS_ACTIVITY_LATENCY_CRITICAL:            u64 = 0xFF00000000;
    const NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED:  u64 = 0x00100000;
    const NS_ACTIVITY_SUDDEN_TERMINATION_DISABLED: u64 = 0x00004000;
    const NS_ACTIVITY_AUTOMATIC_TERMINATION_DISABLED: u64 = 0x00008000;
    const ACTIVITY_OPTIONS: u64 = NS_ACTIVITY_LATENCY_CRITICAL
        | NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED
        | NS_ACTIVITY_SUDDEN_TERMINATION_DISABLED
        | NS_ACTIVITY_AUTOMATIC_TERMINATION_DISABLED; // = 0xFF0010C000

    type Id = *mut c_void;
    type Sel = *const c_void;
    type Class = *mut c_void;

    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Class;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_retain(o: Id) -> Id;
        fn objc_msgSend();
    }
    // Link Foundation so the NSProcessInfo / NSString classes are present at runtime.
    #[link(name = "Foundation", kind = "framework")]
    extern "C" {}

    unsafe fn cls(name: &[u8]) -> Class {
        // name must be NUL-terminated
        objc_getClass(name.as_ptr() as *const c_char)
    }
    unsafe fn sel(name: &[u8]) -> Sel {
        sel_registerName(name.as_ptr() as *const c_char)
    }

    /// Take the activity assertion, retaining the token for the process lifetime. Warns
    /// and returns without effect if the selector is unavailable or any step fails.
    pub fn begin() {
        unsafe {
            // NSProcessInfo* pi = [NSProcessInfo processInfo];
            let ns_process_info = cls(b"NSProcessInfo\0");
            if ns_process_info.is_null() {
                tracing::warn!("activity: NSProcessInfo class not found");
                return;
            }
            let sel_process_info = sel(b"processInfo\0");
            let msg_send_id: extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            let pi: Id = msg_send_id(ns_process_info as Id, sel_process_info);
            if pi.is_null() {
                tracing::warn!("activity: [NSProcessInfo processInfo] returned nil");
                return;
            }

            // Guard with respondsToSelector: before calling.
            let sel_begin = sel(b"beginActivityWithOptions:reason:\0");
            let sel_responds = sel(b"respondsToSelector:\0");
            let msg_send_responds: extern "C" fn(Id, Sel, Sel) -> bool =
                std::mem::transmute(objc_msgSend as *const ());
            if !msg_send_responds(pi, sel_responds, sel_begin) {
                tracing::warn!("activity: beginActivityWithOptions:reason: unavailable on this OS");
                return;
            }

            // NSString* reason = @"Cascade live audio";
            let ns_string = cls(b"NSString\0");
            let sel_with_utf8 = sel(b"stringWithUTF8String:\0");
            let msg_send_str: extern "C" fn(Id, Sel, *const c_char) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            let reason: Id = msg_send_str(
                ns_string as Id, sel_with_utf8,
                b"Cascade live audio\0".as_ptr() as *const c_char);

            // id token = [pi beginActivityWithOptions:ACTIVITY_OPTIONS reason:reason];
            let msg_send_begin: extern "C" fn(Id, Sel, u64, Id) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            let token: Id = msg_send_begin(pi, sel_begin, ACTIVITY_OPTIONS, reason);
            if token.is_null() {
                tracing::warn!("activity: beginActivityWithOptions returned nil");
                return;
            }

            // Retain for the process lifetime — ending the activity would drop the
            // assertion. objc_retain bumps the refcount and we never release, so it lives
            // for the process. The pointer is Copy, so the retain is what holds it.
            let _held = objc_retain(token);
            let _ = _held;

            tracing::debug!("Activity assertion held (LatencyCritical)");

        }
    }
}

/// Take a process activity assertion (LatencyCritical + idle-sleep and termination
/// disabled), held for the process lifetime. Prevents macOS App Nap and timer coalescing
/// from freezing our threads under system load. No-op off macOS. Call once at startup.
pub fn begin_activity() {
    #[cfg(target_os = "macos")]
    imp::begin();
}
