//! Application settings and user interface preferences.
//!
//! Preferences are stored as one JSON document under a single key so that
//! export/import is a file copy and adding a preference needs no migration.

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::Db;

pub const PREFERENCES_KEY: &str = "ui.preferences";
pub const EMERGENCY_STOP_KEY: &str = "runtime.emergency_stop";
pub const ONBOARDING_KEY: &str = "runtime.onboarding_complete";
pub const TELEMETRY_KEY: &str = "privacy.telemetry_opt_in";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeMode {
    System,
    Light,
    Dark,
    HighContrast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Density {
    Comfortable,
    Cosy,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarPosition {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessagePresentation {
    Bubbles,
    Flat,
}

/// Font families the application ships. Arbitrary families are not accepted:
/// the value is an enumeration, not a CSS string, so a preferences file cannot
/// inject styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FontFamily {
    Sans,
    Serif,
    Mono,
    /// A face with wider letterforms, easier for some dyslexic readers.
    Humanist,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeMode,
    /// One of the accent tokens defined by the design system.
    pub accent: String,
    pub background: String,
    pub font_family: FontFamily,
    /// Base font size in pixels, clamped on write.
    pub font_size_px: u8,
    pub density: Density,
    pub sidebar_position: SidebarPosition,
    pub sidebar_width_px: u16,
    pub sidebar_collapsed: bool,
    /// Tabs the user has chosen to show, in order.
    pub visible_tabs: Vec<String>,
    pub chat_max_width_px: u16,
    pub message_presentation: MessagePresentation,
    pub reduced_motion: bool,
    pub show_inspector: bool,
    pub dashboard_widgets: Vec<String>,
    /// Named snapshots of this whole document, for quick switching.
    pub saved_layouts: Vec<SavedLayout>,
    pub sidebar_section_order: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedLayout {
    pub name: String,
    pub preferences: serde_json::Value,
}

/// Tabs the application knows about. Used to validate `visible_tabs` so an
/// imported preferences file cannot reference a screen that does not exist.
pub const KNOWN_TABS: &[&str] = &[
    "chat",
    "workspaces",
    "projects",
    "agents",
    "tasks",
    "knowledge",
    "connections",
    "marketplace",
    "activity",
    "settings",
];

/// Tabs that may never be hidden — without these the user could lock
/// themselves out of the controls that undo the hiding.
pub const REQUIRED_TABS: &[&str] = &["chat", "settings"];

pub const KNOWN_ACCENTS: &[&str] = &["signal", "ember", "verdant", "violet", "slate"];
pub const KNOWN_BACKGROUNDS: &[&str] = &["depth", "flat", "grid"];
pub const KNOWN_WIDGETS: &[&str] = &[
    "active-projects",
    "recent-chats",
    "connection-health",
    "pending-approvals",
    "knowledge-status",
    "budget-summary",
];

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            accent: "signal".into(),
            background: "depth".into(),
            font_family: FontFamily::Sans,
            font_size_px: 15,
            density: Density::Comfortable,
            sidebar_position: SidebarPosition::Left,
            sidebar_width_px: 280,
            sidebar_collapsed: false,
            visible_tabs: KNOWN_TABS.iter().map(|t| t.to_string()).collect(),
            chat_max_width_px: 820,
            message_presentation: MessagePresentation::Bubbles,
            reduced_motion: false,
            show_inspector: false,
            dashboard_widgets: vec![
                "active-projects".into(),
                "recent-chats".into(),
                "connection-health".into(),
                "pending-approvals".into(),
            ],
            saved_layouts: Vec::new(),
            sidebar_section_order: vec![
                "chats".into(),
                "offices".into(),
                "labs".into(),
                "boardrooms".into(),
                "think-tanks".into(),
                "projects".into(),
                "favorites".into(),
                "archived".into(),
            ],
        }
    }
}

impl Preferences {
    /// Clamp and filter anything out of range. Applied on every write and on
    /// every import, so a hand-edited file cannot produce an unusable UI.
    pub fn sanitised(mut self) -> Self {
        self.font_size_px = self.font_size_px.clamp(12, 24);
        self.sidebar_width_px = self.sidebar_width_px.clamp(200, 520);
        self.chat_max_width_px = self.chat_max_width_px.clamp(560, 1600);

        if !KNOWN_ACCENTS.contains(&self.accent.as_str()) {
            self.accent = "signal".into();
        }
        if !KNOWN_BACKGROUNDS.contains(&self.background.as_str()) {
            self.background = "depth".into();
        }

        self.visible_tabs
            .retain(|tab| KNOWN_TABS.contains(&tab.as_str()));
        self.visible_tabs.dedup();
        for required in REQUIRED_TABS {
            if !self.visible_tabs.iter().any(|t| t == required) {
                self.visible_tabs.push((*required).to_string());
            }
        }

        self.dashboard_widgets
            .retain(|w| KNOWN_WIDGETS.contains(&w.as_str()));
        self.dashboard_widgets.dedup();

        self.saved_layouts.truncate(12);
        self.saved_layouts.retain(|l| !l.name.trim().is_empty());
        self
    }
}

pub struct SettingsRepo<'a> {
    db: &'a Db,
}

impl<'a> SettingsRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn get_raw(&self, key: &str) -> Result<Option<String>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn set_raw(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.db.conn()?;
        conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![key, value, otwono_types::ids::format_ts(&otwono_types::now())],
        )?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn get_bool(&self, key: &str, default: bool) -> Result<bool> {
        Ok(self
            .get_raw(key)?
            .map(|v| v == "true" || v == "1")
            .unwrap_or(default))
    }

    pub fn set_bool(&self, key: &str, value: bool) -> Result<()> {
        self.set_raw(key, if value { "true" } else { "false" })
    }

    /// Preferences, falling back to defaults when unset or unreadable. A
    /// corrupt preferences row must never stop the application starting.
    pub fn preferences(&self) -> Result<Preferences> {
        match self.get_raw(PREFERENCES_KEY)? {
            Some(text) => match serde_json::from_str::<Preferences>(&text) {
                Ok(prefs) => Ok(prefs.sanitised()),
                Err(error) => {
                    tracing::warn!(%error, "preferences were unreadable; falling back to defaults");
                    Ok(Preferences::default())
                }
            },
            None => Ok(Preferences::default()),
        }
    }

    pub fn set_preferences(&self, prefs: Preferences) -> Result<Preferences> {
        let sanitised = prefs.sanitised();
        let text = serde_json::to_string(&sanitised).context("serialising preferences")?;
        self.set_raw(PREFERENCES_KEY, &text)?;
        Ok(sanitised)
    }

    /// The global emergency stop. While engaged, no capability check passes.
    pub fn emergency_stop(&self) -> Result<bool> {
        self.get_bool(EMERGENCY_STOP_KEY, false)
    }

    pub fn set_emergency_stop(&self, engaged: bool) -> Result<()> {
        self.set_bool(EMERGENCY_STOP_KEY, engaged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn defaults_are_returned_when_nothing_is_stored() {
        let db = repo_db();
        let prefs = SettingsRepo::new(&db).preferences().unwrap();
        assert_eq!(prefs, Preferences::default());
        assert_eq!(prefs.theme, ThemeMode::System);
        assert!(prefs.visible_tabs.contains(&"chat".to_string()));
    }

    #[test]
    fn preferences_persist_and_reload() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        let mut prefs = Preferences::default();
        prefs.theme = ThemeMode::HighContrast;
        prefs.accent = "ember".into();
        prefs.font_size_px = 19;
        prefs.sidebar_collapsed = true;
        repo.set_preferences(prefs.clone()).unwrap();

        let reloaded = repo.preferences().unwrap();
        assert_eq!(reloaded.theme, ThemeMode::HighContrast);
        assert_eq!(reloaded.accent, "ember");
        assert_eq!(reloaded.font_size_px, 19);
        assert!(reloaded.sidebar_collapsed);
    }

    #[test]
    fn out_of_range_values_are_clamped_rather_than_rejected() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        let mut prefs = Preferences::default();
        prefs.font_size_px = 200;
        prefs.sidebar_width_px = 5;
        prefs.chat_max_width_px = 9_000;
        let saved = repo.set_preferences(prefs).unwrap();
        assert_eq!(saved.font_size_px, 24);
        assert_eq!(saved.sidebar_width_px, 200);
        assert_eq!(saved.chat_max_width_px, 1600);
    }

    #[test]
    fn unknown_tokens_fall_back_to_shipped_ones() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        let mut prefs = Preferences::default();
        prefs.accent = "url(javascript:alert(1))".into();
        prefs.background = "../../etc/passwd".into();
        prefs.visible_tabs = vec!["chat".into(), "not-a-tab".into()];
        prefs.dashboard_widgets = vec!["active-projects".into(), "evil-widget".into()];
        let saved = repo.set_preferences(prefs).unwrap();
        assert_eq!(saved.accent, "signal");
        assert_eq!(saved.background, "depth");
        assert!(!saved.visible_tabs.contains(&"not-a-tab".to_string()));
        assert_eq!(saved.dashboard_widgets, vec!["active-projects".to_string()]);
    }

    #[test]
    fn the_user_cannot_hide_the_tabs_that_would_lock_them_out() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        let mut prefs = Preferences::default();
        prefs.visible_tabs = vec!["agents".into()];
        let saved = repo.set_preferences(prefs).unwrap();
        for required in REQUIRED_TABS {
            assert!(
                saved.visible_tabs.iter().any(|t| t == required),
                "{required} must remain reachable"
            );
        }
    }

    #[test]
    fn corrupt_preferences_do_not_stop_the_application() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        repo.set_raw(PREFERENCES_KEY, "{ this is not json").unwrap();
        assert_eq!(repo.preferences().unwrap(), Preferences::default());
    }

    #[test]
    fn reset_to_default_is_a_delete() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        let mut prefs = Preferences::default();
        prefs.accent = "violet".into();
        repo.set_preferences(prefs).unwrap();
        repo.delete(PREFERENCES_KEY).unwrap();
        assert_eq!(repo.preferences().unwrap(), Preferences::default());
    }

    #[test]
    fn the_emergency_stop_defaults_to_disengaged_and_persists() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        assert!(!repo.emergency_stop().unwrap());
        repo.set_emergency_stop(true).unwrap();
        assert!(repo.emergency_stop().unwrap());
        repo.set_emergency_stop(false).unwrap();
        assert!(!repo.emergency_stop().unwrap());
    }

    #[test]
    fn telemetry_is_off_unless_explicitly_enabled() {
        let db = repo_db();
        let repo = SettingsRepo::new(&db);
        assert!(!repo.get_bool(TELEMETRY_KEY, false).unwrap());
    }
}
