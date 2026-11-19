import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

type Role = "streamer" | "bot";
type Level = "everyone" | "moderator" | "streamer";
type Tab = "chat" | "commands";

const ROLES: Role[] = ["streamer", "bot"];
const ROLE_LABEL: Record<Role, string> = { streamer: "STREAMER", bot: "BOT" };
const LEVEL_LABEL: Record<Level, string> = {
  everyone: "Tout le monde",
  moderator: "Modérateurs",
  streamer: "Streamer seulement",
};

interface DeviceCode {
  device_code: string;
  expires_in: number;
  interval: number;
  user_code: string;
  verification_uri: string;
}
interface LoginInfo {
  role: Role;
  login: string;
  user_id: string;
}
interface RoleError {
  role: Role;
  message: string;
}
interface ChatMessage {
  message_id: string;
  user_id: string;
  login: string;
  display_name: string;
  text: string;
  from_bot: boolean;
  from_streamer: boolean;
  is_moderator: boolean;
}
interface BotCommand {
  name: string;
  enabled: boolean;
  response: string;
  level: Level;
  global_cooldown: number;
  user_cooldown: number;
  reply_as: Role;
  builtin: boolean;
  kind: "text" | "list" | "counter" | "counter_reset";
}
interface BotConfig {
  enabled: boolean;
  commands: BotCommand[];
  counter: number;
}

// --- État de l'interface ---
const accounts: Partial<Record<Role, LoginInfo>> = {};
const pending: Record<Role, boolean> = { streamer: false, bot: false };
const errors: Partial<Record<Role, string>> = {};
let currentTab: Tab = "chat";

const $ = <T extends HTMLElement = HTMLElement>(selector: string): T =>
  document.querySelector<T>(selector)!;

// --- Affichage : un seul endroit décide de ce qui est visible ---
function render() {
  const hasStreamer = !!accounts.streamer;
  const hasBot = !!accounts.bot;
  const both = hasStreamer && hasBot;

  // Cartes de connexion : streamer d'abord, puis bot.
  const showCard: Record<Role, boolean> = {
    streamer: !hasStreamer,
    bot: hasStreamer && !hasBot,
  };

  for (const role of ROLES) {
    const card = $(`#card-${role}`);
    card.hidden = !showCard[role];
    card.querySelector<HTMLElement>(".btn-login")!.hidden = pending[role];
    card.querySelector<HTMLElement>(".pending")!.hidden = !pending[role];

    const err = card.querySelector<HTMLElement>(".error")!;
    err.textContent = errors[role] ?? "";
    err.hidden = !errors[role];
  }

  // Résumé des comptes connectés, avec leur rôle.
  const list = $("#summary-list");
  list.replaceChildren();
  for (const role of ROLES) {
    const acc = accounts[role];
    if (!acc) continue;

    const li = document.createElement("li");

    const badge = document.createElement("span");
    badge.className = `badge badge-${role}`;
    badge.textContent = ROLE_LABEL[role];

    const name = document.createElement("strong");
    name.textContent = acc.login;

    const btn = document.createElement("button");
    btn.className = "btn-small";
    btn.textContent = "Déconnecter";
    btn.addEventListener("click", () => disconnect(role));

    li.append(badge, " ", name, " ", btn);
    list.appendChild(li);
  }
  $("#summary").hidden = !hasStreamer && !hasBot;

  // Onglets : seulement quand les deux comptes sont connectés.
  $("#tabs").hidden = !both;
  $("#chat-section").hidden = !(both && currentTab === "chat");
  $("#commands-section").hidden = !(both && currentTab === "commands");
  $("#tab-chat").classList.toggle("active", currentTab === "chat");
  $("#tab-commands").classList.toggle("active", currentTab === "commands");
}

function switchTab(tab: Tab) {
  currentTab = tab;
  render();
  if (tab === "commands") loadCommands();
}

// --- Connexion / déconnexion ---
async function startLogin(role: Role) {
  errors[role] = undefined;
  try {
    const dc = await invoke<DeviceCode>("twitch_login_start", { role });

    const card = $(`#card-${role}`);
    card.querySelector<HTMLElement>(".user-code")!.textContent = dc.user_code;
    card.querySelector<HTMLElement>(".auth-url")!.textContent = dc.verification_uri;

    pending[role] = true;
    render();

    // Le streamer : le navigateur s'ouvre tout seul.
    // Le bot : on laisse l'utilisateur copier le lien dans une fenêtre privée.
    if (role === "streamer") {
      await openUrl(dc.verification_uri);
    }
  } catch (e) {
    pending[role] = false;
    errors[role] = String(e);
    render();
  }
}

async function disconnect(role: Role) {
  await invoke("twitch_logout", { role });
  pending[role] = false;
  delete accounts[role];
  errors[role] = undefined;
  render();
}

// --- Chat ---
function addChatLine(m: ChatMessage) {
  const log = $("#chat-log");

  const line = document.createElement("div");
  line.className = "chat-line";
  if (m.from_bot) line.classList.add("from-bot");
  if (m.from_streamer) line.classList.add("from-streamer");

  const name = document.createElement("strong");
  name.textContent = m.display_name || m.login;
  const text = document.createElement("span");
  text.textContent = `: ${m.text}`; // textContent : jamais de HTML venant d'un viewer
  line.append(name, text);
  log.appendChild(line);

  while (log.children.length > 200) {
    log.removeChild(log.firstChild!);
  }
  log.scrollTop = log.scrollHeight;
}

async function sendMessage() {
  const input = $<HTMLInputElement>("#send-input");
  const status = $("#send-status");
  const role = $<HTMLSelectElement>("#send-as").value as Role;
  const text = input.value.trim();
  if (!text) return;

  status.textContent = "Envoi…";
  try {
    await invoke("send_chat_message", { text, role });
    input.value = "";
    status.textContent = "Message envoyé";
  } catch (e) {
    status.textContent = `Échec : ${e}`;
  }
}

// --- Commandes du bot ---
function makeSelect(options: [string, string][], value: string): HTMLSelectElement {
  const select = document.createElement("select");
  for (const [val, label] of options) {
    const opt = document.createElement("option");
    opt.value = val;
    opt.textContent = label;
    select.appendChild(opt);
  }
  select.value = value;
  return select;
}

function labelled(text: string, control: HTMLElement): HTMLLabelElement {
  const label = document.createElement("label");
  label.className = "field";
  label.append(`${text} `, control);
  return label;
}

function buildCommandRow(cmd: BotCommand): HTMLElement {
  const status = $("#commands-status");
  let currentName = cmd.name;

  const row = document.createElement("div");
  row.className = cmd.enabled ? "cmd" : "cmd disabled";

  // En-tête : case à cocher, nom modifiable, suppression (commandes créées seulement).
  const head = document.createElement("div");
  head.className = "cmd-head";

  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = cmd.enabled;
  enabled.title = "Activer / désactiver cette commande";

  const bang = document.createElement("strong");
  bang.textContent = "!";

  const nameInput = document.createElement("input");
  nameInput.className = "name-input";
  nameInput.value = cmd.name;
  nameInput.maxLength = 25;
  nameInput.title = "Nom de la commande (modifiable, puis Enregistrer)";

  const spacer = document.createElement("span");
  spacer.className = "spacer";

  head.append(enabled, bang, nameInput, spacer);

  if (!cmd.builtin) {
    const del = document.createElement("button");
    del.className = "btn-small";
    del.textContent = "Supprimer";
    del.addEventListener("click", async () => {
      try {
        await invoke("bot_delete_command", { name: currentName });
        status.textContent = `!${currentName} supprimée`;
        await loadCommands();
      } catch (e) {
        status.textContent = `Erreur : ${e}`;
      }
    });
    head.append(del);
  }

  // Zone de texte de la réponse.
  const response = document.createElement("textarea");
  response.value = cmd.response;
  response.rows = 2;
  response.maxLength = 500;

  // Réglages. La commande de remise à zéro ne peut pas être ouverte à tout le monde.
  const levelChoices = Object.entries(LEVEL_LABEL).filter(
    ([value]) => !(cmd.kind === "counter_reset" && value === "everyone"),
  );
  const level = makeSelect(levelChoices, cmd.level);
  const replyAs = makeSelect(
    [
      ["bot", "Bot"],
      ["streamer", "Streamer"],
    ],
    cmd.reply_as,
  );
  const globalCd = document.createElement("input");
  globalCd.type = "number";
  globalCd.min = "0";
  globalCd.max = "3600";
  globalCd.value = String(cmd.global_cooldown);
  const userCd = document.createElement("input");
  userCd.type = "number";
  userCd.min = "0";
  userCd.max = "3600";
  userCd.value = String(cmd.user_cooldown);

  const options = document.createElement("div");
  options.className = "field-row";
  options.append(
    labelled("Accessible à", level),
    labelled("Compte qui répond", replyAs),
    labelled("Délai global (s)", globalCd),
    labelled("Délai par viewer (s)", userCd),
  );

  const saveBtn = document.createElement("button");
  saveBtn.textContent = "Enregistrer";

  const save = async () => {
    const updated: BotCommand = {
      ...cmd,
      name: currentName,
      enabled: enabled.checked,
      response: response.value,
      level: level.value as Level,
      reply_as: replyAs.value as Role,
      global_cooldown: Number(globalCd.value) || 0,
      user_cooldown: Number(userCd.value) || 0,
    };
    try {
      const finalName = await invoke<string>("bot_update_command", {
        command: updated,
        newName: nameInput.value,
      });
      currentName = finalName;
      nameInput.value = finalName;
      row.classList.toggle("disabled", !enabled.checked);
      status.textContent = `!${finalName} enregistrée`;
    } catch (e) {
      status.textContent = `Erreur : ${e}`;
    }
  };

  // La case à cocher agit tout de suite (utile en direct) ; le reste s'enregistre avec le bouton.
  enabled.addEventListener("change", save);
  saveBtn.addEventListener("click", save);

  row.append(head, response, options, saveBtn);
  return row;
}

async function loadCommands() {
  const config = await invoke<BotConfig>("bot_get_state");
  $<HTMLInputElement>("#bot-enabled").checked = config.enabled;
  $("#counter-value").textContent = String(config.counter);

  const list = $("#commands-list");
  list.replaceChildren();
  for (const cmd of config.commands) {
    list.appendChild(buildCommandRow(cmd));
  }
}

async function createCommand() {
  const status = $("#commands-status");
  const name = $<HTMLInputElement>("#new-name");
  const response = $<HTMLTextAreaElement>("#new-response");
  const level = $<HTMLSelectElement>("#new-level");

    const command: BotCommand = {
    name: name.value,
    enabled: true,
    response: response.value,
    level: level.value as Level,
    global_cooldown: 10,
    user_cooldown: 30,
    reply_as: "bot",
    builtin: false,
    kind: "text",
  };

  try {
    await invoke("bot_create_command", { command });
    name.value = "";
    response.value = "";
    status.textContent = "Commande ajoutée";
    await loadCommands();
  } catch (e) {
    status.textContent = `Erreur : ${e}`;
  }
}

function addBotLog(message: string) {
  const log = $("#bot-log");
  const line = document.createElement("div");
  line.className = "chat-line";
  line.textContent = `${new Date().toLocaleTimeString("fr-FR")}  ${message}`;
  log.appendChild(line);

  while (log.children.length > 100) {
    log.removeChild(log.firstChild!);
  }
  log.scrollTop = log.scrollHeight;
}

// --- Démarrage ---
window.addEventListener("DOMContentLoaded", async () => {
  // Boutons des cartes de connexion.
  for (const role of ROLES) {
    const card = $(`#card-${role}`);
    card.querySelector(".btn-login")!.addEventListener("click", () => startLogin(role));
    card.querySelector(".btn-cancel")!.addEventListener("click", () => disconnect(role));
  }

  // Onglets.
  $("#tab-chat").addEventListener("click", () => switchTab("chat"));
  $("#tab-commands").addEventListener("click", () => switchTab("commands"));

  // Envoi de messages.
  $("#send-btn").addEventListener("click", sendMessage);
  $("#send-input").addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") sendMessage();
  });

  // Commandes : interrupteur général et création.
  $<HTMLInputElement>("#bot-enabled").addEventListener("change", async (e) => {
    const checked = (e.target as HTMLInputElement).checked;
    try {
      await invoke("bot_set_enabled", { enabled: checked });
      $("#commands-status").textContent = checked ? "Bot activé" : "Bot désactivé";
    } catch (err) {
      $("#commands-status").textContent = `Erreur : ${err}`;
    }
  });
  $("#new-add").addEventListener("click", createCommand);

    // Boutons du compteur.
  const setCounter = (value: number) => {
    $("#counter-value").textContent = String(value);
  };
  $("#counter-minus").addEventListener("click", async () =>
    setCounter(await invoke<number>("bot_counter_adjust", { delta: -1 })),
  );
  $("#counter-plus").addEventListener("click", async () =>
    setCounter(await invoke<number>("bot_counter_adjust", { delta: 1 })),
  );
  $("#counter-reset").addEventListener("click", async () =>
    setCounter(await invoke<number>("bot_counter_reset")),
  );

  // Événements envoyés par Rust (on les écoute AVANT de restaurer les sessions).
  await listen<LoginInfo>("twitch-login-success", (event) => {
    const acc = event.payload;
    accounts[acc.role] = acc;
    pending[acc.role] = false;
    errors[acc.role] = undefined;
    render();
  });

  await listen<RoleError>("twitch-login-error", (event) => {
    const { role, message } = event.payload;
    pending[role] = false;
    errors[role] = message;
    render();
  });

  await listen<Role>("twitch-session-lost", (event) => {
    const role = event.payload;
    delete accounts[role];
    pending[role] = false;
    errors[role] = "Session expirée : reconnecte ce compte.";
    render();
  });

  await listen<string>("chat-status", (event) => {
    $("#chat-status").textContent = `Chat : ${event.payload}`;
  });

  await listen<ChatMessage>("chat-message", (event) => addChatLine(event.payload));
  await listen<string>("bot-log", (event) => addBotLog(event.payload));
  await listen<number>("bot-counter", (event) => {
    $("#counter-value").textContent = String(event.payload);
  });

  // Restauration des sessions sauvegardées.
  const restored = await invoke<LoginInfo[]>("twitch_restore");
  for (const acc of restored) {
    accounts[acc.role] = acc;
  }

  $("#loading").hidden = true;
  $("#app").hidden = false;
  render();
});