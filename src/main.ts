import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

type Role = "streamer" | "bot";
type Level = "everyone" | "moderator" | "streamer";
type Tab = "chat" | "commands" | "auto";

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
  kind:
    | "text"
    | "list"
    | "counter"
    | "counter_reset"
    | "title"
    | "category"
    | "shoutout"
    | "clip"
    | "followage";
}
interface AutoSettings {
  enabled: boolean;
  only_when_live: boolean;
  min_chat_lines: number;
  min_gap_min: number;
}
interface AutoMessage {
  id: number;
  name: string;
  enabled: boolean;
  text: string;
  interval_min: number;
  reply_as: Role;
}
interface BotConfig {
  enabled: boolean;
  commands: BotCommand[];
  counter: number;
  auto: AutoSettings;
  auto_messages: AutoMessage[];
}

// Commandes qui agissent en silence : pas de texte de réponse.
const SILENT_KINDS: BotCommand["kind"][] = ["title", "category", "shoutout"];
// Commandes qui ne peuvent pas être ouvertes à tout le monde.
const MOD_ONLY_KINDS: BotCommand["kind"][] = ["counter_reset", "title", "category", "shoutout"];

const KIND_HINT: Partial<Record<BotCommand["kind"], string>> = {
  list: "Liste des commandes accessibles à tous.",
  counter: "Augmente le compteur de 1.",
  counter_reset: "Remet le compteur à zéro.",
  title: "Usage : !titre nouveau titre. Agit avec le compte du streamer, sans message dans le chat.",
  category: "Usage : !categorie nom du jeu. Agit avec le compte du streamer, sans message dans le chat.",
  shoutout:
    "Usage : !so @pseudo. Shoutout officiel Twitch (chaîne en direct requise), sans message dans le chat.",
  clip: "Crée un clip de la chaîne en direct, avec le compte du streamer, puis écrit son lien.",
  followage: "Indique depuis quand le viewer suit la chaîne.",
};

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
  $("#auto-section").hidden = !(both && currentTab === "auto");
  $("#tab-chat").classList.toggle("active", currentTab === "chat");
  $("#tab-commands").classList.toggle("active", currentTab === "commands");
  $("#tab-auto").classList.toggle("active", currentTab === "auto");
}

function switchTab(tab: Tab) {
  currentTab = tab;
  render();
  if (tab === "commands") loadCommands();
  if (tab === "auto") loadAuto();
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
  const silent = SILENT_KINDS.includes(cmd.kind);

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

  row.append(head);

  // Explication du rôle de la commande.
  const hintText = KIND_HINT[cmd.kind];
  if (hintText) {
    const hint = document.createElement("p");
    hint.className = "hint";
    hint.textContent = hintText;
    row.append(hint);
  }

  // Zone de texte de la réponse (absente pour les commandes silencieuses).
  const response = document.createElement("textarea");
  response.value = cmd.response;
  response.rows = 2;
  response.maxLength = 500;
  response.hidden = silent;

  // Réglages. Certaines commandes ne peuvent pas être ouvertes à tout le monde.
  const levelChoices = Object.entries(LEVEL_LABEL).filter(
    ([value]) => !(MOD_ONLY_KINDS.includes(cmd.kind) && value === "everyone"),
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
  options.append(labelled("Accessible à", level));
  if (!silent) {
    options.append(labelled("Compte qui répond", replyAs));
  }
  options.append(labelled("Délai global (s)", globalCd), labelled("Délai par viewer (s)", userCd));

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

  row.append(response, options, saveBtn);
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

// --- Messages automatiques ---
function collectAutoSettings(): AutoSettings {
  return {
    enabled: $<HTMLInputElement>("#auto-enabled").checked,
    only_when_live: $<HTMLInputElement>("#auto-only-live").checked,
    min_chat_lines: Number($<HTMLInputElement>("#auto-min-lines").value) || 0,
    min_gap_min: Number($<HTMLInputElement>("#auto-min-gap").value) || 0,
  };
}

async function saveAutoSettings() {
  const status = $("#auto-status");
  try {
    await invoke("bot_auto_set_settings", { settings: collectAutoSettings() });
    status.textContent = "Réglages enregistrés";
  } catch (e) {
    status.textContent = `Erreur : ${e}`;
  }
}

function buildAutoRow(msg: AutoMessage): HTMLElement {
  const status = $("#auto-status");

  const row = document.createElement("div");
  row.className = msg.enabled ? "cmd" : "cmd disabled";

  const head = document.createElement("div");
  head.className = "cmd-head";

  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = msg.enabled;
  enabled.title = "Activer / désactiver ce message";

  const nameInput = document.createElement("input");
  nameInput.className = "name-input";
  nameInput.value = msg.name;
  nameInput.maxLength = 40;
  nameInput.title = "Nom du message (pour te repérer)";

  const spacer = document.createElement("span");
  spacer.className = "spacer";

  const del = document.createElement("button");
  del.className = "btn-small";
  del.textContent = "Supprimer";
  del.addEventListener("click", async () => {
    try {
      await invoke("bot_auto_delete", { id: msg.id });
      status.textContent = `« ${msg.name} » supprimé`;
      await loadAuto();
    } catch (e) {
      status.textContent = `Erreur : ${e}`;
    }
  });

  head.append(enabled, nameInput, spacer, del);

  const text = document.createElement("textarea");
  text.className = "auto-textarea";
  text.value = msg.text;
  text.rows = 2;
  text.maxLength = 500;

  const interval = document.createElement("input");
  interval.type = "number";
  interval.min = "5";
  interval.max = "1440";
  interval.value = String(msg.interval_min);

  const replyAs = makeSelect(
    [
      ["bot", "Bot"],
      ["streamer", "Streamer"],
    ],
    msg.reply_as,
  );

  const options = document.createElement("div");
  options.className = "field-row";
  options.append(labelled("Toutes les (minutes)", interval), labelled("Compte qui écrit", replyAs));

  const saveBtn = document.createElement("button");
  saveBtn.textContent = "Enregistrer";

  const save = async () => {
    const updated: AutoMessage = {
      ...msg,
      name: nameInput.value,
      enabled: enabled.checked,
      text: text.value,
      interval_min: Number(interval.value) || 0,
      reply_as: replyAs.value as Role,
    };
    try {
      await invoke("bot_auto_update", { message: updated });
      row.classList.toggle("disabled", !enabled.checked);
      status.textContent = `« ${updated.name} » enregistré`;
    } catch (e) {
      status.textContent = `Erreur : ${e}`;
    }
  };

  // La case à cocher agit tout de suite ; le reste s'enregistre avec le bouton.
  enabled.addEventListener("change", save);
  saveBtn.addEventListener("click", save);

  row.append(head, text, options, saveBtn);
  return row;
}

async function loadAuto() {
  const config = await invoke<BotConfig>("bot_get_state");
  $<HTMLInputElement>("#auto-enabled").checked = config.auto.enabled;
  $<HTMLInputElement>("#auto-only-live").checked = config.auto.only_when_live;
  $<HTMLInputElement>("#auto-min-lines").value = String(config.auto.min_chat_lines);
  $<HTMLInputElement>("#auto-min-gap").value = String(config.auto.min_gap_min);

  const live = await invoke<boolean>("bot_get_live");
  $("#live-state").textContent = live ? "en direct" : "hors ligne";

  const list = $("#auto-list");
  list.replaceChildren();
  for (const msg of config.auto_messages) {
    list.appendChild(buildAutoRow(msg));
  }
}

async function createAuto() {
  const status = $("#auto-status");
  const name = $<HTMLInputElement>("#auto-new-name");
  const text = $<HTMLTextAreaElement>("#auto-new-text");
  const interval = $<HTMLInputElement>("#auto-new-interval");

  const message: AutoMessage = {
    id: 0,
    name: name.value,
    enabled: true,
    text: text.value,
    interval_min: Number(interval.value) || 0,
    reply_as: "bot",
  };

  try {
    await invoke("bot_auto_create", { message });
    name.value = "";
    text.value = "";
    status.textContent = "Message ajouté";
    await loadAuto();
  } catch (e) {
    status.textContent = `Erreur : ${e}`;
  }
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
  $("#tab-auto").addEventListener("click", () => switchTab("auto"));

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

  // Messages automatiques.
  $("#auto-save-settings").addEventListener("click", saveAutoSettings);
  $<HTMLInputElement>("#auto-enabled").addEventListener("change", saveAutoSettings);
  $("#auto-new-add").addEventListener("click", createAuto);

  $("#open-logs").addEventListener("click", async () => {
    try {
      await invoke("open_logs_folder");
    } catch (e) {
      $("#commands-status").textContent = `Erreur : ${e}`;
    }
  });

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
  await listen<boolean>("bot-live", (event) => {
    $("#live-state").textContent = event.payload ? "en direct" : "hors ligne";
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