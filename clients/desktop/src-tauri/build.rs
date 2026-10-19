fn main() {
    // App commands listed here get `allow-…`/`deny-…` permissions and are subject to the ACL:
    // only capabilities that grant them can call them: rd_set_grab by the capability added at
    // runtime for the configured gateway origin, the setup commands by capabilities/setup.json.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "rd_set_grab",
            "rd_setup_info",
            "rd_save_gateway",
        ]),
    ))
    .expect("failed to run tauri-build");
}
