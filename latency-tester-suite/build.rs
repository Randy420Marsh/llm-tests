//! Embeds the application icon and version info into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // No resource compiler on this machine (rc.exe comes with the Windows SDK / Visual Studio
        // Build Tools): warn and build without the icon instead of failing the build.
        let _ = embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional();
    }
}
