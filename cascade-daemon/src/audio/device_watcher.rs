//! Device-change watcher — OS push notification when the audio device set changes.
//!
//! The watcher replaces polling: instead of
//! periodically enumerating (which probes input devices and flashes the macOS mic-in-use
//! indicator), the OS tells us the instant a device is added, removed, or renamed. On a
//! change we invoke a single callback; in Cascade that callback nudges the Stage 3
//! inventory task (`invalidate_device_cache`) to re-enumerate and push, and the
//! device-manager re-acquire picks up a returning device promptly.
//!
//! Platform-neutral by design:
//! - macOS: `AudioObjectAddPropertyListener` on `kAudioHardwarePropertyDevices`. Native
//!   push, effectively immediate.
//! - Windows: `IMMNotificationClient` via the `wasapi` crate's `DeviceEventCallbacks`.
//!   Arrival, removal and state change all collapse onto the one callback, because the
//!   consumer only needs "the set changed".
//! - Linux: the kernel's uevent netlink socket, filtered to the `sound` subsystem. No
//!   libudev, no PipeWire, no PulseAudio — the socket is the same one udev itself reads.
//!   Falls back to the caller's poll if the socket cannot be opened.
//! - Other / unsupported: `start()` returns `None`; the caller keeps its poll fallback.
//!
//! RENAMES ARE NOT WATCHED on either platform. macOS listens on the device-SET property,
//! which a rename does not touch, and the Windows equivalent (`OnPropertyValueChanged`)
//! fires for every property key on every device — far too noisy to forward unfiltered.
//! A rename is picked up by the periodic inventory re-enumeration instead.
//!
//! The callback runs on an OS-managed thread, so it must be cheap and do no blocking work
//! — just signal. Cascade's callback only sets an atomic flag (via the provided closure).

/// An active watcher. Dropping it removes the OS listener (best-effort). The watcher holds
/// the boxed callback alive for as long as it is registered.
pub struct DeviceWatcher {
    #[cfg(target_os = "macos")]
    inner: macos::CoreAudioWatcher,
    #[cfg(windows)]
    inner: win::WasapiWatcher,
    // Neither: nothing yet; the field keeps the struct non-empty across platforms.
    #[cfg(target_os = "linux")]
    inner: linux::UeventWatcher,
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    _unsupported: (),
}

impl DeviceWatcher {
    /// Start watching for device-set changes. `on_change` is invoked (on an OS thread) each
    /// time the set changes. Returns `Some(watcher)` if a native watcher was installed, or
    /// `None` if this platform has no watcher (caller should keep polling).
    ///
    /// `on_change` must be cheap and non-blocking (set a flag / wake a task). It may be
    /// called concurrently and from a thread you do not control.
    pub fn start<F>(on_change: F) -> Option<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        #[cfg(target_os = "macos")]
        {
            macos::CoreAudioWatcher::new(on_change).map(|w| DeviceWatcher { inner: w })
        }
        #[cfg(windows)]
        {
            win::WasapiWatcher::new(on_change).map(|w| DeviceWatcher { inner: w })
        }
        #[cfg(target_os = "linux")]
        {
            linux::UeventWatcher::new(on_change).map(|w| DeviceWatcher { inner: w })
        }
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        {
            // No native watcher on this platform — caller falls back to polling.
            let _ = on_change;
            None
        }
    }

    /// A short human-readable backend name, for the distinct log line that lets you tell
    /// push from poll during testing.
    pub fn backend(&self) -> &'static str {
        #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
        { self.inner.backend() }
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        { "none" }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::c_void;

    // ── CoreAudio HAL types/constants ────────────────────────────────────────
    // Declared directly here. `objc2-core-audio` supplies the same symbols and is already
    // a dependency, so this block is duplication rather than necessity — worth folding in,
    // but a change with no behaviour behind it. We link the CoreAudio framework ourselves.

    type OSStatus = i32;
    type AudioObjectID = u32;
    type AudioObjectPropertySelector = u32;
    type AudioObjectPropertyScope = u32;
    type AudioObjectPropertyElement = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AudioObjectPropertyAddress {
        m_selector: AudioObjectPropertySelector,
        m_scope: AudioObjectPropertyScope,
        m_element: AudioObjectPropertyElement,
    }

    // kAudioObjectSystemObject = 1
    const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
    // 'dev#' (kAudioHardwarePropertyDevices) as a FourCharCode.
    const K_AUDIO_HARDWARE_PROPERTY_DEVICES: AudioObjectPropertySelector = fourcc(b"dev#");
    // 'glob' (kAudioObjectPropertyScopeGlobal).
    const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: AudioObjectPropertyScope = fourcc(b"glob");
    // kAudioObjectPropertyElementMain / ...Master == 0 on all SDK versions.
    const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: AudioObjectPropertyElement = 0;

    const fn fourcc(b: &[u8; 4]) -> u32 {
        ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | (b[3] as u32)
    }

    // The C listener signature:
    //   OSStatus (*)(AudioObjectID, UInt32 inNumberAddresses,
    //                const AudioObjectPropertyAddress*, void* inClientData)
    type AudioObjectPropertyListenerProc = extern "C" fn(
        in_object_id: AudioObjectID,
        in_number_addresses: u32,
        in_addresses: *const AudioObjectPropertyAddress,
        in_client_data: *mut c_void,
    ) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectAddPropertyListener(
            in_object_id: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_listener: AudioObjectPropertyListenerProc,
            in_client_data: *mut c_void,
        ) -> OSStatus;

        fn AudioObjectRemovePropertyListener(
            in_object_id: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_listener: AudioObjectPropertyListenerProc,
            in_client_data: *mut c_void,
        ) -> OSStatus;
    }

    /// Boxed user callback. We pass a raw pointer to this box as `inClientData`, and
    /// reconstruct it in the C trampoline. The box is kept alive by `CoreAudioWatcher` and
    /// freed on drop (after the listener is removed).
    struct CallbackBox {
        cb: Box<dyn Fn() + Send + Sync + 'static>,
    }

    /// The C trampoline CoreAudio calls on a device-set change. Must not panic across the
    /// FFI boundary and must be cheap (runs on a CoreAudio thread).
    extern "C" fn listener_trampoline(
        _in_object_id: AudioObjectID,
        _in_number_addresses: u32,
        _in_addresses: *const AudioObjectPropertyAddress,
        in_client_data: *mut c_void,
    ) -> OSStatus {
        if !in_client_data.is_null() {
            // Reborrow without taking ownership — the box stays owned by the watcher.
            let cbx = unsafe { &*(in_client_data as *const CallbackBox) };
            // Guard against unwinding across the C boundary.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (cbx.cb)();
            }));
        }
        0 // noErr
    }

    const ADDRESS: AudioObjectPropertyAddress = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_HARDWARE_PROPERTY_DEVICES,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    pub struct CoreAudioWatcher {
        // Kept alive while registered; freed (Box dropped) after the listener is removed.
        client: *mut CallbackBox,
    }

    // The raw pointer is only ever used to register/deregister the listener and is not
    // shared mutably; the watcher owns it exclusively. Safe to move across threads.
    unsafe impl Send for CoreAudioWatcher {}
    unsafe impl Sync for CoreAudioWatcher {}

    impl CoreAudioWatcher {
        pub fn new<F>(on_change: F) -> Option<Self>
        where
            F: Fn() + Send + Sync + 'static,
        {
            let client = Box::into_raw(Box::new(CallbackBox { cb: Box::new(on_change) }));
            let status = unsafe {
                AudioObjectAddPropertyListener(
                    K_AUDIO_OBJECT_SYSTEM_OBJECT,
                    &ADDRESS,
                    listener_trampoline,
                    client as *mut c_void,
                )
            };
            if status == 0 {
                Some(CoreAudioWatcher { client })
            } else {
                // Registration failed — reclaim the box so we don't leak, return None so
                // the caller keeps its poll fallback.
                unsafe { drop(Box::from_raw(client)); }
                None
            }
        }

        pub fn backend(&self) -> &'static str { "coreaudio" }
    }

    impl Drop for CoreAudioWatcher {
        fn drop(&mut self) {
            unsafe {
                let _ = AudioObjectRemovePropertyListener(
                    K_AUDIO_OBJECT_SYSTEM_OBJECT,
                    &ADDRESS,
                    listener_trampoline,
                    self.client as *mut c_void,
                );
                // Now safe to free the callback box — no further callbacks can fire.
                drop(Box::from_raw(self.client));
            }
        }
    }
}

/// Windows: `IMMNotificationClient`, via the `wasapi` crate's `DeviceEventCallbacks`.
///
/// Three notifications are forwarded — added, removed, state changed — all onto the one
/// `on_change` closure, because Cascade's consumer only asks "did the set change?". A
/// single event often produces two of them (an arrival is an add AND a state transition to
/// ACTIVE); the consumer bumps a generation counter and invalidates a cache, so a duplicate
/// costs one extra re-enumeration and nothing else.
///
/// `OnDefaultDeviceChanged` is deliberately not forwarded: Cascade selects devices by
/// identity, never by "whatever is default", so the system default moving is not a change
/// to anything Cascade is using. `OnPropertyValueChanged` is not forwarded either — see
/// the note about renames at the top of this file.
#[cfg(windows)]
mod win {
    use std::sync::Arc;
    use wasapi::{DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration};

    /// Holds the registration alive. `DeviceEventRegistration::drop` calls
    /// `UnregisterEndpointNotificationCallback`, so dropping this removes the listener.
    ///
    /// THAT DROP NEEDS COM STILL INITIALISED on the thread it happens on. `new()` puts the
    /// calling thread in the MTA and Cascade never calls `CoUninitialize`, so a watcher
    /// created and dropped on the main thread — which is what main.rs does, holding it for
    /// the process lifetime — is safe. Moving the drop to a thread that never initialised
    /// COM would not be.
    pub struct WasapiWatcher {
        _reg: DeviceEventRegistration,
    }

    impl WasapiWatcher {
        pub fn new<F>(on_change: F) -> Option<Self>
        where
            F: Fn() + Send + Sync + 'static,
        {
            // Idempotent: a second call on a thread already in the MTA returns S_FALSE.
            let _ = wasapi::initialize_mta();

            let enumerator = match DeviceEnumerator::new() {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("device watcher: cannot create the WASAPI enumerator \
                                    ({e}) — falling back to periodic poll");
                    return None;
                }
            };

            // One closure, three notifications. Arc because each setter takes its own
            // 'static + Send + Sync callable and the crate boxes them separately.
            let cb = Arc::new(on_change);
            let mut callbacks = DeviceEventCallbacks::new();
            let added = Arc::clone(&cb);
            callbacks.set_device_added_callback(move |_id| added());
            let removed = Arc::clone(&cb);
            callbacks.set_device_removed_callback(move |_id| removed());
            let state = Arc::clone(&cb);
            callbacks.set_device_state_callback(move |_id, _state| state());

            match enumerator.register_notification_callback(callbacks) {
                Ok(reg) => Some(Self { _reg: reg }),
                Err(e) => {
                    tracing::warn!("device watcher: cannot register for WASAPI device \
                                    notifications ({e}) — falling back to periodic poll");
                    None
                }
            }
        }

        pub fn backend(&self) -> &'static str { "wasapi" }
    }
}

/// Linux: the kernel's uevent netlink socket, filtered to the `sound` subsystem.
///
/// This is the socket udev itself reads. Subscribing to it directly means no libudev, no
/// PipeWire and no PulseAudio — nothing to install and no daemon to depend on. A card
/// appearing or disappearing arrives as a message rather than being noticed on the next poll.
///
/// Two multicast groups exist: group 1 carries the kernel's own uevents, group 2 the copies
/// systemd-udevd re-broadcasts after processing. Both are joined, because either alone can be
/// unavailable — group 1 needs privilege on some kernels, and group 2 needs udevd running.
/// A duplicate event costs one extra re-enumeration and nothing else, since the consumer only
/// learns "the set changed".
///
/// Returns None if the socket cannot be opened or bound, which leaves the caller on its poll.
#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::io::RawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    pub struct UeventWatcher {
        stop: Arc<AtomicBool>,
        fd:   RawFd,
    }

    impl UeventWatcher {
        pub fn new<F>(on_change: F) -> Option<Self>
        where
            F: Fn() + Send + Sync + 'static,
        {
            // SAFETY: a socket/bind pair with a correctly sized sockaddr_nl. The descriptor
            // is owned by this struct and closed in Drop.
            let fd = unsafe {
                libc::socket(libc::AF_NETLINK,
                             libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                             libc::NETLINK_KOBJECT_UEVENT)
            };
            if fd < 0 {
                tracing::debug!("device watcher: uevent socket unavailable ({}) — using \
                                 periodic poll", std::io::Error::last_os_error());
                return None;
            }

            // SAFETY: sockaddr_nl zeroed then filled; the cast is the standard bind idiom.
            let bound = unsafe {
                let mut addr: libc::sockaddr_nl = std::mem::zeroed();
                addr.nl_family = libc::AF_NETLINK as u16;
                addr.nl_groups = 1 | 2;   // kernel uevents, and udevd's re-broadcast
                libc::bind(fd,
                           &addr as *const _ as *const libc::sockaddr,
                           std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t)
            };
            if bound < 0 {
                let e = std::io::Error::last_os_error();
                // SAFETY: closing a descriptor this function owns and is about to discard.
                unsafe { libc::close(fd); }
                tracing::debug!("device watcher: cannot bind uevent groups ({e}) — using \
                                 periodic poll");
                return None;
            }

            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let spawned = std::thread::Builder::new()
                .name("cascade-uevent".into())
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        // SAFETY: reading into a buffer this thread owns, on a descriptor
                        // that outlives the loop — Drop shuts the socket down to end it.
                        let n = unsafe {
                            libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void,
                                       buf.len(), 0)
                        };
                        if stop_thread.load(Ordering::Relaxed) { return; }
                        if n <= 0 { continue; }

                        // A uevent is NUL-separated key=value pairs after a summary line.
                        // Only sound-subsystem events matter; everything else on this socket
                        // is other hardware and must not trigger a re-enumeration.
                        let msg = &buf[..n as usize];
                        let is_sound = msg.split(|b| *b == 0).any(|field| {
                            field == b"SUBSYSTEM=sound"
                        });
                        if is_sound {
                            on_change();
                        }
                    }
                });
            if spawned.is_err() {
                // SAFETY: closing a descriptor no thread has taken ownership of.
                unsafe { libc::close(fd); }
                return None;
            }
            Some(UeventWatcher { stop, fd })
        }

        pub fn backend(&self) -> &'static str { "uevent" }
    }

    impl Drop for UeventWatcher {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Shut the socket down so the blocking recv returns and the thread leaves,
            // then close it.
            // SAFETY: the descriptor is owned by this struct and closed exactly once.
            unsafe {
                libc::shutdown(self.fd, libc::SHUT_RDWR);
                libc::close(self.fd);
            }
        }
    }
}
