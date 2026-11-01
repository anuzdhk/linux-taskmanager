//! User settings in ~/.config/taskmonitor/config as plain `key = value` lines.

use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// 0 = follow the desktop, 1 = light, 2 = dark.
    pub theme: i32,
    /// Index into the refresh-rate choices (see INTERVALS_MS).
    pub refresh: i32,
    pub start_page: String,
    pub always_on_top: bool,
    pub app_history: bool,
    /// CPU / RAM / GPU in the desktop's top bar.
    pub top_bar_stats: bool,
    /// Closing the window keeps the app running, with stats in the top bar.
    pub run_in_background: bool,
    /// Ask GitHub for a newer version at most once a day.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: 0,
            refresh: 1,
            start_page: "processes".into(),
            always_on_top: false,
            app_history: false,
            top_bar_stats: false,
            run_in_background: false,
            check_updates: true,
        }
    }
}

fn path() -> PathBuf {
    let config = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(config).join("taskmonitor/config")
}

impl Settings {
    pub fn load() -> Self {
        fs::read_to_string(path()).map(|t| Self::parse(&t)).unwrap_or_default()
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "theme" => s.theme = value.parse().unwrap_or(0).clamp(0, 2),
                "refresh" => s.refresh = value.parse().unwrap_or(1).clamp(0, 4),
                "start_page" => s.start_page = value.to_owned(),
                "always_on_top" => s.always_on_top = value == "true",
                "app_history" => s.app_history = value == "true",
                "top_bar_stats" => s.top_bar_stats = value == "true",
                "run_in_background" => s.run_in_background = value == "true",
                "check_updates" => s.check_updates = value != "false",
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = format!(
            "# TaskMonitor settings\ntheme = {}\nrefresh = {}\nstart_page = {}\nalways_on_top = {}\napp_history = {}\ntop_bar_stats = {}\nrun_in_background = {}\ncheck_updates = {}\n",
            self.theme,
            self.refresh,
            self.start_page,
            self.always_on_top,
            self.app_history,
            self.top_bar_stats,
            self.run_in_background,
            self.check_updates
        );
        fs::write(path, text)
    }
}

/// When updates were last checked (Unix seconds). Kept in the cache directory, not the
/// settings file, since it's bookkeeping rather than a choice the user made.
pub fn last_update_check() -> u64 {
    fs::read_to_string(cache_file()).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

pub fn set_last_update_check(when: u64) {
    let path = cache_file();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, when.to_string());
}

fn cache_file() -> PathBuf {
    let cache = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(cache).join("taskmonitor/last-update-check")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_defaults_and_clamping() {
        let s = Settings::parse("theme = 2\nrefresh = 99\napp_history = true\nunknown = x\n");
        assert_eq!(s, Settings { theme: 2, refresh: 4, app_history: true, ..Default::default() });
        assert_eq!(Settings::parse(""), Settings::default());
    }
}
