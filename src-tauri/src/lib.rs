mod settings;
mod twitch;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager, State};

use twitch::auth::TwitchSession;

// L'état partagé de l'application.
pub struct AppState {
    pub feature_enabled: AtomicBool,
    pub twitch: Mutex<Option<TwitchSession>>,
    pub login_attempt: AtomicU64,
    pub chat_task: Mutex<Option<JoinHandle<()>>>,
    pub channel: Mutex<Option<String>>, // identifiant de la chaîne surveillée
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn set_feature(enabled: bool, state: State<'_, AppState>) {
    state.feature_enabled.store(enabled, Ordering::Relaxed);
    println!("Interrupteur mis sur : {}", enabled);
}

#[tauri::command]
fn get_feature(state: State<'_, AppState>) -> bool {
    state.feature_enabled.load(Ordering::Relaxed)
}

// Tâche de l'exercice de la phase 0 (compteur).
async fn ticker(app: AppHandle) {
    let mut counter: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        counter += 1;

        let state = app.state::<AppState>();
        if state.feature_enabled.load(Ordering::Relaxed) {
            let _ = app.emit("tick", counter);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            feature_enabled: AtomicBool::new(true),
            twitch: Mutex::new(None),
            login_attempt: AtomicU64::new(0),
            chat_task: Mutex::new(None),
            channel: Mutex::new(None),
        })
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(ticker(handle));

            let handle_twitch = app.handle().clone();
            tauri::async_runtime::spawn(twitch::auth::token_keeper(handle_twitch));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            set_feature,
            get_feature,
            twitch::auth::twitch_login_start,
            twitch::auth::twitch_status,
            twitch::auth::twitch_logout,
            twitch::auth::twitch_restore,
            settings::get_channel,
            settings::set_channel,
            twitch::chat::send_chat_message
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}