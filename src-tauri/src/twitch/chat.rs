use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio_tungstenite::tungstenite::Message;

use super::auth::{TwitchSession, CLIENT_ID};
use crate::AppState;

const EVENTSUB_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
const SUBSCRIBE_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
const USERS_URL: &str = "https://api.twitch.tv/helix/users";
const SEND_URL: &str = "https://api.twitch.tv/helix/chat/messages";

// Un message de chat, tel qu'envoyé à l'interface.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub message_id: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub text: String,
    pub is_self: bool, // true si le message vient du bot lui-même
}

// Démarre (ou redémarre) la lecture du chat avec la session en mémoire.
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
    *state.channel.lock().unwrap() = None;
    let _ = app.emit("chat-status", "Chat arrêté");
}

// Boucle principale : se connecte, lit, et se reconnecte si ça casse.
async fn chat_loop(app: AppHandle) {
    let client = reqwest::Client::new();
    let mut backoff: u64 = 1;

    loop {
        let session = {
            let state = app.state::<AppState>();
            let copy = state.twitch.lock().unwrap().clone();
            copy
        };
        let Some(session) = session else {
            let _ = app.emit("chat-status", "Chat arrêté (non connecté)");
            return;
        };

        let _ = app.emit("chat-status", "Connexion au chat…");

        let mut connected = false;
        let reason = run_connection(&app, &client, &session, &mut connected).await;

        if connected {
            backoff = 1;
        }
        println!("Chat déconnecté : {reason}");
        let _ = app.emit(
            "chat-status",
            format!("Chat déconnecté ({reason}). Nouvelle tentative dans {backoff} s…"),
        );

        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

// Retrouve l'identifiant Twitch d'une chaîne à partir de son pseudo.
async fn resolve_channel(
    client: &reqwest::Client,
    session: &TwitchSession,
    login: &str,
) -> Result<String, String> {
    let url = format!("{USERS_URL}?login={login}");
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", session.access_token))
        .header("Client-Id", CLIENT_ID)
        .send()
        .await
        .map_err(|e| format!("Twitch injoignable : {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("code {}", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;

    v["data"][0]["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("aucune chaîne nommée « {login} »"))
}

// Une connexion WebSocket, du début à la fin. Retourne la raison de l'arrêt.
async fn run_connection(
    app: &AppHandle,
    client: &reqwest::Client,
    session: &TwitchSession,
    connected: &mut bool,
) -> String {
    // 1. Quelle chaîne surveiller ? (vide = la chaîne du compte connecté)
    let channel_login = crate::settings::load(app).channel_login;
    let broadcaster_id = if channel_login.is_empty() {
        session.user_id.clone()
    } else {
        match resolve_channel(client, session, &channel_login).await {
            Ok(id) => id,
            Err(e) => return format!("chaîne introuvable : {e}"),
        }
    };
    {
        let state = app.state::<AppState>();
        *state.channel.lock().unwrap() = Some(broadcaster_id.clone());
    }
    let target = if channel_login.is_empty() {
        "chaîne du compte connecté".to_string()
    } else {
        channel_login.clone()
    };

    // 2. Connexion au WebSocket EventSub.
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
                            let chat = ChatMessage {
                                message_id: ev["message_id"].as_str().unwrap_or("").into(),
                                user_id: sender_id.into(),
                                login: ev["chatter_user_login"].as_str().unwrap_or("").into(),
                                display_name: ev["chatter_user_name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .into(),
                                text: ev["message"]["text"].as_str().unwrap_or("").into(),
                                is_self: sender_id == session.user_id,
                            };
                            let _ = app.emit("chat-message", chat);
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

// Demande à Twitch de nous envoyer les messages du chat de la chaîne cible.
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

// Envoie un message dans le chat de la chaîne cible, avec le compte du bot.
pub async fn send_message(
    client: &reqwest::Client,
    session: &TwitchSession,
    broadcaster_id: &str,
    text: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "broadcaster_id": broadcaster_id,
        "sender_id": session.user_id,
        "message": text
    });

    let resp = client
        .post(SEND_URL)
        .header("Authorization", format!("Bearer {}", session.access_token))
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

// Commande appelée par l'interface.
#[tauri::command]
pub async fn send_chat_message(app: AppHandle, text: String) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("Message vide.".into());
    }
    if text.chars().count() > 500 {
        return Err("Message trop long (500 caractères maximum).".into());
    }

    let (session, broadcaster_id) = {
        let state = app.state::<AppState>();
        let s = state.twitch.lock().unwrap().clone();
        let c = state.channel.lock().unwrap().clone();
        (s, c)
    };

    let session = session.ok_or("Compte du bot non connecté.")?;
    let broadcaster_id =
        broadcaster_id.ok_or("Le chat n'est pas encore connecté, patiente un instant.")?;

    send_message(&reqwest::Client::new(), &session, &broadcaster_id, &text).await
}