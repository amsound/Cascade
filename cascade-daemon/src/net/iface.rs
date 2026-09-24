//! Network interface enumeration and change monitoring.
//!
//! Binding:
//!   - "any" → 0.0.0.0 (INADDR_ANY, every interface)
//!   - named → that NIC's IPv4 address only
//!
//! Changes are reported by an event-driven OS watcher, with zero overhead between events:
//!   - macOS: PF_ROUTE socket → kqueue EVFILT_READ (route/interface events)
//!   - Linux: netlink RTMGRP_LINK | RTMGRP_IPV4_IFADDR
//!   - Windows: `NotifyAddrChange`, which blocks until the address table changes
//!
//! On loss of the configured interface the audio socket falls back to "any" in place, and
//! when the interface returns it rebinds back to it — no restart either way. The configured
//! choice is never changed. See the interface watcher handler in main.rs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// One network interface visible to the host.
#[derive(Debug, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub addr: Ipv4Addr,
}

/// Enumerate every up, non-loopback IPv4 interface.
pub fn list_interfaces() -> Vec<NetworkInterface> {
    #[cfg(windows)]
    return windows_list();
    #[cfg(not(windows))]
    {
    let mut out = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 { return out; }
        let mut ifa = ifap;
        while !ifa.is_null() {
            let i = &*ifa;
            if i.ifa_addr.is_null() { ifa = i.ifa_next; continue; }
            // sa_family_t differs by platform (u8 on Darwin, u16 on Linux) — cast to the
            // type rather than to a fixed width.
            if (*i.ifa_addr).sa_family != libc::AF_INET as libc::sa_family_t {
                ifa = i.ifa_next;
                continue;
            }
            let flags = i.ifa_flags as i32;
            // Skip loopback and down interfaces
            if flags & libc::IFF_LOOPBACK != 0 { ifa = i.ifa_next; continue; }
            if flags & libc::IFF_UP == 0       { ifa = i.ifa_next; continue; }

            let sin = &*(i.ifa_addr as *const libc::sockaddr_in);
            let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));

            let name = std::ffi::CStr::from_ptr(i.ifa_name)
                .to_string_lossy().into_owned();
            out.push(NetworkInterface { name, addr });
            ifa = i.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
    }
}

/// Windows enumeration via `GetAdaptersAddresses`.
///
/// Filters to the same set the unix arm does — up, non-loopback, IPv4 — so the picker sees
/// one list with one meaning on every platform. The `FriendlyName` is used rather than the
/// adapter GUID because it is what the user sees in Windows itself; it is also what a
/// configuration stores, so it has to be the stable-looking one.
#[cfg(windows)]
fn windows_list() -> Vec<NetworkInterface> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;

    // Two-call idiom: ask for the size, then fill. The table can grow between the two, so
    // the fill is retried a bounded number of times rather than assumed to fit.
    let mut out = Vec::new();
    let mut size: u32 = 16 * 1024;
    for _ in 0..4 {
        let mut buf = vec![0u8; size as usize];
        let head = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        // Safety: `head` points at `size` writable bytes, which is what is promised.
        let rc = unsafe {
            GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(head), &mut size)
        };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            continue; // `size` now holds what is actually needed
        }
        if rc != ERROR_SUCCESS.0 && rc != NO_ERROR.0 {
            return out;
        }
        // Safety: on success the buffer holds a null-terminated linked list of adapters,
        // each with a null-terminated list of unicast addresses.
        unsafe {
            let mut ad = head;
            while !ad.is_null() {
                let a = &*ad;
                if a.IfType == IF_TYPE_SOFTWARE_LOOPBACK || a.OperStatus != IfOperStatusUp {
                    ad = a.Next;
                    continue;
                }
                let name = a.FriendlyName.to_string().unwrap_or_default();
                let mut ua = a.FirstUnicastAddress;
                while !ua.is_null() {
                    let sa = (*ua).Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        let octets = sin.sin_addr.S_un.S_addr.to_ne_bytes();
                        let addr = Ipv4Addr::from(octets);
                        if !addr.is_loopback() && !addr.is_unspecified() && !name.is_empty() {
                            out.push(NetworkInterface { name: name.clone(), addr });
                        }
                    }
                    ua = (*ua).Next;
                }
                ad = a.Next;
            }
        }
        return out;
    }
    out
}

/// Resolve the configured interface name to a bind SocketAddr.
/// "any" (case-insensitive) → 0.0.0.0:port.
/// Named interface → that interface's IPv4 address : port.
/// Returns None if a named interface is configured but not found.
pub fn resolve_bind_addr(iface: &str, port: u16) -> Option<SocketAddr> {
    if iface.eq_ignore_ascii_case("any") {
        return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port));
    }
    let ifaces = list_interfaces();
    ifaces.iter()
        .find(|i| i.name.eq_ignore_ascii_case(iface))
        .map(|i| SocketAddr::new(IpAddr::V4(i.addr), port))
}

/// Check whether a named interface is currently present.
pub fn interface_present(name: &str) -> bool {
    if name.eq_ignore_ascii_case("any") { return true; }
    list_interfaces().iter().any(|i| i.name.eq_ignore_ascii_case(name))
}

// ── OS-native change watcher ───────────────────────────────────────────────

/// How long a watcher waits before trying again after its OS wait fails for a reason other
/// than an interruption.
const WATCH_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Start a background thread that blocks waiting for interface/route change
/// events from the OS. Invokes `on_change` with the fresh interface list inline
/// on this same thread whenever something changes — no second handler thread and
/// no channel hop (the callback work is cheap: build a UI payload + presence
/// check). Zero overhead between events (no polling).
///
/// macOS: opens a PF_ROUTE socket and uses kqueue EVFILT_READ.
/// Linux: opens a netlink socket with RTMGRP_LINK | RTMGRP_IPV4_IFADDR.
/// Windows: blocks in `NotifyAddrChange`, which returns once per address-table change.
///
/// The watcher never gives up for the rest of the run. An interrupted wait simply waits
/// again. Any other failure is logged once, and the watcher retries every `WATCH_RETRY` —
/// reopening its socket on macOS and Linux — and reports the interface list again as it
/// recovers, since changes made while it was failing were never delivered.
pub fn start_watcher<F>(on_change: F)
where
    F: FnMut(Vec<NetworkInterface>) + Send + 'static,
{
    std::thread::Builder::new()
        .name("cascade-ifwatch".into())
        .spawn(move || {
            #[cfg(target_os = "macos")]
            watch_macos(on_change);
            #[cfg(target_os = "linux")]
            watch_linux(on_change);
            #[cfg(windows)]
            watch_windows(on_change);
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            let _ = on_change; // unsupported platform — watcher is a no-op
        })
        .ok();
}

#[cfg(target_os = "macos")]
fn watch_macos<F: FnMut(Vec<NetworkInterface>)>(mut on_change: F) {
    let mut failing = false;
    loop {
        // SAFETY: plain socket/kqueue syscalls on descriptors this function owns and closes.
        let err = unsafe {
            // PF_ROUTE socket: kernel sends RTM_* messages on route/interface changes
            let sock = libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_UNSPEC);
            let kq = if sock >= 0 { libc::kqueue() } else { -1 };
            if sock < 0 || kq < 0 {
                let e = std::io::Error::last_os_error();
                if sock >= 0 { libc::close(sock); }
                e
            } else {
                // kqueue + EVFILT_READ: wake when the route socket has data
                let mut kev = libc::kevent {
                    ident:  sock as usize,
                    filter: libc::EVFILT_READ,
                    flags:  libc::EV_ADD | libc::EV_ENABLE,
                    fflags: 0,
                    data:   0,
                    udata:  std::ptr::null_mut(),
                };
                libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null());
                if failing {
                    tracing::info!("interface watcher: running again");
                    failing = false;
                    on_change(list_interfaces());
                }
                let mut buf = [0u8; 2048];
                let e = loop {
                    // Block until route socket readable (interface/route event)
                    let n = libc::kevent(kq, std::ptr::null(), 0, &mut kev, 1, std::ptr::null());
                    if n < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::EINTR) { continue; }
                        break e;
                    }
                    if n == 0 { continue; }
                    // Drain the message. A failed read (ENOBUFS: messages were dropped)
                    // changes nothing here — the whole table is re-read below either way.
                    libc::recv(sock, buf.as_mut_ptr() as *mut _, buf.len(), 0);
                    // Re-enumerate and handle inline (no channel/second thread).
                    on_change(list_interfaces());
                };
                libc::close(kq);
                libc::close(sock);
                e
            }
        };
        if !failing {
            tracing::warn!("interface watcher: {err} — retrying every {}s", WATCH_RETRY.as_secs());
            failing = true;
        }
        std::thread::sleep(WATCH_RETRY);
    }
}

#[cfg(target_os = "linux")]
fn watch_linux<F: FnMut(Vec<NetworkInterface>)>(mut on_change: F) {
    let mut failing = false;
    loop {
        // SAFETY: plain netlink socket syscalls on a descriptor this function owns and closes.
        let err = unsafe {
            let sock = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE);
            let mut addr: libc::sockaddr_nl = std::mem::zeroed();
            addr.nl_family = libc::AF_NETLINK as u16;
            addr.nl_groups = (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR) as u32;
            if sock < 0 || libc::bind(sock,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&addr) as u32) < 0 {
                let e = std::io::Error::last_os_error();
                if sock >= 0 { libc::close(sock); }
                e
            } else {
                if failing {
                    tracing::info!("interface watcher: running again");
                    failing = false;
                    on_change(list_interfaces());
                }
                let mut buf = [0u8; 4096];
                let e = loop {
                    let n = libc::recv(sock, buf.as_mut_ptr() as *mut _, buf.len(), 0);
                    if n < 0 {
                        let e = std::io::Error::last_os_error();
                        match e.raw_os_error() {
                            Some(libc::EINTR) => continue,
                            // The kernel dropped messages because this socket's buffer
                            // overflowed — a burst of link/address changes, such as a VPN or
                            // container network coming up. Which ones is unknowable, so
                            // re-read the whole table and carry on listening.
                            Some(libc::ENOBUFS) => {
                                on_change(list_interfaces());
                                continue;
                            }
                            _ => break e,
                        }
                    }
                    if n == 0 {
                        break std::io::Error::new(std::io::ErrorKind::UnexpectedEof,
                                                  "netlink socket closed");
                    }
                    on_change(list_interfaces());
                };
                libc::close(sock);
                e
            }
        };
        if !failing {
            tracing::warn!("interface watcher: {err} — retrying every {}s", WATCH_RETRY.as_secs());
            failing = true;
        }
        std::thread::sleep(WATCH_RETRY);
    }
}

/// Windows: `NotifyAddrChange` blocks until the IPv4 address table changes.
///
/// Called with no handle and no overlapped structure it is synchronous, which is exactly
/// the shape this thread wants — the same "block, then report" loop as the other two arms,
/// with no polling in between.
///
/// A failure is retried after `WATCH_RETRY` rather than immediately, so a call that keeps
/// failing cannot turn the watcher into a busy loop, and the list is reported after each
/// retry because a change made in between was never delivered.
#[cfg(windows)]
fn watch_windows<F: FnMut(Vec<NetworkInterface>)>(mut on_change: F) {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::NetworkManagement::IpHelper::NotifyAddrChange;
    let mut failing = false;
    loop {
        // Safety: passing null for both the handle and the overlapped structure is the
        // documented way to make this call synchronous — it then blocks in place rather
        // than completing asynchronously through either of them.
        let rc = unsafe { NotifyAddrChange(std::ptr::null_mut(), std::ptr::null()) };
        if rc != NO_ERROR.0 {
            if !failing {
                tracing::warn!("interface watcher: NotifyAddrChange failed ({rc}) — retrying \
                                every {}s", WATCH_RETRY.as_secs());
                failing = true;
            }
            std::thread::sleep(WATCH_RETRY);
            on_change(list_interfaces());
            continue;
        }
        if failing {
            tracing::info!("interface watcher: running again");
            failing = false;
        }
        on_change(list_interfaces());
    }
}
