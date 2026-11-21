use std::time::Duration;

use serde_json::Value;

use super::auth::{TwitchSession, CLIENT_ID};

const API: &str = "https://api.twitch.tv/helix";

fn net(e: reqwest::Error) -> String {
    format!("Twitch injoignable : {e}")
}

fn authed(req: reqwest::RequestBuilder, s: &TwitchSession) -> reqwest::RequestBuilder {
    req.header("Authorization", format!("Bearer {}", s.access_token))
        .header("Client-Id", CLIENT_ID)
}

// Transforme une réponse d'erreur de Twitch en texte lisible.
async fn describe_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    let message = resp
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v["message"].as_str().map(|m| m.to_string()))
        .unwrap_or_default();
    match status.as_u16() {
        401 => format!("autorisation refusée ({message}) : reconnecte le compte du streamer"),
        429 => "trop de demandes, réessaie dans un instant".to_string(),
        _ => format!("code {status} {message}"),
    }
}

// Retrouve l'identifiant et le nom affiché d'un utilisateur à partir de son pseudo.
pub async fn get_user(
    client: &reqwest::Client,
    s: &TwitchSession,
    login: &str,
) -> Result<(String, String), String> {
    let url = format!("{API}/users?login={}", urlencoding::encode(login));
    let resp = authed(client.get(url), s).send().await.map_err(net)?;
    if !resp.status().is_success() {
        return Err(describe_error(resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;
    let user = &v["data"][0];
    match (user["id"].as_str(), user["display_name"].as_str()) {
        (Some(id), Some(name)) => Ok((id.to_string(), name.to_string())),
        _ => Err(format!("aucun utilisateur nommé « {login} »")),
    }
}

// Cherche une catégorie (jeu) par son nom : on prend le nom exact s'il existe, sinon le premier résultat.
pub async fn search_category(
    client: &reqwest::Client,
    s: &TwitchSession,
    query: &str,
) -> Result<(String, String), String> {
    let url = format!(
        "{API}/search/categories?first=10&query={}",
        urlencoding::encode(query)
    );
    let resp = authed(client.get(url), s).send().await.map_err(net)?;
    if !resp.status().is_success() {
        return Err(describe_error(resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;

    let list = v["data"].as_array().cloned().unwrap_or_default();
    let wanted = query.to_lowercase();
    let best = list
        .iter()
        .find(|c| {
            c["name"]
                .as_str()
                .map(|n| n.to_lowercase() == wanted)
                .unwrap_or(false)
        })
        .or_else(|| list.first());

    match best {
        Some(c) => match (c["id"].as_str(), c["name"].as_str()) {
            (Some(id), Some(name)) => Ok((id.to_string(), name.to_string())),
            _ => Err("réponse de Twitch incomplète".into()),
        },
        None => Err(format!("aucune catégorie trouvée pour « {query} »")),
    }
}

async fn patch_channel(
    client: &reqwest::Client,
    s: &TwitchSession,
    body: Value,
) -> Result<(), String> {
    let url = format!("{API}/channels?broadcaster_id={}", s.user_id);
    let resp = authed(client.patch(url), s)
        .json(&body)
        .send()
        .await
        .map_err(net)?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(describe_error(resp).await)
    }
}

pub async fn set_title(
    client: &reqwest::Client,
    s: &TwitchSession,
    title: &str,
) -> Result<(), String> {
    patch_channel(client, s, serde_json::json!({ "title": title })).await
}

pub async fn set_category(
    client: &reqwest::Client,
    s: &TwitchSession,
    game_id: &str,
) -> Result<(), String> {
    patch_channel(client, s, serde_json::json!({ "game_id": game_id })).await
}

// Crée un clip de la chaîne en direct, attend qu'il existe, et renvoie son lien public.
pub async fn create_clip(client: &reqwest::Client, s: &TwitchSession) -> Result<String, String> {
    let url = format!("{API}/clips?broadcaster_id={}", s.user_id);
    let resp = authed(client.post(url), s).send().await.map_err(net)?;
    if !resp.status().is_success() {
        return Err(describe_error(resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;
    let clip_id = v["data"][0]["id"]
        .as_str()
        .ok_or("réponse de Twitch sans identifiant de clip")?
        .to_string();

    // La création est asynchrone : on interroge Twitch jusqu'à ce que le clip apparaisse.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let url = format!("{API}/clips?id={clip_id}");
        let resp = authed(client.get(url), s).send().await.map_err(net)?;
        if resp.status().is_success() {
            let v: Value = resp
                .json()
                .await
                .map_err(|e| format!("réponse illisible : {e}"))?;
            if let Some(link) = v["data"][0]["url"].as_str() {
                return Ok(link.to_string());
            }
        }
    }
    Err("le clip n'est pas apparu après 20 secondes (la création a probablement échoué)".into())
}

// Shoutout officiel de Twitch vers la chaîne cible, au nom du streamer.
pub async fn shoutout(
    client: &reqwest::Client,
    s: &TwitchSession,
    target_id: &str,
) -> Result<(), String> {
    let url = format!(
        "{API}/chat/shoutouts?from_broadcaster_id={0}&to_broadcaster_id={1}&moderator_id={0}",
        s.user_id, target_id
    );
    let resp = authed(client.post(url), s).send().await.map_err(net)?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(describe_error(resp).await)
    }
}

// Date (texte RFC 3339) à laquelle un viewer a suivi la chaîne, ou None s'il ne la suit pas.
pub async fn followed_at(
    client: &reqwest::Client,
    s: &TwitchSession,
    user_id: &str,
) -> Result<Option<String>, String> {
    let url = format!(
        "{API}/channels/followers?broadcaster_id={}&user_id={}",
        s.user_id, user_id
    );
    let resp = authed(client.get(url), s).send().await.map_err(net)?;
    if !resp.status().is_success() {
        return Err(describe_error(resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;
    Ok(v["data"][0]["followed_at"].as_str().map(|d| d.to_string()))
}

// La chaîne du streamer est-elle en direct ?
pub async fn is_live(client: &reqwest::Client, s: &TwitchSession) -> Result<bool, String> {
    let url = format!("{API}/streams?user_id={}", s.user_id);
    let resp = authed(client.get(url), s).send().await.map_err(net)?;
    if !resp.status().is_success() {
        return Err(describe_error(resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible : {e}"))?;
    Ok(v["data"].as_array().map_or(false, |list| !list.is_empty()))
}