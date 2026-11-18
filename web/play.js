Error.stackTraceLimit = 80;
import { enableTouch, setTouchMode } from "./touch.js";
import { Host, Guest, describePath } from "./rtc.js";
import init, { mount_shard, run, run_map, net_open, net_close, net_message, net_tick, net_host, net_join_first, net_start, audio_tick, audio_pull, persist_dump, persist_load } from "./pkg/iw4l_web.js";

const GAME_DIR = "Call of Duty Modern Warfare 2";
const $ = (id) => document.getElementById(id);
const logEl = $("log");
function log(text, cls) {
  const line = document.createElement("div");
  if (cls) line.className = cls;
  line.textContent = text;
  logEl.appendChild(line);
  logEl.scrollTop = logEl.scrollHeight;
  console.log(text);
}

// The engine writes through console.log; surface errors in the page as well.
window.addEventListener("error", (e) => log("error: " + e.message, "err"));
window.addEventListener("unhandledrejection", (e) => log("error: " + e.reason, "err"));

const params = new URLSearchParams(location.search);
const packUrl = params.get("pack"); // e.g. ?pack=/packout/mw2.iw4pk
let source = null; // { slice(start, end) -> Promise<Uint8Array>, size }

function fileSource(file) {
  return { size: file.size, slice: async (a, b) => new Uint8Array(await file.slice(a, b).arrayBuffer()) };
}

async function urlSource(url) {
  const head = await fetch(url, { headers: { Range: "bytes=0-0" } });
  const size = Number((head.headers.get("Content-Range") || "").split("/")[1]);
  if (!size) throw new Error("server does not support HTTP Range requests");
  return {
    size,
    slice: async (a, b) => {
      const res = await fetch(url, { headers: { Range: `bytes=${a}-${b - 1}` } });
      if (res.status !== 206) throw new Error("range fetch failed: HTTP " + res.status);
      return new Uint8Array(await res.arrayBuffer());
    },
  };
}

// The game file is shard bytes back to back, then a JSON table, its length and a magic.
async function readToc(src) {
  const tail = await src.slice(src.size - 16, src.size);
  if (new TextDecoder().decode(tail.subarray(8)) !== "IW4BNDL1") throw new Error("not an IW4L game file");
  const tocLen = Number(new DataView(tail.buffer, tail.byteOffset).getBigUint64(0, true));
  const json = await src.slice(src.size - 16 - tocLen, src.size - 16);
  return JSON.parse(new TextDecoder().decode(json)).parts;
}

$("packs").addEventListener("change", (e) => {
  const file = e.target.files[0];
  source = file ? fileSource(file) : null;
  $("picked").textContent = file ? file.name : "no file selected";
  $("start").disabled = !file;
});

async function start() {
  $("start").disabled = true;
  $("bar").hidden = false;
  log("loading engine…");
  await engine;
  const parts = await readToc(source);
  const wanted = ["common", $("map").value];
  let done = 0;
  for (const name of wanted) {
    const part = parts.find((p) => p.name === name);
    if (!part) throw new Error("game file has no part " + name);
    log(`reading ${name} (${Math.round(part.len / 1e6)} MB)…`);
    const bytes = await source.slice(part.offset, part.offset + part.len);
    log(`mounting ${name}…`);
    log(`${name}: ${mount_shard(GAME_DIR, bytes)} files`);
    $("bar").value = ++done / wanted.length;
  }
  await restoreSaved();
  document.body.classList.add("running");
  $("mp-slot").appendChild($("mp"));
  $("iw4l-canvas").focus();
  enableTouch({ force: params.has("touch") });
  if (params.has("launch")) run_map(params.get("launch")); else run();
}

$("start").addEventListener("click", () => { startAudio(); start().catch((e) => log("error: " + e, "err")); });
if (packUrl) {
  urlSource(packUrl).then((src) => {
    source = src;
    $("picked").textContent = "game file from " + packUrl;
    $("start").disabled = false;
    if (params.has("map")) $("map").value = params.get("map");
    if (params.has("autostart")) start().catch((e) => log("error: " + e, "err"));
  }).catch((e) => log("error: " + e, "err"));
}

// ---- Multiplayer: swap two codes, then the engine's lobby takes over ----
let room = null;
let engineReady = false;
const engine = init().then(() => { engineReady = true; });
const peers = $("peers");
const peerRow = new Map();

function showPeer(id, text, ok) {
  let li = peerRow.get(id);
  if (!li) { li = document.createElement("li"); peers.appendChild(li); peerRow.set(id, li); }
  li.textContent = text;
  li.className = ok ? "ok" : "";
}

// The engine module loads once; links can open before the game starts.
const LANES = ["control", "data", "bulk"];

function wire(r) {
  r.onOpen = async (id) => {
    await engine;
    net_open(id);
    showPeer(id, "Player " + id + ": connected, checking route…", true);
    const path = await r.path(id);
    showPeer(id, "Player " + id + ": " + describePath(path), true);
  };
  r.onMessage = (id, lane, bytes) => net_message(id, LANES.indexOf(lane), bytes);
  r.onClose = (id, why) => {
    net_close(id);
    showPeer(id, "Player " + id + ": disconnected (" + why + ")", false);
  };
}

// Hand the engine what arrived, send what it queued. A short timer is enough:
// the relay is stepped here, not on a thread.
setInterval(() => {
  if (!engineReady) return;
  audio_tick();
  const packed = net_tick();
  const view = new DataView(packed.buffer, packed.byteOffset, packed.byteLength);
  for (let at = 0; at < packed.length;) {
    const member = view.getUint32(at, true), lane = packed[at + 4], len = view.getUint32(at + 5, true);
    room?.send(member, LANES[lane], packed.subarray(at + 9, at + 9 + len));
    at += 9 + len;
  }
}, 5);

let pending = null;
async function newInvite() {
  const { member, code } = await room.invite();
  pending = member;
  $("invite").value = code;
  $("answer").value = "";
}

$("host-new").addEventListener("click", () => {
  room?.close();
  room = new Host(); wire(room);
  $("host-box").hidden = false; $("join-box").hidden = true;
  newInvite().catch((e) => log("error: " + e, "err"));
});
$("another").addEventListener("click", () => newInvite().catch((e) => log("error: " + e, "err")));
$("accept").addEventListener("click", () =>
  room.accept(pending, $("answer").value).then(() => log("friend connecting…")).catch((e) => log("error: " + e.message, "err")));

$("join-new").addEventListener("click", () => {
  room?.close();
  room = new Guest(); wire(room);
  $("join-box").hidden = false; $("host-box").hidden = true;
});
$("make-answer").addEventListener("click", () =>
  room.join($("host-invite").value).then((code) => { $("my-answer").value = code; }).catch((e) => log("error: " + e.message, "err")));

// Diagnostic: start the engine with no game files to see how far it gets.
if (params.has("bare")) {
  (async () => {
    log("bare start: webgpu=" + !!navigator.gpu + " isolated=" + self.crossOriginIsolated);
    await engine;
    document.body.classList.add("running");
    run();
  })().catch((e) => log("error: " + e, "err"));
}

// Diagnostic: host and guest in one tab, exchange codes, ping both channels.
if (params.has("rtctest")) {
  (async () => {
    const host = new Host(), guest = new Guest();
    const t0 = performance.now();
    const seen = new Set();
    const enc = new TextEncoder(), dec = new TextDecoder();
    host.onMessage = (id, lane, b) => { seen.add("host<-" + lane + ":" + dec.decode(b)); if (lane === "bulk") seen.add(dec.decode(b) === "sum:" + sum(blob) ? "bulk-ok" : "bulk-bad"); log("host got " + lane + " " + dec.decode(b)); };
    guest.onMessage = (id, lane, b) => { log("guest got " + lane + " " + dec.decode(b)); guest.send(0, lane, enc.encode("pong")); };
    // A 300 KB blob across the reliable lane exercises the chunking.
    const blob = new Uint8Array(300000).map((_, i) => (i * 31) & 255);
    const sum = (b) => b.reduce((n, x) => (n * 33 + x) >>> 0, 5381);
    guest.onMessage = (id, lane, b) => {
      if (b.length === blob.length) { guest.send(0, "bulk", enc.encode("sum:" + sum(b))); return; }
      log("guest got " + lane + " " + dec.decode(b)); guest.send(0, lane, enc.encode("pong"));
    };
    host.onOpen = async (id) => {
      host.send(id, "bulk", blob);
      log("RTCTEST open after " + Math.round(performance.now() - t0) + " ms");
      host.send(id, "control", enc.encode("ping"));
      host.send(id, "data", enc.encode("ping"));
      log("RTCTEST path " + await host.path(id));
    };
    const { member, code } = await host.invite();
    log("RTCTEST invite " + code.length + " chars");
    const answer = await guest.join(code);
    log("RTCTEST answer " + answer.length + " chars");
    await host.accept(member, answer);
    await new Promise((r) => setTimeout(r, 6000));
    log("RTCTEST result " + (seen.has("host<-control:pong") && seen.has("host<-data:pong") && seen.has("bulk-ok") ? "PASS" : "FAIL " + [...seen]));
  })().catch((e) => log("RTCTEST error: " + e, "err"));
}

$("mp-toggle").addEventListener("click", () => {
  $("mp-slot").classList.toggle("open");
  $("mp").open = true;
});

// The same actions as the in-game menu, for when the controls are awkward.
const matchMap = () => params.get("launch") || $("map").value;
$("eng-host").addEventListener("click", () => { net_host(matchMap(), "dm"); log("opening a room…"); });
$("eng-join").addEventListener("click", () => { net_join_first(); log("joining the host's room…"); });
$("eng-start").addEventListener("click", () => { net_start(matchMap(), "dm"); log("starting the match…"); });

// Sound: the engine mixes in wasm; a Web Audio node pulls from it. Created from
// the Start click so the browser lets it play.
let audioCtx = null;
export function startAudio() {
  if (audioCtx) return;
  try {
    audioCtx = new AudioContext({ latencyHint: 'interactive' });
    const node = audioCtx.createScriptProcessor(2048, 0, 2);
    node.onaudioprocess = (e) => {
      if (!engineReady) return;
      const frames = e.outputBuffer.length;
      const pcm = audio_pull(frames, audioCtx.sampleRate);
      const l = e.outputBuffer.getChannelData(0), r = e.outputBuffer.getChannelData(1);
      let peak = 0;
      for (let i = 0; i < frames; i++) { l[i] = pcm[i * 2]; r[i] = pcm[i * 2 + 1]; peak = Math.max(peak, Math.abs(l[i]), Math.abs(r[i])); }
      const stat = (window.__audio ||= { blocks: 0, peak: 0, state: "" });
      stat.blocks++; stat.peak = Math.max(stat.peak, peak); stat.state = audioCtx.state;
    };
    node.connect(audioCtx.destination);
    const resume = () => audioCtx.state !== 'running' && audioCtx.resume();
    addEventListener('pointerdown', resume);
    addEventListener('keydown', resume);
  } catch (e) { console.warn('audio unavailable', e); }
}

// Touch layout follows the match: sticks while playing, pointer while a menu is up.
{
  let ownClient = "0"; // the hosting tab is client 0; a guest learns its number at signon
  const original = console.log.bind(console);
  console.log = (...args) => {
    original(...args);
    const line = typeof args[0] === "string" ? args[0] : "";
    let m;
    if ((m = line.match(/signon Active epoch=\d+ client=(\d+)/))) ownClient = m[1];
    else if (line.includes("spawn: script client=" + ownClient + " playing")) setTouchMode("game");
    else if (line.includes("menu open: ")) setTouchMode("menu");
  };
}

// Settings, class loadouts and the player profile live in the engine's memory
// filesystem; keep them in IndexedDB so they survive a reload.
const DB_NAME = "iw4l", STORE = "files";
function db() {
  return new Promise((resolve, reject) => {
    const open = indexedDB.open(DB_NAME, 1);
    open.onupgradeneeded = () => open.result.createObjectStore(STORE);
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  });
}
async function restoreSaved() {
  try {
    const d = await db();
    const all = await new Promise((resolve, reject) => {
      const store = d.transaction(STORE).objectStore(STORE);
      const keys = store.getAllKeys(), vals = store.getAll();
      vals.onsuccess = () => resolve(keys.result.map((k, i) => [k, vals.result[i]]));
      vals.onerror = () => reject(vals.error);
    });
    for (const [path, bytes] of all) persist_load(path, bytes);
    if (all.length) log("restored " + all.length + " saved setting file(s)");
  } catch (e) { console.warn("saved settings unavailable", e); }
}
let savedDigest = "";
async function saveChanged() {
  if (!engineReady) return;
  const packed = persist_dump();
  const view = new DataView(packed.buffer, packed.byteOffset, packed.byteLength);
  const files = [];
  const text = new TextDecoder();
  for (let at = 0; at < packed.length;) {
    const plen = view.getUint32(at, true); at += 4;
    const path = text.decode(packed.subarray(at, at + plen)); at += plen;
    const len = view.getUint32(at, true); at += 4;
    files.push([path, packed.slice(at, at + len)]); at += len;
  }
  const digest = files.map(([p, b]) => p + ":" + b.length + ":" + b.reduce((h, x) => (h * 31 + x) | 0, 7)).join("|");
  if (digest === savedDigest) return;
  savedDigest = digest;
  try {
    const d = await db();
    const tx = d.transaction(STORE, "readwrite");
    for (const [path, bytes] of files) tx.objectStore(STORE).put(bytes, path);
  } catch (e) { console.warn("could not save settings", e); }
}
setInterval(() => saveChanged().catch(() => {}), 5000);
window.__saved = async () => { const d = await db(); return await new Promise((r) => { const q = d.transaction(STORE).objectStore(STORE).getAllKeys(); q.onsuccess = () => r(q.result); }); };
