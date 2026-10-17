//! The ScreenCaptureKit Swift bridge loads the Swift runtime through `@rpath`. A dependency's
//! `rustc-link-arg` only applies to that dependency's own targets, so every final binary that
//! links this crate (our tests and examples here, `rd-agent` in its own build script) must add
//! `/usr/lib/swift` to its rpath.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
