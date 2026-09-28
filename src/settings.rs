//! Settings persisted between runs (theme, recent databases, query history).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::ThemeId;

/// Maximum remembered databases / queries.
const MAX_RECENT: usize = 8;
const MAX_HISTORY: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeId,
    pub recent_databases: Vec<String>,
    pub history: Vec<String>,
    pub row_limit: usize,
    /// File the settings are saved to; `None` keeps them in memory only
    /// (used by tests so they never touch the user's settings).
    #[serde(skip)]
    store: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeId::default(),
            recent_databases: Vec::new(),
            history: Vec::new(),
            row_limit: 100_000,
            store: None,
        }
    }
}

impl Settings {
    /// Where settings live, e.g. `~/.config/joust/settings.json`.
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("joust").join("settings.json"))
    }

    /// Loads the user's settings, falling back to defaults on any error.
    pub fn load() -> Self {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Loads settings from `path` (defaults if missing or invalid); later
    /// saves go back to `path`.
    pub fn load_from(path: &Path) -> Self {
        let settings: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        Self {
            store: Some(path.to_path_buf()),
            ..settings
        }
    }

    /// Best-effort save; failures are ignored (settings are a convenience).
    pub fn save(&self) {
        let Some(path) = &self.store else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Moves `uri` to the front of the recent-database list.
    pub fn remember_database(&mut self, uri: &str) {
        self.recent_databases.retain(|existing| existing != uri);
        self.recent_databases.insert(0, uri.to_string());
        self.recent_databases.truncate(MAX_RECENT);
    }

    /// Moves `sql` to the front of the query history.
    pub fn remember_query(&mut self, sql: &str) {
        let sql = sql.trim();
        if sql.is_empty() {
            return;
        }
        self.history.retain(|existing| existing != sql);
        self.history.insert(0, sql.to_string());
        self.history.truncate(MAX_HISTORY);
    }
}

/// Default location of the sample database, e.g. `~/.local/share/joust/sample.lancedb`.
pub fn sample_database_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("joust")
        .join("sample.lancedb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_lists_dedupe_and_cap() {
        let mut settings = Settings::default();
        for i in 0..20 {
            settings.remember_database(&format!("db{i}"));
        }
        settings.remember_database("db5");
        assert_eq!(settings.recent_databases.len(), MAX_RECENT);
        assert_eq!(settings.recent_databases[0], "db5");

        settings.remember_query("  SELECT 1  ");
        settings.remember_query("SELECT 2");
        settings.remember_query("SELECT 1");
        assert_eq!(settings.history, ["SELECT 1", "SELECT 2"]);
        settings.remember_query("   ");
        assert_eq!(settings.history.len(), 2);
    }

    #[test]
    fn saves_and_loads_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");

        let mut settings = Settings::load_from(&path);
        assert_eq!(
            settings.theme,
            ThemeId::default(),
            "missing file → defaults"
        );
        settings.theme = ThemeId::Dracula;
        settings.remember_query("SELECT 1");
        settings.save();

        let reloaded = Settings::load_from(&path);
        assert_eq!(reloaded, settings);
        assert!(std::fs::read_to_string(&path).unwrap().contains("Dracula"));

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load_from(&path).theme, ThemeId::default());
    }

    #[test]
    fn default_settings_are_in_memory_only() {
        // No store: `save` must be a no-op, so tests never write user files.
        assert_eq!(Settings::default().store, None);
        Settings::default().save();
        assert!(Settings::path().is_none_or(|p| p.ends_with("joust/settings.json")));
    }

    #[test]
    fn default_locations() {
        // Reading the real settings is harmless; they are never written here.
        assert_eq!(Settings::load().store, Settings::path());
        assert!(sample_database_path().ends_with("joust/sample.lancedb"));
    }

    #[test]
    fn deserialises_partial_json() {
        let settings: Settings = serde_json::from_str(r#"{"theme":"Nord"}"#).unwrap();
        assert_eq!(settings.theme, ThemeId::Nord);
        assert_eq!(settings.row_limit, 100_000);
    }
}
