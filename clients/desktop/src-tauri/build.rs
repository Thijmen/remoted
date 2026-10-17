fn main() {
    // App commands listed here get `allow-…`/`deny-…` permissions and are subject to the ACL:
    // only capabilities that grant them (capabilities/gateway.json, or the --url override added
    // at runtime) can call them.
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["rd_set_grab"])),
    )
    .expect("failed to run tauri-build");
}
