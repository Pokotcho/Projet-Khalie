use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use super::{storage, Role};
use crate::AppState;

// Identifiant public de ton application Twitch (pas un secret).
pub const CLIENT_ID: &str = "1uwt92895h196h07j7asl0fme7smlo";

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const REVOKE_URL: &str = "https://id.twitch.tv/oauth2/revoke";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const CANCELLED: &str = "annulée";

// Permissions demandées à chaque compte. Si tu en ajoutes plus tard pour un rôle,
// ce compte devra se reconnecter pour les accorder.
fn scopes_for(role: Role) -> &'static str {
    match role {
        Role::Streamer => {
            "user:read:chat user:write:chat channel:manage:broadcast clips:edit \
             moderator:manage:shoutouts moderator:read:followers"
        }
        Role::Bot => "user:read:chat user:write:chat",
    }
}

// Vrai s'il manque des permissions à cette session.
fn missing_scopes(role: Role, session: &TwitchSession) -> bool {
    scopes_for(role)
        .split_whitespace()
        .any(|needed| !session.scopes.iter().any(|granted| granted == needed))
}

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
    scopes: Option<Vec<String>>,
}

// Session d'un compte, gardée en mémoire et dans le trousseau Windows.
// Elle contient des tokens : elle n'est jamais envoyée à l'interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitchSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub login: String,
    pub user_id: String,
    #[serde(default)]
    pub scopes: Vec<String>, // permissions accordées par le compte
    #[serde(default)]
    pub refreshed_at: u64, // date (en secondes) du dernier renouvellement
}

// Ce qu'on a le droit d'envoyer à l'interface.
#[derive(Debug, Clone, Serialize)]
pub struct LoginInfo {
    pub role: Role,
    pub login: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct RoleError {
    role: Role,
    message: String,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_cancelled(app: &AppHandle, role: Role, attempt_id: u64) -> bool {
    app.state::<AppState>().attempt(role).load(Ordering::SeqCst) != attempt_id
}

// --- Appels à Twitch ---

async fn request_device_code(
    client: &reqwest::Client,
    role: Role,
) -> Result<DeviceCodeResponse, String> {
    let resp = client
        .post(DEVICE_URL)
        .form(&[("client_id", CLIENT_ID), ("scopes", scopes_for(role))])
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

async fn poll_for_token(
    client: &reqwest::Client,
    dc: &DeviceCodeResponse,
    app: &AppHandle,
    role: Role,
    attempt_id: u64,
) -> Result<TokenResponse, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in);
    let mut interval = dc.interval.max(1);

    loop {
        if is_cancelled(app, role, attempt_id) {
            return Err(CANCELLED.into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Le code a expiré, recommence la connexion.".into());
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if is_cancelled(app, role, attempt_id) {
            return Err(CANCELLED.into());
        }

        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", CLIENT_ID),
                ("scopes", scopes_for(role)),
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

enum TokenState {
    Valid,
    Invalid,
    Unreachable(String),
}

async fn check_token(client: &reqwest::Client, token: &str) -> TokenState {
    let resp = match client
        .get(VALIDATE_URL)
        .header("Authorization", format!("OAuth {token}"))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return TokenState::Unreachable(e.to_string()),
    };

    if resp.status().is_success() {
        TokenState::Valid
    } else if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        TokenState::Invalid
    } else {
        TokenState::Unreachable(format!("code {}", resp.status()))
    }
}

enum RefreshError {
    Rejected(String), // Twitch refuse définitivement : il faut se reconnecter
    Network(String),  // problème temporaire : on réessaiera
}

async fn refresh_session(
    client: &reqwest::Client,
    session: &TwitchSession,
) -> Result<TwitchSession, RefreshError> {
    let Some(refresh) = session.refresh_token.as_deref() else {
        return Err(RefreshError::Rejected("pas de refresh token".into()));
    };

    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .map_err(|e| RefreshError::Network(e.to_string()))?;

    let status = resp.status();
    if status.is_success() {
        let token: TokenResponse = resp
            .json()
            .await
            .map_err(|e| RefreshError::Network(e.to_string()))?;
        Ok(TwitchSession {
            access_token: token.access_token,
            // Twitch peut renvoyer un NOUVEAU refresh token : on garde le plus récent.
            refresh_token: token.refresh_token.or_else(|| session.refresh_token.clone()),
            expires_at: now_unix() + token.expires_in.unwrap_or(0),
            login: session.login.clone(),
            user_id: session.user_id.clone(),
            scopes: session.scopes.clone(),
            refreshed_at: now_unix(),
        })
    } else if status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        let err: TwitchError = resp.json().await.unwrap_or_default();
        Err(RefreshError::Rejected(
            err.message.unwrap_or_else(|| format!("code {status}")),
        ))
    } else {
        Err(RefreshError::Network(format!("code {status}")))
    }
}

// --- Connexion ---

async fn complete_login(
    client: &reqwest::Client,
    dc: &DeviceCodeResponse,
    app: &AppHandle,
    role: Role,
    attempt_id: u64,
) -> Result<LoginInfo, String> {
    let token = poll_for_token(client, dc, app, role, attempt_id).await?;
    let who = validate_token(client, &token.access_token).await?;

    // Si l'utilisateur a annulé entre-temps, on invalide le token qu'on vient d'obtenir.
    if is_cancelled(app, role, attempt_id) {
        let _ = revoke_token(client, &token.access_token).await;
        return Err(CANCELLED.into());
    }

    // Un même compte Twitch ne peut pas servir pour les deux rôles.
    // (On ne révoque pas ce token : il expirera tout seul, et on évite de toucher
    // à l'autre session de ce compte.)
    let other_role = role.other();
    let other_user_id = {
        let state = app.state::<AppState>();
        let id = state.user_id_of(other_role);
        id
    };
    if other_user_id.as_deref() == Some(who.user_id.as_str()) {
        return Err(format!(
            "Le compte « {} » est déjà connecté comme {}. Connecte un compte différent \
             (utilise une fenêtre de navigation privée).",
            who.login,
            other_role.key()
        ));
    }

    let session = TwitchSession {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now_unix() + token.expires_in.unwrap_or(0),
        login: who.login.clone(),
        user_id: who.user_id.clone(),
        scopes: who.scopes.clone().unwrap_or_default(),
        refreshed_at: now_unix(),
    };

    if let Err(e) = storage::save(role, &session) {
        log::info!("Sauvegarde de la session impossible : {e}");
    }

    {
        let state = app.state::<AppState>();
        state.set_session(role, Some(session));
    }

    // Le chat se lit avec le compte du streamer : on le démarre dès qu'il est connecté.
    if role == Role::Streamer {
        super::chat::start(app);
    }

    Ok(LoginInfo {
        role,
        login: who.login,
        user_id: who.user_id,
    })
}

// --- Commandes appelées par l'interface ---

#[tauri::command]
pub async fn twitch_login_start(app: AppHandle, role: Role) -> Result<DeviceCodeResponse, String> {
    // Nouveau numéro de tentative, et refus si ce rôle est déjà connecté.
    let attempt_id = {
        let state = app.state::<AppState>();
        if state.session(role).is_some() {
            return Err("Ce compte est déjà connecté : déconnecte-le d'abord.".into());
        }
        state.attempt(role).fetch_add(1, Ordering::SeqCst) + 1
    };

    let client = reqwest::Client::new();
    let dc = request_device_code(&client, role).await?;

    let dc_for_task = dc.clone();
    tauri::async_runtime::spawn(async move {
        match complete_login(&client, &dc_for_task, &app, role, attempt_id).await {
            Ok(info) => {
                let _ = app.emit("twitch-login-success", info);
            }
            Err(e) if e == CANCELLED => {}
            Err(e) => {
                let _ = app.emit("twitch-login-error", RoleError { role, message: e });
            }
        }
    });

    Ok(dc)
}

#[tauri::command]
pub async fn twitch_logout(app: AppHandle, role: Role) -> Result<(), String> {
    // On annule toute connexion en attente de ce rôle et on retire sa session.
    let session = {
        let state = app.state::<AppState>();
        state.attempt(role).fetch_add(1, Ordering::SeqCst);
        state.take_session(role)
    };

    storage::clear(role);

    if role == Role::Streamer {
        super::chat::stop(&app);
    }

    if let Some(s) = session {
        let client = reqwest::Client::new();

        match revoke_token(&client, &s.access_token).await {
            Ok(()) => log::info!("Token {} révoqué chez Twitch.", role.key()),
            Err(e) => log::info!("Révocation impossible : {e}"),
        }
    }
    Ok(())
}

// Relit la session sauvegardée d'un rôle et la remet en service si possible.
// Relit la session sauvegardée d'un rôle et la remet en service si possible.
async fn restore_role(app: &AppHandle, role: Role) -> Option<LoginInfo> {
    // Déjà en mémoire (ex. rechargement de la page) ?
    let current = {
        let state = app.state::<AppState>();
        let info = state.session(role).map(|s| LoginInfo {
            role,
            login: s.login.clone(),
            user_id: s.user_id.clone(),
        });
        info
    };
    if current.is_some() {
        return current;
    }

    let mut session = storage::load(role)?;
    let client = reqwest::Client::new();

    match check_token(&client, &session.access_token).await {
        TokenState::Valid => {}
        TokenState::Unreachable(e) => {
            log::info!("Twitch injoignable, session {} non activée : {e}", role.key());
            return None;
        }
        TokenState::Invalid => match refresh_session(&client, &session).await {
            Ok(new_session) => {
                if let Err(e) = storage::save(role, &new_session) {
                    log::info!("Sauvegarde de la session impossible : {e}");
                }
                log::info!("Token {} rafraîchi au démarrage.", role.key());
                session = new_session;
            }
            Err(RefreshError::Rejected(e)) => {
                log::info!("Session {} expirée, reconnexion nécessaire : {e}", role.key());
                storage::clear(role);
                return None;
            }
            Err(RefreshError::Network(e)) => {
                log::info!("Rafraîchissement impossible pour l'instant : {e}");
                return None;
            }
        },
    }

    // Anciennes sessions : on demande à Twitch la liste des permissions accordées.
    if session.scopes.is_empty() {
        if let Ok(v) = validate_token(&client, &session.access_token).await {
            session.scopes = v.scopes.unwrap_or_default();
            let _ = storage::save(role, &session);
        }
    }

    // Permissions manquantes (nouvelles fonctions) : le compte doit se reconnecter.
    if missing_scopes(role, &session) {
        log::info!(
            "Session {} : permissions manquantes, reconnexion nécessaire.",
            role.key()
        );
        let _ = revoke_token(&client, &session.access_token).await;
        storage::clear(role);
        let _ = app.emit(
            "twitch-login-error",
            RoleError {
                role,
                message: "Nouvelles permissions nécessaires : reconnecte ce compte.".into(),
            },
        );
        return None;
    }

    let info = LoginInfo {
        role,
        login: session.login.clone(),
        user_id: session.user_id.clone(),
    };
    {
        let state = app.state::<AppState>();
        state.set_session(role, Some(session));
    }

    if role == Role::Streamer {
        super::chat::start(app);
    }
    Some(info)
}

// Restaure les deux comptes au démarrage et renvoie ceux qui sont connectés.
#[tauri::command]
pub async fn twitch_restore(app: AppHandle) -> Vec<LoginInfo> {
    let mut accounts = Vec::new();
    for role in [Role::Streamer, Role::Bot] {
        if let Some(info) = restore_role(&app, role).await {
            accounts.push(info);
        }
    }
    accounts
}

// --- Maintien des sessions ---

// Tâche de fond : maintient les sessions en bon état.
pub async fn token_keeper(app: AppHandle) {
    let client = reqwest::Client::new();
    let mut last_validation: HashMap<&'static str, Instant> = HashMap::new();

    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        for role in [Role::Streamer, Role::Bot] {
            let active = {
                let state = app.state::<AppState>();
                let a = state.session(role).is_some();
                a
            };

            // Session sauvegardée mais pas encore active (ex. pas de réseau au démarrage) :
            // on réessaie de la remettre en service.
            if !active {
                if storage::load(role).is_some() {
                    if let Some(info) = restore_role(&app, role).await {
                        log::info!("Session {} remise en service.", role.key());
                        let _ = app.emit("twitch-login-success", info);
                    }
                }
                continue;
            }

            // Validation demandée par Twitch au moins une fois par heure.
            let due = last_validation
                .get(role.key())
                .map(|t| t.elapsed() >= Duration::from_secs(3600))
                .unwrap_or(true);
            keep_one(&app, &client, role, due).await;
            if due {
                last_validation.insert(role.key(), Instant::now());
            }
        }
    }
}

async fn keep_one(app: &AppHandle, client: &reqwest::Client, role: Role, validate_now: bool) {
    let current = {
        let state = app.state::<AppState>();
        let s = state.session(role);
        s
    };
    let Some(session) = current else { return };

    // On rafraîchit si le token expire dans moins de 10 minutes, ou si le jeton de
    // rafraîchissement a plus de 7 jours (pour repousser sa limite de 30 jours).
    let mut must_refresh = session.expires_at <= now_unix() + 600
        || now_unix().saturating_sub(session.refreshed_at) > 7 * 24 * 3600;

    // Validation horaire : un token que Twitch refuse (autorisation retirée…) est détecté ici.
    if validate_now && !must_refresh {
        if let TokenState::Invalid = check_token(client, &session.access_token).await {
            log::info!(
                "Token {} refusé par Twitch lors de la validation horaire.",
                role.key()
            );
            must_refresh = true;
        }
    }
    if !must_refresh {
        return;
    }

    match refresh_session(client, &session).await {
        Ok(new_session) => {
            // On ne remet la session en place que si le compte est toujours connecté.
            let applied = {
                let state = app.state::<AppState>();
                let ok = state.replace_session_if_present(role, new_session.clone());
                ok
            };
            if applied {
                if let Err(e) = storage::save(role, &new_session) {
                    log::info!("Sauvegarde de la session impossible : {e}");
                }
                log::info!("Token {} rafraîchi automatiquement.", role.key());
            } else {
                let _ = revoke_token(client, &new_session.access_token).await;
            }
        }
        Err(RefreshError::Rejected(e)) => {
            log::info!(
                "Session {} rejetée par Twitch, reconnexion nécessaire : {e}",
                role.key()
            );
            storage::clear(role);
            {
                let state = app.state::<AppState>();
                state.set_session(role, None);
            }
            if role == Role::Streamer {
                super::chat::stop(app);
            }
            let _ = app.emit("twitch-session-lost", role);
        }
        Err(RefreshError::Network(e)) => {
            log::info!("Rafraîchissement {} reporté (réseau) : {e}", role.key());
        }
    }
}