//! Embeds the program icon in the Windows executable, so Explorer, the taskbar and
//! shortcuts show it. Needs the Windows resource compiler, so it only runs on Windows hosts.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("embed Windows icon");
    }
}
