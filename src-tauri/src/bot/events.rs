use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use super::log;
use crate::outbox::Outgoing;
use crate::twitch::Role;
use crate::AppState;

pub struct FollowEvent {
    pub user_id: String,
    pub login: String,
    pub name: String,
    pub followed_at: i64,
}

// Écart minimal entre deux messages de bienvenue, et attente maximale acceptée.
const SPACING: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(60);

// Prochain moment où un message de bienvenue peut partir.
static NEXT_SLOT: Mutex<Option<Instant>> = Mutex::new(None);

// Appelée à chaque nouveau follow reçu de Twitch.
pub async fn on_follow(app: AppHandle, ev: FollowEvent) {
    let (streamer_id, settings, live) = {
        let state = app.state::<AppState>();
        let s = state.user_id_of(Role::Streamer);
        let e = state.bot.config.read().unwrap().events.clone();
        let l = state.bot.live.load(Ordering::Relaxed);
        (s, e, l)
    };

    // Le streamer lui-même n'est pas concerné.
    if streamer_id.as_deref() == Some(ev.user_id.as_str()) {
        return;
    }

    // On enregistre toujours le follow, même si le message de bienvenue est désactivé.
    let is_new = {
        let state = app.state::<AppState>();
        state
            .db
            .follow_seen(&ev.user_id, &ev.login, &ev.name, ev.followed_at)
    };
    let is_new = match is_new {
        Ok(v) => v,
        Err(e) => {
            log(&app, format!("Follow de {} : {e}", ev.name));
            return;
        }
    };
    if !is_new {
        log(
            &app,
            format!("{} se réabonne : pas de message de bienvenue", ev.name),
        );
        return;
    }
    log(&app, format!("Nouveau follower : {}", ev.name));

    if !settings.follow_enabled {
        return;
    }
    if settings.only_when_live && !live {
        return;
    }

    // Au moins SPACING entre deux messages de bienvenue ; si l'attente devient trop longue
    // (beaucoup de followers d'un coup), on ignore ce message.
    let wait = {
        let mut slot = NEXT_SLOT.lock().unwrap();
        let now = Instant::now();
        let at = match *slot {
            Some(t) if t > now => t,
            _ => now,
        };
        let delay = at.duration_since(now);
        if delay > MAX_WAIT {
            None
        } else {
            *slot = Some(at + SPACING);
            Some(delay)
        }
    };
    let Some(wait) = wait else {
        log(
            &app,
            format!("Trop de nouveaux followers d'un coup : pas de message pour {}", ev.name),
        );
        return;
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }

    let text: String = settings
        .follow_message
        .replace("[@pseudo]", &format!("@{}", ev.name))
        .replace("[pseudo]", &ev.name)
        .chars()
        .take(500)
        .collect();
    let queued = {
        let state = app.state::<AppState>();
        let r = state.outbox.try_send(Outgoing {
            role: settings.reply_as,
            text,
            queued_at: Instant::now(),
        });
        r
    };
    match queued {
        Ok(()) => log(&app, format!("Message de bienvenue envoyé à {}", ev.name)),
        Err(_) => log(&app, "Message de bienvenue abandonné : file d'envoi pleine"),
    }
}