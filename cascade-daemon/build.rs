//! Embeds the Windows application icon. No-op on every other target.
//!
//! The icon is `assets/cascade.ico`, generated from the same hexagon the web UI draws by
//! `cross/make-icons.py`. Windows takes icons as a linked PE resource, so unlike the macOS
//! bundle — where the .icns is just a file dropped into Resources/ — this has to happen at
//! link time, which is why it needs a build script at all.
//!
//! COSMETIC, THEREFORE NEVER FATAL. Every failure path here warns and returns, leaving a
//! working .exe with the default icon. A missing resource compiler on someone's machine
//! must not be the reason Cascade will not build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Stamp the build time into the binary so a log says WHICH build produced it. Without
    // this, diagnosing a remote machine means inferring the version from which log lines
    // happen to be present, which is guesswork and has been wrong.
    // Always re-run, so the stamp is never stale.
    println!("cargo:rerun-if-changed=.");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=CASCADE_BUILD_UNIX={stamp}");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let ico = root.join("assets/cascade.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    if !ico.exists() {
        warn(&format!("{} not found — run cross/make-icons.py; building without an icon",
                      ico.display()));
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // The .rc names the icon by a bare filename and is compiled with OUT_DIR as the working
    // directory: an absolute path inside a .rc has to survive the resource compiler's own
    // string escaping, and on Windows hosts that means backslashes inside a quoted C-style
    // string. Copying sidesteps the whole question.
    if std::fs::copy(&ico, out.join("cascade.ico")).is_err() {
        warn("could not stage the icon into OUT_DIR; building without an icon");
        return;
    }
    // Resource id 1: Windows shows the LOWEST-numbered icon resource as the application
    // icon, so this must sort first if any others are ever added.
    if std::fs::write(out.join("icon.rc"), "1 ICON \"cascade.ico\"\n").is_err() {
        warn("could not write the resource script; building without an icon");
        return;
    }

    let Some(res) = compile(&out) else {
        warn("no usable resource compiler (tried `zig rc`, `llvm-rc`, `windres`) — \
              building without an icon");
        return;
    };
    // -bins, not the crate-wide form: this object belongs in the executable, and attaching
    // it to every link would put it into build-script and test binaries as well.
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

/// Compile OUT_DIR/icon.rc, returning the linkable artefact.
///
/// `zig rc` is tried first because zig is already a hard requirement for the cross builds
/// (see cross/README.md), so it is the one compiler that is certain to be present wherever
/// a Windows binary is currently produced from macOS. `llvm-rc` and `windres` cover a
/// native Windows host and a mingw host respectively.
fn compile(out: &Path) -> Option<PathBuf> {
    let res = out.join("icon.res");

    let zig_ok = Command::new("zig")
        .args(["rc", "/fo", res.to_str()?, "icon.rc"])
        .current_dir(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if zig_ok && res.exists() {
        return Some(res);
    }

    let llvm_ok = Command::new("llvm-rc")
        .args(["/fo", res.to_str()?, "icon.rc"])
        .current_dir(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if llvm_ok && res.exists() {
        return Some(res);
    }

    // windres emits a COFF object rather than a .res, which links just the same.
    let obj = out.join("icon.o");
    let windres_ok = Command::new("windres")
        .args(["icon.rc", "-O", "coff", "-o", obj.to_str()?])
        .current_dir(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if windres_ok && obj.exists() {
        return Some(obj);
    }

    None
}

fn warn(msg: &str) {
    println!("cargo:warning=icon: {msg}");
}
