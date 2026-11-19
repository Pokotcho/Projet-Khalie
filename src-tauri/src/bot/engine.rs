use tauri::{AppHandle, Manager};

use super::{log, Kind, Level};
use crate::twitch::chat::{self, ChatMessage};
use crate::twitch::Role;
use crate::AppState;

// Remplace les champs dynamiques ([@pseudo], [pseudo], [commandes]) dans une réponse.
fn fill(template: &str, msg: &ChatMessage, commands: &str) -> String {
    let name = who(msg);
    template
        .replace("[@pseudo]", &format!("@{name}"))
        .replace("[pseudo]", name)
        .replace("[commandes]", commands)
}

fn who(msg: &ChatMessage) -> &str {
    if msg.display_name.is_empty() {
        &msg.login
    } else {
        &msg.display_name
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
    let Some(first) = rest.split_whitespace().next() else {
        return;
    };
    let name = first.to_lowercase();

    // On copie ce dont on a besoin, sans garder de verrou pendant l'envoi.
    let (enabled, command, public_list) = {
        let state = app.state::<AppState>();
        let config = state.bot.config.read().unwrap();
        let command = config.commands.iter().find(|c| c.name == name).cloned();
        let list: Vec<String> = config
            .commands
            .iter()
            .filter(|c| c.enabled && c.level == Level::Everyone)
            .map(|c| format!("!{}", c.name))
            .collect();
        (config.enabled, command, list)
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

    // Construction de la réponse.
    let reply = match command.kind {
        Kind::Text => fill(&command.response, &msg, ""),
        Kind::List => fill(&command.response, &msg, &public_list.join(", ")),
    };
    let reply: String = reply.chars().take(500).collect();

    // Choix du compte qui écrit.
    let (sender, streamer, client) = {
        let state = app.state::<AppState>();
        let a = state.session(command.reply_as);
        let b = state.session(Role::Streamer);
        let c = state.bot.client.clone();
        (a, b, c)
    };
    let (Some(sender), Some(streamer)) = (sender, streamer) else {
        log(
            &app,
            format!(
                "!{} : compte {} non connecté, réponse non envoyée",
                command.name,
                command.reply_as.key()
            ),
        );
        return;
    };

    match chat::send_message(&client, &sender, &streamer.user_id, &reply).await {
        Ok(()) => log(&app, format!("!{} par {} : réponse envoyée", command.name, who(&msg))),
        Err(e) => log(&app, format!("!{} : envoi impossible : {e}", command.name)),
    }
}