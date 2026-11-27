// Bets : paris avec les points de chaîne, avec une cagnotte commune et des cotes dynamiques.
// Toutes les actions (commandes du chat ET boutons de l'application) passent par les mêmes
// fonctions `do_*` : le moteur est la seule source de vérité, et chaque changement envoie un
// instantané (`bet-updated`) à l'interface, puis plus tard à l'overlay OBS.
use std::time::Instant;

use tauri::{AppHandle, Emitter, Manager};

use super::{log, BetSettings, Command, Kind};
use crate::db::{letter, BetOutcome, BetSnapshot, VoteResult};
use crate::outbox::Outgoing;
use crate::twitch::chat::ChatMessage;
use crate::twitch::Role;
use crate::AppState;

const MAX_OPTIONS: usize = 6;
const MAX_QUESTION: usize = 150;
const MAX_LABEL: usize = 60;
const MAX_STAKE: i64 = 1_000_000_000_000;

// --- Outils ---

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn who(msg: &ChatMessage) -> &str {
    if msg.display_name.is_empty() {
        &msg.login
    } else {
        &msg.display_name
    }
}

// Réglages du bet et nom des points.
fn settings(app: &AppHandle) -> (BetSettings, String) {
    let state = app.state::<AppState>();
    let config = state.bot.config.read().unwrap();
    (config.bets.clone(), config.points.name.clone())
}

// Nom actuel d'une commande (le nom peut avoir été changé dans l'application).
fn cmd_name(app: &AppHandle, kind: Kind, fallback: &str) -> String {
    let state = app.state::<AppState>();
    let config = state.bot.config.read().unwrap();
    config
        .commands
        .iter()
        .find(|c| c.kind == kind)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| fallback.to_string())
}

fn say(app: &AppHandle, role: Role, text: String) {
    let text: String = text.chars().take(500).collect();
    let state = app.state::<AppState>();
    if state
        .outbox
        .try_send(Outgoing {
            role,
            text,
            queued_at: Instant::now(),
        })
        .is_err()
    {
        log(app, "Bet : file d'envoi pleine, message abandonné");
    }
}

// Envoie l'état du bet à l'interface.
pub fn emit_snapshot(app: &AppHandle) {
    let snap = app.state::<AppState>().db.bet_snapshot();
    if let Ok(s) = snap {
        let _ = app.emit("bet-updated", s);
    }
}

fn letters(n: usize) -> String {
    (0..n as i64).map(letter).collect::<Vec<_>>().join(", ")
}

// "A" ou "a" -> 0
fn letter_index(s: &str) -> Option<i64> {
    let mut chars = s.trim().chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphabetic() {
        return None;
    }
    Some((c.to_ascii_uppercase() as u8 - b'A') as i64)
}

fn format_odds(odds: Option<f64>) -> String {
    match odds {
        Some(o) => format!("×{o:.2}").replace('.', ","),
        None => "—".to_string(),
    }
}

// --- Messages (fonctions pures) ---

// Remplace les champs [xxx] en un seul passage : ce qu'écrit un viewer (objet, réponses, pseudo)
// n'est jamais interprété comme un champ.
fn fill(template: &str, fields: &[(&str, String)]) -> String {
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

// « A - Moi • B - Lui • C - Égalité »
fn options_text(s: &BetSettings, snap: &BetSnapshot) -> String {
    snap.options
        .iter()
        .map(|o| {
            fill(
                &s.option_format,
                &[("[lettre]", o.letter.clone()), ("[texte]", o.label.clone())],
            )
        })
        .collect::<Vec<_>>()
        .join(&s.list_separator)
}

// « A ×1,67 • B ×3,33 • C — »
fn odds_text(s: &BetSettings, snap: &BetSnapshot) -> String {
    snap.options
        .iter()
        .map(|o| {
            fill(
                &s.cote_format,
                &[
                    ("[lettre]", o.letter.clone()),
                    ("[texte]", o.label.clone()),
                    ("[cote]", format_odds(o.odds)),
                    ("[total]", o.total.to_string()),
                    ("[votants]", o.voters.to_string()),
                ],
            )
        })
        .collect::<Vec<_>>()
        .join(&s.list_separator)
}

// Les trois messages d'annonce : l'objet, les réponses, puis comment voter.
fn announce_messages(
    s: &BetSettings,
    snap: &BetSnapshot,
    vote_cmd: &str,
    points_name: &str,
) -> [String; 3] {
    let objet = ("[objet]", snap.question.clone());
    [
        fill(&s.announce_title, &[objet.clone()]),
        fill(
            &s.announce_options,
            &[("[reponses]", options_text(s, snap)), objet],
        ),
        fill(
            &s.announce_vote,
            &[
                ("[commande_vote]", vote_cmd.to_string()),
                ("[mise_min]", s.min_stake.max(1).to_string()),
                ("[points]", points_name.to_string()),
            ],
        ),
    ]
}

fn recap_message(s: &BetSettings, snap: &BetSnapshot) -> String {
    let template = if snap.pot == 0 { &s.recap_empty } else { &s.recap };
    fill(
        template,
        &[
            ("[cotes]", odds_text(s, snap)),
            ("[objet]", snap.question.clone()),
            ("[total]", snap.pot.to_string()),
        ],
    )
}

// `answer` : (lettre, texte) de la réponse gagnante.
fn winner_message(
    s: &BetSettings,
    o: &BetOutcome,
    points_name: &str,
    question: &str,
    answer: (&str, &str),
) -> String {
    let template = if o.pot == 0 {
        &s.winner_none
    } else if o.refunded {
        &s.winner_refunded
    } else {
        match o.winners.len() {
            1 => &s.winner_one,
            2 => &s.winner_two,
            _ => &s.winner_many,
        }
    };
    fill(
        template,
        &[
            ("[total]", o.pot.to_string()),
            ("[points]", points_name.to_string()),
            ("[gagnant]", o.winners.first().map(|w| w.name.clone()).unwrap_or_default()),
            ("[second]", o.winners.get(1).map(|w| w.name.clone()).unwrap_or_default()),
            ("[autres]", o.winners.len().saturating_sub(1).to_string()),
            ("[lettre]", answer.0.to_string()),
            ("[reponse]", answer.1.to_string()),
            ("[objet]", question.to_string()),
        ],
    )
}

// --- Actions (communes au chat et à l'interface) ---

// Découpe « Objet ; Réponse A ; Réponse B ».
fn parse_bet(args: &str, cmd: &str) -> Result<(String, Vec<String>), String> {
    let parts: Vec<String> = args
        .split(';')
        .map(clean)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 3 {
        return Err(format!(
            "Usage : !{cmd} Objet du bet ; Réponse A ; Réponse B (2 à {MAX_OPTIONS} réponses, séparées par des « ; »)."
        ));
    }
    Ok((parts[0].clone(), parts[1..].to_vec()))
}

pub fn do_create(app: &AppHandle, question: String, options: Vec<String>) -> Result<(), String> {
    let question = clean(&question);
    let options: Vec<String> = options.iter().map(|o| clean(o)).collect();
    if question.is_empty() || question.chars().count() > MAX_QUESTION {
        return Err(format!("L'objet du bet doit faire entre 1 et {MAX_QUESTION} caractères."));
    }
    if options.len() < 2 || options.len() > MAX_OPTIONS {
        return Err(format!("Il faut entre 2 et {MAX_OPTIONS} réponses."));
    }
    for (i, o) in options.iter().enumerate() {
        if o.is_empty() || o.chars().count() > MAX_LABEL {
            return Err(format!("Chaque réponse doit faire entre 1 et {MAX_LABEL} caractères."));
        }
        if options[..i].iter().any(|p| p.eq_ignore_ascii_case(o)) {
            return Err(format!("La réponse « {o} » est en double."));
        }
    }

    let created = app.state::<AppState>().db.bet_create(&question, &options)?;
    if created.is_none() {
        return Err("Un bet est déjà en cours : termine-le ou annule-le d'abord.".into());
    }
    emit_snapshot(app);

    let (s, points_name) = settings(app);
    if let Some(snap) = app.state::<AppState>().db.bet_snapshot()? {
        let vote_cmd = cmd_name(app, Kind::Vote, "vote");
        for text in announce_messages(&s, &snap, &vote_cmd, &points_name) {
            say(app, s.reply_as, text);
        }
    }
    log(app, format!("Bet ouvert : {question}"));
    Ok(())
}

pub fn do_close(app: &AppHandle) -> Result<(), String> {
    let (s, _) = settings(app);
    let state = app.state::<AppState>();
    let (id, status) = state.db.bet_active()?.ok_or("Aucun bet en cours.")?;
    if status != "open" || !state.db.bet_close(id)? {
        return Err("Les votes sont déjà terminés.".into());
    }
    emit_snapshot(app);
    if let Some(snap) = state.db.bet_snapshot()? {
        say(app, s.reply_as, recap_message(&s, &snap));
    }
    log(app, "Bet : votes terminés");
    Ok(())
}

pub fn do_resolve(app: &AppHandle, letter_text: &str) -> Result<(), String> {
    let (s, points_name) = settings(app);
    let state = app.state::<AppState>();
    let (id, _) = state.db.bet_active()?.ok_or("Aucun bet en cours.")?;
    let labels = state.db.bet_options(id)?;
    let count = labels.len();
    let idx = letter_index(letter_text)
        .filter(|i| (*i as usize) < count)
        .ok_or_else(|| format!("Réponse inconnue : choisis parmi {}.", letters(count)))?;
    let question = state
        .db
        .bet_snapshot()?
        .map(|snap| snap.question)
        .unwrap_or_default();
    let outcome = state
        .db
        .bet_resolve(id, idx)?
        .ok_or("Ce bet est déjà terminé.")?;
    emit_snapshot(app);
    let text = winner_message(
        &s,
        &outcome,
        &points_name,
        &question,
        (letter(idx).as_str(), labels[idx as usize].as_str()),
    );
    say(app, s.reply_as, text);
    log(
        app,
        format!("Bet terminé : réponse {} gagnante, {} points répartis", letter(idx), outcome.pot),
    );
    Ok(())
}

pub fn do_cancel(app: &AppHandle) -> Result<(), String> {
    let (s, points_name) = settings(app);
    let state = app.state::<AppState>();
    let (id, _) = state.db.bet_active()?.ok_or("Aucun bet en cours.")?;
    let refunded = state.db.bet_cancel(id)?.ok_or("Ce bet est déjà terminé.")?;
    emit_snapshot(app);
    let question = state
        .db
        .bet_snapshot()?
        .map(|snap| snap.question)
        .unwrap_or_default();
    say(
        app,
        s.reply_as,
        fill(
            &s.cancelled,
            &[
                ("[total]", refunded.to_string()),
                ("[points]", points_name),
                ("[objet]", question),
            ],
        ),
    );
    log(app, "Bet annulé");
    Ok(())
}

// --- Votes et informations (viewers) ---

// « A 100 », « a tout », « A all » -> (réponse, montant ; None = tout)
fn parse_vote(args: &str) -> Option<(i64, Option<i64>)> {
    let mut words = args.split_whitespace();
    let idx = letter_index(words.next()?)?;
    let amount = words.next()?.to_lowercase();
    if words.next().is_some() {
        return None;
    }
    if amount == "tout" || amount == "all" {
        return Some((idx, None));
    }
    if !amount.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: i64 = amount.parse().ok()?;
    if n > MAX_STAKE {
        return None;
    }
    Some((idx, Some(n)))
}

// Réponse à un viewer : au plus une fois toutes les 10 s par viewer, pour ne pas inonder le chat.
fn reply_throttled(app: &AppHandle, command: &Command, msg: &ChatMessage, role: Role, text: String) {
    let may = app.state::<AppState>().bot.try_use(
        &format!("{}:erreur", command.name),
        &msg.user_id,
        0,
        10,
        false,
    );
    if may {
        say(app, role, format!("@{} {}", who(msg), text));
    }
}

fn vote(app: &AppHandle, command: &Command, msg: &ChatMessage, args: &str) {
    let (s, points_name) = settings(app);
    let reply = |text: String| reply_throttled(app, command, msg, s.reply_as, text);

    // Le streamer ne mise pas sur sa propre chaîne.
    if msg.from_streamer {
        return;
    }
    if msg.is_moderator && !s.mods_can_vote {
        reply("les modérateurs ne peuvent pas voter.".into());
        return;
    }
    let Some((idx, amount)) = parse_vote(args) else {
        reply(format!("usage : !{} A 100 (ou !{} A tout).", command.name, command.name));
        return;
    };

    let state = app.state::<AppState>();
    let active = match state.db.bet_active() {
        Ok(Some((id, status))) if status == "open" => id,
        Ok(Some(_)) => {
            reply("les votes sont terminés.".into());
            return;
        }
        Ok(None) => {
            reply("aucun bet n'est ouvert aux votes.".into());
            return;
        }
        Err(e) => {
            log(app, format!("Bet : {e}"));
            return;
        }
    };

    match state
        .db
        .bet_vote(active, &msg.user_id, idx, amount, s.min_stake as i64)
    {
        Ok(VoteResult::Placed { option, added, stake }) => {
            log(
                app,
                format!(
                    "Bet : {} mise {added} sur {} (total {stake})",
                    who(msg),
                    letter(option)
                ),
            );
            emit_snapshot(app);
        }
        Ok(VoteResult::NotOpen) => reply("les votes sont terminés.".into()),
        Ok(VoteResult::UnknownOption) => {
            let n = state.db.bet_options(active).map(|o| o.len()).unwrap_or(0);
            reply(format!("réponse inconnue : choisis parmi {}.", letters(n)));
        }
        Ok(VoteResult::OtherOption(o)) => reply(format!(
            "tu as déjà misé sur {} : tu ne peux pas changer de réponse.",
            letter(o)
        )),
        Ok(VoteResult::TooLow) => reply(format!(
            "mise minimale : {} {points_name}.",
            s.min_stake.max(1)
        )),
        Ok(VoteResult::NoFunds(balance)) => reply(format!(
            "solde insuffisant : tu as {balance} {points_name}."
        )),
        Err(e) => log(app, format!("Bet : {e}")),
    }
}

// !mise : état du bet et mise du viewer.
fn info(app: &AppHandle, msg: &ChatMessage, s: &BetSettings, points_name: &str) {
    let state = app.state::<AppState>();
    let snap = match state.db.bet_snapshot() {
        Ok(Some(snap)) if snap.status == "open" || snap.status == "closed" => snap,
        _ => {
            say(app, s.reply_as, format!("@{} aucun bet en cours.", who(msg)));
            return;
        }
    };
    let state_text = if snap.status == "open" { "votes ouverts" } else { "votes terminés" };
    let mine = match state.db.bet_vote_of(snap.id, &msg.user_id) {
        Ok(Some((option, stake))) => format!("ta mise : {stake} {points_name} sur {}", letter(option)),
        _ => "tu n'as pas encore misé".to_string(),
    };
    say(
        app,
        s.reply_as,
        format!(
            "@{} Bet : {} — {} ({state_text}) — {mine}",
            who(msg),
            snap.question,
            odds_text(s, &snap)
        ),
    );
}

// Point d'entrée appelé par le moteur de commandes.
pub fn run(app: &AppHandle, command: &Command, msg: &ChatMessage, args: &str) {
    let (s, points_name) = settings(app);
    let report = |r: Result<(), String>| {
        if let Err(e) = r {
            say(app, s.reply_as, e);
        }
    };
    match command.kind {
        Kind::Bet => report(
            parse_bet(args, &command.name).and_then(|(q, o)| do_create(app, q, o)),
        ),
        Kind::BetEnd => report(do_close(app)),
        Kind::BetWin => match args.split_whitespace().next() {
            Some(l) => report(do_resolve(app, l)),
            None => say(
                app,
                s.reply_as,
                format!("Usage : !{} A (la lettre de la réponse gagnante).", command.name),
            ),
        },
        Kind::BetCancel => report(do_cancel(app)),
        Kind::Vote => vote(app, command, msg, args),
        Kind::BetInfo => info(app, msg, &s, &points_name),
        _ => {}
    }
}

// --- Commandes appelées par l'interface ---

#[tauri::command]
pub fn bet_get_state(app: AppHandle) -> Result<Option<BetSnapshot>, String> {
    app.state::<AppState>().db.bet_snapshot()
}

#[tauri::command]
pub fn bet_create(app: AppHandle, question: String, options: Vec<String>) -> Result<(), String> {
    do_create(&app, question, options)
}

#[tauri::command]
pub fn bet_close(app: AppHandle) -> Result<(), String> {
    do_close(&app)
}

#[tauri::command]
pub fn bet_resolve(app: AppHandle, letter: String) -> Result<(), String> {
    do_resolve(&app, &letter)
}

#[tauri::command]
pub fn bet_cancel(app: AppHandle) -> Result<(), String> {
    do_cancel(&app)
}
