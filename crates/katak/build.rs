//! Embeds the icon and version information into katak.exe on Windows, so
//! Explorer, the Start menu and the taskbar show the Katak logo.

fn main() {
    println!("cargo:rerun-if-changed=assets/katak.ico");
    // Build scripts run on the host; only Windows targets get resources.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/katak.ico")
            .set("ProductName", "Katak")
            .set(
                "FileDescription",
                "Katak - viewer and editor for huge CSV files",
            )
            .set(
                "LegalCopyright",
                "Copyright (c) 2026 Erkin Pardayev. MIT License.",
            );
        // Without a resource compiler the program still works, just
        // without an icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=no icon embedded in katak.exe: {e}");
        }
    }
}
