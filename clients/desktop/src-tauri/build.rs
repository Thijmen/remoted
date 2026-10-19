fn main() {
    // App commands listed here get `allow-…`/`deny-…` permissions and are subject to the ACL:
    // only capabilities that grant them can call them: rd_set_grab and rd_whats_new by the
    // capability added at runtime for the configured gateway origin, the setup commands (and
    // rd_whats_new) by capabilities/setup.json, the update window's by capabilities/update.json.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "rd_set_grab",
            "rd_setup_info",
            "rd_save_gateway",
            "rd_update_state",
            "rd_update_check",
            "rd_update_install",
            "rd_whats_new",
        ]),
    ))
    .expect("failed to run tauri-build");
}
