// Base de données SQLite : viewers, points, séries de streams.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::twitch::Role;
use crate::AppState;

// Nombre de sauvegardes journalières conservées.
const KEEP_BACKUPS: usize = 10;

// Ce qui attend en mémoire avant d'être écrit dans la base.
struct Pending {
    login: String,
    display_name: String,
    messages: i64,
    stream: i64, // stream auquel ce viewer a assisté via un message (0 = aucun)
}

// Un bonus de série à journaliser.
pub struct StreakReward {
    pub display_name: String,
    pub streak: i64,
    pub bonus: i64,
}

pub struct Db {
    conn: Mutex<Option<Connection>>,
    pending: Mutex<HashMap<String, Pending>>,
    current_stream: AtomicI64, // numéro du stream en cours (0 = pas de stream)
}

// Crée la fiche du viewer, ou la met à jour s'il existe déjà.
const UPSERT_VIEWER: &str = "
    INSERT INTO viewers (twitch_id, login, display_name, first_seen, last_seen, messages)
    VALUES (?1, ?2, ?3, ?4, ?4, ?5)
    ON CONFLICT(twitch_id) DO UPDATE SET
        login        = excluded.login,
        display_name = excluded.display_name,
        last_seen    = excluded.last_seen,
        messages     = viewers.messages + excluded.messages";

// Donne des points et du temps de présence (crée la fiche si le viewer n'existe pas encore).
const UPSERT_PRESENCE: &str = "
    INSERT INTO viewers (twitch_id, login, display_name, first_seen, last_seen, minutes_present, points)
    VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
    ON CONFLICT(twitch_id) DO UPDATE SET
        login           = excluded.login,
        display_name    = excluded.display_name,
        last_seen       = excluded.last_seen,
        minutes_present = viewers.minutes_present + excluded.minutes_present,
        points          = viewers.points + excluded.points";

// Le stream juste avant celui-ci (None s'il n'y en a pas).
fn previous_stream(conn: &Connection, stream_id: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(id) FROM streams WHERE id < ?1",
        [stream_id],
        |r| r.get::<_, Option<i64>>(0),
    )
}

// Enregistre qu'un viewer a assisté à ce stream (une seule fois par stream),
// met à jour sa série, et donne le bonus si la série atteint un multiple de `every`.
fn apply_attendance(
    tx: &rusqlite::Transaction,
    stream_id: i64,
    prev: Option<i64>,
    twitch_id: &str,
    every: i64,
    bonus: i64,
) -> rusqlite::Result<Option<StreakReward>> {
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO attendance (stream_id, twitch_id) VALUES (?1, ?2)",
        rusqlite::params![stream_id, twitch_id],
    )?;
    if inserted == 0 {
        return Ok(None); // déjà compté pour ce stream
    }

    let (streak, last, name): (i64, Option<i64>, String) = tx.query_row(
        "SELECT streak, last_stream_id, display_name FROM viewers WHERE twitch_id = ?1",
        [twitch_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    // La série continue seulement si le viewer était au stream précédent.
    let new_streak = if last.is_some() && last == prev { streak + 1 } else { 1 };
    let reward = if every > 0 && new_streak % every == 0 { bonus } else { 0 };

    tx.execute(
        "UPDATE viewers SET
             streak = ?2,
             best_streak = MAX(best_streak, ?2),
             streams_attended = streams_attended + 1,
             last_stream_id = ?3,
             points = points + ?4
         WHERE twitch_id = ?1",
        rusqlite::params![twitch_id, new_streak, stream_id, reward],
    )?;

    Ok(if reward > 0 {
        Some(StreakReward {
            display_name: name,
            streak: new_streak,
            bonus: reward,
        })
    } else {
        None
    })
}

impl Db {
    pub fn new() -> Self {
        Self {
            conn: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            current_stream: AtomicI64::new(0),
        }
    }

    // Exécute une opération sur la base (verrou gardé très peu de temps).
    pub fn with<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let guard = self.conn.lock().unwrap();
        let conn = guard.as_ref().ok_or("Base de données non disponible.")?;
        f(conn).map_err(|e| format!("Erreur base de données : {e}"))
    }

    pub fn current_stream_id(&self) -> i64 {
        self.current_stream.load(Ordering::Relaxed)
    }

    // Note un message d'un viewer en mémoire (l'écriture se fait par groupe, voir flush).
    // `attend` : ce message compte comme une présence au stream en cours.
    pub fn record_message(&self, twitch_id: &str, login: &str, display_name: &str, attend: bool) {
        let stream = if attend { self.current_stream_id() } else { 0 };
        let mut map = self.pending.lock().unwrap();
        let entry = map.entry(twitch_id.to_string()).or_insert(Pending {
            login: String::new(),
            display_name: String::new(),
            messages: 0,
            stream: 0,
        });
        entry.login = login.to_string();
        entry.display_name = display_name.to_string();
        entry.messages += 1;
        if stream != 0 {
            entry.stream = stream;
        }
    }

    // Écrit tout ce qui attend en mémoire, en une seule opération.
    // Renvoie le nombre de viewers écrits et les bonus de série donnés.
    pub fn flush(&self, every: i64, bonus: i64) -> Result<(usize, Vec<StreakReward>), String> {
        let batch: HashMap<String, Pending> = std::mem::take(&mut *self.pending.lock().unwrap());
        if batch.is_empty() {
            return Ok((0, Vec::new()));
        }
        let now = chrono::Utc::now().timestamp();
        let count = batch.len();

        let result = self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare(UPSERT_VIEWER)?;
                for (id, p) in &batch {
                    stmt.execute(rusqlite::params![id, p.login, p.display_name, now, p.messages])?;
                }
            }
            let mut rewards = Vec::new();
            for (id, p) in &batch {
                if p.stream != 0 {
                    let prev = previous_stream(&tx, p.stream)?;
                    if let Some(r) = apply_attendance(&tx, p.stream, prev, id, every, bonus)? {
                        rewards.push(r);
                    }
                }
            }
            tx.commit()?;
            Ok(rewards)
        });

        match result {
            Ok(rewards) => Ok((count, rewards)),
            Err(e) => {
                // En cas d'échec, on remet tout en attente pour ne rien perdre.
                let mut map = self.pending.lock().unwrap();
                for (id, p) in batch {
                    let entry = map.entry(id).or_insert(Pending {
                        login: p.login,
                        display_name: p.display_name,
                        messages: 0,
                        stream: 0,
                    });
                    entry.messages += p.messages;
                    if p.stream != 0 {
                        entry.stream = p.stream;
                    }
                }
                Err(e)
            }
        }
    }

    // Donne `points` points et `minutes` minutes de présence à chaque viewer de la liste,
    // et les compte comme présents au stream en cours.
    pub fn award_presence(
        &self,
        chatters: &[crate::twitch::api::Chatter],
        points: i64,
        minutes: i64,
        stream_id: i64,
        every: i64,
        bonus: i64,
    ) -> Result<(usize, Vec<StreakReward>), String> {
        if chatters.is_empty() {
            return Ok((0, Vec::new()));
        }
        let now = chrono::Utc::now().timestamp();
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare(UPSERT_PRESENCE)?;
                for c in chatters {
                    stmt.execute(rusqlite::params![c.id, c.login, c.name, now, minutes, points])?;
                }
            }
            let mut rewards = Vec::new();
            if stream_id != 0 {
                let prev = previous_stream(&tx, stream_id)?;
                for c in chatters {
                    if let Some(r) = apply_attendance(&tx, stream_id, prev, &c.id, every, bonus)? {
                        rewards.push(r);
                    }
                }
            }
            tx.commit()?;
            Ok((chatters.len(), rewards))
        })
    }

    // Début de direct : reprend le stream précédent s'il s'est terminé il y a moins de
    // `gap_secs` secondes, sinon en crée un nouveau. Renvoie son numéro.
    pub fn stream_start(&self, gap_secs: i64) -> Result<i64, String> {
        let now = chrono::Utc::now().timestamp();
        let id = self.with(|conn| {
            let last: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT id, ended_at FROM streams ORDER BY id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match last {
                Some((id, ended)) if now - ended < gap_secs => {
                    conn.execute(
                        "UPDATE streams SET ended_at = ?1 WHERE id = ?2",
                        rusqlite::params![now, id],
                    )?;
                    Ok(id)
                }
                _ => {
                    conn.execute(
                        "INSERT INTO streams (started_at, ended_at) VALUES (?1, ?1)",
                        [now],
                    )?;
                    Ok(conn.last_insert_rowid())
                }
            }
        })?;
        self.current_stream.store(id, Ordering::Relaxed);
        Ok(id)
    }

    // Note « la chaîne était encore en direct à cet instant » pour le stream en cours.
    pub fn stream_touch(&self) -> Result<(), String> {
        let id = self.current_stream_id();
        if id == 0 {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        self.with(|conn| {
            conn.execute(
                "UPDATE streams SET ended_at = ?1 WHERE id = ?2",
                rusqlite::params![now, id],
            )
            .map(|_| ())
        })
    }

    // Fin de direct.
    pub fn stream_end(&self) {
        if let Err(e) = self.stream_touch() {
            ::log::warn!("{e}");
        }
        self.current_stream.store(0, Ordering::Relaxed);
    }
}

fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("khalie.db"))
}

fn backups_dir(db_file: &Path) -> PathBuf {
    db_file.with_file_name("backups-db")
}

// Ouvre la base et vérifie qu'elle n'est pas abîmée.
fn open_checked(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    let verdict: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if verdict != "ok" {
        return Err(format!("base abîmée : {verdict}"));
    }
    Ok(conn)
}

// Crée / met à jour les tables. "user_version" permet de faire évoluer la base.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE viewers (
                 twitch_id       TEXT PRIMARY KEY,
                 login           TEXT NOT NULL,
                 display_name    TEXT NOT NULL,
                 first_seen      INTEGER NOT NULL,
                 last_seen       INTEGER NOT NULL,
                 first_follow    INTEGER,
                 last_follow     INTEGER,
                 messages        INTEGER NOT NULL DEFAULT 0,
                 minutes_present INTEGER NOT NULL DEFAULT 0,
                 points          INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX idx_viewers_points ON viewers(points DESC);
             CREATE INDEX idx_viewers_login ON viewers(login);
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    if version < 2 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE streams (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 started_at INTEGER NOT NULL,
                 ended_at   INTEGER NOT NULL
             );
             CREATE TABLE attendance (
                 stream_id INTEGER NOT NULL,
                 twitch_id TEXT NOT NULL,
                 PRIMARY KEY (stream_id, twitch_id)
             );
             ALTER TABLE viewers ADD COLUMN streak INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE viewers ADD COLUMN best_streak INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE viewers ADD COLUMN streams_attended INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE viewers ADD COLUMN last_stream_id INTEGER;
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    Ok(())
}

// Sauvegardes, de la plus récente à la plus ancienne (la date est dans le nom).
fn list_backups(db_file: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(backups_dir(db_file))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |x| x == "db"))
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    files
}

// Une sauvegarde par jour (au premier lancement du jour), les plus récentes sont gardées.
fn daily_backup(conn: &Connection, db_file: &Path) {
    let dir = backups_dir(db_file);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let today = chrono::Local::now().format("%Y%m%d");
    let target = dir.join(format!("khalie-{today}.db"));
    if target.exists() {
        return;
    }
    // VACUUM INTO produit une copie cohérente, même si la base est ouverte.
    let escaped = target.to_string_lossy().replace('\'', "''");
    if let Err(e) = conn.execute_batch(&format!("VACUUM INTO '{escaped}'")) {
        ::log::warn!("Sauvegarde de la base impossible : {e}");
        return;
    }
    for old in list_backups(db_file).into_iter().skip(KEEP_BACKUPS) {
        let _ = fs::remove_file(old);
    }
}

// Base abîmée : on la met de côté, puis on remonte les sauvegardes, de la plus récente à la plus ancienne.
fn recover(path: &Path, problem: &str) -> Option<Connection> {
    ::log::warn!("Base de données inutilisable ({problem})");
    if path.exists() {
        let aside = path.with_extension(format!("db.corrompu-{}", chrono::Utc::now().timestamp()));
        if let Err(e) = fs::rename(path, &aside) {
            ::log::error!("Mise de côté de la base impossible : {e}");
            return None;
        }
    }
    for backup in list_backups(path) {
        if fs::copy(&backup, path).is_ok() {
            if let Ok(conn) = open_checked(path) {
                ::log::warn!("Base restaurée depuis {}", backup.display());
                return Some(conn);
            }
            let _ = fs::remove_file(path);
        }
    }
    // Rien de récupérable : on repart d'une base neuve.
    match open_checked(path) {
        Ok(conn) => Some(conn),
        Err(e) => {
            ::log::error!("Création de la base impossible : {e}");
            None
        }
    }
}

// Appelée au démarrage de l'application.
pub fn init(app: &AppHandle) {
    let path = match db_path(app) {
        Ok(p) => p,
        Err(e) => {
            ::log::error!("Base de données : dossier introuvable : {e}");
            return;
        }
    };

    let conn = match open_checked(&path) {
        Ok(c) => c,
        Err(problem) => match recover(&path, &problem) {
            Some(c) => c,
            None => return,
        },
    };

    // La sauvegarde du jour est faite AVANT toute modification de la structure.
    daily_backup(&conn, &path);

    if let Err(e) = migrate(&conn) {
        ::log::error!("Préparation de la base impossible : {e}");
        return;
    }

    let state = app.state::<AppState>();
    *state.db.conn.lock().unwrap() = Some(conn);

    match state
        .db
        .with(|c| c.query_row("SELECT COUNT(*) FROM viewers", [], |r| r.get::<_, i64>(0)))
    {
        Ok(n) => ::log::info!("Base de données prête : {n} viewer(s) ({})", path.display()),
        Err(e) => ::log::warn!("{e}"),
    }
}

// Journalise les bonus de série.
pub fn announce(app: &AppHandle, rewards: Vec<StreakReward>) {
    if rewards.is_empty() {
        return;
    }
    let name = {
        let state = app.state::<AppState>();
        let n = state.bot.config.read().unwrap().points.name.clone();
        n
    };
    for r in rewards {
        crate::bot::log(
            app,
            format!(
                "Série : {} atteint {} streams d'affilée (+{} {})",
                r.display_name, r.streak, r.bonus, name
            ),
        );
    }
}

// Écrit les messages en attente toutes les 5 secondes.
pub async fn flusher(app: AppHandle) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (every, bonus) = {
            let state = app.state::<AppState>();
            let p = state.bot.config.read().unwrap().points.clone();
            (p.streak_every as i64, p.streak_bonus as i64)
        };
        let result = {
            let state = app.state::<AppState>();
            state.db.flush(every, bonus)
        };
        match result {
            Ok((_, rewards)) => announce(&app, rewards),
            Err(e) => ::log::warn!("{e}"),
        }
    }
}

// --- Liste des viewers pour l'interface ---

#[derive(Serialize)]
pub struct ViewerRow {
    twitch_id: String,
    login: String,
    display_name: String,
    first_seen: i64,
    last_seen: i64,
    first_follow: Option<i64>,
    last_follow: Option<i64>,
    messages: i64,
    minutes_present: i64,
    points: i64,
    streak: i64,           // série en cours (0 si elle est cassée)
    best_streak: i64,
    streams_attended: i64,
}

#[derive(Serialize)]
pub struct ViewersPage {
    rows: Vec<ViewerRow>,
    total: i64, // nombre total de viewers (hors streamer et bot)
}

#[tauri::command]
pub fn db_list_viewers(
    state: State<'_, AppState>,
    search: String,
    sort: String,
    limit: u32,
) -> Result<ViewersPage, String> {
    // Le streamer et le bot ne sont pas des viewers.
    let streamer_id = state.user_id_of(Role::Streamer).unwrap_or_default();
    let bot_id = state.user_id_of(Role::Bot).unwrap_or_default();
    let current = state.db.current_stream_id();

    // Le tri est choisi dans une liste fermée : jamais de texte venant de l'interface dans la requête.
    let order = match sort.as_str() {
        "messages" => "messages DESC",
        "presence" => "minutes_present DESC",
        "recent" => "last_seen DESC",
        "streak" => "streak_now DESC",
        "streams" => "streams_attended DESC",
        "name" => "login ASC",
        _ => "points DESC",
    };

    let escaped = search
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let limit = limit.clamp(1, 500) as i64;

    let sql = format!(
        "SELECT twitch_id, login, display_name, first_seen, last_seen, first_follow,
                last_follow, messages, minutes_present, points,
                CASE WHEN last_stream_id IS NOT NULL AND last_stream_id >= ?5
                     THEN streak ELSE 0 END AS streak_now,
                best_streak, streams_attended
         FROM viewers
         WHERE twitch_id NOT IN (?1, ?2)
           AND (login LIKE ?3 ESCAPE '\\' OR display_name LIKE ?3 ESCAPE '\\')
         ORDER BY {order}, login ASC
         LIMIT ?4"
    );

    state.db.with(|conn| {
        // Une série est « vivante » si le viewer était au dernier stream terminé,
        // ou, pendant un direct, au stream précédent ou au stream en cours.
        let threshold: i64 = if current != 0 {
            previous_stream(conn, current)?.unwrap_or(0)
        } else {
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM streams", [], |r| r.get(0))?
        };

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM viewers WHERE twitch_id NOT IN (?1, ?2)",
            rusqlite::params![streamer_id, bot_id],
            |r| r.get(0),
        )?;

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params![streamer_id, bot_id, pattern, limit, threshold],
                |r| {
                    Ok(ViewerRow {
                        twitch_id: r.get(0)?,
                        login: r.get(1)?,
                        display_name: r.get(2)?,
                        first_seen: r.get(3)?,
                        last_seen: r.get(4)?,
                        first_follow: r.get(5)?,
                        last_follow: r.get(6)?,
                        messages: r.get(7)?,
                        minutes_present: r.get(8)?,
                        points: r.get(9)?,
                        streak: r.get(10)?,
                        best_streak: r.get(11)?,
                        streams_attended: r.get(12)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<ViewerRow>>>()?;

        Ok(ViewersPage { rows, total })
    })
}