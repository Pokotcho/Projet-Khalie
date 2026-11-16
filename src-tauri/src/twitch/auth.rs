use serde::{Deserialize, Serialize};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use std::sync::atomic::Ordering;

use crate::AppState;

// Identifiant public de ton application Twitch.
const CLIENT_ID: &str = "1uwt92895h196h07j7asl0fme7smlo";
const SCOPES: &str = "user:read:chat user:write:chat";

const REVOKE_URL: &str = "https://id.twitch.tv/oauth2/revoke";
const CANCELLED: &str = "annulée";

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

// Réponse de Twitch à la demande de code (renvoyée aussi à l'interface).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub expires_in: u64,
    pub interval: u64,
    pub user_code: String,
    pub verification_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct TwitchError {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ValidateResponse {
    login: String,
    user_id: String,
}

// Session gardée en mémoire (jamais envoyée à l'interface : elle contient les tokens).
#[derive(Debug, Clone)]
pub struct TwitchSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub login: String,
    pub user_id: String,
}

// Ce qu'on a le droit d'envoyer à l'interface.
#[derive(Debug, Clone, Serialize)]
pub struct LoginInfo {
    pub login: String,
    pub user_id: String,
}

async fn request_device_code(client: &reqwest::Client) -> Result<DeviceCodeResponse, String> {
    let resp = client
        .post(DEVICE_URL)
        .form(&[("client_id", CLIENT_ID), ("scopes", SCOPES)])
        .send()
        .await
        .map_err(|e| format!("Impossible de joindre Twitch : {e}"))?;

    if !resp.status().is_success() {
        let err: TwitchError = resp.json().await.unwrap_or_default();
        return Err(format!(
            "Twitch a refusé la demande : {}",
            err.message.unwrap_or_else(|| "erreur inconnue".into())
        ));
    }
    resp.json::<DeviceCodeResponse>()
        .await
        .map_err(|e| format!("Réponse de Twitch illisible : {e}"))
}

async fn poll_for_token(
    client: &reqwest::Client,
    dc: &DeviceCodeResponse,
    app: &AppHandle,
    attempt_id: u64,
) -> Result<TokenResponse, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in);
    let mut interval = dc.interval.max(1);

    loop {
        if is_cancelled(app, attempt_id) {
            return Err(CANCELLED.into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Le code a expiré, recommence la connexion.".into());
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if is_cancelled(app, attempt_id) {
            return Err(CANCELLED.into());
        }

        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", CLIENT_ID),
                ("scopes", SCOPES),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", DEVICE_GRANT),
            ])
            .send()
            .await
            .map_err(|e| format!("Impossible de joindre Twitch : {e}"))?;

        if resp.status().is_success() {
            return resp
                .json::<TokenResponse>()
                .await
                .map_err(|e| format!("Réponse de Twitch illisible : {e}"));
        }

        let err: TwitchError = resp.json().await.unwrap_or_default();
        match err.message.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            other => {
                return Err(format!(
                    "Connexion échouée : {}",
                    other.unwrap_or("erreur inconnue")
                ))
            }
        }
    }
}

fn is_cancelled(app: &AppHandle, attempt_id: u64) -> bool {
    app.state::<AppState>().login_attempt.load(Ordering::SeqCst) != attempt_id
}

async fn revoke_token(client: &reqwest::Client, token: &str) -> Result<(), String> {
    let resp = client
        .post(REVOKE_URL)
        .form(&[("client_id", CLIENT_ID), ("token", token)])
        .send()
        .await
        .map_err(|e| format!("Impossible de joindre Twitch : {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("Révocation refusée (code {})", resp.status()))
    }
}

async fn validate_token(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<ValidateResponse, String> {
    let resp = client
        .get(VALIDATE_URL)
        .header("Authorization", format!("OAuth {access_token}"))
        .send()
        .await
        .map_err(|e| format!("Impossible de joindre Twitch : {e}"))?;

    if !resp.status().is_success() {
        return Err("Le token reçu n'a pas été validé par Twitch.".into());
    }
    resp.json::<ValidateResponse>()
        .await
        .map_err(|e| format!("Réponse de Twitch illisible : {e}"))
}

async fn complete_login(
    client: &reqwest::Client,
    dc: &DeviceCodeResponse,
    app: &AppHandle,
    attempt_id: u64,
) -> Result<LoginInfo, String> {
    let token = poll_for_token(client, dc, app, attempt_id).await?;
    let who = validate_token(client, &token.access_token).await?;

    // Si l'utilisateur a annulé entre-temps, on invalide le token qu'on vient d'obtenir.
    if is_cancelled(app, attempt_id) {
        let _ = revoke_token(client, &token.access_token).await;
        return Err(CANCELLED.into());
    }

    let session = TwitchSession {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_in: token.expires_in,
        login: who.login.clone(),
        user_id: who.user_id.clone(),
    };

    let state = app.state::<AppState>();
    *state.twitch.lock().unwrap() = Some(session);

    Ok(LoginInfo {
        login: who.login,
        user_id: who.user_id,
    })
}

// --- Commandes appelées par l'interface ---

#[tauri::command]
pub async fn twitch_login_start(app: AppHandle) -> Result<DeviceCodeResponse, String> {
    // Nouveau numéro de tentative, et refus si on est déjà connecté.
    let attempt_id = {
        let state = app.state::<AppState>();
        if state.twitch.lock().unwrap().is_some() {
            return Err("Déjà connecté : déconnecte-toi d'abord.".into());
        }
        state.login_attempt.fetch_add(1, Ordering::SeqCst) + 1
    };

    let client = reqwest::Client::new();
    let dc = request_device_code(&client).await?;

    let dc_for_task = dc.clone();
    tauri::async_runtime::spawn(async move {
        match complete_login(&client, &dc_for_task, &app, attempt_id).await {
            Ok(info) => {
                let _ = app.emit("twitch-login-success", info);
            }
            Err(e) if e == CANCELLED => {}
            Err(e) => {
                let _ = app.emit("twitch-login-error", e);
            }
        }
    });

    Ok(dc)
}

#[tauri::command]
pub fn twitch_status(state: tauri::State<'_, AppState>) -> Option<LoginInfo> {
    state.twitch.lock().unwrap().as_ref().map(|s| LoginInfo {
        login: s.login.clone(),
        user_id: s.user_id.clone(),
    })
}

#[tauri::command]
pub async fn twitch_logout(app: AppHandle) -> Result<(), String> {
    // On annule toute connexion en attente et on retire la session de la mémoire.
    let session = {
        let state = app.state::<AppState>();
        state.login_attempt.fetch_add(1, Ordering::SeqCst);
        let taken = state.twitch.lock().unwrap().take();
        taken
    };

    if let Some(s) = session {
        let client = reqwest::Client::new();

        match revoke_token(&client, &s.access_token).await {
            Ok(()) => println!("Token révoqué chez Twitch."),
            Err(e) => println!("Révocation impossible : {e}"),
        }

        // Vérification : Twitch doit maintenant refuser ce token.
        match validate_token(&client, &s.access_token).await {
            Ok(_) => println!("ATTENTION : le token est encore valide après la révocation !"),
            Err(_) => println!("Vérifié : Twitch ne reconnaît plus ce token."),
        }
    }
    Ok(())
}