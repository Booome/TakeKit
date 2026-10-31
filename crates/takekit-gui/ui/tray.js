// The popover's page. Once the panel has hidden itself this is the only UI
// left, so it has to be able to end a take on its own.
//
// No bundler and no npm: Tauri injects window.__TAURI__.core, so invoke() is
// the whole API surface.

// `invoke`, `PROBE`, `$` and `clock` come from shared.js, which loads first.

// True from the moment 停止 is pressed until the take is written out, so the
// polling below cannot re-enable the key mid-finalise.
let stopping = false;

// A failed command's word, held for a few seconds. `tick` rewrites the state
// line every 250 ms, so a failure written straight into it survived one frame:
// the popover showed 录制中 again before anyone could read 停止失败, and the
// take looked like it was still going with no word about why.
let flash = null;

function say(text) {
  flash = { text, until: Date.now() + 4000 };
}

function stateWord(status, paused) {
  if (stopping) return t("tray.stateStopping");
  if (flash && Date.now() < flash.until) return flash.text;
  if (paused) return t("tray.statePaused");
  return status.recording ? t("tray.stateRecording") : t("tray.stateIdle");
}

function row(source) {
  const el = document.createElement("div");
  el.className = "row";

  const point = document.createElement("span");
  point.className = source.alive ? "point" : "point stopped";

  const name = document.createElement("span");
  name.className = "name";
  name.textContent = source.id;

  const size = document.createElement("span");
  size.className = "size";
  size.textContent = `${(source.bytes / (1024 * 1024)).toFixed(1)} MB`;

  el.append(point, name, size);
  if (!source.alive) {
    // The diamond already says this; the word is for anyone who cannot tell
    // the shapes apart.
    const note = document.createElement("span");
    note.className = "note";
    note.textContent = t("tray.stopped");
    el.append(note);
  }
  return el;
}

// The window is sized to this card, not the card to the window. An idle take is
// one line of text and a take with three sources is three rows, so a height
// fixed at build time is always wrong for something: at 132px the second row of
// keys was already half cut off by the taskbar. The card is the only thing that
// knows how tall it needs to be, and a page cannot resize its own window — so
// it measures and asks.
const card = document.querySelector(".card");

// The last height sent, so a tick that changed nothing sends nothing. 0 means
// nothing has been sent yet.
let fitted = 0;

function fit() {
  // A hidden window can report a zero-width layout; measuring that would ask
  // for a 0×0 card and the first open would snap from a sliver to the real
  // size. Nothing to measure yet is not a measurement.
  if (!card.offsetWidth) return;
  // `display`-level measurement: with `.measuring` the card stops filling the
  // window, and `offsetHeight` then reports what it actually needs. Both reads
  // are synchronous, so the class is gone again before anything is painted.
  card.classList.add("measuring");
  const height = card.offsetHeight;
  card.classList.remove("measuring");
  if (Math.abs(height - fitted) < 1) return;
  fitted = height;
  PROBE("tray:fit " + height);
  invoke("size_tray", { height }).catch((e) => {
    // A wrong height clips the card; the log is the only place that says why.
    console.error("size_tray", e);
    PROBE("tray:fit-failed:" + String(e).slice(0, 120));
  });
}

async function tick() {
  let status;
  try {
    status = await invoke("status");
  } catch (e) {
    return;
  }

  // Three states, not two. `live` is "bytes are moving" and a paused take is
  // moving none of them; without the split the popover showed a red dot, a
  // frozen clock and the word 录制中 while nothing was being written — the same
  // picture as an encoder that had hung (reported 2026-10-27).
  const paused = status.recording && status.paused;
  document.body.classList.toggle("live", status.recording && !paused);
  document.body.classList.toggle("paused", paused);
  $("clock").textContent = clock(status.elapsed);
  // `stateWord` puts `stopping` first: during finalise the encoder is already
  // out of the session state, so `recording` is false while the take is still
  // writing.
  $("state").textContent = stateWord(status, paused);
  // The one number a 220px card otherwise cannot show: a frozen clock says the
  // counting stopped, only this says for how long — and that the stretch is not
  // in the file, which is the question a pause raises.
  const note = $("pause-note");
  note.classList.toggle("hidden", !paused);
  // `pause_len`, snake: this is the command's own payload, not the panel's
  // state object (which renames it on the way in).
  if (paused) note.textContent = t("tray.pauseNote", { at: shortClock(status.pause_len) });
  $("stop").disabled = !status.recording || stopping;
  // Pause follows the panel's rule — mid-take only. The countdown runs on the
  // veil, so a pending take already reads as `recording: false` here and needs
  // no separate guard. The label is read from the backend every tick instead of
  // being kept locally: the panel can pause while this popover is hidden.
  $("pause").disabled = !status.recording || stopping;
  $("pause").textContent = status.paused ? t("tray.resume") : t("tray.pause");

  const rows = $("rows");
  rows.replaceChildren(...status.sources.map(row));
  $("empty").classList.toggle("hidden", status.sources.length > 0);

  // Last, after the rows and the labels are in place: the height is a function
  // of what is on the card right now.
  fit();
}

$("stop").addEventListener("click", async () => {
  if (stopping) return;
  stopping = true;
  $("stop").disabled = true;
  $("state").textContent = t("tray.stateStopping");
  try {
    await invoke("stop_recording");
  } catch (e) {
    // A 220px card has no room for the engine's error text; the log has it.
    console.error("stop_recording", e);
    say(t("tray.stopFailed"));
  }
  stopping = false;
  await tick();
});

$("show").addEventListener("click", () => {
  invoke("show_main").catch((e) => console.error("show_main", e));
});

// Which way the toggle goes is decided by the backend, not by a flag cached
// here: the panel may have paused or resumed while the popover was hidden, and
// a stale flag would send the opposite command.
$("pause").addEventListener("click", async () => {
  $("pause").disabled = true;
  try {
    const status = await invoke("status");
    await invoke(status.paused ? "resume_recording" : "pause_recording");
  } catch (e) {
    // A 220px card has no room for the engine's error text; the log has it.
    console.error("pause", e);
    say(t("tray.pauseFailed"));
  }
  await tick();
});

// Adding a source mid-take happens in the panel: this brings it back with the
// picker already open, so the popover does not need a second copy of six kinds
// and a window search in 220 pixels.
$("add").addEventListener("click", () => {
  invoke("add_source_from_tray").catch((e) =>
    console.error("add_source_from_tray", e),
  );
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") invoke("hide_tray").catch(() => {});
});

// Polled only while the popover is on screen: the window stays alive when it
// is hidden, and a tick every 250ms into an invisible webview is just wake-ups.
async function loop() {
  const onScreen = document.visibilityState === "visible";
  if (onScreen) await tick();
  setTimeout(loop, onScreen ? 250 : 1000);
}

document.addEventListener("visibilitychange", () => {
  PROBE(`tray:${document.visibilityState}`);
  if (document.visibilityState === "visible") tick();
});

// ── Language and palette ───────────────────────────────────────────────────
// This window has its own document: it has to be told, or the popover would
// stay in the old language while the panel switched. `ui:settings` carries the
// pair in force, and the startup read covers a panel that changed it before
// this page was ready.
function applySettings(info) {
  if (!info) return;
  applyTheme(info.theme);
  setUiLang(info.language);
  tick();
}

// A failed read is not silent: the popover would sit in the default
// language and theme with nothing to say why. The probe log is where a
// "why is the tray wrong" question gets answered.
invoke("ui_settings")
  .then(applySettings)
  .catch((e) => {
    console.error("ui_settings failed:", e);
    PROBE("tray:ui-settings-failed:" + String(e).slice(0, 120));
  });
window.__TAURI__.event.listen("ui:settings", (e) => applySettings(e.payload));

tick();
loop();
