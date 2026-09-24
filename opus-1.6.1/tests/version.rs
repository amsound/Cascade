//! Guards the whole reason this crate exists: that Cascade links libopus 1.6.1 and not
//! whatever copy happens to be installed on the build machine. If someone reintroduces a
//! pkg-config/system-discovery path, or bumps `vendor/` without meaning to, this fails.

#[test]
fn links_vendored_opus_1_6_1() {
    let raw = unsafe { std::ffi::CStr::from_ptr(audiopus_sys::opus_get_version_string()) };
    let version = raw.to_str().expect("opus version string is not valid UTF-8");
    assert!(
        version.contains("1.6.1"),
        "expected libopus 1.6.1, linked against {:?} — a system libopus has \
         probably shadowed the vendored build",
        version
    );
}
