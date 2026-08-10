use std::io;

const WINDOWS_ICON: &str = "../../assets/branding/captastic.ico";

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed={WINDOWS_ICON}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(WINDOWS_ICON)
        .set("FileDescription", "Captastic screenshot capture")
        .set("ProductName", "Captastic");
    resource.compile()
}
