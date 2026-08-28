//! Interface preferences, and their export and import.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_store::repo::settings::{
    Preferences, SettingsRepo, KNOWN_ACCENTS, KNOWN_BACKGROUNDS, KNOWN_TABS, KNOWN_WIDGETS,
    REQUIRED_TABS,
};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct PreferencesResponse {
    pub preferences: Preferences,
    /// What the interface offers, so the settings screen is driven by the
    /// service rather than by a duplicated list in the client.
    pub options: PreferenceOptions,
}

#[derive(Debug, Serialize)]
pub struct PreferenceOptions {
    pub themes: Vec<&'static str>,
    pub accents: Vec<&'static str>,
    pub backgrounds: Vec<&'static str>,
    pub fonts: Vec<&'static str>,
    pub densities: Vec<&'static str>,
    pub tabs: Vec<&'static str>,
    pub required_tabs: Vec<&'static str>,
    pub widgets: Vec<&'static str>,
    pub font_size_range: (u8, u8),
    pub sidebar_width_range: (u16, u16),
    pub chat_width_range: (u16, u16),
}

fn options() -> PreferenceOptions {
    PreferenceOptions {
        themes: vec!["system", "light", "dark", "high-contrast"],
        accents: KNOWN_ACCENTS.to_vec(),
        backgrounds: KNOWN_BACKGROUNDS.to_vec(),
        fonts: vec!["sans", "serif", "mono", "humanist"],
        densities: vec!["comfortable", "cosy", "compact"],
        tabs: KNOWN_TABS.to_vec(),
        required_tabs: REQUIRED_TABS.to_vec(),
        widgets: KNOWN_WIDGETS.to_vec(),
        font_size_range: (12, 24),
        sidebar_width_range: (200, 520),
        chat_width_range: (560, 1600),
    }
}

pub async fn get(State(state): State<AppState>) -> ApiResult<Json<PreferencesResponse>> {
    Ok(Json(PreferencesResponse {
        preferences: SettingsRepo::new(&state.db).preferences()?,
        options: options(),
    }))
}

pub async fn put(
    State(state): State<AppState>,
    Json(preferences): Json<Preferences>,
) -> ApiResult<Json<PreferencesResponse>> {
    let saved = SettingsRepo::new(&state.db).set_preferences(preferences)?;
    Ok(Json(PreferencesResponse {
        preferences: saved,
        options: options(),
    }))
}

pub async fn reset(State(state): State<AppState>) -> ApiResult<Json<PreferencesResponse>> {
    let repo = SettingsRepo::new(&state.db);
    repo.delete(otwono_store::repo::settings::PREFERENCES_KEY)?;
    Ok(Json(PreferencesResponse {
        preferences: repo.preferences()?,
        options: options(),
    }))
}

/// The portable form of a user's settings.
#[derive(Debug, Serialize, Deserialize)]
pub struct SettingsExport {
    pub schema_version: u32,
    pub kind: String,
    pub exported_at: String,
    pub app_version: String,
    pub preferences: Preferences,
}

pub const SETTINGS_EXPORT_KIND: &str = "otwono.settings";

pub async fn export(State(state): State<AppState>) -> ApiResult<Json<SettingsExport>> {
    Ok(Json(SettingsExport {
        schema_version: otwono_types::PACKAGE_SCHEMA_VERSION,
        kind: SETTINGS_EXPORT_KIND.to_string(),
        exported_at: otwono_types::ids::format_ts(&otwono_types::now()),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        preferences: SettingsRepo::new(&state.db).preferences()?,
    }))
}

pub async fn import(
    State(state): State<AppState>,
    Json(body): Json<SettingsExport>,
) -> ApiResult<Json<PreferencesResponse>> {
    if body.kind != SETTINGS_EXPORT_KIND {
        return Err(ApiError::BadRequest(format!(
            "That file is not an OTWONO settings export (it says it is {:?}).",
            body.kind
        )));
    }
    if body.schema_version > otwono_types::PACKAGE_SCHEMA_VERSION {
        return Err(ApiError::BadRequest(format!(
            "That settings file was written by a newer version of OTWONO (format {}, this build \
             understands {}). Update OTWONO and try again.",
            body.schema_version,
            otwono_types::PACKAGE_SCHEMA_VERSION
        )));
    }
    // `set_preferences` sanitises, so an edited file cannot produce an
    // unusable interface.
    let saved = SettingsRepo::new(&state.db).set_preferences(body.preferences)?;
    Ok(Json(PreferencesResponse {
        preferences: saved,
        options: options(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_store::repo::settings::ThemeMode;

    #[tokio::test]
    async fn preferences_are_returned_with_the_options_that_drive_the_screen() {
        let state = AppState::for_tests();
        let Json(response) = get(State(state)).await.unwrap();
        assert_eq!(response.preferences.theme, ThemeMode::System);
        assert!(response.options.themes.contains(&"high-contrast"));
        assert!(response.options.required_tabs.contains(&"settings"));
        assert_eq!(response.options.font_size_range, (12, 24));
    }

    #[tokio::test]
    async fn saving_preferences_sanitises_them() {
        let state = AppState::for_tests();
        let mut preferences = Preferences::default();
        preferences.font_size_px = 200;
        preferences.accent = "not-a-token".into();

        let Json(response) = put(State(state), Json(preferences)).await.unwrap();
        assert_eq!(response.preferences.font_size_px, 24);
        assert_eq!(response.preferences.accent, "signal");
    }

    #[tokio::test]
    async fn reset_restores_the_shipped_defaults() {
        let state = AppState::for_tests();
        let mut preferences = Preferences::default();
        preferences.theme = ThemeMode::Dark;
        let _ = put(State(state.clone()), Json(preferences)).await.unwrap();

        let Json(response) = reset(State(state)).await.unwrap();
        assert_eq!(response.preferences, Preferences::default());
    }

    #[tokio::test]
    async fn settings_round_trip_through_export_and_import() {
        let state = AppState::for_tests();
        let mut preferences = Preferences::default();
        preferences.theme = ThemeMode::HighContrast;
        preferences.accent = "ember".into();
        preferences.sidebar_collapsed = true;
        let _ = put(State(state.clone()), Json(preferences)).await.unwrap();

        let Json(exported) = export(State(state.clone())).await.unwrap();
        assert_eq!(exported.kind, SETTINGS_EXPORT_KIND);

        let _ = reset(State(state.clone())).await.unwrap();
        let Json(response) = import(State(state), Json(exported)).await.unwrap();
        assert_eq!(response.preferences.theme, ThemeMode::HighContrast);
        assert_eq!(response.preferences.accent, "ember");
        assert!(response.preferences.sidebar_collapsed);
    }

    #[tokio::test]
    async fn importing_the_wrong_kind_of_file_says_so() {
        let state = AppState::for_tests();
        let Json(mut exported) = export(State(state.clone())).await.unwrap();
        exported.kind = "otwono.agent".into();
        let error = import(State(state), Json(exported)).await.unwrap_err();
        assert!(
            matches!(error, ApiError::BadRequest(ref m) if m.contains("not an OTWONO settings export"))
        );
    }

    #[tokio::test]
    async fn importing_a_newer_format_says_to_update_rather_than_guessing() {
        let state = AppState::for_tests();
        let Json(mut exported) = export(State(state.clone())).await.unwrap();
        exported.schema_version = otwono_types::PACKAGE_SCHEMA_VERSION + 1;
        let error = import(State(state), Json(exported)).await.unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m) if m.contains("Update OTWONO")));
    }
}
