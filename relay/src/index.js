// Relais Throne -> Khalie
// Reçoit les notifications de Throne (webhook signé), les range dans une base D1,
// et les remet à l'application Khalie quand elle les demande.

// Clé publique de Throne (Ed25519, format SPKI en base64). Source : page d'aide Throne
// « How do I set up webhook integration? ».
const THRONE_PUBLIC_KEY_B64 = "MCowBQYDK2VwAyEAPXbUfxh7XL4SYUVcfhmYMIbxvtR9E9LDd8gPJ1PwSD8=";

const MAX_AGE_SECS = 600; // une notification signée plus vieille que 10 minutes est refusée
const KEEP_DAYS = 30; // les événements sont gardés 30 jours dans le relais
const PAGE_SIZE = 50;

function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function hexToBytes(hex) {
  if (!/^[0-9a-fA-F]+$/.test(hex) || hex.length % 2 !== 0) return null;
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.substr(i * 2, 2), 16);
  return out;
}

let keyPromise = null;
function throneKey() {
  keyPromise ??= crypto.subtle.importKey(
    "spki",
    b64ToBytes(THRONE_PUBLIC_KEY_B64),
    { name: "Ed25519" },
    false,
    ["verify"],
  );
  return keyPromise;
}

// Message signé par Throne : timestamp + "." + corps brut de la requête.
async function verifyThrone(request, bodyBytes) {
  const ts = request.headers.get("X-Signature-Timestamp") ?? "";
  const sigHex = request.headers.get("X-Signature-Ed25519") ?? "";
  if (!/^\d+$/.test(ts)) return false;
  if (Math.abs(Date.now() / 1000 - Number(ts)) > MAX_AGE_SECS) return false;
  const sig = hexToBytes(sigHex);
  if (!sig || sig.length !== 64) return false;

  const prefix = new TextEncoder().encode(`${ts}.`);
  const message = new Uint8Array(prefix.length + bodyBytes.length);
  message.set(prefix);
  message.set(bodyBytes, prefix.length);

  try {
    return await crypto.subtle.verify({ name: "Ed25519" }, await throneKey(), sig, message);
  } catch {
    return false;
  }
}

function sameToken(a, b) {
  if (!a || !b || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

function json(data, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": "no-store" },
  });
}

// Throne -> relais
async function onThrone(request, env) {
  const bodyBytes = new Uint8Array(await request.arrayBuffer());
  if (!(await verifyThrone(request, bodyBytes))) {
    console.log("Notification refusée : signature invalide");
    return new Response("signature invalide", { status: 401 });
  }

  const body = new TextDecoder().decode(bodyBytes);
  let evt;
  try {
    evt = JSON.parse(body);
  } catch {
    return new Response("JSON illisible", { status: 400 });
  }
  if (typeof evt.event_id !== "string" || typeof evt.event_type !== "string") {
    return new Response("événement incomplet", { status: 400 });
  }

  const now = Math.floor(Date.now() / 1000);
  // INSERT OR IGNORE : si Throne renvoie deux fois le même événement, il n'est gardé qu'une fois.
  await env.DB.prepare(
    "INSERT OR IGNORE INTO events (event_id, received_at, event_type, body) VALUES (?1, ?2, ?3, ?4)",
  )
    .bind(evt.event_id, now, evt.event_type, body)
    .run();
  await env.DB.prepare("DELETE FROM events WHERE received_at < ?1")
    .bind(now - KEEP_DAYS * 86400)
    .run();

  console.log(`Événement reçu : ${evt.event_type} (${evt.event_id})`);
  return new Response("ok");
}

// Application Khalie -> relais : « donne-moi les événements après le numéro X ».
async function onEvents(request, env, url) {
  const auth = request.headers.get("Authorization") ?? "";
  const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
  if (!env.APP_TOKEN || !sameToken(token, env.APP_TOKEN)) {
    return json({ error: "non autorisé" }, 401);
  }

  const after = Math.max(0, parseInt(url.searchParams.get("after") ?? "0", 10) || 0);
  const { results } = await env.DB.prepare(
    "SELECT seq, event_id, received_at, event_type, body FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
  )
    .bind(after, PAGE_SIZE)
    .all();

  const events = results.map((r) => ({
    seq: r.seq,
    event_id: r.event_id,
    received_at: r.received_at,
    event_type: r.event_type,
    payload: JSON.parse(r.body),
  }));
  return json({ events });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/health") return new Response("ok");
    if (url.pathname === "/throne" && request.method === "POST") return onThrone(request, env);
    if (url.pathname === "/events" && request.method === "GET") return onEvents(request, env, url);
    return new Response("introuvable", { status: 404 });
  },
};
