//! `captastic-cli.exe`, the command line that ships beside `captastic.exe` (ADR 0014). It asks the
//! running Captastic for captures and prints the answers as JSON.

fn main() {
    captastic_app::command_line_main();
}
