#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    if captastic_windows::DaemonControl::is_running() {
        return;
    }
    if let Err(error) = launch_daemon() {
        captastic_windows::show_error_dialog(&format!("Captastic could not start.\n\n{error}"));
    }
}

#[cfg(windows)]
fn launch_daemon() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut executable = std::env::current_exe()?;
    executable.set_file_name("captastic.exe");
    if !executable.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} was not found", executable.display()),
        ));
    }
    Command::new(executable)
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("captastic-desktop is currently available only on Windows");
}
