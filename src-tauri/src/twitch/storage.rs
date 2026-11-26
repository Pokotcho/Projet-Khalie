use keyring::Entry;

use super::auth::TwitchSession;
use super::Role;

const SERVICE: &str = "layka-twitch";

// Une entrée du trousseau par rôle : twitch-streamer et twitch-bot.
fn entry(role: Role) -> Result<Entry, String> {
    let account = format!("twitch-{}", role.key());
    Entry::new(SERVICE, &account).map_err(|e| format!("Trousseau indisponible : {e}"))
}

pub fn save(role: Role, session: &TwitchSession) -> Result<(), String> {
    let json = serde_json::to_string(session).map_err(|e| e.to_string())?;
    entry(role)?
        .set_password(&json)
        .map_err(|e| format!("Écriture dans le trousseau impossible : {e}"))
}

pub fn load(role: Role) -> Option<TwitchSession> {
    let json = entry(role).ok()?.get_password().ok()?;
    serde_json::from_str(&json).ok()
}

pub fn clear(role: Role) {
    if let Ok(e) = entry(role) {
        let _ = e.delete_credential();
    }
}

// Secrets divers (ex. : token du relais Throne), un par nom.
fn secret_entry(name: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, name).map_err(|e| format!("Trousseau indisponible : {e}"))
}

pub fn save_secret(name: &str, value: &str) -> Result<(), String> {
    secret_entry(name)?
        .set_password(value)
        .map_err(|e| format!("Écriture dans le trousseau impossible : {e}"))
}

pub fn load_secret(name: &str) -> Option<String> {
    secret_entry(name).ok()?.get_password().ok()
}

pub fn clear_secret(name: &str) {
    if let Ok(e) = secret_entry(name) {
        let _ = e.delete_credential();
    }
}