import { enableTouch } from "./touch.js";
import { Host, Guest, describePath } from "./rtc.js";
import init, { mount_shard, run } from "./pkg/iw4l_web.js";

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
  await init();
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
  document.body.classList.add("running");
  $("iw4l-canvas").focus();
  enableTouch({ force: params.has("touch") });
  run();
}

$("start").addEventListener("click", () => start().catch((e) => log("error: " + e, "err")));
if (packUrl) {
  urlSource(packUrl).then((src) => {
    source = src;
    $("picked").textContent = "game file from " + packUrl;
    $("start").disabled = false;
    if (params.has("map")) $("map").value = params.get("map");
    if (params.has("autostart")) start().catch((e) => log("error: " + e, "err"));
  }).catch((e) => log("error: " + e, "err"));
}

// ---- Multiplayer lobby: connect, then prove both channels work ----
let room = null;
const peers = $("peers");
const peerRow = new Map();

function showPeer(id, text, ok) {
  let li = peerRow.get(id);
  if (!li) { li = document.createElement("li"); peers.appendChild(li); peerRow.set(id, li); }
  li.textContent = text;
  li.className = ok ? "ok" : "";
}

function wire(r) {
  r.onOpen = async (id) => {
    showPeer(id, "Player " + id + ": connected, checking route…", true);
    r.send(id, "control", new TextEncoder().encode("ping"));
    const path = await r.path(id);
    showPeer(id, "Player " + id + ": " + describePath(path), true);
  };
  r.onMessage = (id, lane, bytes) => {
    const text = new TextDecoder().decode(bytes);
    if (text === "ping") r.send(id, lane, new TextEncoder().encode("pong"));
    else log("player " + id + " " + lane + ": " + text);
  };
  r.onClose = (id, why) => showPeer(id, "Player " + id + ": disconnected (" + why + ")", false);
}

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
    await init();
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
    host.onMessage = (id, lane, b) => { seen.add("host<-" + lane + ":" + dec.decode(b)); log("host got " + lane + " " + dec.decode(b)); };
    guest.onMessage = (id, lane, b) => { log("guest got " + lane + " " + dec.decode(b)); guest.send(0, lane, enc.encode("pong")); };
    host.onOpen = async (id) => {
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
    log("RTCTEST result " + (seen.has("host<-control:pong") && seen.has("host<-data:pong") ? "PASS" : "FAIL " + [...seen]));
  })().catch((e) => log("RTCTEST error: " + e, "err"));
}
