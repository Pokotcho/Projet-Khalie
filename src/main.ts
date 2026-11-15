import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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
