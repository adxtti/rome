//! Embeds Windows version metadata in the executable.
//!
//! Only runs when both host and target are Windows (CI builds natively on
//! Windows). Cross builds (cargo xwin from macOS/Linux) have no resource
//! compiler, so they skip this step and still succeed.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let host_windows = std::env::var("HOST").is_ok_and(|h| h.contains("windows"));
    if !(target_windows && host_windows) {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "ROME Studio")
        .set("FileDescription", "ROME Studio asset viewer (u_rome)")
        .set("CompanyName", "u_rome contributors")
        .set("LegalCopyright", "MIT OR Apache-2.0")
        .set("OriginalFilename", "u_rome_studio.exe")
        .set("InternalName", "u_rome_studio");
    if let Err(e) = res.compile() {
        println!("cargo:warning=skipping Windows version resources: {e}");
    }
}
