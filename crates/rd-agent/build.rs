//! - Picks the LaunchAgent plist template: `packaging/macos/dev.thijmen.rd.agent.plist.template`
//!   when the repository has one, else the copy embedded in this crate.
//! - macOS: adds `/usr/lib/swift` to the rpath; rd-platform-macos's ScreenCaptureKit bridge
//!   loads the Swift runtime through `@rpath` (a dependency cannot set this for us).

use std::path::PathBuf;

const TEMPLATE: &str = "dev.thijmen.rd.agent.plist.template";

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let packaging_dir = manifest.join("../../packaging/macos");
    let packaged = packaging_dir.join(TEMPLATE);
    let embedded = manifest.join("assets").join(TEMPLATE);
    // The template files only: watching packaging/macos would rebuild rd-agent after every
    // build-app.sh, which writes its app bundle under packaging/macos/build. Cargo still notices
    // a watched file being added or removed.
    println!("cargo:rerun-if-changed={}", packaged.display());
    println!("cargo:rerun-if-changed={}", embedded.display());
    let source = if packaged.is_file() {
        packaged
    } else {
        embedded
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join(TEMPLATE);
    std::fs::copy(&source, &out)
        .unwrap_or_else(|e| panic!("copying {} to {}: {e}", source.display(), out.display()));
}
