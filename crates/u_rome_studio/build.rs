//! Embeds the ROME icon and version metadata in the Windows executable.
//!
//! Native Windows builds use the SDK's `rc.exe`. Cross builds from macOS or
//! Linux use `llvm-rc` (Homebrew `llvm`; `scripts/build-windows.sh` puts it on
//! PATH). If no resource compiler is found the step is skipped with a warning
//! and the build still succeeds, just without the exe icon.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/rome.ico");
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if !target_windows {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "ROME Studio")
        .set("FileDescription", "ROME Studio asset viewer (u_rome)")
        .set("CompanyName", "u_rome contributors")
        .set("LegalCopyright", "MIT OR Apache-2.0")
        .set("OriginalFilename", "u_rome_studio.exe")
        .set("InternalName", "u_rome_studio")
        .set_icon("assets/rome.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=skipping Windows version resources: {e}");
    }
}
