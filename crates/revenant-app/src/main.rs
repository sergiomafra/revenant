// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // When the system's permission prompt relaunches this binary to open a disk, that child does only that and exits.
    if let Some(code) =
        revenant_core::source::elevated_entry(&std::env::args_os().collect::<Vec<_>>())
    {
        std::process::exit(code)
    }
    revenant_app::run()
}
