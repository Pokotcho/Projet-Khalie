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

        Kind::Follow => {
            if msg.user_id == streamer.user_id {
                return Some(fill("[@pseudo] tu es le streamer de cette chaîne !", msg, &[]));
            }
            match api::followed_at(&client, &streamer, &msg.user_id).await {
                Ok(Some(date)) => match DateTime::parse_from_rfc3339(&date) {
                    Ok(parsed) => {
                        let result = {
                            let state = app.state::<AppState>();
                            state.db.record_follow(
                                &msg.user_id,
                                &msg.login,
                                who(msg),
                                parsed.timestamp(),
                            )
                        };
                        match result {
                            Ok((first, last)) => {
                                let fmt = |t: i64| {
                                    DateTime::<Utc>::from_timestamp(t, 0)
                                        .map(|d| d.format("%d/%m/%Y").to_string())
                                        .unwrap_or_default()
                                };
                                if fmt(first) == fmt(last) {
                                    Some(fill(
                                        "[@pseudo] suit la chaîne depuis le [dernier].",
                                        msg,
                                        &[("[dernier]", fmt(last))],
                                    ))
                                } else {
                                    Some(fill(
                                        &command.response,
                                        msg,
                                        &[("[premier]", fmt(first)), ("[dernier]", fmt(last))],
                                    ))
                                }
                            }
                            Err(e) => {
                                log(app, format!("!{} : {e}", command.name));
                                None
                            }
                        }
                    }
                    Err(_) => {
                        log(app, format!("!{} : date illisible ({date})", command.name));
                        None
                    }
                },
                Ok(None) => Some(fill("[@pseudo] ne suit pas encore la chaîne.", msg, &[])),
                Err(e) => {
                    log(app, format!("!{} : follow impossible : {e}", command.name));
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

    // On compte l'activité du chat (sert aux messages automatiques).
    {
        let state = app.state::<AppState>();
        state
            .bot
            .chat_lines
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // Mémorise le viewer dans la base de données. Son message compte comme une présence
    // au stream en cours, sauf pour le streamer et les comptes exclus (bots).
    {
        let state = app.state::<AppState>();
        let excluded = state
            .bot
            .config
            .read()
            .unwrap()
            .points
            .excluded
            .iter()
            .any(|x| *x == msg.login.to_lowercase());
        state.db.record_message(
            &msg.user_id,
            &msg.login,
            who(&msg),
            !excluded && !msg.from_streamer,
        );
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
    let (enabled, command, public_list, counter, insufficient, points_name) = {
        let state = app.state::<AppState>();
        let config = state.bot.config.read().unwrap();
        let command = config.commands.iter().find(|c| c.name == name).cloned();
        let list: Vec<String> = config
            .commands
            .iter()
            .filter(|c| c.enabled && c.level == Level::Everyone)
            .map(|c| format!("!{}", c.name))
            .collect();
        (
            config.enabled,
            command,
            list,
            config.counter,
            config.points.insufficient_message.clone(),
            config.points.name.clone(),
        )
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

    // Coût en points : le streamer ne paie jamais, les modérateurs paient comme les autres.
    // On vérifie le solde avant les délais, pour ne pas consommer le délai d'une commande refusée.
    let cost = command.cost as i64;
    let pays = cost > 0 && !msg.from_streamer;
    if pays {
        let balance = {
            let state = app.state::<AppState>();
            state.db.balance(&msg.user_id)
        };
        match balance {
            Ok(b) if b >= cost => {}
            Ok(b) => {
                not_enough(&app, &command, &msg, &insufficient, &points_name, cost, b);
                return;
            }
            Err(e) => {
                log(&app, format!("!{} : {e}", command.name));
                return;
            }
        }
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

    // Paiement (une seule opération : jamais de solde négatif).
    if pays {
        let spent = {
            let state = app.state::<AppState>();
            state.db.try_spend(&msg.user_id, cost)
        };
        match spent {
            Ok(true) => {}
            Ok(false) => {
                let b = {
                    let state = app.state::<AppState>();
                    state.db.balance(&msg.user_id).unwrap_or(0)
                };
                not_enough(&app, &command, &msg, &insufficient, &points_name, cost, b);
                return;
            }
            Err(e) => {
                log(&app, format!("!{} : {e}", command.name));
                return;
            }
        }
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
        Kind::Title
        | Kind::Category
        | Kind::Shoutout
        | Kind::Clip
        | Kind::Followage
        | Kind::Follow => run_streamer_action(&app, &command, &msg, &args).await,
        Kind::Give | Kind::Take | Kind::Streak | Kind::BestStreak | Kind::Points => {
            run_points_action(&app, &command, &msg, &args).await
        }
    };

    // Commande silencieuse (ou échec déjà journalisé) : rien à écrire, et on rembourse.
    let Some(reply) = reply else {
        if pays {
            let state = app.state::<AppState>();
            if let Err(e) = state.db.refund(&msg.user_id, cost) {
                log(&app, format!("Remboursement impossible : {e}"));
            }
        }
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

// Répond « solde insuffisant » (au plus une fois toutes les 10 s par viewer et par commande).
fn not_enough(
    app: &AppHandle,
    command: &Command,
    msg: &ChatMessage,
    template: &str,
    points_name: &str,
    cost: i64,
    balance: i64,
) {
    let state = app.state::<AppState>();
    let may_reply = state.bot.try_use(
        &format!("{}:solde", command.name),
        &msg.user_id,
        0,
        10,
        false,
    );
    if !may_reply {
        return;
    }
    let text: String = fill(
        template,
        msg,
        &[
            ("[cout]", cost.to_string()),
            ("[solde]", balance.to_string()),
            ("[points]", points_name.to_string()),
        ],
    )
    .chars()
    .take(500)
    .collect();
    let _ = state.outbox.try_send(Outgoing {
        role: command.reply_as,
        text,
        queued_at: Instant::now(),
    });
}

// Pseudo cité en premier dans les arguments (sans @, en minuscules).
fn mentioned(args: &str) -> Option<String> {
    let word = args.split_whitespace().next()?;
    let login = word.trim_start_matches('@').to_lowercase();
    if !login.is_empty() && login.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(login)
    } else {
        None
    }
}

// « 50 @pseudo » (ou « @pseudo 50 ») : renvoie (montant, pseudo).
fn parse_give(args: &str) -> Option<(i64, String)> {
    let mut amount: Option<i64> = None;
    let mut login: Option<String> = None;
    for word in args.split_whitespace() {
        if word.chars().all(|c| c.is_ascii_digit()) {
            if amount.is_none() {
                amount = word.parse::<i64>().ok();
            }
        } else if login.is_none() {
            let l = word.trim_start_matches('@').to_lowercase();
            if !l.is_empty() && l.chars().all(|c| c.is_alphanumeric() || c == '_') {
                login = Some(l);
            }
        }
    }
    // Seule limite : une valeur technique pour éviter tout dépassement de nombre.
    let amount = amount.filter(|n| *n > 0 && *n <= 1_000_000_000_000)?;
    Some((amount, login?))
}

// Retrouve la personne visée : (identifiant, pseudo, nom affiché).
// Si elle n'est pas encore dans la base et que `create_if_missing` est vrai, on la cherche sur Twitch.
async fn lookup_target(
    app: &AppHandle,
    msg: &ChatMessage,
    login: &str,
    create_if_missing: bool,
) -> Result<(String, String, String), String> {
    if login == msg.login.to_lowercase() {
        return Ok((msg.user_id.clone(), msg.login.clone(), who(msg).to_string()));
    }
    let found = {
        let state = app.state::<AppState>();
        state.db.find_by_login(login)
    };
    match found {
        Ok(Some((id, name))) => return Ok((id, login.to_string(), name)),
        Ok(None) => {}
        Err(e) => {
            log(app, e);
            return Err("erreur de base de données, réessaie plus tard.".into());
        }
    }
    if !create_if_missing {
        return Err(format!("je ne connais pas encore {login}."));
    }
    let (streamer, client) = {
        let state = app.state::<AppState>();
        let s = state.session(Role::Streamer);
        let c = state.bot.client.clone();
        (s, c)
    };
    let Some(streamer) = streamer else {
        return Err("le compte du streamer n'est pas connecté.".into());
    };
    match api::get_user(&client, &streamer, login).await {
        Ok((id, name)) => Ok((id, login.to_string(), name)),
        Err(e) => Err(format!("{e}.")),
    }
}

// Commandes liées aux points : !give, !take, !streak, !beststreak, !points.
async fn run_points_action(
    app: &AppHandle,
    command: &Command,
    msg: &ChatMessage,
    args: &str,
) -> Option<String> {
    let points_name = {
        let state = app.state::<AppState>();
        let n = state.bot.config.read().unwrap().points.name.clone();
        n
    };

    match command.kind {
        Kind::Give | Kind::Take => {
            let Some((amount, login)) = parse_give(args) else {
                return Some(fill(
                    &format!("[@pseudo] usage : !{} 50 @pseudo", command.name),
                    msg,
                    &[],
                ));
            };
            let (id, login, display) = match lookup_target(app, msg, &login, true).await {
                Ok(target) => target,
                Err(e) => return Some(fill(&format!("[@pseudo] {e}"), msg, &[])),
            };
            let delta = if command.kind == Kind::Give { amount } else { -amount };
            let result = {
                let state = app.state::<AppState>();
                state.db.adjust_points(&id, &login, &display, delta)
            };
            match result {
                Ok((applied, balance)) => {
                    log(
                        app,
                        format!(
                            "!{} : {} {} {} à {} par {} (nouveau solde : {})",
                            command.name,
                            if applied >= 0 { "+" } else { "-" },
                            applied.abs(),
                            points_name,
                            display,
                            who(msg),
                            balance
                        ),
                    );
                    Some(fill(
                        &command.response,
                        msg,
                        &[
                            ("[montant]", applied.abs().to_string()),
                            ("[cible]", display),
                            ("[points]", points_name),
                            ("[solde]", balance.to_string()),
                        ],
                    ))
                }
                Err(e) => {
                    log(app, format!("!{} : {e}", command.name));
                    None
                }
            }
        }
        // !streak, !beststreak et !points : seul le texte de réponse change.
        _ => {
            let (id, display) = match mentioned(args) {
                None => (msg.user_id.clone(), who(msg).to_string()),
                Some(login) => match lookup_target(app, msg, &login, false).await {
                    Ok((id, _, display)) => (id, display),
                    Err(e) => return Some(fill(&format!("[@pseudo] {e}"), msg, &[])),
                },
            };
            let stats = {
                let state = app.state::<AppState>();
                state.db.stats(&id)
            };
            match stats {
                Ok(s) => Some(fill(
                    &command.response,
                    msg,
                    &[
                        ("[cible]", display),
                        ("[serie]", s.streak.to_string()),
                        ("[meilleure_serie]", s.best_streak.to_string()),
                        ("[streams]", s.streams.to_string()),
                        ("[solde]", s.points.to_string()),
                        ("[points]", points_name),
                    ],
                )),
                Err(e) => {
                    log(app, format!("!{} : {e}", command.name));
                    None
                }
            }
        }
    }
}