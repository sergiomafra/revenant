//! The Revenant desktop app: the Tauri shell that hosts the interface in `app/` and the commands it calls.

pub mod access;
pub mod disks;
pub mod settings;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use settings::Settings;

/// An error as the interface sees it: only a `snake_case` code, which the interface translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ErrorView {
    pub code: &'static str,
}

impl From<&revenant_core::EngineError> for ErrorView {
    fn from(e: &revenant_core::EngineError) -> ErrorView {
        ErrorView { code: e.code() }
    }
}

/// The stored settings, or the defaults when there are none (or the config folder is unknown).
#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    match app.path().app_config_dir() {
        Ok(dir) => settings::load(&dir),
        Err(_) => Settings::default(),
    }
}

/// Stores the settings in the app's config folder.
#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Result<(), ErrorView> {
    let dir = app.path().app_config_dir().map_err(|_| ErrorView {
        code: settings::WRITE_FAILED,
    })?;
    settings::save(&dir, &settings)
}

/// Opens the main window and runs the app until it is closed.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            disks::list_disks,
            access::request_access
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Revenant app");
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    const CONF: &str = include_str!("../tauri.conf.json");

    #[test]
    fn window_config_matches_spec() {
        let conf: Value = serde_json::from_str(CONF).expect("tauri.conf.json is JSON");
        let windows = conf["app"]["windows"]
            .as_array()
            .expect("app.windows is a list");
        assert_eq!(windows.len(), 1, "one main window");
        let w = &windows[0];
        assert_eq!(w["title"], "Revenant");
        assert_eq!(w["width"], 1280);
        assert_eq!(w["height"], 800);
        assert_eq!(w["minWidth"], 1024);
        assert_eq!(w["minHeight"], 700);
        assert_eq!(conf["productName"], "Revenant");
    }

    #[test]
    fn csp_is_strict() {
        let conf: Value = serde_json::from_str(CONF).expect("tauri.conf.json is JSON");
        let csp = conf["app"]["security"]["csp"]
            .as_str()
            .expect("a content security policy is set");
        assert!(csp.contains("default-src 'self'"));
        assert!(!csp.contains("unsafe-eval"));
        assert!(!csp.contains('*'));
        assert!(!csp.contains("script-src") || !csp.contains("script-src 'unsafe-inline'"));
    }
}
