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

// The veil is shown before `overlay:countdown` arrives, so it must start
// empty: a digit left in the DOM flashes for a frame every time it comes up.
// (It used to say GO for the TAKEKIT_OVERLAY_DEMO path; the demo only needs
// the cover, not the text.)
big.textContent = "";
big.classList.remove("go");

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
// `take:done` ends it — Rust takes the veil down and brings the panel back.
// There is deliberately no "saved" banner at the end (README B4 note): the
// fresh history row is the success feedback, and a checklist that completed
// needs no second ending. The veil is up exactly as long as the finalise
// takes — no early dismiss, since (focusable(false) + click-through) nothing
// can reach this window to dismiss it, and the wait is short.

const sub = document.getElementById("sub");
const list = document.getElementById("list");

const fmtClock = (secs) => {
  const s = Math.max(0, Math.floor(secs));
  const p2 = (n) => String(n).padStart(2, "0");
  return `${p2(Math.floor(s / 3600))}:${p2(Math.floor((s % 3600) / 60))}:${p2(s % 60)}`;
};

window.__TAURI__.event.listen("take:finalize", (e) => {
  const p = e.payload || {};
  stopLoop();
  big.classList.remove("go");
  big.textContent = "STOP";
  sub.textContent = fmtClock(Number(p.elapsed) || 0);
  list.replaceChildren();
  PROBE(`overlay:finalize total=${p.total ?? "?"}`);
});

window.__TAURI__.event.listen("take:source", (e) => {
  const p = e.payload || {};
  const row = document.createElement("div");
  row.className = "row";
  const name = document.createElement("span");
  name.className = "row-id";
  name.textContent = String(p.id ?? "");
  row.append(name);
  if (p.error) {
    // cam  ✗  <why> — the reason is the source's own log tail or a stand-in
    // line, already one sentence in English/Chinese from the backend.
    const bad = document.createElement("span");
    bad.className = "row-bad";
    bad.textContent = "✗";
    const why = document.createElement("span");
    why.className = "row-why";
    why.textContent = String(p.error);
    row.append(bad, why);
  } else {
    const time = document.createElement("span");
    time.className = "row-num";
    time.textContent = fmtClock(Number(p.seconds) || 0);
    row.append(time);
    const size = document.createElement("span");
    size.className = "row-num";
    size.textContent = p.width && p.height ? `${p.width}×${p.height}` : "";
    row.append(size);
    const bytes = document.createElement("span");
    bytes.className = "row-num";
    bytes.textContent = `${((Number(p.bytes) || 0) / (1024 * 1024)).toFixed(1)} MB`;
    row.append(bytes);
  }
  list.append(row);
  PROBE(`overlay:source ${p.id} ok=${!p.error}`);
});

window.__TAURI__.event.listen("take:done", () => {
  PROBE("overlay:done");
});

// The stop screen's two tones arrive like the countdown-off one does on the
// panel: a synthesised tone, played by whichever surface is visible.
window.__TAURI__.event.listen("take:beep", (e) => {
  PROBE(`take:beep-handled audio=${window.audioState()}`);
  window.beep(e.payload.hz, e.payload.ms);
});
