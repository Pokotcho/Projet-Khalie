import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;

async function greet() {
  if (greetMsgEl && greetInputEl) {
    // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
    greetMsgEl.textContent = await invoke("greet", {
      name: greetInputEl.value,
    });
  }
}

window.addEventListener("DOMContentLoaded", () => {
  greetInputEl = document.querySelector("#greet-input");
  greetMsgEl = document.querySelector("#greet-msg");
  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    greet();
  });
});

window.addEventListener("DOMContentLoaded", async () => {
  const toggle = document.querySelector<HTMLInputElement>("#feature-toggle")!;
  const tickValue = document.querySelector<HTMLElement>("#tick-value")!;

  // Au démarrage : on demande à Rust l'état actuel de l'interrupteur.
  toggle.checked = await invoke<boolean>("get_feature");

  // Quand on clique sur la case : on prévient Rust.
  toggle.addEventListener("change", () => {
    invoke("set_feature", { enabled: toggle.checked });
  });

  // On écoute les événements "tick" envoyés par la tâche Rust.
  await listen<number>("tick", (event) => {
    tickValue.textContent = String(event.payload);
  });
});

interface DeviceCode {
  device_code: string;
  expires_in: number;
  interval: number;
  user_code: string;
  verification_uri: string;
}
interface LoginInfo {
  login: string;
  user_id: string;
}

window.addEventListener("DOMContentLoaded", async () => {
  const loginBtn = document.querySelector<HTMLButtonElement>("#twitch-login-btn")!;
  const logoutBtn = document.querySelector<HTMLButtonElement>("#twitch-logout-btn")!;
  const status = document.querySelector<HTMLElement>("#twitch-status")!;
  const codeBox = document.querySelector<HTMLElement>("#twitch-code")!;
  const link = document.querySelector<HTMLAnchorElement>("#twitch-link")!;
  const userCode = document.querySelector<HTMLElement>("#twitch-usercode")!;

  type UiState = "out" | "pending" | "in";

  // Un seul endroit qui décide de ce qui est visible selon l'état.
  const setUi = (state: UiState, info?: LoginInfo) => {
    loginBtn.style.display = state === "in" ? "none" : "inline-block";
    loginBtn.disabled = state === "pending";
    logoutBtn.style.display = state === "out" ? "none" : "inline-block";
    logoutBtn.textContent = state === "pending" ? "Annuler" : "Se déconnecter";
    codeBox.style.display = state === "pending" ? "block" : "none";

    if (state === "out") status.textContent = "Non connecté";
    if (state === "pending")
      status.textContent = "En attente de ton autorisation dans le navigateur…";
    if (state === "in" && info) status.textContent = `Connecté en tant que ${info.login}`;
  };

    // Pendant la vérification de la session sauvegardée, on masque les boutons.
  status.textContent = "Vérification de la connexion…";
  loginBtn.style.display = "none";
  logoutBtn.style.display = "none";
  codeBox.style.display = "none";

  const current = await invoke<LoginInfo | null>("twitch_restore");
  if (current) setUi("in", current);
  else setUi("out");

  loginBtn.addEventListener("click", async () => {
    status.textContent = "Demande du code à Twitch…";
    try {
      const dc = await invoke<DeviceCode>("twitch_login_start");
      userCode.textContent = dc.user_code;
      link.onclick = (e) => {
        e.preventDefault();
        openUrl(dc.verification_uri);
      };
      setUi("pending");
      await openUrl(dc.verification_uri);
    } catch (e) {
      setUi("out");
      status.textContent = `Erreur : ${e}`;
    }
  });

  logoutBtn.addEventListener("click", async () => {
    await invoke("twitch_logout");
    setUi("out");
  });

  await listen<LoginInfo>("twitch-login-success", (event) => setUi("in", event.payload));
  await listen<string>("twitch-login-error", (event) => {
    setUi("out");
    status.textContent = `Erreur : ${event.payload}`;
  });
  await listen("twitch-session-lost", () => {
    setUi("out");
    status.textContent = "Session expirée : reconnecte-toi.";
  });
});

interface ChatMessage {
  message_id: string;
  user_id: string;
  login: string;
  display_name: string;
  text: string;
  is_self: boolean;
}

window.addEventListener("DOMContentLoaded", async () => {
  // --- Affichage du chat ---
  const chatStatus = document.querySelector<HTMLElement>("#chat-status")!;
  const chatLog = document.querySelector<HTMLElement>("#chat-log")!;

  await listen<string>("chat-status", (event) => {
    chatStatus.textContent = `Chat : ${event.payload}`;
  });

  await listen<ChatMessage>("chat-message", (event) => {
    const m = event.payload;

    const line = document.createElement("div");
    if (m.is_self) line.style.opacity = "0.6"; // messages du bot en plus discret

    const name = document.createElement("strong");
    name.textContent = m.display_name || m.login;
    const text = document.createElement("span");
    text.textContent = `: ${m.text}`; // textContent : jamais de HTML venant d'un viewer
    line.append(name, text);
    chatLog.appendChild(line);

    while (chatLog.children.length > 200) {
      chatLog.removeChild(chatLog.firstChild!);
    }
    chatLog.scrollTop = chatLog.scrollHeight;
  });

  // --- Chaîne surveillée et envoi de messages ---
  const channelInput = document.querySelector<HTMLInputElement>("#channel-input")!;
  const channelSave = document.querySelector<HTMLButtonElement>("#channel-save-btn")!;
  const channelStatus = document.querySelector<HTMLElement>("#channel-status")!;
  const sendInput = document.querySelector<HTMLInputElement>("#send-input")!;
  const sendBtn = document.querySelector<HTMLButtonElement>("#send-btn")!;
  const sendStatus = document.querySelector<HTMLElement>("#send-status")!;

  channelInput.value = await invoke<string>("get_channel");

  channelSave.addEventListener("click", async () => {
    try {
      const saved = await invoke<string>("set_channel", { login: channelInput.value });
      channelInput.value = saved;
      channelStatus.textContent = saved
        ? `Chaîne enregistrée : ${saved}`
        : "Chaîne du compte connecté";
    } catch (e) {
      channelStatus.textContent = `Erreur : ${e}`;
    }
  });

  const send = async () => {
    const text = sendInput.value.trim();
    if (!text) return;
    sendStatus.textContent = "Envoi…";
    try {
      await invoke("send_chat_message", { text });
      sendInput.value = "";
      sendStatus.textContent = "Message envoyé";
    } catch (e) {
      sendStatus.textContent = `Échec : ${e}`;
    }
  };

  sendBtn.addEventListener("click", send);
  sendInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") send();
  });
});
