// The popover's page. Once the panel has hidden itself this is the only UI
// left, so it has to be able to end a take on its own.
//
// No bundler and no npm: Tauri injects window.__TAURI__.core, so invoke() is
// the whole API surface.

// `invoke`, `PROBE`, `$` and `clock` come from shared.js, which loads first.

// True from the moment 停止 is pressed until the take is written out, so the
// polling below cannot re-enable the key mid-finalise.
let stopping = false;

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

async function tick() {
  let status;
  try {
    status = await invoke("status");
  } catch (e) {
    return;
  }

  document.body.classList.toggle("live", status.recording);
  $("clock").textContent = clock(status.elapsed);
  // `stopping` first: during finalise the encoder is already out of the
  // session state, so `recording` is false while the take is still writing.
  $("state").textContent = stopping
    ? t("tray.stateStopping")
    : status.recording
      ? t("tray.stateRecording")
      : t("tray.stateIdle");
  $("stop").disabled = !status.recording || stopping;

  const rows = $("rows");
  rows.replaceChildren(...status.sources.map(row));
  $("empty").classList.toggle("hidden", status.sources.length > 0);
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
    $("state").textContent = t("tray.stopFailed");
  }
  stopping = false;
  await tick();
});

$("show").addEventListener("click", () => {
  invoke("show_main").catch((e) => console.error("show_main", e));
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
