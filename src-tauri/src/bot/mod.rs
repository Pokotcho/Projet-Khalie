pub mod auto;
pub mod engine;
pub mod points;
pub mod events;
pub mod followers;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

// Comportement de la commande.
// Comportement de la commande.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Text,         // répond simplement avec le texte
    List,         // !commandes : insère la liste des commandes pour tous
    Counter,      // augmente le compteur de 1
    #[serde(alias = "counterreset")]
    CounterReset, // remet le compteur à zéro
    Title,        // !titre : change le titre du stream (compte du streamer)
    Category,     // !categorie : change la catégorie (compte du streamer)
    Shoutout,     // !so : shoutout officiel Twitch (compte du streamer)
    Clip,         // !clip : crée un clip (compte du streamer)
    Followage,    // !followage : depuis quand le viewer suit la chaîne
    Follow,       // !follow : premier et dernier follow du viewer
    Give,         // !give 50 @pseudo : donne des points (modérateurs)
    Take,         // !take 50 @pseudo : retire des points (modérateurs)
    Streak,       // !streak : série en cours
    BestStreak,   // !beststreak : meilleure série
    Points,       // !points : solde de points
    PointsList,   // !commandesPoints : liste des commandes liées aux points
}

// Certaines commandes ne peuvent pas être ouvertes à tout le monde.
fn level_allowed(kind: Kind, level: Level) -> bool {
    match kind {
        Kind::CounterReset | Kind::Title | Kind::Category | Kind::Shoutout | Kind::Give | Kind::Take => {
            level != Level::Everyone
        }
        _ => true,
    }
}

// Ces commandes agissent en silence : pas de texte de réponse à renseigner.
fn needs_response(kind: Kind) -> bool {
    !matches!(kind, Kind::Title | Kind::Category | Kind::Shoutout)
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
    #[serde(default)]
    pub cost: u32, // coût en points (0 = gratuite)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub enabled: bool, // interrupteur général
    pub commands: Vec<Command>,
    #[serde(default)]
    pub counter: u64, // valeur du compteur
    #[serde(default)]
    pub auto: AutoSettings, // réglages des messages automatiques
    #[serde(default)]
    pub auto_messages: Vec<AutoMessage>,
    #[serde(default)]
    pub points: PointsSettings, // réglages des points de présence
    #[serde(default)]
    pub events: EventSettings, // messages déclenchés par des événements
}

// Réglages communs à tous les messages automatiques.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSettings {
    pub enabled: bool,        // interrupteur général des messages automatiques
    pub only_when_live: bool, // n'envoyer que quand le stream est en direct
    pub min_chat_lines: u32,  // messages minimum dans le chat depuis le dernier message auto
    pub min_gap_min: u32,     // minutes minimum entre deux messages auto
}

impl Default for AutoSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            only_when_live: true,
            min_chat_lines: 5,
            min_gap_min: 2,
        }
    }
}

// Un message envoyé automatiquement à intervalle régulier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoMessage {
    pub id: u64,
    pub name: String, // pour se repérer dans la liste
    pub enabled: bool,
    pub text: String,
    pub interval_min: u32, // minutes entre deux envois de CE message
    #[serde(default = "default_reply_as")]
    pub reply_as: Role,
}

// Réglages des points de présence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PointsSettings {
    pub enabled: bool,
    pub name: String,         // nom des points (ex. : « Khaliecoins »)
    pub interval_min: u32,    // minutes entre deux distributions
    pub amount: u32,          // points donnés à chaque distribution
    pub excluded: Vec<String>, // pseudos (minuscules) qui ne gagnent rien
    pub streak_every: u32,     // bonus tous les X streams d'affilée (0 = désactivé)
    pub streak_bonus: u32,     // points donnés à chaque palier
    pub stream_gap_min: u32,   // deux directs séparés de moins de X minutes = un seul stream
    pub insufficient_message: String, // réponse quand le solde ne suffit pas pour une commande
}

impl Default for PointsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            name: "points".into(),
            interval_min: 5,
            amount: 10,
            excluded: ["nightbot", "streamelements", "streamlabs", "moobot", "fossabot", "wizebot"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            streak_every: 5,
            streak_bonus: 100,
            stream_gap_min: 120,
            insufficient_message:
                "[@pseudo] il te faut [cout] [points] pour cette commande (tu en as [solde])."
                    .into(),
        }
    }
}

// Réglages des messages déclenchés par des événements (nouveau follower…).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EventSettings {
    pub follow_enabled: bool,    // message de bienvenue aux nouveaux followers
    pub only_when_live: bool,    // seulement quand le stream est en direct
    pub follow_message: String,  // [@pseudo] et [pseudo] = le nouveau follower
    pub reply_as: Role,
}

impl Default for EventSettings {
    fn default() -> Self {
        Self {
            follow_enabled: true,
            only_when_live: false,
            follow_message: "Merci pour le follow [@pseudo] ! Bienvenue dans la communauté 💜"
                .into(),
            reply_as: Role::Bot,
        }
    }
}

// Une commande est « liée aux points » si c'est l'une des commandes de points ou si elle a un coût.
fn is_points_command(c: &Command) -> bool {
    c.cost > 0
        || matches!(
            c.kind,
            Kind::Follow
                | Kind::Give
                | Kind::Take
                | Kind::Streak
                | Kind::BestStreak
                | Kind::Points
                | Kind::PointsList
        )
}

fn builtin(
    name: &str,
    kind: Kind,
    level: Level,
    response: &str,
    global: u32,
    user: u32,
) -> Command {
    Command {
        name: name.into(),
        enabled: true,
        response: response.into(),
        level,
        global_cooldown: global,
        user_cooldown: user,
        reply_as: Role::Bot,
        builtin: true,
        kind,
        cost: 0,
    }
}

impl BotConfig {
    fn defaults() -> Vec<Command> {
        vec![
            builtin(
                "discord",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] Rejoins la communauté sur Discord : (lien à renseigner)",
                10,
                30,
            ),
            builtin(
                "youtube",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] Retrouve la chaîne YouTube ici : (lien à renseigner)",
                10,
                30,
            ),
            builtin(
                "instagram",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] Retrouve la chaîne Instagram ici : (lien à renseigner)",
                10,
                30,
            ),
            builtin(
                "tiktok",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] Retrouve la chaîne TikTok ici : (lien à renseigner)",
                10,
                30,
            ),
            builtin(
                "reseaux",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] Retrouve tous les réseaux ici : (liens à renseigner)",
                10,
                30,
            ),
            builtin(
                "planning",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] Le planning des prochains streams : (à renseigner)",
                10,
                30,
            ),
            builtin(
                "lurk",
                Kind::Text,
                Level::Everyone,
                "[@pseudo] passe en mode lurk, merci pour le soutien et à tout à l'heure !",
                0,
                30,
            ),
            builtin(
                "commandes",
                Kind::List,
                Level::Everyone,
                "[@pseudo] Commandes disponibles : [commandes]",
                10,
                30,
            ),
            builtin(
                "deathcounter",
                Kind::Counter,
                Level::Everyone,
                "Oups n°[compteur] !",
                10,
                30,
            ),
            builtin(
                "reset",
                Kind::CounterReset,
                Level::Moderator,
                "[@pseudo] Compteur remis à zéro.",
                0,
                0,
            ),

            builtin("titre", Kind::Title, Level::Moderator, "", 0, 0),

            builtin("categorie", Kind::Category, Level::Moderator, "", 0, 0),

            builtin("so", Kind::Shoutout, Level::Moderator, "", 0, 0),

            builtin(
                "clip",
                Kind::Clip,
                Level::Moderator,
                "[@pseudo] Clip créé : [lien]",
                30,
                60,
            ),

            builtin(
                "follow",
                Kind::Follow,
                Level::Everyone,
                "[@pseudo] premier follow : [premier] — dernier follow : [dernier]",
                5,
                15,
            ),
            builtin(
                "give",
                Kind::Give,
                Level::Moderator,
                "[@pseudo] a donné [montant] [points] à [cible].",
                0,
                0,
            ),
            builtin(
                "take",
                Kind::Take,
                Level::Moderator,
                "[@pseudo] a retiré [montant] [points] à [cible].",
                0,
                0,
            ),
            builtin(
                "streak",
                Kind::Streak,
                Level::Everyone,
                "[cible] est sur une série de [serie] stream(s) d'affilée.",
                5,
                15,
            ),
            builtin(
                "beststreak",
                Kind::BestStreak,
                Level::Everyone,
                "Meilleure série de [cible] : [meilleure_serie] stream(s) d'affilée.",
                5,
                15,
            ),
            builtin(
                "points",
                Kind::Points,
                Level::Everyone,
                "[cible] a [solde] [points].",
                5,
                15,
            ),
            builtin(
                "commandespoints",
                Kind::PointsList,
                Level::Everyone,
                "[@pseudo] Commandes liées aux points : [commandes]",
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
            counter: 0,
            auto: AutoSettings::default(),
            auto_messages: Vec::new(),
            points: PointsSettings::default(),   
            events: EventSettings::default(),         
        }
    }
}

// État du bot : réglages + mémoire des délais.
pub struct BotState {
    pub config: RwLock<BotConfig>,
    pub client: reqwest::Client,
    global_last: Mutex<HashMap<String, Instant>>,
    user_last: Mutex<HashMap<(String, String), Instant>>,
    pub live: AtomicBool,     // la chaîne est-elle en direct ?
    pub chat_lines: AtomicU64, // nombre de messages vus dans le chat depuis le démarrage
}

impl BotState {
    pub fn new() -> Self {
        Self {
            config: RwLock::new(BotConfig::default()),
            client: reqwest::Client::new(),
            global_last: Mutex::new(HashMap::new()),
            user_last: Mutex::new(HashMap::new()),
            live: AtomicBool::new(false),
            chat_lines: AtomicU64::new(0),
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

// --- Fichiers de configuration ---

// Nombre de versions de bot.json conservées dans le dossier "backups".
const KEEP_VERSIONS: usize = 10;

// Le compteur est enregistré à part (counter.json), pour ne pas créer une nouvelle
// version de bot.json à chaque mise à jour du compteur.
#[derive(Serialize, Deserialize)]
struct CounterFile {
    value: u64,
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("bot.json"))
}

fn counter_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("counter.json"))
}

fn backups_dir(config_file: &std::path::Path) -> PathBuf {
    config_file.with_file_name("backups")
}

// Écrit un fichier de façon sûre : fichier temporaire, puis renommage d'un seul coup.
// On n'obtient jamais un fichier à moitié écrit.
fn write_atomically(path: &std::path::Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content).map_err(|e| format!("Écriture impossible : {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("Remplacement impossible : {e}"))
}

// Lit et décode bot.json (None s'il est absent ou illisible).
fn read_config_file(path: &std::path::Path) -> Option<BotConfig> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// Contenu de référence d'une configuration, SANS le compteur (enregistré à part).
// Sert à écrire bot.json et à savoir si les réglages ont réellement changé.
fn canonical_json(config: &BotConfig) -> String {
    let mut copy = config.clone();
    copy.counter = 0;
    serde_json::to_string_pretty(&copy).unwrap_or_default()
}

// Met de côté un fichier abîmé, pour ne rien détruire.
fn set_aside(path: &std::path::Path) {
    let aside = path.with_extension(format!("json.corrompu-{}", chrono::Utc::now().timestamp()));
    match fs::rename(path, &aside) {
        Ok(()) => ::log::warn!("bot.json illisible : mis de côté dans {}", aside.display()),
        Err(e) => ::log::warn!("bot.json illisible, mise de côté impossible : {e}"),
    }
}

// Liste les versions sauvegardées, de la plus récente à la plus ancienne.
fn list_versions(config_file: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(backups_dir(config_file))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |x| x == "json"))
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();
    // Les noms contiennent la date : l'ordre alphabétique est l'ordre chronologique.
    files.sort();
    files.reverse();
    files
}

// Garde l'état actuel de bot.json comme nouvelle version, s'il est valide et différent
// de la dernière version enregistrée. Ne conserve que les KEEP_VERSIONS plus récentes.
fn keep_version(config_file: &std::path::Path) {
    // Jamais de copie d'un fichier abîmé.
    let Some(current) = read_config_file(config_file) else {
        return;
    };
    let current_json = canonical_json(&current);

    // Identique à la dernière version : rien à ajouter.
    let versions = list_versions(config_file);
    if let Some(newest) = versions.first().and_then(|p| read_config_file(p)) {
        if canonical_json(&newest) == current_json {
            return;
        }
    }

    let dir = backups_dir(config_file);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S%3f");
    let target = dir.join(format!("bot-{stamp}.json"));
    if fs::write(&target, &current_json).is_err() {
        return;
    }

    // On ne garde que les plus récentes.
    for old in list_versions(config_file).into_iter().skip(KEEP_VERSIONS) {
        let _ = fs::remove_file(old);
    }
}

// Valeur du compteur : counter.json en priorité, sinon l'ancienne valeur qui était
// enregistrée dans bot.json (migration, une seule fois).
fn load_counter(app: &AppHandle, legacy: u64) -> u64 {
    counter_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<CounterFile>(&t).ok())
        .map(|c| c.value)
        .unwrap_or(legacy)
}

fn save_counter(app: &AppHandle, value: u64) -> Result<(), String> {
    let path = counter_path(app)?;
    let json = serde_json::to_string(&CounterFile { value }).map_err(|e| e.to_string())?;
    write_atomically(&path, &json)
}

fn load_config(app: &AppHandle) -> BotConfig {
    let mut restored = false;

    let mut config = match config_path(app) {
        Ok(path) => {
            if let Some(c) = read_config_file(&path) {
                c
            } else {
                // Fichier absent ou illisible : on met de côté l'éventuel fichier abîmé,
                // puis on remonte les versions, de la plus récente à la plus ancienne.
                if path.exists() {
                    set_aside(&path);
                }
                let from_history = list_versions(&path)
                    .iter()
                    .find_map(|file| read_config_file(file))
                    .or_else(|| read_config_file(&path.with_extension("json.bak")));

                match from_history {
                    Some(c) => {
                        restored = true;
                        ::log::warn!("Réglages du bot restaurés depuis une version sauvegardée");
                        c
                    }
                    None => BotConfig::default(),
                }
            }
        }
        Err(_) => BotConfig::default(),
    };

    // Le compteur vient de counter.json (ou, une seule fois, de l'ancien bot.json).
    config.counter = load_counter(app, config.counter);

    // !followage est retirée (remplacée par !follow).
    let before = config.commands.len();
    config.commands.retain(|c| !(c.builtin && c.kind == Kind::Followage));
    let removed_obsolete = config.commands.len() != before;

    // Ajoute les commandes fournies par l'application qui manqueraient (nouvelles versions).
    for def in BotConfig::defaults() {
        let exists = config
            .commands
            .iter()
            .any(|c| c.builtin && c.kind == def.kind && c.kind != Kind::Text)
            || config.commands.iter().any(|c| c.name == def.name);
        if !exists {
            config.commands.push(def);
        }
    }

    // Après une restauration, on réécrit tout de suite un bot.json propre.
    if restored || removed_obsolete {
        if let Err(e) = save_config(app, &config) {
            ::log::warn!("Réécriture après restauration impossible : {e}");
        }
    }
    config
}

// Enregistre bot.json uniquement si les réglages ont changé (le compteur n'en fait pas partie).
fn save_config(app: &AppHandle, config: &BotConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let new_json = canonical_json(config);

    match read_config_file(&path) {
        // Rien n'a changé : inutile de réécrire, et pas de nouvelle version.
        Some(existing) if canonical_json(&existing) == new_json => return Ok(()),
        Some(_) => {}
        // Fichier présent mais abîmé : on le garde de côté avant de le remplacer.
        None => {
            if path.exists() {
                set_aside(&path);
            }
        }
    }

    write_atomically(&path, &new_json)?;
    keep_version(&path);
    Ok(())
}

// Appelée au démarrage : charge les réglages enregistrés.
pub fn init(app: &AppHandle) {
    let config = load_config(app);
    let counter = config.counter;
    let state = app.state::<AppState>();
    *state.bot.config.write().unwrap() = config;

    // Première version au démarrage (si elle manque), et premier counter.json (migration).
    if let Ok(path) = config_path(app) {
        keep_version(&path);
    }
    if counter_path(app).map(|p| !p.exists()).unwrap_or(false) {
        if let Err(e) = save_counter(app, counter) {
            ::log::warn!("Création de counter.json impossible : {e}");
        }
    }
}

fn persist(app: &AppHandle) -> Result<(), String> {
    let config = {
        let state = app.state::<AppState>();
        let c = state.bot.config.read().unwrap().clone();
        c
    };
    save_config(app, &config)
}

// Envoie une ligne au journal de l'interface (et au fichier de journaux).
pub fn log(app: &AppHandle, message: impl Into<String>) {
    let message: String = message.into();
    ::log::info!("[bot] {message}");
    let _ = app.emit("bot-log", message);
}

// --- Compteur ---

// Ajoute (ou retire) une valeur au compteur, l'enregistre dans counter.json,
// et prévient l'interface.
pub fn counter_adjust(app: &AppHandle, delta: i64) -> u64 {
    let value = {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        let next = (config.counter as i64 + delta).max(0) as u64;
        config.counter = next;
        next
    };
    if let Err(e) = save_counter(app, value) {
        log(app, format!("Sauvegarde du compteur impossible : {e}"));
    }
    let _ = app.emit("bot-counter", value);
    value
}

pub fn counter_reset(app: &AppHandle) -> u64 {
    let value = {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        config.counter = 0;
        0u64
    };
    if let Err(e) = save_counter(app, value) {
        log(app, format!("Sauvegarde du compteur impossible : {e}"));
    }
    let _ = app.emit("bot-counter", value);
    value
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

// Met à jour une commande existante, et la renomme si besoin. Renvoie le nom final.
// Met à jour une commande existante, et la renomme si besoin. Renvoie le nom final.
#[tauri::command]
pub fn bot_update_command(
    app: AppHandle,
    command: Command,
    new_name: String,
) -> Result<String, String> {
    let new_name = validate_name(&new_name)?;

    let final_name = {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();

        let index = config
            .commands
            .iter()
            .position(|c| c.name == command.name)
            .ok_or("Commande introuvable.")?;

        if new_name != command.name && config.commands.iter().any(|c| c.name == new_name) {
            return Err(format!("La commande !{new_name} existe déjà."));
        }

        let kind = config.commands[index].kind;
        if !level_allowed(kind, command.level) {
            return Err("Cette commande ne peut pas être ouverte à tout le monde.".into());
        }
        let response = if needs_response(kind) {
            clean_response(&command.response)?
        } else {
            String::new()
        };

        if command.cost > 1_000_000 {
            return Err("Le coût ne peut pas dépasser 1 000 000 points.".into());
        }
        if command.cost > 0 && !needs_response(kind) {
            return Err("Cette commande agit en silence : elle ne peut pas avoir de coût.".into());
        }

        let existing = &mut config.commands[index];
        existing.name = new_name.clone();
        existing.enabled = command.enabled;
        existing.response = response;
        existing.level = command.level;
        existing.global_cooldown = command.global_cooldown;
        existing.user_cooldown = command.user_cooldown;
        existing.reply_as = command.reply_as;
        existing.cost = command.cost;
        new_name
    };

    persist(&app)?;
    Ok(final_name)
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
            cost: command.cost.min(1_000_000),
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

#[tauri::command]
pub fn bot_counter_adjust(app: AppHandle, delta: i64) -> u64 {
    counter_adjust(&app, delta)
}

#[tauri::command]
pub fn bot_counter_reset(app: AppHandle) -> u64 {
    counter_reset(&app)
}

// --- Messages automatiques ---

fn validate_auto(name: &str, text: &str, interval: u32) -> Result<(String, String), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err("Donne un nom court (40 caractères max) au message.".into());
    }
    let text = clean_response(text)?;
    if !(5..=1440).contains(&interval) {
        return Err("L'intervalle doit être compris entre 5 et 1440 minutes.".into());
    }
    Ok((name.to_string(), text))
}

#[tauri::command]
pub fn bot_get_live(state: State<'_, AppState>) -> bool {
    state.bot.live.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn bot_auto_set_settings(app: AppHandle, settings: AutoSettings) -> Result<(), String> {
    if settings.min_chat_lines > 100 || settings.min_gap_min > 60 {
        return Err("Valeurs hors limites (chat : 0 à 100, écart : 0 à 60 minutes).".into());
    }
    {
        let state = app.state::<AppState>();
        state.bot.config.write().unwrap().auto = settings;
    }
    persist(&app)
}

#[tauri::command]
pub fn bot_auto_create(app: AppHandle, message: AutoMessage) -> Result<(), String> {
    let (name, text) = validate_auto(&message.name, &message.text, message.interval_min)?;
    {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        if config.auto_messages.len() >= 50 {
            return Err("50 messages automatiques au maximum.".into());
        }
        config.auto_messages.push(AutoMessage {
            id: chrono::Utc::now().timestamp_millis() as u64,
            name,
            enabled: true,
            text,
            interval_min: message.interval_min,
            reply_as: message.reply_as,
        });
    }
    persist(&app)
}

#[tauri::command]
pub fn bot_auto_update(app: AppHandle, message: AutoMessage) -> Result<(), String> {
    let (name, text) = validate_auto(&message.name, &message.text, message.interval_min)?;
    {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        let existing = config
            .auto_messages
            .iter_mut()
            .find(|m| m.id == message.id)
            .ok_or("Message introuvable.")?;
        existing.name = name;
        existing.enabled = message.enabled;
        existing.text = text;
        existing.interval_min = message.interval_min;
        existing.reply_as = message.reply_as;
    }
    persist(&app)
}

#[tauri::command]
pub fn bot_auto_delete(app: AppHandle, id: u64) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut config = state.bot.config.write().unwrap();
        config.auto_messages.retain(|m| m.id != id);
    }
    persist(&app)
}

// --- Points de présence ---

#[tauri::command]
pub fn bot_points_set_settings(app: AppHandle, settings: PointsSettings) -> Result<(), String> {
    let name = settings.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 30 {
        return Err("Le nom des points doit faire entre 1 et 30 caractères.".into());
    }
    if !(1..=60).contains(&settings.interval_min) {
        return Err("L'intervalle doit être compris entre 1 et 60 minutes.".into());
    }
    if !(1..=10_000).contains(&settings.amount) {
        return Err("Le montant doit être compris entre 1 et 10 000 points.".into());
    }
    if settings.streak_every > 1000 {
        return Err("Le palier de série doit être compris entre 0 et 1000 (0 = désactivé).".into());
    }
    if settings.streak_bonus > 100_000 {
        return Err("Le bonus de série ne peut pas dépasser 100 000 points.".into());
    }
    if !(1..=1440).contains(&settings.stream_gap_min) {
        return Err("L'écart entre deux directs doit être compris entre 1 et 1440 minutes.".into());
    }

    let insufficient = settings.insufficient_message.trim().to_string();
    if insufficient.is_empty() || insufficient.chars().count() > 500 {
        return Err("Le message de solde insuffisant doit faire entre 1 et 500 caractères.".into());
    }

    let mut excluded: Vec<String> = settings
        .excluded
        .iter()
        .map(|s| s.trim().trim_start_matches('@').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    excluded.sort();
    excluded.dedup();
    if excluded.len() > 100 {
        return Err("100 comptes exclus au maximum.".into());
    }
    {
        let state = app.state::<AppState>();
        state.bot.config.write().unwrap().points = PointsSettings {
            enabled: settings.enabled,
            name,
            interval_min: settings.interval_min,
            amount: settings.amount,
            excluded,
            streak_every: settings.streak_every,
            streak_bonus: settings.streak_bonus,
            stream_gap_min: settings.stream_gap_min,
            insufficient_message: insufficient,
        };
    }
    persist(&app)
}

// --- Événements ---

#[tauri::command]
pub fn bot_events_set_settings(app: AppHandle, settings: EventSettings) -> Result<(), String> {
    let text = settings.follow_message.trim().to_string();
    if text.is_empty() || text.chars().count() > 500 {
        return Err("Le message doit faire entre 1 et 500 caractères.".into());
    }
    {
        let state = app.state::<AppState>();
        state.bot.config.write().unwrap().events = EventSettings {
            follow_enabled: settings.follow_enabled,
            only_when_live: settings.only_when_live,
            follow_message: text,
            reply_as: settings.reply_as,
        };
    }
    persist(&app)
}