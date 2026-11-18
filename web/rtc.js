// Serverless peer-to-peer transport for IW4L.
//
// The host makes one invite code per friend; the friend pastes it and sends an
// answer code back. No server is involved: the codes carry the WebRTC session
// descriptions (with their ICE candidates) and travel over any chat.
//
// Every peer has two data channels, carrying three lanes:
//   control  reliable and ordered     (lobby, requests, state that must arrive)
//   bulk     same channel, big blobs  (map bootstrap), split into chunks
//   data     unordered, no resends    (snapshots and inputs; newest wins)

// Order of attempts: direct UDP first, then relays on 443 for networks that
// block UDP or sit behind carrier-grade NAT. Replace `turn` with your own
// relay (coturn on port 443) by setting window.IW4L_ICE before loading.
// Players behind strict NATs add a relay in the page address:
//   ?turn=turns:relay.example.com:443?transport=tcp&turnuser=NAME&turnpass=SECRET
function turnFromAddress() {
  const q = new URLSearchParams(globalThis.location?.search ?? "");
  const urls = q.get("turn");
  if (!urls) return [];
  const server = { urls: urls.split(",") };
  if (q.get("turnuser")) { server.username = q.get("turnuser"); server.credential = q.get("turnpass") ?? ""; }
  return [server];
}

export const DEFAULT_ICE = {
  iceServers: [
    { urls: ["stun:stun.l.google.com:19302", "stun:stun.cloudflare.com:3478"] },
    ...(globalThis.IW4L_TURN ?? []),
    ...turnFromAddress(),
  ],
  iceCandidatePoolSize: 2,
};

const GATHER_TIMEOUT_MS = 6000;

// Reliable messages are cut into chunks so a big blob never trips the browser's
// message size limit. Header: lane (1), message id (4), index (2), count (2).
const CHUNK = 15000;
const HEADER = 9;
const LANE_ID = { control: 0, bulk: 2 };
const LANE_NAME = { 0: "control", 2: "bulk" };
const DATA_BACKLOG = 256 * 1024;

async function pipe(bytes, stream) {
  const out = new Response(new Blob([bytes]).stream().pipeThrough(stream));
  return new Uint8Array(await out.arrayBuffer());
}

function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function unb64url(text) {
  const pad = "=".repeat((4 - (text.length % 4)) % 4);
  const bin = atob(text.replace(/-/g, "+").replace(/_/g, "/") + pad);
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

/** Pack a session description into a short, paste-friendly code. */
export async function encodeCode(desc) {
  const json = new TextEncoder().encode(JSON.stringify({ t: desc.type, s: desc.sdp }));
  return "IW4L1." + b64url(await pipe(json, new CompressionStream("deflate-raw")));
}

export async function decodeCode(code) {
  const text = code.trim();
  if (!text.startsWith("IW4L1.")) throw new Error("This is not an IW4L code.");
  const json = await pipe(unb64url(text.slice(6)), new DecompressionStream("deflate-raw"));
  const { t, s } = JSON.parse(new TextDecoder().decode(json));
  return { type: t, sdp: s };
}

function gathered(pc) {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      pc.removeEventListener("icegatheringstatechange", check);
      resolve();
    };
    const check = () => pc.iceGatheringState === "complete" && done();
    pc.addEventListener("icegatheringstatechange", check);
    setTimeout(done, GATHER_TIMEOUT_MS);
  });
}

/** One remote player. */
class Link {
  constructor(id, pc, hooks) {
    this.id = id;
    this.pc = pc;
    this.hooks = hooks;
    this.control = null;
    this.data = null;
    this.open = false;
    pc.addEventListener("iceconnectionstatechange", () => console.log("rtc link " + id + " ice " + pc.iceConnectionState));
    pc.addEventListener("connectionstatechange", () => {
      console.log("rtc link " + id + " " + pc.connectionState);
      const s = pc.connectionState;
      if (s === "failed" || s === "disconnected" || s === "closed") this.#closed(s);
    });
  }

  attach(channel) {
    channel.binaryType = "arraybuffer";
    if (channel.label === "control") this.control = channel;
    else if (channel.label === "data") this.data = channel;
    else return;
    channel.addEventListener("message", (e) => {
      const bytes = new Uint8Array(e.data);
      if (channel.label === "data") this.hooks.onMessage(this.id, "data", bytes);
      else this.#chunk(bytes);
    });
    channel.addEventListener("open", () => {
      if (!this.open && this.control?.readyState === "open" && this.data?.readyState === "open") {
        this.open = true;
        this.hooks.onOpen(this.id);
      }
    });
    channel.addEventListener("close", () => this.#closed("channel closed"));
  }

  #closed(reason) {
    if (this.gone) return;
    this.gone = true;
    this.hooks.onClose(this.id, reason);
  }

  #chunk(bytes) {
    if (bytes.length < HEADER) return;
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const lane = LANE_NAME[bytes[0]];
    const id = view.getUint32(1, true), index = view.getUint16(5, true), count = view.getUint16(7, true);
    if (!lane) return;
    const body = bytes.subarray(HEADER);
    if (count === 1) return this.hooks.onMessage(this.id, lane, body);
    this.partial ??= new Map();
    const key = lane + ":" + id;
    let entry = this.partial.get(key);
    if (!entry) { entry = { parts: new Array(count), have: 0 }; this.partial.set(key, entry); }
    if (!entry.parts[index]) { entry.parts[index] = body; entry.have++; }
    if (entry.have < count) return;
    this.partial.delete(key);
    const whole = new Uint8Array(entry.parts.reduce((n, p) => n + p.length, 0));
    let at = 0;
    for (const part of entry.parts) { whole.set(part, at); at += part.length; }
    this.hooks.onMessage(this.id, lane, whole);
  }

  send(lane, bytes) {
    if (lane === "data") {
      // Newest state wins: when the link is congested, drop instead of queueing.
      if (this.data?.readyState === "open" && this.data.bufferedAmount < DATA_BACKLOG) this.data.send(bytes);
      return;
    }
    const ch = this.control;
    if (ch?.readyState !== "open") return;
    const id = (this.nextMessage = ((this.nextMessage ?? 0) + 1) >>> 0);
    const count = Math.max(1, Math.ceil(bytes.length / CHUNK));
    for (let index = 0; index < count; index++) {
      const body = bytes.subarray(index * CHUNK, (index + 1) * CHUNK);
      const framed = new Uint8Array(HEADER + body.length);
      const view = new DataView(framed.buffer);
      framed[0] = LANE_ID[lane];
      view.setUint32(1, id, true); view.setUint16(5, index, true); view.setUint16(7, count, true);
      framed.set(body, HEADER);
      ch.send(framed);
    }
  }

  close() {
    this.control?.close();
    this.data?.close();
    this.pc.close();
  }

  /** "direct", "nat" (via STUN) or "relay" (via TURN): what the match runs over. */
  async path() {
    const stats = await this.pc.getStats();
    let pairId = null;
    stats.forEach((r) => {
      if (r.type === "transport" && r.selectedCandidatePairId) pairId = r.selectedCandidatePairId;
    });
    const pair = pairId ? stats.get(pairId) : null;
    if (!pair) return "unknown";
    const local = stats.get(pair.localCandidateId);
    const remote = stats.get(pair.remoteCandidateId);
    const kinds = [local?.candidateType, remote?.candidateType];
    if (kinds.includes("relay")) return "relay";
    if (kinds.includes("srflx") || kinds.includes("prflx")) return "nat";
    return "direct";
  }
}

/** Shared by Host and Guest: member bookkeeping and the callbacks. */
class Room {
  constructor(ice = DEFAULT_ICE) {
    this.ice = ice;
    this.links = new Map();
    this.nextId = 1;
    this.onMessage = () => {};
    this.onOpen = () => {};
    this.onClose = () => {};
  }

  _link(pc, id = this.nextId++) {
    const link = new Link(id, pc, {
      onMessage: (...a) => this.onMessage(...a),
      onOpen: (...a) => this.onOpen(...a),
      onClose: (...a) => {
        this.links.delete(a[0]);
        this.onClose(...a);
      },
    });
    this.links.set(id, link);
    return link;
  }

  send(member, lane, bytes) {
    this.links.get(member)?.send(lane, bytes);
  }

  broadcast(lane, bytes) {
    for (const link of this.links.values()) link.send(lane, bytes);
  }

  path(member) {
    return this.links.get(member)?.path() ?? Promise.resolve("unknown");
  }

  close() {
    for (const link of this.links.values()) link.close();
    this.links.clear();
  }
}

/** The player who starts the match. Authoritative: guests talk to the host only. */
export class Host extends Room {
  /** Make an invite for one friend. Resolves to { member, code }. */
  async invite() {
    const pc = new RTCPeerConnection(this.ice);
    const link = this._link(pc);
    link.attach(pc.createDataChannel("control", { ordered: true }));
    link.attach(pc.createDataChannel("data", { ordered: false, maxRetransmits: 0 }));
    await pc.setLocalDescription(await pc.createOffer());
    await gathered(pc);
    return { member: link.id, code: await encodeCode(pc.localDescription) };
  }

  /** Finish the handshake with the answer code the friend sent back. */
  async accept(member, answerCode) {
    const link = this.links.get(member);
    if (!link) throw new Error("This invite is no longer open.");
    await link.pc.setRemoteDescription(await decodeCode(answerCode));
  }
}

/** A friend joining a host. Host is always member 0. */
export class Guest extends Room {
  /** Paste the host's invite; resolves to the answer code to send back. */
  async join(inviteCode) {
    const pc = new RTCPeerConnection(this.ice);
    const link = this._link(pc, 0);
    pc.addEventListener("datachannel", (e) => link.attach(e.channel));
    await pc.setRemoteDescription(await decodeCode(inviteCode));
    await pc.setLocalDescription(await pc.createAnswer());
    await gathered(pc);
    return encodeCode(pc.localDescription);
  }
}

/** Plain-language explanation of a connection path for the lobby screen. */
export function describePath(path) {
  switch (path) {
    case "direct": return "Direct connection";
    case "nat": return "Direct connection through your router";
    case "relay": return "Through a relay (slower, but works behind strict networks)";
    default: return "Connecting…";
  }
}
