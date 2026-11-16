use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

mod twitch;
use std::sync::Mutex;
use twitch::auth::TwitchSession;

// L'état partagé : une seule valeur pour l'instant, l'interrupteur.
pub struct AppState {
    pub feature_enabled: AtomicBool,
    pub twitch: Mutex<Option<TwitchSession>>,
    pub login_attempt: AtomicU64,
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

// Appelée par l'interface pour changer l'interrupteur.
#[tauri::command]
fn set_feature(enabled: bool, state: State<'_, AppState>) {
    state.feature_enabled.store(enabled, Ordering::Relaxed);
    println!("Interrupteur mis sur : {}", enabled);
}

// Appelée par l'interface pour connaître l'état actuel.
#[tauri::command]
fn get_feature(state: State<'_, AppState>) -> bool {
    state.feature_enabled.load(Ordering::Relaxed)
}

// La tâche d'arrière-plan : elle tourne en boucle, indépendamment de la fenêtre.
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
        })
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(ticker(handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![greet,set_feature, get_feature, twitch::auth::twitch_login_start, twitch::auth::twitch_status, twitch::auth::twitch_logout])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}