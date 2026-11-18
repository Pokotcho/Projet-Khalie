import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

type Role = "streamer" | "bot";
const ROLES: Role[] = ["streamer", "bot"];
const ROLE_LABEL: Record<Role, string> = { streamer: "STREAMER", bot: "BOT" };

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
}

// --- État de l'interface ---
const accounts: Partial<Record<Role, LoginInfo>> = {};
const pending: Record<Role, boolean> = { streamer: false, bot: false };
const errors: Partial<Record<Role, string>> = {};

const $ = <T extends HTMLElement = HTMLElement>(selector: string): T =>
  document.querySelector<T>(selector)!;

// --- Affichage : un seul endroit décide de ce qui est visible ---
function render() {
  const hasStreamer = !!accounts.streamer;
  const hasBot = !!accounts.bot;

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

  // Le chat n'apparaît que lorsque les deux comptes sont connectés.
  $("#chat-section").hidden = !(hasStreamer && hasBot);
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

// --- Démarrage ---
window.addEventListener("DOMContentLoaded", async () => {
  // Boutons des cartes de connexion.
  for (const role of ROLES) {
    const card = $(`#card-${role}`);
    card.querySelector(".btn-login")!.addEventListener("click", () => startLogin(role));
    card.querySelector(".btn-cancel")!.addEventListener("click", () => disconnect(role));
  }

  // Envoi de messages.
  $("#send-btn").addEventListener("click", sendMessage);
  $("#send-input").addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") sendMessage();
  });

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

  // Restauration des sessions sauvegardées.
  const restored = await invoke<LoginInfo[]>("twitch_restore");
  for (const acc of restored) {
    accounts[acc.role] = acc;
  }

  $("#loading").hidden = true;
  $("#app").hidden = false;
  render();
});