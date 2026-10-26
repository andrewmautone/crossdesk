fn main() {
    println!("cargo:rerun-if-changed=assets/crossdesk.rc");
    println!("cargo:rerun-if-changed=assets/generated/crossdesk.ico");
    // Embed the app icon in the .exe so Explorer and the taskbar show it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/crossdesk.rc", embed_resource::NONE);
    }
}
