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

  const current = await invoke<LoginInfo | null>("twitch_status");
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
});
