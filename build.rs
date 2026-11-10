// Embed the Windows resource block: the AllStore icon (shown in Explorer, the
// taskbar and Alt-Tab) plus the version strings that Task Manager and the
// "Apps & features" list read.

fn main() {
    println!("cargo:rerun-if-changed=assets/allstore.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/allstore.ico");
    res.set("ProductName", "AllStore");
    res.set("FileDescription", "AllStore — native Windows app store");
    res.set(
        "LegalCopyright",
        "© 2026 AllStore contributors — MIT License",
    );
    res.set("OriginalFilename", "allstore.exe");
    res.set("InternalName", "allstore");
    res.set("CompanyName", "AllStore");

    // winresource derives FileVersion/ProductVersion from the crate version, so
    // there is a single source of truth (Cargo.toml).
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon resource not embedded: {e}");
    }
}
