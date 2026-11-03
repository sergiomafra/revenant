//! The user's settings (theme and language), kept as `settings.json` in the app's config folder.
//!
//! A missing or unreadable file gives the defaults (automatic theme, the system's language): settings are never a
//! reason for the app not to open.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ErrorView;

/// The file name inside the app's config folder.
pub const FILE_NAME: &str = "settings.json";

/// The error code when the settings cannot be written.
pub const WRITE_FAILED: &str = "settings_write_failed";

/// Light, dark, or following the system (`auto`, the default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
    #[default]
    Auto,
}

/// The interface languages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "pt-BR")]
    PtBr,
    #[serde(rename = "es")]
    Es,
    #[serde(rename = "en")]
    En,
}

/// What the interface stores. `language: None` follows the system's language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub language: Option<Language>,
}

/// Reads the settings from `dir`; a missing, unreadable or malformed file gives the defaults.
pub fn load(dir: &Path) -> Settings {
    fs::read(dir.join(FILE_NAME))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Writes the settings to `dir` (created if needed), replacing the file in one step.
pub fn save(dir: &Path, settings: &Settings) -> Result<(), ErrorView> {
    let failed = |_| ErrorView { code: WRITE_FAILED };
    fs::create_dir_all(dir).map_err(failed)?;
    let json = serde_json::to_vec_pretty(settings).expect("settings serialize to JSON");
    // Write a sibling file, then rename it over the old one: a crash never leaves half a file.
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    fs::write(&tmp, json).map_err(failed)?;
    fs::rename(&tmp, dir.join(FILE_NAME)).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("nested");
        let s = Settings {
            theme: Theme::Dark,
            language: Some(Language::Es),
        };
        save(&conf, &s).unwrap();
        assert_eq!(load(&conf), s);
        let none = Settings {
            theme: Theme::Light,
            language: None,
        };
        save(&conf, &none).unwrap();
        assert_eq!(load(&conf), none);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(dir.path()),
            Settings {
                theme: Theme::Auto,
                language: None
            }
        );
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), "{{").unwrap();
        assert_eq!(load(dir.path()), Settings::default());
    }
}
