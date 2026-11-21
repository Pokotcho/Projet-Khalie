use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;

use crate::bot::log;
use crate::twitch::{chat, Role};
use crate::AppState;

// Limites volontairement prudentes.
const WINDOW: Duration = Duration::from_secs(30);
const MAX_PER_WINDOW: usize = 18;
const MIN_GAP: Duration = Duration::from_millis(1100);
const MAX_AGE: Duration = Duration::from_secs(60);

// Un message en attente d'envoi.
pub struct Outgoing {
    pub role: Role, // compte qui écrit
    pub text: String,
    pub queued_at: Instant,
}

// Tâche de fond : envoie les messages un par un en respectant les limites.
pub async fn worker(app: AppHandle, mut rx: mpsc::Receiver<Outgoing>) {
    let client = reqwest::Client::new();
    // Heures d'envoi récentes, par compte.
    let mut windows: HashMap<&'static str, VecDeque<Instant>> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let window = windows.entry(msg.role.key()).or_default();

        // On attend qu'il y ait de la place dans la fenêtre glissante.
        loop {
            let now = Instant::now();
            while window
                .front()
                .map_or(false, |t| now.duration_since(*t) >= WINDOW)
            {
                window.pop_front();
            }
            if window.len() < MAX_PER_WINDOW {
                break;
            }
            let Some(&oldest) = window.front() else {
                break;
            };
            let wait = WINDOW.saturating_sub(now.duration_since(oldest)) + Duration::from_millis(100);
            tokio::time::sleep(wait).await;
        }

        // Un message devenu trop vieux n'a plus d'intérêt.
        if msg.queued_at.elapsed() > MAX_AGE {
            log(&app, "Message abandonné : trop longtemps en attente");
            continue;
        }

        let (sender, streamer) = {
            let state = app.state::<AppState>();
            let a = state.session(msg.role);
            let b = state.session(Role::Streamer);
            (a, b)
        };
        let (Some(sender), Some(streamer)) = (sender, streamer) else {
            log(
                &app,
                format!("Message non envoyé : compte {} non connecté", msg.role.key()),
            );
            continue;
        };

        // On compte l'envoi avant de le faire : si Twitch refuse, la limite reste respectée.
        window.push_back(Instant::now());
        if let Err(e) = chat::send_message(&client, &sender, &streamer.user_id, &msg.text).await {
            log(&app, format!("Envoi impossible : {e}"));
        }

        tokio::time::sleep(MIN_GAP).await;
    }
}