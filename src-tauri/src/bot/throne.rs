// Dons Throne : récupère les événements du relais Cloudflare, les enregistre dans la base
// de données et remercie les donateurs dans le chat.
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager};

use super::{log, ThroneSettings};
use crate::db::Donation;
use crate::outbox::Outgoing;
use crate::twitch::storage;
use crate::twitch::Role;
use crate::AppState;

// Nom du token du relais dans le trousseau Windows.
const TOKEN_NAME: &str = "throne-relay-token";
// Dernier numéro d'événement du relais déjà traité (stocké dans la table meta).
const SEQ_KEY: &str = "throne_last_seq";
const POLL: Duration = Duration::from_secs(10);
const SPACING: Duration = Duration::from_secs(5);
const PAGE_SIZE: usize = 50;
// Un don reçu depuis plus longtemps que ça est enregistré, mais n'est plus remercié dans le chat.
const ANNOUNCE_MAX_AGE_SECS: i64 = 6 * 3600;

// --- Mise en forme des messages ---

// Remplace les retours à la ligne, tabulations et espaces multiples par un seul espace.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// 10000 + "USD" -> "100,00 USD" ; 123456 + "EUR" -> "1 234,56 EUR".
fn format_amount(minor: i64, currency: &str) -> String {
    let abs = minor.unsigned_abs();
    let (whole, cents) = (abs / 100, abs % 100);
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(ch);
    }
    let sign = if minor < 0 { "-" } else { "" };
    if currency.is_empty() {
        format!("{sign}{grouped},{cents:02}")
    } else {
        format!("{sign}{grouped},{cents:02} {currency}")
    }
}

// Remplace les champs [xxx] en un seul passage : ce qui est écrit par un donateur
// (pseudo, message) n'est jamais interprété comme un champ.
fn fill_template(template: &str, fields: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = template;
    'outer: while !rest.is_empty() {
        for (key, value) in fields {
            if let Some(after) = rest.strip_prefix(*key) {
                out.push_str(value);
                rest = after;
                continue 'outer;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn build_message(s: &ThroneSettings, d: &Donation) -> String {
    let template = match d.kind.as_str() {
        "gift_purchased" => &s.gift_message,
        "contribution_purchased" => &s.contribution_message,
        _ => &s.crowdfunded_message,
    };
    let donor = match d.donor.as_deref() {
        Some(n) if !n.eq_ignore_ascii_case("anonymous") => clean(n),
        _ => clean(&s.anonymous_name),
    };
    let item = d.item.as_deref().map(clean).unwrap_or_default();
    let amount = match d.amount {
        Some(a) => format_amount(a, d.currency.as_deref().unwrap_or("")),
        None => String::new(),
    };
    let message = d.message.as_deref().map(clean).unwrap_or_default();

    fill_template(
        template,
        &[
            ("[donateur]", &donor),
            ("[objet]", &item),
            ("[montant]", &amount),
            ("[message]", &message),
        ],
    )
    .chars()
    .take(500)
    .collect()
}

fn queue(app: &AppHandle, role: Role, text: String) -> Result<(), ()> {
    let state = app.state::<AppState>();
    state
        .outbox
        .try_send(Outgoing {
            role,
            text,
            queued_at: Instant::now(),
        })
        .map_err(|_| ())
}

// --- Lecture des événements du relais ---

fn parse_event(ev: &Value) -> Option<Donation> {
    let kind = ev["event_type"].as_str()?;
    if !matches!(
        kind,
        "gift_purchased" | "contribution_purchased" | "gift_crowdfunded"
    ) {
        return None;
    }
    let data = &ev["payload"]["data"];
    let text = |k: &str| {
        data[k]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // Throne : montant en centimes (1099 = 10,99). Les contributions utilisent « amount »,
    // les cadeaux « price ».
    let field = if kind == "contribution_purchased" { "amount" } else { "price" };
    let amount = data[field]
        .as_i64()
        .or_else(|| data[field].as_f64().map(|f| f.round() as i64));

    Some(Donation {
        event_id: ev["event_id"].as_str()?.to_string(),
        received_at: ev["received_at"].as_i64().unwrap_or(0),
        kind: kind.to_string(),
        donor: text("gifter_username"),
        item: text("item_name"),
        amount,
        currency: text("currency"),
        message: text("message"),
        surprise: data["is_surprise_gift"].as_bool().unwrap_or(false),
    })
}

// Adresse du relais : https obligatoire, sans « / » final.
fn relay_base(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    if u.starts_with("https://") && u.len() > 10 {
        Some(u.to_string())
    } else {
        None
    }
}

enum Poll {
    Idle, // rien de plus à lire pour le moment
    More, // une page pleine : il en reste probablement
}

async fn poll_once(app: &AppHandle, client: &reqwest::Client) -> Result<Poll, String> {
    let settings = app.state::<AppState>().bot.config.read().unwrap().throne.clone();
    let Some(base) = relay_base(&settings.relay_url) else {
        return Ok(Poll::Idle); // pas encore configuré
    };
    let Some(token) = storage::load_secret(TOKEN_NAME) else {
        return Ok(Poll::Idle);
    };

    let after = app
        .state::<AppState>()
        .db
        .meta_get(SEQ_KEY)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let resp = client
        .get(format!("{base}/events?after={after}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("relais injoignable ({e})"))?;
    let status = resp.status();
    if status.as_u16() == 401 {
        return Err("token refusé par le relais (à vérifier dans l'onglet Événements)".into());
    }
    if !status.is_success() {
        return Err(format!("réponse inattendue du relais (code {})", status.as_u16()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("réponse illisible ({e})"))?;
    let events = body["events"].as_array().cloned().unwrap_or_default();

    let now = chrono::Utc::now().timestamp();
    let mut announced = false;
    for ev in &events {
        let seq = ev["seq"].as_i64().ok_or("événement sans numéro")?;

        if let Some(d) = parse_event(ev) {
            // Enregistré une seule fois : « false » = déjà connu, donc pas de nouveau message.
            let is_new = app.state::<AppState>().db.record_donation(&d)?;
            if is_new {
                log(
                    app,
                    format!(
                        "Don Throne reçu ({}) : {} — {}",
                        d.kind,
                        d.donor.as_deref().unwrap_or("anonyme"),
                        d.item.as_deref().unwrap_or("?")
                    ),
                );
                let recent = now - d.received_at <= ANNOUNCE_MAX_AGE_SECS;
                if settings.enabled && recent {
                    if announced {
                        tokio::time::sleep(SPACING).await;
                    }
                    if queue(app, settings.reply_as, build_message(&settings, &d)).is_err() {
                        log(app, "Message de remerciement abandonné : file d'envoi pleine");
                    }
                    announced = true;
                }
            }
        }
        // On avance le curseur seulement une fois l'événement traité.
        app.state::<AppState>().db.meta_set(SEQ_KEY, &seq.to_string())?;
    }

    Ok(if events.len() >= PAGE_SIZE { Poll::More } else { Poll::Idle })
}

// Tâche de fond : interroge le relais toutes les 10 secondes.
pub async fn poll_loop(app: AppHandle) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default();
    let mut last_error = String::new();
    let mut delay = POLL;

    loop {
        tokio::time::sleep(delay).await;
        delay = POLL;
        match poll_once(&app, &client).await {
            Ok(poll) => {
                if !last_error.is_empty() {
                    log(&app, "Relais Throne : connexion rétablie");
                    last_error.clear();
                }
                if matches!(poll, Poll::More) {
                    delay = Duration::from_secs(1);
                }
            }
            Err(e) => {
                // On ne répète pas la même erreur dans le journal.
                if e != last_error {
                    log(&app, format!("Relais Throne : {e}"));
                    last_error = e;
                }
                delay = Duration::from_secs(30);
            }
        }
    }
}

// --- Commandes appelées par l'interface ---

#[tauri::command]
pub fn throne_has_token() -> bool {
    storage::load_secret(TOKEN_NAME).is_some()
}

#[tauri::command]
pub fn throne_set_token(token: String) -> Result<(), String> {
    let t = token.trim();
    if t.is_empty() || t.len() > 200 || t.chars().any(char::is_whitespace) {
        return Err("Token invalide (ni vide, ni d'espaces, 200 caractères maximum).".into());
    }
    storage::save_secret(TOKEN_NAME, t)
}

#[tauri::command]
pub fn throne_clear_token() {
    storage::clear_secret(TOKEN_NAME);
}

// Vérifie l'adresse et le token en demandant au relais une page d'événements.
#[tauri::command]
pub async fn throne_test_connection(app: AppHandle) -> Result<String, String> {
    let settings = app.state::<AppState>().bot.config.read().unwrap().throne.clone();
    let base = relay_base(&settings.relay_url)
        .ok_or("Renseigne l'adresse du relais (elle commence par https://).")?;
    let token = storage::load_secret(TOKEN_NAME).ok_or("Aucun token enregistré.")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    // Numéro volontairement énorme : on ne récupère aucun événement, on teste seulement l'accès.
    let resp = client
        .get(format!("{base}/events?after=999999999999"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("Relais injoignable : {e}"))?;
    match resp.status().as_u16() {
        200 => Ok("Connexion au relais réussie.".into()),
        401 => Err("Le relais refuse le token : vérifie-le.".into()),
        404 => Err("Adresse introuvable : vérifie l'adresse du relais.".into()),
        code => Err(format!("Réponse inattendue du relais (code {code}).")),
    }
}

// Envoie dans le chat un message d'exemple, avec les réglages actuels. Rien n'est enregistré.
#[tauri::command]
pub fn throne_test_message(app: AppHandle, kind: String) -> Result<(), String> {
    let settings = app.state::<AppState>().bot.config.read().unwrap().throne.clone();
    let sample = match kind.as_str() {
        "gift_purchased" => Donation {
            event_id: String::new(),
            received_at: 0,
            kind: "gift_purchased".into(),
            donor: Some("marie_123".into()),
            item: Some("AirPods Max".into()),
            amount: Some(10000),
            currency: Some("USD".into()),
            message: Some("Keep doing what you're doing!".into()),
            surprise: false,
        },
        "contribution_purchased" => Donation {
            event_id: String::new(),
            received_at: 0,
            kind: "contribution_purchased".into(),
            donor: Some("Anonymous".into()),
            item: Some("AirPods Max".into()),
            amount: Some(2550),
            currency: Some("EUR".into()),
            message: None,
            surprise: false,
        },
        "gift_crowdfunded" => Donation {
            event_id: String::new(),
            received_at: 0,
            kind: "gift_crowdfunded".into(),
            donor: None,
            item: Some("AirPods Max".into()),
            amount: Some(10000),
            currency: Some("USD".into()),
            message: None,
            surprise: false,
        },
        _ => return Err("Type de don inconnu.".into()),
    };
    queue(&app, settings.reply_as, build_message(&settings, &sample))
        .map_err(|_| "File d'envoi pleine : réessaie dans un instant.".to_string())
}
