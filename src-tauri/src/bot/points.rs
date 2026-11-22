use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use super::log;
use crate::twitch::{api, Role};
use crate::AppState;

// Toutes les N minutes, pendant le direct, donne des points aux viewers présents dans le chat.
pub async fn presence_loop(app: AppHandle) {
    let client = reqwest::Client::new();
    let mut last_tick = Instant::now();
    let mut was_live = false;

    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;

        let (settings, live) = {
            let state = app.state::<AppState>();
            let s = state.bot.config.read().unwrap().points.clone();
            let l = state.bot.live.load(Ordering::Relaxed);
            (s, l)
        };

        // Au début du direct, le minuteur repart de zéro.
        if live && !was_live {
            last_tick = Instant::now();
        }
        was_live = live;

        if !settings.enabled || !live {
            continue;
        }
        if last_tick.elapsed() < Duration::from_secs(settings.interval_min as u64 * 60) {
            continue;
        }
        last_tick = Instant::now();

        let (streamer, bot_id) = {
            let state = app.state::<AppState>();
            let s = state.session(Role::Streamer);
            let b = state.user_id_of(Role::Bot);
            (s, b)
        };
        let Some(streamer) = streamer else {
            continue;
        };

        let chatters = match api::get_chatters(&client, &streamer).await {
            Ok(list) => list,
            Err(e) => {
                log(&app, format!("Points : lecture des viewers présents impossible : {e}"));
                continue;
            }
        };

        // Ni le streamer, ni le bot, ni les comptes exclus.
        let eligible: Vec<api::Chatter> = chatters
            .into_iter()
            .filter(|c| c.id != streamer.user_id)
            .filter(|c| Some(&c.id) != bot_id.as_ref())
            .filter(|c| !settings.excluded.iter().any(|x| *x == c.login.to_lowercase()))
            .collect();

        let result = {
            let state = app.state::<AppState>();
            state.db.award_presence(
                &eligible,
                settings.amount as i64,
                settings.interval_min as i64,
            )
        };
        match result {
            Ok(n) => log(
                &app,
                format!("Points : {n} viewer(s) ont reçu {} {}", settings.amount, settings.name),
            ),
            Err(e) => log(&app, format!("Points : {e}")),
        }
    }
}