pub mod engine;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::twitch::Role;
use crate::AppState;

// Qui a le droit d'utiliser une commande.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Everyone,
    Moderator, // modérateurs et streamer
    Streamer,
}

// Comportement de la commande (d'autres s'ajouteront : compteur, followage, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text, // répond simplement avec le texte
    List, // !commandes : insère la liste des commandes pour tous
}

fn default_reply_as() -> Role {
    Role::Bot
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub name: String, // sans le "!", en minuscules
    pub enabled: bool,
    pub response: String,
    pub level: Level,
    pub global_cooldown: u32, // secondes
    pub user_cooldown: u32,   // secondes
    #[serde(default = "default_reply_as")]
    pub reply_as: Role,
    pub builtin: bool, // fournie par l'application : non supprimable
    pub kind: Kind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub enabled: bool, // interrupteur général
    pub commands: Vec<Command>,
}

fn builtin(name: &str, kind: Kind, response: &str, global: u32, user: u32) -> Command {
    Command {
        name: name.into(),
        enabled: true,
        response: response.into(),
        level: Level::Everyone,
        global_cooldown: global,
        user_cooldown: user,
        reply_as: Role::Bot,
        builtin: true,
        kind,
    }
}

impl BotConfig {
    fn defaults() -> Vec<Command> {
        vec![
            builtin(
                "discord",
                Kind::Text,
                "[@pseudo] Rejoins la communauté sur Discord : (lien à renseigner dans l'onglet Commandes)",
                10,
                30,
            ),
            builtin(
                "youtube",
                Kind::Text,
                "[@pseudo] Retrouve la chaîne YouTube ici : (lien à renseigner)",
                10,
                30,
            ),
            builtin(
                "reseaux",
                Kind::Text,
                "[@pseudo] Retrouve tous les réseaux ici : (liens à renseigner)",
                10,
                30,
            ),
            builtin(
                "planning",
                Kind::Text,
                "[@pseudo] Le planning des prochains streams : (à renseigner)",
                10,
                30,
            ),
            builtin(
                "lurk",
                Kind::Text,
                "[@pseudo] passe en mode lurk, merci pour le soutien et à tout à l'heure !",
                0,
                30,
            ),
            builtin(
                "commandes",
                Kind::List,
                "[@pseudo] Commandes disponibles : [commandes]",
                10,
                30,
            ),
        ]
    }
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            commands: Self::defaults(),
        }
    }
}

// État du bot : réglages + mémoire des délais.
pub struct BotState {
    pub config: RwLock<BotConfig>,
    pub client: reqwest::Client,
    global_last: Mutex<HashMap<String, Instant>>,
    user_last: Mutex<HashMap<(String, String), Instant>>,
}

impl BotState {
    pub fn new() -> Self {
        Self {
            config: RwLock::new(BotConfig::default()),
            client: reqwest::Client::new(),
            global_last: Mutex::new(HashMap::new()),
            user_last: Mutex::new(HashMap::new()),
        }
    }

    // Vérifie les délais. Si la commande peut être utilisée, enregistre l'usage et renvoie true.
    pub fn try_use(
        &self,
        name: &str,
        user_id: &str,
        global_cd: u32,
        user_cd: u32,
        bypass: bool,
    ) -> bool {
        let now = Instant::now();
        let mut global = self.global_last.lock().unwrap();
        let mut users = self.user_last.lock().unwrap();
        let user_key = (name.to_string(), user_id.to_string());

        if !bypass {
            if let Some(last) = global.get(name) {
                if now.duration_since(*last) < Duration::from_secs(global_cd as u64) {
                    return false;
                }
            }
            if let Some(last) = users.get(&user_key) {
                if now.duration_since(*last) < Duration::from_secs(user_cd as u64) {
                    return false;
                }
            }
        }

        global.insert(name.to_string(), now);
        users.insert(user_key, now);
        true
    }
}

// --- Fichier de configuration ---

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("bot.json"))
}

fn load_config(app: &AppHandle) -> BotConfig {
    let mut config: BotConfig = config_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Ajoute les commandes fournies par l'application qui manqueraient (nouvelles versions).
    for def in BotConfig::defaults() {
        if !config.commands.iter().any(|c| c.name == def.name) {
            config.commands.push(def);
        }
    }
    config
}

fn save_config(app: &AppHandle, config: &BotConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| format!("Écriture de bot.json impossible : {e}"))
}

// Appelée au démarrage : charge les réglages enregistrés.
pub fn init(app: &AppHandle) {
    let config = load_config(app);
    let state = app.state::<AppState>();
    *state.bot.config.write().unwrap() = config;
}

fn persist(app: &AppHandle) -> Result<(), String> {
    let config = {
        let state = app.state::<AppState>();
        let c = state.bot.config.read().unwrap().clone();
        c
    };
    save_config(app, &config)
}

// Envoie une ligne au journal de l'interface.
pub fn log(app: &AppHandle, message: impl Into<String>) {
    let _ = app.emit("bot-log", message.into());
}

// --- Validation ---

fn validate_name(name: &str) -> Result<String, String> {
    let n = name.trim().trim_start_matches('!').to_lowercase();
    if n.is_empty() || n.chars().count() > 25 || !n.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return Err("Nom invalide : lettres, chiffres et _ uniquement (25 caractères max).".into());
    }
    Ok(n)
}

fn clean_response(response: &str) -> Result<String, String> {
    let r = response.trim();
    if r.is_empty() {
        return Err("La réponse ne peut pas être vide.".into());
    }
    if r.chars().count() > 500 {
        return Err("Réponse trop longue (500 caractères maximum).".into());
    }
    Ok(r.to_string())
}

// --- Commandes appelées par l'interface ---

#[tauri::command]
pub fn bot_get_state(state: State<'_, AppState>) -> BotConfig {
    let config = state.bot.config.read().unwrap().clone();
    config
}

#[tauri::command]
pub fn bot_set_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        state.bot.config.write().unwrap().enabled = enabled;
    }
    persist(&app)
}

// Met à jour une commande existante (nom, type et statut "fournie par l'application" ne changent pas).
#[tauri::command]
pub fn bot_update_command(app: AppHandle, command: Command) -> Result<(), String> {
    let response = clean_response(&command.response)?;
    {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        let existing = config
            .commands
            .iter_mut()
            .find(|c| c.name == command.name)
            .ok_or("Commande introuvable.")?;

        existing.enabled = command.enabled;
        existing.response = response;
        existing.level = command.level;
        existing.global_cooldown = command.global_cooldown;
        existing.user_cooldown = command.user_cooldown;
        existing.reply_as = command.reply_as;
    }
    persist(&app)
}

// Crée une commande simple (réponse seulement).
#[tauri::command]
pub fn bot_create_command(app: AppHandle, command: Command) -> Result<(), String> {
    let name = validate_name(&command.name)?;
    let response = clean_response(&command.response)?;
    {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        if config.commands.iter().any(|c| c.name == name) {
            return Err(format!("La commande !{name} existe déjà."));
        }
        config.commands.push(Command {
            name,
            enabled: true,
            response,
            level: command.level,
            global_cooldown: command.global_cooldown,
            user_cooldown: command.user_cooldown,
            reply_as: command.reply_as,
            builtin: false,
            kind: Kind::Text,
        });
    }
    persist(&app)
}

// Supprime une commande créée par l'utilisateur (pas celles fournies par l'application).
#[tauri::command]
pub fn bot_delete_command(app: AppHandle, name: String) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        let before = config.commands.len();
        config.commands.retain(|c| c.name != name || c.builtin);
        if config.commands.len() == before {
            return Err(
                "Impossible de supprimer cette commande (introuvable ou fournie par l'application)."
                    .into(),
            );
        }
    }
    persist(&app)
}