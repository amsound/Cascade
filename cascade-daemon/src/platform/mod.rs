//! Desktop integration: the tray icon, the menu-bar item, and the OS shutdown handshake.
//!
//! Nothing in here participates in audio. It exists so that a daemon with no terminal can
//! still be seen, opened and stopped, and so that the OS can stop it cleanly.
//!
//! The two platforms need opposite shapes and are not hidden behind one interface:
//!
//! - Windows owns a hidden top-level window with a message pump ON ITS OWN THREAD, started
//!   from the async runtime. The process keeps its existing thread layout.
//! - macOS requires `NSApplication` on the PROCESS MAIN THREAD, so the runtime moves to a
//!   spawned thread and AppKit takes main. That inversion is driven from `main()`.
//!
//! Pretending those are the same call would hide the one fact anyone reading this needs.

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;
