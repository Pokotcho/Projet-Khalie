// Base de données SQLite : viewers, points, historique des follows.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;
use tauri::{AppHandle, Manager};

use crate::AppState;

// Nombre de sauvegardes journalières conservées.
const KEEP_BACKUPS: usize = 10;

// Ce qui attend en mémoire avant d'être écrit dans la base.
struct Pending {
    login: String,
    display_name: String,
    messages: i64,
}

pub struct Db {
    conn: Mutex<Option<Connection>>,
    pending: Mutex<HashMap<String, Pending>>,
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

impl Db {
    pub fn new() -> Self {
        Self {
            conn: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
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

    // Note un message d'un viewer en mémoire (l'écriture se fait par groupe, voir flush).
    pub fn record_message(&self, twitch_id: &str, login: &str, display_name: &str) {
        let mut map = self.pending.lock().unwrap();
        let entry = map.entry(twitch_id.to_string()).or_insert(Pending {
            login: String::new(),
            display_name: String::new(),
            messages: 0,
        });
        entry.login = login.to_string();
        entry.display_name = display_name.to_string();
        entry.messages += 1;
    }

    // Écrit tout ce qui attend en mémoire, en une seule opération.
    // Renvoie le nombre de viewers écrits.
    pub fn flush(&self) -> Result<usize, String> {
        let batch: HashMap<String, Pending> = std::mem::take(&mut *self.pending.lock().unwrap());
        if batch.is_empty() {
            return Ok(0);
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
            tx.commit()
        });

        // En cas d'échec, on remet tout en attente pour ne rien perdre.
        if let Err(e) = result {
            let mut map = self.pending.lock().unwrap();
            for (id, p) in batch {
                let entry = map.entry(id).or_insert(Pending {
                    login: p.login,
                    display_name: p.display_name,
                    messages: 0,
                });
                entry.messages += p.messages;
            }
            return Err(e);
        }
        Ok(count)
    }

    // Donne `points` points et `minutes` minutes de présence à chaque viewer de la liste.
    pub fn award_presence(
        &self,
        chatters: &[crate::twitch::api::Chatter],
        points: i64,
        minutes: i64,
    ) -> Result<usize, String> {
        if chatters.is_empty() {
            return Ok(0);
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
            tx.commit()?;
            Ok(chatters.len())
        })
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

// Crée les tables si besoin. "user_version" permet de faire évoluer la base plus tard.
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

    if let Err(e) = migrate(&conn) {
        ::log::error!("Préparation de la base impossible : {e}");
        return;
    }
    daily_backup(&conn, &path);

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

// Écrit les messages en attente toutes les 5 secondes.
pub async fn flusher(app: AppHandle) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let result = {
            let state = app.state::<AppState>();
            state.db.flush()
        };
        match result {
            Ok(0) => {}
            Ok(_) => {}
            Err(e) => ::log::warn!("{e}"),
        }
    }
}