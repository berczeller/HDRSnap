fn main() {
    // Icon, manifest (per-monitor DPI v2) and version info for the exe.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/app.ico");
        println!("cargo:rerun-if-changed=assets/app.manifest");
        embed_resource::compile_for("assets/app.rc", ["HDRSnap"], embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
