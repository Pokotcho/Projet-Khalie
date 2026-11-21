use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio_tungstenite::tungstenite::Message;

use super::auth::{TwitchSession, CLIENT_ID};
use super::Role;
use crate::AppState;
use crate::outbox::Outgoing;

const EVENTSUB_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
const SUBSCRIBE_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
const SEND_URL: &str = "https://api.twitch.tv/helix/chat/messages";

// Un message de chat, tel qu'envoyé à l'interface.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub message_id: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub text: String,
    pub from_bot: bool,      // message écrit par le compte du bot
    pub from_streamer: bool, // message écrit par le compte du streamer
    pub is_moderator: bool,
}

// Démarre (ou redémarre) la lecture du chat avec le compte du streamer.
pub fn start(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut slot = state.chat_task.lock().unwrap();
    if let Some(old) = slot.take() {
        old.abort();
    }
    let handle = tauri::async_runtime::spawn(chat_loop(app.clone()));
    *slot = Some(handle);
}

// Arrête la lecture du chat.
pub fn stop(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Some(handle) = state.chat_task.lock().unwrap().take() {
        handle.abort();
    }
    let _ = app.emit("chat-status", "Chat arrêté");
}

// Boucle principale : se connecte, lit, et se reconnecte si ça casse.
async fn chat_loop(app: AppHandle) {
    let client = reqwest::Client::new();
    let mut backoff: u64 = 1;

    loop {
        let session = {
            let state = app.state::<AppState>();
            let s = state.session(Role::Streamer);
            s
        };
        let Some(session) = session else {
            let _ = app.emit("chat-status", "Chat arrêté (streamer non connecté)");
            return;
        };

        let _ = app.emit("chat-status", "Connexion au chat…");

        let mut connected = false;
        let reason = run_connection(&app, &client, &session, &mut connected).await;

        if connected {
            backoff = 1;
        }
        log::info!("Chat déconnecté : {reason}");
        let _ = app.emit(
            "chat-status",
            format!("Chat déconnecté ({reason}). Nouvelle tentative dans {backoff} s…"),
        );

        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

// Une connexion WebSocket, du début à la fin. Retourne la raison de l'arrêt.
// La chaîne surveillée est celle du streamer, et c'est son compte qui lit.
async fn run_connection(
    app: &AppHandle,
    client: &reqwest::Client,
    session: &TwitchSession,
    connected: &mut bool,
) -> String {
    let broadcaster_id = session.user_id.clone();
    let target = session.login.clone();

    let (mut ws, _) = match tokio_tungstenite::connect_async(EVENTSUB_URL).await {
        Ok(pair) => pair,
        Err(e) => return format!("connexion impossible : {e}"),
    };

    let mut idle_timeout = Duration::from_secs(15);

    loop {
        let next = match tokio::time::timeout(idle_timeout, ws.next()).await {
            Ok(n) => n,
            Err(_) => return "plus de nouvelles de Twitch".into(),
        };

        let msg = match next {
            Some(Ok(m)) => m,
            Some(Err(e)) => return format!("erreur de lecture : {e}"),
            None => return "connexion fermée".into(),
        };

        match msg {
            Message::Text(text) => {
                let v: Value = match serde_json::from_str(text.as_str()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match v["metadata"]["message_type"].as_str().unwrap_or("") {
                    "session_welcome" => {
                        let session_id = v["payload"]["session"]["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let keepalive = v["payload"]["session"]["keepalive_timeout_seconds"]
                            .as_u64()
                            .unwrap_or(10);
                        idle_timeout = Duration::from_secs(keepalive + 5);

                        if let Err(e) =
                            subscribe_chat(client, session, &session_id, &broadcaster_id).await
                        {
                            return format!("abonnement refusé : {e}");
                        }
                        *connected = true;
                        let _ = app.emit("chat-status", format!("Chat connecté ({target})"));
                    }

                    "notification" => {
                        if v["metadata"]["subscription_type"].as_str()
                            == Some("channel.chat.message")
                        {
                            let ev = &v["payload"]["event"];
                            let sender_id = ev["chatter_user_id"].as_str().unwrap_or("");

                            let bot_id = {
                                let state = app.state::<AppState>();
                                let id = state.user_id_of(Role::Bot);
                                id
                            };

                            // Badge "moderator" dans la liste des badges du message.
                            let is_moderator = ev["badges"]
                                .as_array()
                                .map(|badges| {
                                    badges
                                        .iter()
                                        .any(|b| b["set_id"].as_str() == Some("moderator"))
                                })
                                .unwrap_or(false);

                            let chat = ChatMessage {
                                message_id: ev["message_id"].as_str().unwrap_or("").into(),
                                user_id: sender_id.into(),
                                login: ev["chatter_user_login"].as_str().unwrap_or("").into(),
                                display_name: ev["chatter_user_name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .into(),
                                text: ev["message"]["text"].as_str().unwrap_or("").into(),
                                from_bot: bot_id.as_deref() == Some(sender_id),
                                from_streamer: sender_id == session.user_id,
                                is_moderator,
                            };

                            let _ = app.emit("chat-message", chat.clone());
                            // Le bot traite le message sans bloquer la lecture du chat.
                            tauri::async_runtime::spawn(crate::bot::engine::handle(
                                app.clone(),
                                chat,
                            ));
                        }
                    }
                    "session_reconnect" => return "Twitch demande une reconnexion".into(),
                    "revocation" => return "abonnement révoqué par Twitch".into(),
                    _ => {}
                }
            }
            Message::Close(_) => return "fermée par Twitch".into(),
            _ => continue,
        }
    }
}

// Demande à Twitch de nous envoyer les messages du chat de la chaîne du streamer.
async fn subscribe_chat(
    client: &reqwest::Client,
    session: &TwitchSession,
    session_id: &str,
    broadcaster_id: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "type": "channel.chat.message",
        "version": "1",
        "condition": {
            "broadcaster_user_id": broadcaster_id,
            "user_id": session.user_id
        },
        "transport": {
            "method": "websocket",
            "session_id": session_id
        }
    });

    let resp = client
        .post(SUBSCRIBE_URL)
        .header("Authorization", format!("Bearer {}", session.access_token))
        .header("Client-Id", CLIENT_ID)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Twitch injoignable : {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        Err(format!("code {status} : {detail}"))
    }
}

// --- Envoi de messages ---

// Envoie un message dans le chat du streamer, avec le compte indiqué.
pub async fn send_message(
    client: &reqwest::Client,
    sender: &TwitchSession,
    broadcaster_id: &str,
    text: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "broadcaster_id": broadcaster_id,
        "sender_id": sender.user_id,
        "message": text
    });

    let resp = client
        .post(SEND_URL)
        .header("Authorization", format!("Bearer {}", sender.access_token))
        .header("Client-Id", CLIENT_ID)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Twitch injoignable : {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("code {status} : {detail}"));
    }

    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;

    if v["data"][0]["is_sent"].as_bool() == Some(true) {
        Ok(())
    } else {
        let reason = v["data"][0]["drop_reason"]["message"]
            .as_str()
            .unwrap_or("raison inconnue");
        Err(format!("message refusé par Twitch : {reason}"))
    }
}

// Commande appelée par l'interface : le message est mis dans la file d'envoi.
#[tauri::command]
pub fn send_chat_message(app: AppHandle, text: String, role: Role) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("Message vide.".into());
    }
    if text.chars().count() > 500 {
        return Err("Message trop long (500 caractères maximum).".into());
    }

    let state = app.state::<AppState>();
    if state.session(role).is_none() {
        return Err(format!("Le compte {} n'est pas connecté.", role.key()));
    }
    state
        .outbox
        .try_send(Outgoing {
            role,
            text,
            queued_at: Instant::now(),
        })
        .map_err(|_| "File d'envoi pleine, réessaie dans un instant.".to_string())
}