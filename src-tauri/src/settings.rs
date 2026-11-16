use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    pub channel_login: String,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    settings_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| format!("Écriture des paramètres impossible : {e}"))
}

#[tauri::command]
pub fn get_channel(app: AppHandle) -> String {
    load(&app).channel_login
}

#[tauri::command]
pub fn set_channel(app: AppHandle, login: String) -> Result<String, String> {
    // On nettoie : espaces, @ éventuel, minuscules.
    let cleaned = login.trim().trim_start_matches('@').to_lowercase();

    if cleaned.len() > 25
        || !cleaned.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("Pseudo invalide : lettres, chiffres et _ uniquement (25 max).".into());
    }

    let mut settings = load(&app);
    settings.channel_login = cleaned.clone();
    save(&app, &settings)?;

    // Si le bot est déjà connecté, on relance la lecture sur la nouvelle chaîne.
    let connected = {
        let state = app.state::<crate::AppState>();
        let c = state.twitch.lock().unwrap().is_some();
        c
    };
    if connected {
        crate::twitch::chat::start(&app);
    }

    Ok(cleaned)
}