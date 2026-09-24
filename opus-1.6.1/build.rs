//! Builds libopus 1.6.1 from the vendored source in `vendor/`, using `cc` and nothing else.
//!
//! Why this crate exists: the upstream `audiopus_sys` build script tries `pkg_config`
//! FIRST and only falls back to a cmake build of its own bundled copy. On any machine with
//! a system libopus installed, that silently links whatever version happens to be there —
//! 1.3.1 on Debian 12, 1.4 on Debian 13, 1.5.2 on current Homebrew. Cascade requires
//! exactly 1.6.1 on every platform, so "whatever is installed" is not acceptable, and
//! neither is a build that needs cmake and pkg-config present on a headless Pi.
//!
//! This build script has no discovery logic of any kind. One code path: compile the
//! vendored source. The only external requirement is a C compiler.
//!
//! The source lists are PARSED from the vendored `*.mk` files rather than hardcoded here,
//! so a version bump is "replace vendor/, update the checksum in README.md" and not
//! "re-derive the file list by hand".

use std::{collections::HashMap, env, fs, path::PathBuf};

/// Parse opus's `*.mk` fragments into `VARIABLE -> [source paths]`.
///
/// Plain make: `VAR = \` followed by continuation lines, terminated by a line with no
/// trailing backslash. Comments and blank lines are ignored.
fn parse_mk(text: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            current = None;
            continue;
        }
        let (body, continues) = match line.strip_suffix('\\') {
            Some(b) => (b.trim(), true),
            None => (line, false),
        };

        let body = if let Some((name, rest)) = body.split_once('=') {
            let name = name.trim().to_string();
            out.entry(name.clone()).or_default();
            current = Some(name);
            rest.trim()
        } else {
            body
        };

        if let Some(var) = &current {
            for tok in body.split_whitespace() {
                out.get_mut(var).unwrap().push(tok.to_string());
            }
        }
        if !continues {
            current = None;
        }
    }
    out
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    let mut lists: HashMap<String, Vec<String>> = HashMap::new();
    for mk in ["celt_sources.mk", "silk_sources.mk", "opus_sources.mk"] {
        let path = root.join(mk);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));
        lists.extend(parse_mk(&text));
        println!("cargo:rerun-if-changed={}", path.display());
    }

    // ── Defines, collected once and applied to every archive ─────────────────────────
    // Gathered into a list rather than pushed straight into one cc::Build because the
    // ISA-specific objects below are compiled as separate archives (they need their own
    // -m flags) and must see the identical configuration.
    let mut defines: Vec<(&str, Option<String>)> = Vec::new();

    // OPUS_BUILD is mandatory when compiling the library itself. No config.h is generated,
    // so HAVE_CONFIG_H stays undefined and opus uses its own in-tree defaults.
    defines.push(("OPUS_BUILD", None));

    // Read the version from the vendored tree so it cannot drift from the source. Without
    // it opus falls back to `#define PACKAGE_VERSION "unknown"` and reports
    // "libopus unknown", which would leave tests/version.rs unable to tell a correct build
    // from a wrong one. Autotools and cmake both define this the same way.
    let version_file = root.join("package_version");
    let version = fs::read_to_string(&version_file)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", version_file.display(), e))
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().strip_prefix('"'))
        .and_then(|v| v.strip_suffix('"'))
        .expect("package_version is not of the form PACKAGE_VERSION=\"x.y.z\"")
        .to_string();
    defines.push(("PACKAGE_VERSION", Some(format!("\"{}\"", version))));
    println!("cargo:rerun-if-changed={}", version_file.display());
    println!("cargo:version={}", version);

    // Float build: FIXED_POINT is deliberately NOT defined — Cascade is f32 end to end and
    // every target has hardware float.
    defines.push(("HAVE_LRINTF", None));
    defines.push(("HAVE_LRINT", None));

    // Stack allocation. VAR_ARRAYS uses C99 variable-length arrays (gcc/clang); MSVC has
    // no VLAs and gets alloca instead.
    if target_env == "msvc" {
        defines.push(("USE_ALLOCA", None));
    } else {
        defines.push(("VAR_ARRAYS", None));
    }

    // ── Source selection ─────────────────────────────────────────────────────────────
    // Base float build. The DEEP_PLC / DRED / OSCE / LOSSGEN groups (and the 22MB of neural
    // weights behind them) are 1.5+ additions the reference predates and Cascade does not
    // use; `vendor/dnn/` is pruned accordingly. To enable one: restore that directory, add
    // its source group here, and define the matching ENABLE_*.
    let mut groups = vec!["CELT_SOURCES", "SILK_SOURCES", "SILK_SOURCES_FLOAT",
                          "OPUS_SOURCES", "OPUS_SOURCES_FLOAT"];

    // ── SIMD: run-time CPU detection, exactly as an ordinary upstream build does ──────
    // Presume what the target ABI already guarantees (SSE/SSE2 on x86_64, NEON on aarch64)
    // and detect the rest at run time.
    //
    // An earlier version of this file left OPUS_HAVE_RTCD off and presumed only the
    // baseline, to keep the build simple. That was the wrong trade: it silently dropped
    // SSE4.1 and AVX2, so the vendored build decoded SLOWER than the system libopus it
    // replaced. Vendoring is here to pin the VERSION, not to ship a cut-down codec.
    let mut sse41: Vec<String> = Vec::new();
    let mut avx2:  Vec<String> = Vec::new();

    match target_arch.as_str() {
        "x86_64" => {
            groups.extend(["CELT_SOURCES_SSE", "CELT_SOURCES_SSE2",
                           "CELT_SOURCES_X86_RTCD", "SILK_SOURCES_X86_RTCD"]);
            for d in ["OPUS_HAVE_RTCD",
                      // How x86cpu.c is allowed to read CPUID. Without one of
                      // CPU_INFO_BY_C / CPU_INFO_BY_ASM it #errors outright ("no CPU
                      // detection method available"). The C form uses the compiler's
                      // __get_cpuid intrinsic, which gcc/clang both provide and which
                      // avoids the inline-asm variants' PIC register juggling.
                      "CPU_INFO_BY_C",
                      "OPUS_X86_MAY_HAVE_SSE",    "OPUS_X86_PRESUME_SSE",
                      "OPUS_X86_MAY_HAVE_SSE2",   "OPUS_X86_PRESUME_SSE2",
                      "OPUS_X86_MAY_HAVE_SSE4_1", "OPUS_X86_MAY_HAVE_AVX2"] {
                defines.push((d, None));
            }
            for g in ["CELT_SOURCES_SSE4_1", "SILK_SOURCES_SSE4_1"] {
                sse41.extend(lists.get(g).cloned().unwrap_or_default());
            }
            for g in ["CELT_SOURCES_AVX2", "SILK_SOURCES_AVX2", "SILK_SOURCES_FLOAT_AVX2"] {
                avx2.extend(lists.get(g).cloned().unwrap_or_default());
            }
        }
        "aarch64" => {
            // NEON is mandatory in the aarch64 ABI, so it is presumed rather than detected —
            // there is no run-time choice left to make for the intrinsics opus uses here.
            groups.extend(["CELT_SOURCES_ARM_NEON_INTR", "SILK_SOURCES_ARM_NEON_INTR"]);
            for d in ["OPUS_ARM_MAY_HAVE_NEON", "OPUS_ARM_MAY_HAVE_NEON_INTR",
                      "OPUS_ARM_PRESUME_NEON", "OPUS_ARM_PRESUME_NEON_INTR",
                      "OPUS_ARM_PRESUME_AARCH64_NEON_INTR"] {
                defines.push((d, None));
            }
        }
        // Anything else builds the portable C path: correct everywhere, just slower.
        _ => {}
    }

    // Shared configuration for every archive.
    let configure = |b: &mut cc::Build| {
        b.include(root.join("include"))
         .include(root.join("celt"))
         .include(root.join("silk"))
         .include(root.join("silk/float"))
         .include(root.join("src"))
         .include(&root)
         .warnings(false)   // upstream C, not ours to lint
         .opt_level(3);     // codec speed matters even in debug builds of Cascade
        for (d, v) in &defines {
            b.define(d, v.as_deref());
        }
    };

    let mut build = cc::Build::new();
    configure(&mut build);

    let mut count = 0usize;
    for group in &groups {
        let files = lists.get(*group)
            .unwrap_or_else(|| panic!("source group {} missing from the vendored .mk files",
                                      group));
        for f in files {
            // Skip translation units that are empty in THIS configuration. Both are
            // guarded to fixed-point-only or debug-only builds, so they compile to nothing
            // and ranlib warns "has no symbols" on every build. Nothing is lost: the float
            // SSE paths live in pitch_sse.c and vq_sse2.c, which are compiled.
            if f.ends_with("celt/x86/pitch_sse2.c") || f.ends_with("silk/debug.c") {
                continue;
            }
            build.file(root.join(f));
            count += 1;
        }
    }
    assert!(count > 100, "only {} source files selected — .mk parsing is wrong", count);
    build.compile("opus");

    // ISA-specific objects. Each needs its own -m flag, and cc applies flags per Build,
    // hence separate archives. They are only ever ENTERED through opus's run-time dispatch
    // tables, so compiling them does not require the build host to support the instructions.
    for (name, files, flags) in [
        ("opus_sse41", &sse41, &["-msse4.1"][..]),
        ("opus_avx2",  &avx2,  &["-mavx2", "-mfma"][..]),
    ] {
        if files.is_empty() {
            continue;
        }
        let mut b = cc::Build::new();
        configure(&mut b);
        for f in flags {
            b.flag(f);
        }
        for f in files {
            b.file(root.join(f));
        }
        b.compile(name);
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:vendored=1");
    println!("cargo:static=1");
}
