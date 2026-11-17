// Touch controls: an on-screen layer that feeds the engine the same keyboard and
// mouse events a desktop player would produce. The engine itself is unchanged.
//
// Two layouts, switched by the button in the corner:
//   Menu  a touch acts as the mouse: tap to click, drag to move the pointer
//   Game  left stick walks, the right side turns the view, buttons do the rest

const canvas = document.getElementById("iw4l-canvas");

const KEY = {
  forward: ["KeyW", "w"], back: ["KeyS", "s"], left: ["KeyA", "a"], right: ["KeyD", "d"],
  jump: ["Space", " "], crouch: ["KeyC", "c"], reload: ["KeyR", "r"], use: ["KeyF", "f"],
  sprint: ["ShiftLeft", "Shift"], board: ["Tab", "Tab"], menu: ["Escape", "Escape"],
  weapon: ["Digit1", "1"], melee: ["KeyV", "v"], grenade: ["KeyG", "g"],
};

const LOOK_SPEED = 1.6; // pixels of mouse movement per pixel dragged

const keysDown = new Set();

function key(name, down) {
  if (keysDown.has(name) === down) return;
  down ? keysDown.add(name) : keysDown.delete(name);
  const [code, k] = KEY[name];
  canvas.dispatchEvent(new KeyboardEvent(down ? "keydown" : "keyup", { code, key: k, bubbles: true, cancelable: true }));
}

function pointer(type, x, y, buttons, extra = {}) {
  canvas.dispatchEvent(new PointerEvent(type, {
    pointerType: "mouse", pointerId: 1, isPrimary: true, bubbles: true, cancelable: true,
    clientX: x, clientY: y, button: type === "pointermove" ? -1 : extra.button ?? 0, buttons, ...extra,
  }));
}

let mode = "menu";
let root, stickEl, knobEl, modeBtn;

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
}

function build() {
  const css = document.createElement("style");
  css.textContent = `
  #touch{position:fixed;inset:0;z-index:20;pointer-events:none;touch-action:none;user-select:none;-webkit-user-select:none;
    padding:env(safe-area-inset-top) env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left)}
  #touch *{touch-action:none}
  #touch .zone{position:absolute;pointer-events:auto}
  #touch .stick{left:4%;bottom:8%;width:min(34vw,34vh);aspect-ratio:1;border-radius:50%;border:2px solid rgba(240,163,10,.55);background:rgba(8,10,6,.35)}
  #touch .knob{position:absolute;left:50%;top:50%;width:42%;height:42%;margin:-21%;border-radius:50%;background:rgba(240,163,10,.55);border:2px solid #f0a30a}
  #touch .look{right:0;top:0;width:55%;height:100%}
  #touch .btn{position:absolute;pointer-events:auto;width:clamp(48px,11vh,72px);aspect-ratio:1;border-radius:50%;
    border:2px solid rgba(240,163,10,.7);background:rgba(8,10,6,.55);color:#e9eadf;font:700 .8rem/1 "Exo 2",system-ui,sans-serif;
    display:grid;place-items:center;text-align:center}
  #touch .btn.on{background:rgba(240,163,10,.75);color:#150e00}
  #touch .fire{right:5%;bottom:30%;width:clamp(64px,16vh,96px)}
  #touch .aim{right:20%;bottom:40%}
  #touch .jump{right:20%;bottom:14%}
  #touch .crouch{right:5%;bottom:10%}
  #touch .reload{right:32%;bottom:26%}
  #touch .use{right:32%;bottom:44%}
  #touch .sprint{left:4%;bottom:46%}
  #touch .weapon{right:5%;top:42%}
  #touch .grenade{right:15%;top:30%}
  #touch .melee{right:5%;top:28%}
  #touch .board{left:50%;top:2%;margin-left:-24px}
  #touch .mode{position:absolute;pointer-events:auto;left:2%;top:2%;padding:.5rem .8rem;border-radius:4px;border:1px solid #f0a30a;
    background:rgba(8,10,6,.7);color:#f0a30a;font:400 .9rem "Russo One",system-ui,sans-serif}
  #touch .menu-zone{inset:0}
  #touch[data-mode=menu] .game, #touch[data-mode=game] .menu-zone{display:none}
  `;
  document.head.appendChild(css);

  root = el("div"); root.id = "touch"; root.dataset.mode = mode;

  // Menu: the whole screen is a mouse.
  const menu = el("div", "zone menu-zone");
  let down = false;
  const at = (t) => [t.clientX, t.clientY];
  menu.addEventListener("touchstart", (e) => { e.preventDefault(); const [x, y] = at(e.changedTouches[0]); pointer("pointermove", x, y, 0); down = true; pointer("pointerdown", x, y, 1); }, { passive: false });
  menu.addEventListener("touchmove", (e) => { e.preventDefault(); const [x, y] = at(e.changedTouches[0]); pointer("pointermove", x, y, down ? 1 : 0); }, { passive: false });
  const up = (e) => { e.preventDefault(); const [x, y] = at(e.changedTouches[0]); down = false; pointer("pointerup", x, y, 0); };
  menu.addEventListener("touchend", up, { passive: false });
  menu.addEventListener("touchcancel", up, { passive: false });
  root.appendChild(menu);

  // Game: stick.
  stickEl = el("div", "zone stick game"); knobEl = el("div", "knob"); stickEl.appendChild(knobEl);
  let stickId = null;
  const moveStick = (t) => {
    const r = stickEl.getBoundingClientRect();
    const radius = r.width / 2;
    let dx = (t.clientX - (r.left + radius)) / radius, dy = (t.clientY - (r.top + radius)) / radius;
    const len = Math.hypot(dx, dy);
    if (len > 1) { dx /= len; dy /= len; }
    knobEl.style.transform = `translate(${dx * radius * 0.6}px,${dy * radius * 0.6}px)`;
    key("forward", dy < -0.3); key("back", dy > 0.3); key("left", dx < -0.3); key("right", dx > 0.3);
    key("sprint", dy < -0.92);
  };
  const stopStick = () => { stickId = null; knobEl.style.transform = ""; ["forward", "back", "left", "right", "sprint"].forEach((k) => key(k, false)); };
  stickEl.addEventListener("touchstart", (e) => { e.preventDefault(); stickId = e.changedTouches[0].identifier; moveStick(e.changedTouches[0]); }, { passive: false });
  stickEl.addEventListener("touchmove", (e) => { e.preventDefault(); for (const t of e.changedTouches) if (t.identifier === stickId) moveStick(t); }, { passive: false });
  const endStick = (e) => { for (const t of e.changedTouches) if (t.identifier === stickId) stopStick(); };
  stickEl.addEventListener("touchend", endStick); stickEl.addEventListener("touchcancel", endStick);
  root.appendChild(stickEl);

  // Game: look.
  const look = el("div", "zone look game");
  let lookId = null, lx = 0, ly = 0;
  look.addEventListener("touchstart", (e) => { e.preventDefault(); const t = e.changedTouches[0]; lookId = t.identifier; lx = t.clientX; ly = t.clientY; }, { passive: false });
  look.addEventListener("touchmove", (e) => {
    e.preventDefault();
    for (const t of e.changedTouches) {
      if (t.identifier !== lookId) continue;
      pointer("pointermove", t.clientX, t.clientY, 0, { movementX: (t.clientX - lx) * LOOK_SPEED, movementY: (t.clientY - ly) * LOOK_SPEED });
      lx = t.clientX; ly = t.clientY;
    }
  }, { passive: false });
  const endLook = (e) => { for (const t of e.changedTouches) if (t.identifier === lookId) lookId = null; };
  look.addEventListener("touchend", endLook); look.addEventListener("touchcancel", endLook);
  root.appendChild(look);

  // Game: buttons. Hold buttons keep the key down while touched.
  const hold = (cls, label, onDown, onUp) => {
    const b = el("div", "btn game " + cls, label);
    b.addEventListener("touchstart", (e) => { e.preventDefault(); b.classList.add("on"); onDown(); }, { passive: false });
    const end = (e) => { e.preventDefault(); b.classList.remove("on"); onUp(); };
    b.addEventListener("touchend", end, { passive: false }); b.addEventListener("touchcancel", end, { passive: false });
    root.appendChild(b);
  };
  const keyBtn = (cls, label, name) => hold(cls, label, () => key(name, true), () => key(name, false));
  const mouseBtn = (cls, label, button, mask) => hold(cls, label,
    () => pointer("pointerdown", innerWidth / 2, innerHeight / 2, mask, { button }),
    () => pointer("pointerup", innerWidth / 2, innerHeight / 2, 0, { button }));
  mouseBtn("fire", "FIRE", 0, 1);
  mouseBtn("aim", "AIM", 2, 2);
  keyBtn("jump", "JUMP", "jump"); keyBtn("crouch", "CROUCH", "crouch"); keyBtn("reload", "RELOAD", "reload");
  keyBtn("use", "USE", "use"); keyBtn("weapon", "SWAP", "weapon"); keyBtn("grenade", "NADE", "grenade");
  keyBtn("melee", "KNIFE", "melee"); keyBtn("board", "TAB", "board");

  modeBtn = el("button", "mode", "Menu mode");
  modeBtn.type = "button";
  modeBtn.addEventListener("click", () => setMode(mode === "menu" ? "game" : "menu"));
  root.appendChild(modeBtn);

  // Escape lives next to the mode switch so the pause menu is always reachable.
  const esc = el("button", "mode", "Pause");
  esc.type = "button"; esc.style.left = "auto"; esc.style.right = "2%";
  esc.addEventListener("click", () => { key("menu", true); setTimeout(() => key("menu", false), 60); });
  root.appendChild(esc);

  document.body.appendChild(root);
}

function setMode(next) {
  mode = next;
  root.dataset.mode = mode;
  modeBtn.textContent = mode === "menu" ? "Menu mode" : "Game mode";
  if (mode === "menu") for (const k of [...keysDown]) key(k, false);
}

/** Start the overlay once the game is on screen. Returns true on touch devices. */
export function enableTouch({ force = false } = {}) {
  const coarse = matchMedia("(pointer: coarse)").matches || navigator.maxTouchPoints > 0;
  if (!coarse && !force) return false;
  if (!root) build();
  // Landscape and fullscreen give the controls room; both are best effort.
  document.documentElement.requestFullscreen?.().catch(() => {});
  screen.orientation?.lock?.("landscape").catch(() => {});
  return true;
}
