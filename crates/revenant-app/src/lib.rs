//! The Revenant desktop app: the Tauri shell that hosts the interface in `app/` and, later, the engine commands.

/// Opens the main window and runs the app until it is closed.
pub fn run() {
    tauri::Builder::default()
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
}
