use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

use super::log;
use crate::outbox::Outgoing;
use crate::twitch::{api, Role};
use crate::AppState;

// Vérifie toutes les 10 secondes si un message automatique doit partir.
pub async fn scheduler(app: AppHandle) {
    let mut last_sent: HashMap<u64, Instant> = HashMap::new();
    let mut last_any: Option<Instant> = None;
    let mut lines_at_last: u64 = 0;
    let mut was_live = false;

    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Copie des réglages, sans garder de verrou.
        let (settings, messages, counter) = {
            let state = app.state::<AppState>();
            let config = state.bot.config.read().unwrap();
            (
                config.auto.clone(),
                config.auto_messages.clone(),
                config.counter,
            )
        };
        let (live, lines) = {
            let state = app.state::<AppState>();
            (
                state.bot.live.load(Ordering::Relaxed),
                state.bot.chat_lines.load(Ordering::Relaxed),
            )
        };

        // Les messages supprimés n'ont plus de minuteur.
        last_sent.retain(|id, _| messages.iter().any(|m| m.id == *id));
        // Un nouveau message démarre son minuteur maintenant (pas d'envoi immédiat).
        for m in &messages {
            last_sent.entry(m.id).or_insert_with(Instant::now);
        }

        // Quand le stream démarre, tous les minuteurs repartent de zéro.
        if live && !was_live {
            for t in last_sent.values_mut() {
                *t = Instant::now();
            }
            lines_at_last = lines;
        }
        was_live = live;

        if !settings.enabled {
            continue;
        }
        if settings.only_when_live && !live {
            continue;
        }
        // Écart minimal entre deux messages automatiques.
        if let Some(t) = last_any {
            if t.elapsed() < Duration::from_secs(settings.min_gap_min as u64 * 60) {
                continue;
            }
        }
        // Assez d'activité dans le chat depuis le dernier message automatique ?
        if lines.saturating_sub(lines_at_last) < settings.min_chat_lines as u64 {
            continue;
        }

        // Parmi les messages actifs arrivés à échéance, on prend le plus en retard.
        let due = messages
            .iter()
            .filter(|m| m.enabled)
            .filter_map(|m| {
                let elapsed = last_sent.get(&m.id)?.elapsed();
                let interval = Duration::from_secs(m.interval_min as u64 * 60);
                if elapsed >= interval {
                    Some((m, elapsed - interval))
                } else {
                    None
                }
            })
            .max_by_key(|(_, overdue)| *overdue);
        let Some((message, _)) = due else {
            continue;
        };

        let text: String = message
            .text
            .replace("[compteur]", &counter.to_string())
            .chars()
            .take(500)
            .collect();
        let item = Outgoing {
            role: message.reply_as,
            text,
            queued_at: Instant::now(),
        };
        let queued = {
            let state = app.state::<AppState>();
            let r = state.outbox.try_send(item);
            r
        };
        match queued {
            Ok(()) => log(&app, format!("Message automatique « {} » envoyé", message.name)),
            Err(_) => log(&app, "Message automatique abandonné : file d'envoi pleine"),
        }

        last_sent.insert(message.id, Instant::now());
        last_any = Some(Instant::now());
        lines_at_last = lines;
    }
}

// Interroge Twitch toutes les 30 secondes pour savoir si la chaîne est en direct.
pub async fn live_watcher(app: AppHandle) {
    let client = reqwest::Client::new();
    loop {
        let session = {
            let state = app.state::<AppState>();
            let s = state.session(Role::Streamer);
            s
        };

        match session {
            Some(streamer) => match api::is_live(&client, &streamer).await {
                Ok(live) => {
                    let changed = {
                        let state = app.state::<AppState>();
                        let before = state.bot.live.swap(live, Ordering::Relaxed);
                        before != live
                    };
                    if changed {
                        let message = if live {
                            "Stream en direct détecté"
                        } else {
                            "Stream hors ligne détecté"
                        };
                        log(&app, message);
                        let _ = app.emit("bot-live", live);
                    }
                }
                Err(e) => ::log::warn!("Vérification du direct impossible : {e}"),
            },
            None => {
                // Streamer non connecté : on considère la chaîne hors ligne.
                let was_live = {
                    let state = app.state::<AppState>();
                    let w = state.bot.live.swap(false, Ordering::Relaxed);
                    w
                };
                if was_live {
                    let _ = app.emit("bot-live", false);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}