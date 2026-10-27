// The prompt layer. Task B2 proved only that the veil covers the screen; B3
// adds the countdown that runs before a take starts, its beeps and the Esc
// escape.
//
// The veil owns the clock: the panel is hidden while the countdown runs, and a
// hidden window has its timers throttled — a chain of setTimeouts would fire as
// one burst. One requestAnimationFrame loop on one performance.now() clock
// cannot drift that way, and the veil is visible for the whole countdown, so
// rAF runs at full rate here.

// `invoke` and `PROBE` come from shared.js, which loads first.

const big = document.getElementById("big");
// The stop screen's own fields. Declared here, not next to the listeners that
// fill them, because startCountdown has to wipe them: a new take beginning
// behind the veil must not show the previous take's duration and file sizes.
const sub = document.getElementById("sub");
const list = document.getElementById("list");
const foot = document.getElementById("foot");

// The veil is shown before `overlay:countdown` arrives, so it must start
// empty: a digit left in the DOM flashes for a frame every time it comes up.
// (It used to say GO for the TAKEKIT_OVERLAY_DEMO path; the demo only needs
// the cover, not the text.)
big.textContent = "";
big.classList.remove("go");

// Which screen the veil is showing, as far as a click is concerned. Only the
// finished stop screen is dismissible: Rust gives the window the clicks once
// the take is over (it is click-through while the take runs, so the app being
// recorded keeps painting), and this is the half that knows a click there means
// "I have read it".
let stopScreen = false;

// ── The countdown ───────────────────────────────────────────────────────────

const GO_SECS = 1.0; // GO stays up this long, then the veil comes down

// The live run, or null. `raf` is the pending frame; `digit` is the number
// currently on screen (so each number beeps exactly once); `gone` is set once
// Esc or the schedule has ended the run, so a late frame cannot restart it.
let run = null;

function stopLoop() {
  if (run && run.raf) cancelAnimationFrame(run.raf);
  run = null;
}

window.__TAURI__.event.listen("overlay:countdown", (e) => {
  const secs = Number(e.payload && e.payload.secs) || 0;
  startCountdown(secs);
});

// Rust wakes the audio before the engine probe, which can take seconds: that
// is the free window for resume() to finish, so the first beep of the
// countdown is not the one that pays for it.
window.__TAURI__.event.listen("overlay:wake", () => {
  window.wakeAudio();
});

// The take was cancelled (Esc, or the hotkey again): stop the clock. Hiding
// the window does not stop it — Chromium keeps feeding a just-hidden window
// frames for a moment, so without this the remaining beeps (and the GO tone)
// would play over a take that no longer exists.
window.__TAURI__.event.listen("overlay:stop", () => {
  PROBE("overlay:stop");
  stopLoop();
});

function startCountdown(secs) {
  stopLoop();
  big.classList.remove("go");
  // The stop screen's leftovers belong to the previous take. `big` was reset
  // here but `sub` and `list` were not, so a new countdown could run on top of
  // the old duration and per-file rows — last take's "00:00:13" and "0.3 MB"
  // stayed on screen under the freshly painted "1".
  sub.textContent = "";
  list.replaceChildren();
  // A countdown is not dismissible, and its "click anywhere to close" line
  // would be a promise this screen does not keep.
  stopScreen = false;
  foot.hidden = true;
  // Whatever the veil last showed is still in the DOM — the demo's GO, or a
  // digit left by a cancelled countdown — and the veil becomes visible before
  // its first frame arrives. Paint the first digit NOW so a stale one never
  // flashes on the freshly-shown veil (a green GO here reads as "the
  // countdown already finished").
  big.textContent = String(Math.ceil(secs));
  PROBE(`overlay:countdown:${secs}`);
  run = {
    secs,
    // Set on the first painted frame, not here: see step().
    t0: null,
    raf: 0,
    digit: null,
    goShown: false,
    finished: false,
    gone: false,
  };
  run.raf = requestAnimationFrame(step);
}

function step() {
  if (!run || run.gone) return;
  // The clock starts at the first painted frame, not at the event. A window
  // that was just made visible takes a moment to produce its first frame, and
  // counting from the event would shorten the FIRST digit by exactly that
  // much — the 3 would show for ~0.7s while 2 and 1 got a full second each.
  // Starting here costs the same delay once, at the head, where it belongs.
  if (run.t0 === null) run.t0 = performance.now();
  const t = (performance.now() - run.t0) / 1000;

  if (t < run.secs) {
    const n = Math.ceil(run.secs - t);
    if (n !== run.digit) {
      run.digit = n;
      big.textContent = String(n);
      window.beep(440, 120);
      // t is the JS-side clock (the probe's own timestamp is Rust's, and the
      // first frame's message can be delayed by the compositor bringing the
      // just-shown window up). audio is the context state at the moment of
      // the beep: "running" everywhere means the beeps were heard on time,
      // which is how the countdown is actually judged.
      PROBE(`overlay:digit:${n} t=${t.toFixed(3)} audio=${window.audioState()}`);
    }
    run.raf = requestAnimationFrame(step);
    return;
  }

  if (!run.goShown) {
    run.goShown = true;
    big.textContent = "GO";
    big.classList.add("go");
    window.beep(880, 1000);
    PROBE("overlay:go");
  }
  if (!run.finished && t >= run.secs + GO_SECS) {
    run.finished = true;
    // One call does both, in this order: hide the veil, then start the
    // encoders. Spawning while GO is still on screen puts the veil in the
    // take — every screen capture (desktop *and* window sources, which record
    // the screen rectangle the window occupies) sees it.
    PROBE("overlay:finish-invoked");
    invoke("overlay_finish").catch((e) =>
      PROBE("overlay:finish-failed:" + String(e).slice(0, 120)),
    );
    stopLoop();
    return;
  }
  run.raf = requestAnimationFrame(step);
}

// ── Esc ─────────────────────────────────────────────────────────────────────
// Not handled here: the veil is built with `focusable(false)` (WS_EX_NOACTIVATE)
// so it never receives keyboard input — that is exactly what keeps the window
// being recorded active. Esc during the countdown is polled by the Rust side
// (`take::watch_esc`) and cancels the pending take.

// ── The stop screen (B4) ────────────────────────────────────────────────────
// `take:finalize` paints STOP and freezes the timer at the moment the button
// was pressed; `take:source` appends one checklist row per file as it lands;
// `take:done` completes it and makes it dismissible. There is deliberately no
// "saved" banner at the end (README B4 note): the fresh history row is the
// success feedback, and a checklist that completed needs no second ending.
//
// How long it lives (2026-10-27): Rust keeps it up for a minute, which is long
// enough to read the timings and short enough that the panel behind it is never
// hostage. A click anywhere leaves earlier — but only from `take:done` on: the
// files are still being written before that, and the panel has nothing to show
// yet. Rust is what makes the window clickable at that same moment
// (`overlay::set_clickable`), so the timing lives in one place.

// `clock` comes from shared.js (loaded before this file): the panel, the
// veil and the popover all print a duration, and a second copy of the
// formatter here used to be free to drift.
const fmtClock = (secs) => clock(Math.max(0, secs));

// The checklist prints the same take's length once per source, so `00:00:03`
// three times over is three times the width of the number for no more
// information. `shortClock` (shared.js) is that shorter form.

// Sizes in one unit, MB, except where that stops being a size anyone can hold
// in their head: `1536.0 MB` is a number, `1.5 GB` is an amount.
const fmtBytes = (bytes) => {
  const mb = (Number(bytes) || 0) / (1024 * 1024);
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb.toFixed(1)} MB`;
};

window.__TAURI__.event.listen("take:finalize", (e) => {
  const p = e.payload || {};
  stopLoop();
  big.classList.remove("go");
  big.textContent = "STOP";
  sub.textContent = fmtClock(Number(p.elapsed) || 0);
  // One empty slot per file, reserved now, before any of them has landed.
  //
  // The veil centres its three children, so a list that grows a row at a time
  // walks STOP and the timer upwards by half a row per file — measured on the
  // unfixed build, 12.8px on the first row and 15.8px on each one after it.
  // That is the flicker: the screen jumps exactly as many times as the take has
  // sources, right when the user is trying to read the duration.
  //
  // `total` is in this payload, so the height is known here and never has to
  // change: `take:source` fills the next slot instead of adding to the list.
  list.replaceChildren();
  const total = Math.max(0, Number(p.total) || 0);
  for (let i = 0; i < total; i += 1) {
    const slot = document.createElement("div");
    slot.className = "row pending";
    list.append(slot);
  }
  // A new stop: not dismissible until its files are closed.
  stopScreen = false;
  foot.hidden = true;
  PROBE(`overlay:finalize total=${p.total ?? "?"}`);
});

window.__TAURI__.event.listen("take:source", (e) => {
  const p = e.payload || {};
  // Fill the next reserved slot. Only a report with no slot left to fill (more
  // files than `total` promised) creates a row — and only then does the stack
  // move, which is the right trade for a count that was wrong.
  const reserved = list.querySelector(".row.pending");
  const row = reserved || document.createElement("div");
  row.className = "row";
  row.replaceChildren();
  const name = document.createElement("span");
  name.className = "row-id";
  name.textContent = String(p.id ?? "");
  row.append(name);
  if (p.error) {
    // One cell, not two: `✗ ` and the reason belong to the same sentence, and
    // letting the reason span the rest of the row keeps it on the column grid
    // (overlay.css) without a wrapper element to line it up.
    const why = document.createElement("span");
    why.className = "row-why";
    const bad = document.createElement("span");
    bad.className = "row-bad";
    bad.textContent = "✗";
    why.append(bad, document.createTextNode(` ${p.error}`));
    row.append(why);
  } else {
    const time = document.createElement("span");
    time.className = "row-num";
    time.textContent = shortClock(Number(p.seconds) || 0);
    const size = document.createElement("span");
    size.className = "row-num row-res";
    // An audio-only source has no frame size. The dash is not decoration — a
    // blank cell is what used to pull that row's size column left (2026-10-27).
    size.textContent = p.width && p.height ? `${p.width}×${p.height}` : "—";
    const bytes = document.createElement("span");
    bytes.className = "row-num";
    bytes.textContent = fmtBytes(p.bytes);
    row.append(time, size, bytes);
  }
  if (!reserved) list.append(row);
  PROBE(`overlay:source ${p.id} ok=${!p.error}`);
});

window.__TAURI__.event.listen("take:done", () => {
  // Every file is closed, so the screen can be left. Rust made the window
  // clickable on this same event; the line it enables says so.
  stopScreen = true;
  foot.hidden = false;
  PROBE("overlay:done");
});

// Click = leave. The veil covers the whole virtual screen, so there is no
// smaller target to aim at and nothing else on it to press: any click is the
// answer. `pointerdown` rather than `click`, because a click needs the press and
// release in one place and a window that is not focused can lose the release.
window.addEventListener("pointerdown", () => {
  if (!stopScreen) return;
  stopScreen = false;
  foot.hidden = true;
  PROBE("overlay:dismiss-click");
  invoke("overlay_dismiss").catch((e) =>
    PROBE("overlay:dismiss-failed:" + String(e).slice(0, 120)),
  );
});

// The stop screen's hint is in the panel's language, and the two can change
// while this page is alive (the veil is created at startup and reused).
window.__TAURI__.event.listen("ui:settings", (e) => {
  const lang = e.payload && e.payload.language;
  if (lang) setUiLang(lang);
});

// The stop screen's two tones arrive like the countdown-off one does on the
// panel: a synthesised tone, played by whichever surface is visible.
window.__TAURI__.event.listen("take:beep", (e) => {
  PROBE(`take:beep-handled audio=${window.audioState()}`);
  window.beep(e.payload.hz, e.payload.ms);
});
