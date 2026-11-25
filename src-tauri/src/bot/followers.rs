use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use super::log;
use crate::twitch::{api, Role};
use crate::AppState;

// Un seul import à la fois.
static RUNNING: AtomicBool = AtomicBool::new(false);

fn flag_key(streamer_id: &str) -> String {
    format!("followers_imported:{streamer_id}")
}

// Une page, avec 3 essais en cas de problème réseau ou de limite Twitch.
async fn fetch_page(
    client: &reqwest::Client,
    streamer: &crate::twitch::auth::TwitchSession,
    cursor: Option<&str>,
) -> Result<api::FollowersPage, String> {
    let mut last_err = String::new();
    for attempt in 0..3 {
        match api::followers_page(client, streamer, cursor).await {
            Ok(page) => return Ok(page),
            Err(e) => {
                last_err = e;
                tokio::time::sleep(Duration::from_secs(5 * (attempt + 1))).await;
            }
        }
    }
    Err(last_err)
}

// Lit tous les followers et les range dans la base, SANS modifier l'existant.
// Renvoie le nombre de followers lus.
pub async fn run_import(app: &AppHandle) -> Result<usize, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("Un import est déjà en cours.".into());
    }
    let result = import_all(app).await;
    RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn import_all(app: &AppHandle) -> Result<usize, String> {
    let client = reqwest::Client::new();
    let mut cursor: Option<String> = None;
    let mut read = 0usize;
    let mut next_log = 500usize;

    loop {
        // La session est relue à chaque page (le token peut avoir été renouvelé entre-temps).
        let streamer = {
            let state = app.state::<AppState>();
            let s = state.session(Role::Streamer);
            s
        }
        .ok_or("Le compte du streamer n'est pas connecté.")?;

        let page = fetch_page(&client, &streamer, cursor.as_deref()).await?;
        {
            let state = app.state::<AppState>();
            state.db.import_followers(&page.rows)?;
        }
        read += page.rows.len();
        if read >= next_log {
            log(
                app,
                format!("Import des followers : {read} sur {}", page.total),
            );
            next_log += 500;
        }

        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
        // On ménage Twitch : environ 6 pages par seconde au maximum.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(read)
}

// Import automatique : une seule fois pour chaque compte streamer.
pub async fn auto_import(app: AppHandle) {
    let mut done_for: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let streamer_id = {
            let state = app.state::<AppState>();
            let id = state.user_id_of(Role::Streamer);
            id
        };
        let Some(id) = streamer_id else {
            continue;
        };
        if done_for.as_deref() == Some(id.as_str()) {
            continue;
        }

        let flag = {
            let state = app.state::<AppState>();
            let f = state.db.meta_get(&flag_key(&id));
            f
        };
        match flag {
            Ok(Some(_)) => {
                done_for = Some(id);
                continue;
            }
            Ok(None) => {}
            Err(_) => continue, // base pas encore prête
        }

        log(&app, "Import des followers : démarrage (première connexion)");
        match run_import(&app).await {
            Ok(n) => {
                let marked = {
                    let state = app.state::<AppState>();
                    let m = state.db.meta_set(&flag_key(&id), "1");
                    m
                };
                if let Err(e) = marked {
                    log(&app, format!("Import terminé, mais impossible de le noter : {e}"));
                }
                log(
                    &app,
                    format!("Import des followers terminé : {n} follower(s) lus, aucune donnée existante modifiée"),
                );
                done_for = Some(id);
            }
            Err(e) => {
                log(
                    &app,
                    format!("Import des followers interrompu : {e} (nouvel essai dans 5 minutes)"),
                );
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        }
    }
}

// Bouton « Importer les followers » : mêmes garanties, relançable à volonté.
#[tauri::command]
pub async fn followers_resync(app: AppHandle) -> Result<usize, String> {
    let n = run_import(&app).await?;
    log(
        &app,
        format!("Import manuel terminé : {n} follower(s) lus, aucune donnée existante modifiée"),
    );
    Ok(n)
}