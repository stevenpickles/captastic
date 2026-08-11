use std::io;

const WINDOWS_ICON: &str = "../../assets/branding/captastic.ico";
const WINDOWS_MANIFEST: &str = "captastic.manifest";

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed={WINDOWS_ICON}");
    println!("cargo:rerun-if-changed={WINDOWS_MANIFEST}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(WINDOWS_ICON)
        .set_manifest_file(WINDOWS_MANIFEST)
        .set("FileDescription", "Captastic screenshot capture")
        .set("ProductName", "Captastic");
    resource.compile()
}
