mod bot;
mod db;
mod diagnostics;
mod outbox;
mod twitch;

use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

use tauri::async_runtime::JoinHandle;
use tokio::sync::mpsc;

use twitch::auth::TwitchSession;
use twitch::Role;

// Les sessions des deux comptes.
#[derive(Default)]
pub struct Accounts {
    streamer: Option<TwitchSession>,
    bot: Option<TwitchSession>,
}

impl Accounts {
    fn get(&self, role: Role) -> Option<&TwitchSession> {
        match role {
            Role::Streamer => self.streamer.as_ref(),
            Role::Bot => self.bot.as_ref(),
        }
    }

    fn set(&mut self, role: Role, session: Option<TwitchSession>) {
        match role {
            Role::Streamer => self.streamer = session,
            Role::Bot => self.bot = session,
        }
    }

    fn take(&mut self, role: Role) -> Option<TwitchSession> {
        match role {
            Role::Streamer => self.streamer.take(),
            Role::Bot => self.bot.take(),
        }
    }
}

// L'état partagé de l'application.
pub struct AppState {
    pub accounts: Mutex<Accounts>,
    pub streamer_attempt: AtomicU64,
    pub bot_attempt: AtomicU64,
    pub chat_task: Mutex<Option<JoinHandle<()>>>,
    pub bot: bot::BotState,
    pub db: db::Db,
    pub outbox: mpsc::Sender<outbox::Outgoing>,
}

impl AppState {
    pub fn new(outbox: mpsc::Sender<outbox::Outgoing>) -> Self {
        Self {
            accounts: Mutex::new(Accounts::default()),
            streamer_attempt: AtomicU64::new(0),
            bot_attempt: AtomicU64::new(0),
            chat_task: Mutex::new(None),
            bot: bot::BotState::new(),
            db: db::Db::new(),
            outbox,
        }
    }

    // Compteur de tentatives de connexion de ce rôle (sert à annuler une attente).
    pub fn attempt(&self, role: Role) -> &AtomicU64 {
        match role {
            Role::Streamer => &self.streamer_attempt,
            Role::Bot => &self.bot_attempt,
        }
    }

    // Copie de la session d'un rôle (None si non connecté).
    pub fn session(&self, role: Role) -> Option<TwitchSession> {
        self.accounts.lock().unwrap().get(role).cloned()
    }

    pub fn user_id_of(&self, role: Role) -> Option<String> {
        self.accounts
            .lock()
            .unwrap()
            .get(role)
            .map(|s| s.user_id.clone())
    }

    pub fn set_session(&self, role: Role, session: Option<TwitchSession>) {
        self.accounts.lock().unwrap().set(role, session);
    }

    pub fn take_session(&self, role: Role) -> Option<TwitchSession> {
        self.accounts.lock().unwrap().take(role)
    }

    // Remplace la session seulement si le compte est toujours connecté.
    pub fn replace_session_if_present(&self, role: Role, session: TwitchSession) -> bool {
        let mut accounts = self.accounts.lock().unwrap();
        if accounts.get(role).is_some() {
            accounts.set(role, Some(session));
            true
        } else {
            false
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // File d'envoi des messages : l'émetteur va dans l'état, le récepteur à la tâche d'envoi.
    let (outbox_tx, outbox_rx) = mpsc::channel::<outbox::Outgoing>(100);

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::new(outbox_tx))
        .setup(move |app| {
            // Charge les réglages du bot enregistrés.
            bot::init(app.handle());
            db::init(app.handle());            

            tauri::async_runtime::spawn(db::flusher(app.handle().clone()));   
            tauri::async_runtime::spawn(bot::points::presence_loop(app.handle().clone()));         
            tauri::async_runtime::spawn(twitch::auth::token_keeper(app.handle().clone()));
            tauri::async_runtime::spawn(outbox::worker(app.handle().clone(), outbox_rx));
            tauri::async_runtime::spawn(bot::auto::scheduler(app.handle().clone()));
            tauri::async_runtime::spawn(bot::auto::live_watcher(app.handle().clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            twitch::auth::twitch_login_start,
            twitch::auth::twitch_logout,
            twitch::auth::twitch_restore,
            twitch::chat::send_chat_message,
            bot::bot_get_state,
            bot::bot_set_enabled,
            bot::bot_update_command,
            bot::bot_create_command,
            bot::bot_delete_command,
            bot::bot_counter_adjust,
            bot::bot_counter_reset,
            bot::bot_get_live,
            bot::bot_auto_set_settings,
            bot::bot_auto_create,
            bot::bot_auto_update,
            bot::bot_auto_delete,
            bot::bot_points_set_settings,
            db::db_list_viewers,
            diagnostics::open_logs_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}