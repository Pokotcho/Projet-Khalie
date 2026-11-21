use chrono::{DateTime, Datelike, Months, Utc};
use tauri::{AppHandle, Manager};

use super::{counter_adjust, counter_reset, log, Command, Kind, Level};
use crate::twitch::api;

use std::time::Instant;
use crate::outbox::Outgoing;
use crate::twitch::chat::ChatMessage;

use crate::twitch::Role;
use crate::AppState;

fn who(msg: &ChatMessage) -> &str {
    if msg.display_name.is_empty() {
        &msg.login
    } else {
        &msg.display_name
    }
}

// Remplace [@pseudo], [pseudo] puis les champs propres à chaque commande.
fn fill(template: &str, msg: &ChatMessage, extras: &[(&str, String)]) -> String {
    let name = who(msg);
    let mut out = template
        .replace("[@pseudo]", &format!("@{name}"))
        .replace("[pseudo]", name);
    for (key, value) in extras {
        out = out.replace(*key, value.as_str());
    }
    out
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n > 1 { many } else { one })
}

// Durée en français entre deux dates : "2 mois et 5 jours".
fn human_duration(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    let (a, b) = (from.date_naive(), to.date_naive());
    let mut months = (b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32;
    if b.day() < a.day() {
        months -= 1;
    }
    let months = months.max(0);
    let anchor = a.checked_add_months(Months::new(months as u32)).unwrap_or(a);
    let days = (b - anchor).num_days().max(0);

    let years = (months / 12) as i64;
    let months = (months % 12) as i64;

    let mut parts: Vec<String> = Vec::new();
    if years > 0 {
        parts.push(plural(years, "an", "ans"));
    }
    if months > 0 {
        parts.push(plural(months, "mois", "mois"));
    }
    if days > 0 {
        parts.push(plural(days, "jour", "jours"));
    }
    match parts.len() {
        0 => "moins d'un jour".to_string(),
        1 => parts.remove(0),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} et {}", parts.join(", "), last)
        }
    }
}

// Commandes qui agissent sur la chaîne : elles s'exécutent TOUJOURS avec le compte du streamer.
// Renvoie le texte à écrire dans le chat, ou None si la commande reste silencieuse.
async fn run_streamer_action(
    app: &AppHandle,
    command: &Command,
    msg: &ChatMessage,
    args: &str,
) -> Option<String> {
    let (streamer, client) = {
        let state = app.state::<AppState>();
        let s = state.session(Role::Streamer);
        let c = state.bot.client.clone();
        (s, c)
    };
    let Some(streamer) = streamer else {
        log(
            app,
            format!("!{} : le compte du streamer n'est pas connecté", command.name),
        );
        return None;
    };

    match command.kind {
        Kind::Title => {
            if args.is_empty() {
                log(app, format!("!{} : aucun titre indiqué", command.name));
                return None;
            }
            let title: String = args.chars().take(140).collect();
            match api::set_title(&client, &streamer, &title).await {
                Ok(()) => log(app, format!("Titre changé par {} : {title}", who(msg))),
                Err(e) => log(
                    app,
                    format!("!{} : changement du titre impossible : {e}", command.name),
                ),
            }
            None
        }
        Kind::Category => {
            if args.is_empty() {
                log(app, format!("!{} : aucune catégorie indiquée", command.name));
                return None;
            }
            match api::search_category(&client, &streamer, args).await {
                Ok((id, name)) => match api::set_category(&client, &streamer, &id).await {
                    Ok(()) => log(app, format!("Catégorie changée par {} : {name}", who(msg))),
                    Err(e) => log(
                        app,
                        format!("!{} : changement de catégorie impossible : {e}", command.name),
                    ),
                },
                Err(e) => log(app, format!("!{} : {e}", command.name)),
            }
            None
        }
        Kind::Shoutout => {
            let target = args
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('@');
            if target.is_empty() {
                log(app, format!("!{} : indique un pseudo (!so @pseudo)", command.name));
                return None;
            }
            match api::get_user(&client, &streamer, target).await {
                Ok((id, name)) => match api::shoutout(&client, &streamer, &id).await {
                    Ok(()) => log(app, format!("Shoutout envoyé à {name} (demandé par {})", who(msg))),
                    Err(e) => log(
                        app,
                        format!("!{} : shoutout impossible : {e}", command.name),
                    ),
                },
                Err(e) => log(app, format!("!{} : {e}", command.name)),
            }
            None
        }
        Kind::Clip => match api::create_clip(&client, &streamer).await {
            Ok(link) => Some(fill(&command.response, msg, &[("[lien]", link)])),
            Err(e) => {
                log(app, format!("!{} : clip impossible : {e}", command.name));
                Some(fill(
                    "[@pseudo] le clip n'a pas pu être créé (la chaîne est-elle en direct ?)",
                    msg,
                    &[],
                ))
            }
        },
        Kind::Followage => {
            if msg.user_id == streamer.user_id {
                return Some(fill("[@pseudo] tu es le streamer de cette chaîne !", msg, &[]));
            }
            match api::followed_at(&client, &streamer, &msg.user_id).await {
                Ok(Some(date)) => match DateTime::parse_from_rfc3339(&date) {
                    Ok(parsed) => {
                        let since = parsed.with_timezone(&Utc);
                        let duration = human_duration(since, Utc::now());
                        Some(fill(
                            &command.response,
                            msg,
                            &[
                                ("[duree]", duration),
                                ("[date]", since.format("%d/%m/%Y").to_string()),
                            ],
                        ))
                    }
                    Err(_) => {
                        log(app, format!("!{} : date illisible ({date})", command.name));
                        None
                    }
                },
                Ok(None) => Some(fill("[@pseudo] ne suit pas encore la chaîne.", msg, &[])),
                Err(e) => {
                    log(app, format!("!{} : followage impossible : {e}", command.name));
                    None
                }
            }
        }
        _ => None,
    }
}

// Appelée pour chaque message reçu du chat.
pub async fn handle(app: AppHandle, msg: ChatMessage) {
    // Jamais de réaction aux messages du bot lui-même : pas de boucle possible.
    if msg.from_bot {
        return;
    }

    let Some(rest) = msg.text.trim().strip_prefix('!') else {
        return;
    };
    let rest = rest.trim_start();
    let Some(first) = rest.split_whitespace().next() else {
        return;
    };
    let name = first.to_lowercase();
    // Arguments : tout ce qui suit le nom de la commande.
    let args = rest[first.len()..].trim().to_string();

    // On copie ce dont on a besoin, sans garder de verrou pendant l'envoi.
    let (enabled, command, public_list, counter) = {
        let state = app.state::<AppState>();
        let config = state.bot.config.read().unwrap();
        let command = config.commands.iter().find(|c| c.name == name).cloned();
        let list: Vec<String> = config
            .commands
            .iter()
            .filter(|c| c.enabled && c.level == Level::Everyone)
            .map(|c| format!("!{}", c.name))
            .collect();
        (config.enabled, command, list, config.counter)
    };

    if !enabled {
        return;
    }
    let Some(command) = command else {
        return;
    };
    if !command.enabled {
        return;
    }

    // Droits d'accès.
    let allowed = match command.level {
        Level::Everyone => true,
        Level::Moderator => msg.is_moderator || msg.from_streamer,
        Level::Streamer => msg.from_streamer,
    };
    if !allowed {
        log(
            &app,
            format!("!{} refusée à {} (droits insuffisants)", command.name, who(&msg)),
        );
        return;
    }

    // Délais : le streamer et les modérateurs n'y sont pas soumis.
    let bypass = msg.from_streamer || msg.is_moderator;
    let can_use = {
        let state = app.state::<AppState>();
        let ok = state.bot.try_use(
            &command.name,
            &msg.user_id,
            command.global_cooldown,
            command.user_cooldown,
            bypass,
        );
        ok
    };
    if !can_use {
        log(&app, format!("!{} ignorée (délai non écoulé)", command.name));
        return;
    }

    // Exécution selon le type de commande.
    let reply: Option<String> = match command.kind {
        Kind::Text => Some(fill(
            &command.response,
            &msg,
            &[("[compteur]", counter.to_string())],
        )),
        Kind::List => Some(fill(
            &command.response,
            &msg,
            &[
                ("[commandes]", public_list.join(", ")),
                ("[compteur]", counter.to_string()),
            ],
        )),
        Kind::Counter => {
            let value = counter_adjust(&app, 1);
            Some(fill(&command.response, &msg, &[("[compteur]", value.to_string())]))
        }
        Kind::CounterReset => {
            let value = counter_reset(&app);
            Some(fill(&command.response, &msg, &[("[compteur]", value.to_string())]))
        }
        Kind::Title | Kind::Category | Kind::Shoutout | Kind::Clip | Kind::Followage => {
            run_streamer_action(&app, &command, &msg, &args).await
        }
    };

    // Commande silencieuse (ou échec déjà journalisé) : rien à écrire.
    let Some(reply) = reply else {
        return;
    };
    let reply: String = reply.chars().take(500).collect();
    // La réponse part dans la file d'envoi, qui respecte les limites de Twitch.
    let item = Outgoing {
        role: command.reply_as,
        text: reply,
        queued_at: Instant::now(),
    };
    let queued = {
        let state = app.state::<AppState>();
        let r = state.outbox.try_send(item);
        r
    };
    match queued {
        Ok(()) => log(
            &app,
            format!("!{} par {} : réponse mise en file", command.name, who(&msg)),
        ),
        Err(_) => log(
            &app,
            format!("!{} : file d'envoi pleine, réponse abandonnée", command.name),
        ),
    }
}