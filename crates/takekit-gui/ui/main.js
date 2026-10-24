// TakeKit control panel.
//
// No bundler and no npm: Tauri injects window.__TAURI__.core, so invoke() is
// available directly. Source specs mirror the serde shape of
// takekit_core::source::{Source, Capture, Crop} — an externally tagged enum,
// so `{"Window": {...}}`, `{"Camera": {...}}`, `{"Desktop": {"rect": null}}`,
// and for sound `{"Mic": {"device": "..."}}`, `{"SystemAudio": {}}`,
// `{"WindowAudio": {"hwnd": 123, "title": "..."}}`.
//
// `invoke`, `PROBE`, `$` and `clock` come from shared.js, which loads first.

const state = {
  sources: [],
  // Where each source's file begins on the running take's timeline, in
  // seconds, by id. Filled from `add_live_source`'s reply and from every
  // status tick — one number, kept in one place, so the row chip and
  // take.json cannot drift apart.
  starts: new Map(),
  windows: [],
  devices: [],
  // Microphones for the picker's sound groups (`list_mics`), kept apart from
  // `devices` which is the camera list the ffmpeg probe returns.
  mics: [],
  outRoot: "takes",
  encoder: null,
  // "windows" captures in-process through WGC/Media Foundation and keeps
  // recording a window that sits behind others; "ffmpeg" spawns a child per
  // source, the long-standing path.
  backend: "windows",
  marks: true,
  // Yank window sources to the front before recording. Off by default: WGC
  // records covered windows anyway, and moving the user's windows unasked is
  // not a default behaviour.
  raiseSources: false,
  // Seconds of the full-screen countdown before a take starts; 0 = off. The
  // switch only flips between "the seconds remembered here" and off, so
  // toggling it twice does not lose a value B5's number input once allows.
  countdownSecs: 3,
  lastCountdownSecs: 3,
  // The full-screen stop screen while a take finalises (B4/B6). Off means
  // the stop progress lives on this panel's status bar alone.
  finalizePanel: true,
  recording: false,
  // The take's clock is stopped: sources are alive, nothing is being written.
  paused: false,
  // How long the pause in progress has lasted, straight from the backend.
  pauseLen: 0,
  // True between "begin accepted" and the veil saying GO: the panel is hidden
  // and the take is not running yet. F9 again in that window cancels it.
  countdown: false,
  // The stop in progress: between `take:finalize` and `take:done` the files
  // are draining and the clock is frozen at the button press (B4).
  finalizing: false,
  finalizeTotal: 0,
  finalized: 0,
  // The bar's failed state (DESIGN §4.5): why the last begin/spawn/stop did
  // not work. Cleared by the next take, not by the 3s timer — the reason is
  // worth reading at the user's pace.
  error: null,
  elapsed: 0,
  screen: null,
  sourcesLoaded: false,
  // The live shortcut, in the form the registrar round-trips
  // (`control+alt+KeyR`); shown through prettyHotkey(). Replaceable while the
  // app runs.
  hotkey: "F9",
  // True while the chip is waiting for the next key combination.
  capturing: false,
  // Seeded by Rust before load; applySaved() confirms it from the file.
  language: window.__TAKEKIT_LANG__ === "en" ? "en" : "zh",
  theme: "auto",
  // Attached displays (`env_info.monitors`), in the picker's order. A desktop
  // source's rect is matched back against these for its name and size.
  monitors: [],
  // What `env_info` said the automatic encoder resolves to, so the option can
  // read "自动（NVENC）" in whichever language is current.
  encoderName: null,
};

// Built once per data change; typing only flips visibility, no DOM rebuild.
let pickerDom = null;

// Set by the first successful status poll of a take; the probe it writes is
// the one signal that the countdown flag really cleared.
let tickLogged = false;

// The two list panes, as Rust has them. Replaced whole on every refresh: the
// lists are short and always drawn from one snapshot, so a stale half-list is
// not a state this panel can get into.
let journal = { takes: [], log: [], unread_takes: 0, unread_log: 0, unread_log_level: null };
// The take the panel is pointing at as "landed since you last looked". Kept
// apart from the unread count, which is cleared the moment the pane opens.
let freshAt = null;
let activeTab = "sources";

const CROP = () => ({ top: 0, bottom: 0, left: 0, right: 0 });
// fps and quality live on each source; these seed a newly added one.
const DEFAULT_FPS = 30;
const DEFAULT_QUALITY = 20;
const FPS_OPTIONS = [15, 24, 30, 60];
const QUALITY_OPTIONS = [16, 20, 24];

const listEl = $("source-list");
const emptyEl = $("empty");
const picker = $("picker");
const pickerBody = $("picker-body");
const pickerSearch = $("picker-search");

function uniqueId(base) {
  if (!state.sources.some((s) => s.id === base)) return base;
  let i = 2;
  while (state.sources.some((s) => s.id === `${base}${i}`)) i += 1;
  return `${base}${i}`;
}

function truncate(text, n) {
  return text.length > n ? text.slice(0, n - 1) + "…" : text;
}

// Whether this capture records sound rather than picture. Mirrors
// `Capture::is_audio` in the core — the one place that decides, because the
// row, the editor and the picker all branch on it.
function isAudio(capture) {
  return Boolean(capture.Mic || capture.SystemAudio || capture.WindowAudio);
}

// Whether this source can carry sound at all. Only a window can: its process
// tree is what "its own sound" means, which is why the speaker shows up on a
// window row and nowhere else. Note this asks what the source *could* do —
// whether it currently does is `source.audio`, and the row has to keep
// offering the switch after it is turned off, or it could never go back on.
function canCarrySound(source) {
  // A window brings its own process tree; the desktop brings everyone's.
  // Windows has no per-monitor sound, so a desktop source's sound is the whole
  // machine's — which is why the option belongs here and not on a monitor.
  return Boolean(source.capture.Window || source.capture.Desktop);
}

// A remembered source carries no display text — only what the core needs — so
// the row labels are derived back from the capture spec.
// The size of the picture a take of this source records — the same pixels the
// crop's numbers are counted in. `null` when it cannot be known yet: a camera
// only reports one once it has delivered a frame, and sound has none at all.
function frameSize(capture) {
  if (capture.Desktop) {
    // A remembered per-monitor desktop source records that monitor, not the
    // whole virtual screen (2026-10-21 多屏): the row must show its size.
    if (capture.Desktop.rect) return { w: capture.Desktop.rect.w, h: capture.Desktop.rect.h };
    return state.screen ? { w: state.screen.w, h: state.screen.h } : null;
  }
  if (capture.Window) {
    const live = state.windows.find((x) => x.hwnd === capture.Window.hwnd);
    return live ? { w: live.w, h: live.h } : null;
  }
  return null;
}

// `1922×1166` on its own, or `1922×1166 → 1920×1094` when the source is
// cropped. Both numbers are worth showing: what goes in is as interesting as
// what comes out, and the arrow says "cropped" without a word.
function geometryOf(capture) {
  const size = frameSize(capture);
  if (!size) return "";
  const crop = cropOf(capture);
  const cropped = crop.top || crop.bottom || crop.left || crop.right;
  if (!cropped) return `${size.w}×${size.h}`;
  // Edges that no longer fit (the window shrank, or the crop came from the CLI
  // and never fitted) are dropped by the writer — `CropBox::rect_in` records
  // the whole frame rather than a two-pixel sliver. So the row must print the
  // whole frame too: `→ -2280×1392` described a file that never exists.
  if (!cropFits(capture)) return `${size.w}×${size.h}`;
  return `${size.w}×${size.h} → ${size.w - crop.left - crop.right}×${size.h - crop.top - crop.bottom}`;
}

/** Whether the picked edges leave a frame the writer will actually keep. */
function cropFits(capture) {
  const size = frameSize(capture);
  if (!size) return true;
  return fitsIn(cropOf(capture), size.w, size.h);
}

/**
 * The same test against a bare draft and a bare size — the card works on a
 * draft, the row works on a capture, and both have to answer alike.
 * `CropBox::rect_in` keeps a crop of two pixels or more and drops anything
 * smaller, so "fits" is 2, not 1.
 */
function fitsIn(crop, width, height) {
  if (!width || !height) return true;
  const out = outputSize(crop, width, height);
  return out.w >= 2 && out.h >= 2;
}

/** Whether anything is trimmed at all — the test the row and the card share. */
function isCropped(capture) {
  const c = cropOf(capture);
  return Boolean(c.top || c.bottom || c.left || c.right);
}

// What the row says (2026-10-21 方案 A): identity on the left, comparable
// numbers in fixed columns on the right. So the label carries three things —
// the full name (the row truncates it, the tooltip does not), the geometry
// string for the size column, and the hover-only note (engine, background
// capability). The old `meta` mixed geometry and prose in one line under the
// name; the prose is now tooltip-only and the numbers have their own column.
// The monitor a desktop rect belongs to (its centre), or null. The row's
// name comes from this: a per-monitor source says which screen it is, not
// just "desktop" (2026-10-21 多屏).
function monitorFor(rect) {
  if (!rect) return null;
  const cx = rect.x + rect.w / 2;
  const cy = rect.y + rect.h / 2;
  return (
    (state.monitors || []).find(
      (m) => cx >= m.x && cx < m.x + m.w && cy >= m.y && cy < m.y + m.h,
    ) || null
  );
}

function labelFor(capture) {
  const geo = geometryOf(capture);
  const bg = state.backend === "windows" ? t("picker.bgCapable") : "";
  if (capture.Desktop) {
    // The tile and the name already say "desktop"; what is left to say is how
    // big the picture is (or, when that is unknown, which engine grabs it).
    const mon = monitorFor(capture.Desktop.rect);
    return {
      name: mon ? t("label.monitor", { n: mon.index }) : t("label.desktop"),
      geo,
      note: state.backend === "windows" ? "WGC" : "ddagrab / gdigrab",
    };
  }
  if (capture.Window) {
    return { name: capture.Window.title, geo, note: bg };
  }
  if (capture.Mic) {
    return { name: capture.Mic.device, geo, note: "WASAPI" };
  }
  if (capture.SystemAudio) {
    return { name: t("label.systemAudio"), geo, note: "WASAPI" };
  }
  if (capture.WindowAudio) {
    return { name: capture.WindowAudio.title, geo, note: t("label.windowSound") };
  }
  return { name: capture.Camera.device, geo, note: "Media Foundation" };
}

// What makes two sources the same source: the handle, the device, or the
// desktop. Not the label — two windows can share a title. A window's picture
// and its sound are different sources and get different keys, so both can sit
// in the list at once.
function captureKey(capture) {
  if (capture.Desktop) {
    return capture.Desktop.rect ? `d:${JSON.stringify(capture.Desktop.rect)}` : "d:all";
  }
  if (capture.Window) return `w:${capture.Window.hwnd}`;
  if (capture.Mic) return `m:${capture.Mic.device}`;
  if (capture.SystemAudio) return "sys";
  if (capture.WindowAudio) return `wa:${capture.WindowAudio.hwnd}`;
  return `c:${capture.Camera.device}`;
}

// The capture setup, which is what the panel remembers between runs. Field
// names stay snake_case: Tauri does not rename them.
function sessionParams() {
  return {
    sources: state.sources.map(({ id, capture, fps, quality, audio }) => ({
      id,
      capture,
      fps,
      quality,
      audio: Boolean(audio),
    })),
    out_root: state.outRoot,
    encoder: state.encoder,
    backend: state.backend,
    marks: state.marks,
    raise_sources: state.raiseSources,
  };
}

// Debounced: a session edit is a handful of clicks, but the settings file
// should not be written once per keystroke either.
let persistTimer = null;
function persist() {
  clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    invoke("save_session", { session: sessionParams() }).catch((e) =>
      console.error("save_session", e),
    );
  }, 300);
}

// No display text is stored: the row derives its label from the capture spec
// on every render (see `labelFor`), so a language switch relabels as it goes.
//
// Mid-take the same picker feeds the take that is already running instead of
// the saved list — same six kinds, same window search, one branch. Before GO
// the list is only a plan; after GO a source has a file of its own with a
// place on the timeline.
function addSource(id, capture) {
  const source = { id, capture, fps: DEFAULT_FPS, quality: DEFAULT_QUALITY, audio: false };
  if (state.recording) {
    closePicker();
    addLive(source);
    return;
  }
  state.sources.push(source);
  preheatFor(capture);
  persist();
  render();
  closePicker();
}

// Which source is being opened right now. Shown on the add button, not on a
// row: the row does not exist yet.
let addingId = null;

// Hand one source to the take that is running.
//
// The reply's `start` is where its file begins on the take's timeline — a
// measurement (the first sample's own capture time), not the moment the button
// was clicked. That is the number the row shows and the number take.json
// records, so a camera that took half a second to wake up is 0.5 s late in both
// places rather than in one of them.
//
// No toast on success: the row appearing with its offset is the feedback, the
// same way a finished take speaks through its history row. A failure gets one,
// because a click that did nothing needs to say why.
async function addLive(source) {
  addingId = source.id;
  render();
  try {
    const added = await invoke("add_live_source", { source });
    // The Rust side grew the saved list too, so this is the panel catching up,
    // not a second write.
    state.sources.push({ ...source, id: added.id });
    state.starts.set(added.id, added.start);
    PROBE(`source-live:${added.id}@${added.start ?? "-"}`);
  } catch (e) {
    notify("warn", uiErrorText(e));
  } finally {
    addingId = null;
    render();
  }
}

// The row chip's own form: mm:ss while the take is that short, h:mm:ss once it
// is not. The bar's clock always shows hours; a chip inside a row has no room
// for a leading 00: that never changes.
function offsetClock(seconds) {
  const total = Math.max(0, Math.floor(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

// Warm the DDA probe for this source's geometry so start_recording does not
// have to pay the multi-second probe. Fire-and-forget: a failed preheat only
// means start pays the probe itself.
function preheatFor(capture) {
  // The Windows backend has no DDA verdict to warm and no window rectangle to
  // raise — its prepare checks addresses only, in microseconds.
  if (state.backend === "windows") return;
  let rect = null;
  if (capture.Desktop) {
    rect = state.screen;
  } else if (capture.Window) {
    const w = state.windows.find((x) => x.hwnd === capture.Window.hwnd);
    if (w) rect = { x: w.x, y: w.y, w: w.w, h: w.h };
  }
  if (rect) invoke("preheat_engine", { rect }).catch(() => {});
}

function removeSource(id) {
  state.sources = state.sources.filter((s) => s.id !== id);
  persist();
  render();
}

// The same wording wherever a source disappears, so the reason is never
// guessed at.
const prunedNotice = (n) => t("notice.pruned", { n });

// A remembered source can be gone by now: its window closed, moved to another
// virtual desktop, or its app suspended. All three keep the handle alive while
// the screen shows something else, so a take of it would capture the wrong
// thing — drop it instead of offering it again. Returns how many went, which
// the caller decides how to report.
async function pruneSources() {
  if (state.sources.length === 0) return 0;
  // The check runs against a snapshot: the user can add or remove a source
  // while the await is in flight, and the answers map to the snapshot's order,
  // not to whatever `state.sources` looks like when they come back.
  const checked = state.sources.slice();
  let live;
  try {
    live = await invoke("check_sources", { captures: checked.map((s) => s.capture) });
  } catch (e) {
    console.error("check_sources", e);
    return 0;
  }
  const dead = new Set(checked.filter((_, i) => !live[i]).map((s) => s.id));
  const gone = state.sources.filter((s) => dead.has(s.id)).length;
  if (gone === 0) return 0;
  state.sources = state.sources.filter((s) => !dead.has(s.id));
  persist();
  PROBE(`sources-pruned:${gone}`);
  return gone;
}

// A window's caption moves under the row all the time: VS Code puts the open
// file in it, Chrome the active tab. The handle is the identity, so nothing
// about *which* window gets recorded can drift — but the label was captured
// when the source was added, and it would go on naming a file the user left an
// hour ago (2026-10-23).
//
// Only the label, and only in memory: config.json is the truth for everything
// a take reads, and a value that changes when the user clicks a tab has no
// business rewriting that file every couple of seconds. The next launch
// re-derives it, which is why dropping it costs nothing.
let titlesInflight = false;
async function refreshTitles() {
  if (titlesInflight || state.recording || state.sources.length === 0) return;
  if (document.visibilityState !== "visible") return;
  const watched = state.sources.filter((s) => s.capture.Window || s.capture.WindowAudio);
  if (watched.length === 0) return;
  titlesInflight = true;
  let titles;
  try {
    titles = await invoke("source_titles", { captures: watched.map((s) => s.capture) });
  } catch (e) {
    console.error("source_titles", e);
    return;
  } finally {
    titlesInflight = false;
  }
  let changed = false;
  watched.forEach((s, i) => {
    // `null` means the handle is gone; the last title it had beats a blank row
    // for the moment before it is pruned.
    const live = titles[i];
    if (!live) return;
    const slot = s.capture.Window || s.capture.WindowAudio;
    if (slot.title === live) return;
    slot.title = live;
    changed = true;
  });
  if (!changed) return;
  // The open editor carries its own copy of the name, or it would be the one
  // stale place left on screen.
  if (editor) {
    const source = state.sources.find((s) => s.id === editor.id);
    if (source) {
      editor.name = labelFor(source.capture).name;
      renderEditor();
    }
  }
  render();
}

// Every 1.2 s: slow enough to be free, fast enough that a caption change looks
// live. A setTimeout chain for the same reason `tickLoop` uses one.
async function titleLoop() {
  try {
    await refreshTitles();
  } finally {
    setTimeout(titleLoop, 1200);
  }
}


// One source's window can close or leave this desktop mid-take, and ffmpeg
// would go on recording whatever is behind it. Stop that one where it stands —
// the other sources have nothing to do with it — and take it off the list so
// the next take starts without it.
async function dropUnusable(sources) {
  const dead = sources.filter((x) => x.alive && !x.usable);
  if (dead.length === 0) return;
  for (const x of dead) {
    await invoke("stop_source", { id: x.id }).catch((e) => console.error("stop_source", e));
  }
  const gone = new Set(dead.map((x) => x.id));
  state.sources = state.sources.filter((s) => !gone.has(s.id));
  persist();
  render();
  PROBE(`sources-dropped:${dead.length}`);
  notify("warn", t("notice.dropped", { n: dead.length }));
}

// What the row says about a source without opening it: the two numbers that
// decide what the file looks like, and a word when its frame is trimmed.
// "30 Q20" (the old two bare dropdowns) named neither number; these do.
// The row's right-hand numbers: what the file is encoded at. The picture's
// geometry (source size, and the size it is cropped to) has the line below the
// name to itself — see `geometryOf`.
function summaryOf(source) {
  // `quality: null` means the session-wide default (that is what an
  // agent-added source carries) — say the number that will actually be used.
  const q = source.quality ?? DEFAULT_QUALITY;
  // Sound has no fps to speak of; quality is all there is (it picks the AAC
  // bitrate), so the column says that and nothing more.
  if (isAudio(source.capture)) return `AAC · Q${q}`;
  return `${source.fps}fps · Q${q}`;
}

// Source-row tile icons (outline, 1.4 stroke — one family, six kinds).
// Decorative: the row-name beside the tile already says what it is.
const SOURCE_ICONS = {
  desktop:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="1.5" y="2.5" width="13" height="9" rx="1.4"/><path d="M5 14h6M8 11.5V14" stroke-linecap="round"/></svg>',
  window:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="1.5" y="2.5" width="13" height="11" rx="1.4"/><path d="M1.5 5.5h13"/><circle cx="4" cy="4" r="0.6" fill="currentColor" stroke="none"/></svg>',
  camera:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="1.5" y="4" width="9.5" height="8" rx="1.6"/><path d="M11 7.2 14.2 5.4v5.2L11 8.8z" stroke-linejoin="round"/></svg>',
  mic:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="6" y="1.5" width="4" height="7" rx="2"/><path d="M3.5 7.5a4.5 4.5 0 0 0 9 0M8 12v2.5" stroke-linecap="round"/></svg>',
  sysaudio:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M2 6h2.2L7.5 3.2v9.6L4.2 10H2z" stroke-linejoin="round"/><path d="M10 6.2a2.4 2.4 0 0 1 0 3.6M12 4.5a5 5 0 0 1 0 7" stroke-linecap="round"/></svg>',
  winaudio:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="1.5" y="2.5" width="13" height="11" rx="1.4"/><path d="M1.5 5.5h13"/><path d="M5.5 11h1.2L8.2 9.4v3.2L6.7 11H5.5z" stroke-linejoin="round"/><path d="M9.8 10.2a1.3 1.3 0 0 1 0 1.6" stroke-linecap="round"/></svg>',
};

// Which of the six kinds this capture is — the one place that decides it,
// because the row tile and the editor's header tile must agree.
function kindOf(capture) {
  if (capture.Desktop) return "desktop";
  if (capture.Window) return "window";
  if (capture.Mic) return "mic";
  if (capture.SystemAudio) return "sysaudio";
  if (capture.WindowAudio) return "winaudio";
  return "camera";
}

function sourceTile(capture) {
  const tile = document.createElement("span");
  tile.className = "tile";
  tile.setAttribute("aria-hidden", "true");
  tile.innerHTML = SOURCE_ICONS[kindOf(capture)];
  return tile;
}

// The row's speaker, same family as the tiles above (16px, 1.4 stroke). The
// icon is the whole control: crossed out means this file has no sound, the
// waves mean it does. No text — at 26px there is no room for a label, and the
// tooltip does that job on hover.
const SOUND_ICONS = {
  on:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M2 6h2.2L7.5 3.2v9.6L4.2 10H2z" stroke-linejoin="round"/><path d="M10 6.2a2.4 2.4 0 0 1 0 3.6M12 4.5a5 5 0 0 1 0 7" stroke-linecap="round"/></svg>',
  off:
    '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M2 6h2.2L7.5 3.2v9.6L4.2 10H2z" stroke-linejoin="round"/><path d="M10.2 6.2l3.6 3.6M13.8 6.2l-3.6 3.6" stroke-linecap="round"/></svg>',
};

// One click flips the sound and nothing else: no editor round-trip. `audio`
// is the field the core reads, so the write goes there directly; the editor's
// "静音" switch is the same field seen from the other side (see `openEditor`).
function soundToggle(source) {
  const btn = document.createElement("button");
  btn.type = "button";
  const on = Boolean(source.audio);
  btn.className = on ? "row-sound on" : "row-sound";
  btn.innerHTML = SOUND_ICONS[on ? "on" : "off"];
  // The desktop's sound is not "its own" the way a window's is: it is whatever
  // the machine plays. Said in the tooltip, because that is the one place the
  // difference can be explained without a second control.
  const key = on ? "row.soundOn" : "row.soundOff";
  btn.title = source.capture.Desktop
    ? `${t(key)} · ${t("row.soundDesktop")}`
    : t(key);
  btn.setAttribute("aria-label", btn.title);
  btn.setAttribute("aria-pressed", String(on));
  btn.disabled = state.recording;
  btn.addEventListener("click", (e) => {
    // The row itself opens the editor; flipping the speaker is not that.
    e.stopPropagation();
    source.audio = !source.audio;
    persist();
    render();
  });
  return btn;
}

// The speaker's place on a row that has nothing to mute. It is an empty box of
// exactly the same width, not a missing cell: the row is laid out from the
// right, so anything of variable width left of the size column would move the
// numbers every time a list mixed windows with microphones. A window, a
// desktop, a microphone and a camera then all end at the same three columns.
function soundSlot() {
  const slot = document.createElement("span");
  slot.className = "row-sound-slot";
  slot.setAttribute("aria-hidden", "true");
  return slot;
}

// The transport, repainted from state like everything else.
//
// The panel can come back in the middle of a take — the popover's "添加源" does
// exactly that — and a button reading "开始录制" over a running take is a lie
// that also *stops* it when pressed. Skipped while a take is finalising: that
// label belongs to the stop in progress ("正在写入…", DESIGN §4.3).
function paintTransport() {
  if (state.finalizing) return;
  const btn = $("rec-btn");
  btn.textContent = state.recording ? t("bar.stop") : t("bar.rec");
  // Not "live" while paused: the dot means bytes are moving, and they are not.
  btn.classList.toggle("live", state.recording && !state.paused && !state.countdown);

  // Pause only exists mid-take, and only where it can be exact: the ffmpeg
  // backend runs a child per source with a clock of its own, and stopping that
  // is not a pause but a corrupted timeline. Disabled with the reason in the
  // tooltip rather than hidden, so the answer is there when the question comes.
  const pause = $("pause-btn");
  pause.hidden = !state.recording;
  pause.disabled = state.backend !== "windows" || state.countdown;
  pause.textContent = state.paused ? t("bar.resume") : t("bar.pause");
  pause.title = state.backend !== "windows" ? t("bar.pauseNeedsWin") : "";

  // The clock already stops by itself (elapsed comes from the take's own
  // clock), but a frozen number with no explanation reads as a hang.
  const badge = $("paused-badge");
  badge.classList.toggle("hidden", !state.paused);
  if (state.paused) badge.textContent = t("bar.pausedFor", { at: offsetClock(state.pauseLen) });
  document.body.classList.toggle("paused", state.paused);
}

function render() {
  paintTransport();
  // While a take is running the transport bar stays on every tab: a timer and
  // a stop button you cannot see are how a recording gets forgotten.
  document.body.classList.toggle("recording", state.recording);
  // A take closes the editor: a source's numbers must not change under a
  // recording that already read them, and the row it belongs to is disabled.
  if (state.recording && editor) closeEditor();
  listEl.replaceChildren();

  for (const s of state.sources) {
    const row = document.createElement("div");
    row.className = "row";
    // What the row is, for anything that needs to tell kinds apart without
    // reading the icon's markup (the UI matrix does; nothing styles on it).
    row.dataset.kind = kindOf(s.capture);
    // The whole row opens the source's editor. Before this, the row carried
    // two bare dropdowns whose values ("30", "Q20") said nothing about what
    // they were, and there was no room left for a third thing to set.
    row.tabIndex = 0;
    row.setAttribute("role", "button");
    row.classList.toggle("busy", state.recording);

    const tile = sourceTile(s.capture);

    // Derived on every render rather than stored: switching language has to
    // relabel the rows that are already in the list.
    const label = labelFor(s.capture);
    const main = document.createElement("div");
    main.className = "row-main";
    // Where this file sits on the running take. Shown for every row, not just
    // the ones added mid-take: "0:00" is the fact that makes a later "0:32"
    // mean something, and the column has to read the same for both.
    if (state.recording) {
      const start = state.starts.get(s.id);
      const chip = document.createElement("span");
      chip.className = start == null ? "row-start pending" : "row-start";
      chip.textContent = start == null ? "—" : offsetClock(start);
      chip.title =
        start == null
          ? `${t("row.startTitle")} · ${t("row.startPending")}`
          : t("row.startTitle");
      chip.dataset.start = start == null ? "" : String(start);
      main.append(chip);
    }
    const name = document.createElement("div");
    name.className = "row-name";
    name.textContent = truncate(label.name, 48);
    // The ellipsis must have a path to the whole string: the tooltip carries
    // the untruncated name (Essential Text Truncation).
    name.title = label.name;
    main.append(name);

    // The size column: right-aligned tabular figures so every row's size
    // lines up and the column compares at a glance. "—" when the size is not
    // knowable yet (a camera reports one only once it has delivered a frame).
    const geo = document.createElement("div");
    geo.className = label.geo ? "row-geo" : "row-geo dim";
    geo.textContent = label.geo || "—";
    // The prose that used to sit under the name now lives here: same column,
    // same tooltip, never in the scanning lane. A crop that no longer fits
    // says so here, because the number beside it is the whole frame and
    // nothing about it looks trimmed.
    const dropped = isCropped(s.capture) && !cropFits(s.capture);
    if (label.note || !label.geo || dropped) {
      geo.title = [label.geo || t("row.sizeUnknown"), label.note, dropped ? t("row.cropDropped") : ""]
        .filter(Boolean)
        .join(" · ");
    }

    const summary = document.createElement("div");
    summary.className = "row-summary";
    summary.textContent = summaryOf(s);

    const remove = document.createElement("button");
    remove.className = "row-remove";
    remove.title = t("row.remove");
    remove.textContent = "×";
    remove.disabled = state.recording;
    remove.addEventListener("click", (e) => {
      e.stopPropagation();
      removeSource(s.id);
    });

    const open = () => {
      if (state.recording) return;
      openEditor(s.id);
    };
    row.addEventListener("click", (e) => {
      // A click on the × is a removal, not an invitation to edit.
      if (e.target.closest(".row-remove")) return;
      open();
    });
    row.addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      // The row's own buttons answer their own keys. Without this, Enter on
      // the × or the speaker would also open the editor underneath it.
      if (e.target.closest(".row-remove, .row-sound")) return;
      e.preventDefault();
      open();
    });
    row.setAttribute("aria-label", t("row.editLabel", { name: label.name }));

    // What the row says about a source, left to right: name, then the numbers,
    // then whether the file has sound — the last thing before the ×. The
    // speaker used to sit between the name and the size, which cut the scan
    // lane in two ("… 1720×1392 🔊 30fps"); as the final cell it closes the
    // line of facts instead of interrupting it. A switch that disappears when
    // off would have no way back on, so rows that cannot carry sound keep an
    // empty slot of the same width (see `soundSlot`).
    const cells = [tile, main, geo, summary];
    cells.push(canCarrySound(s) ? soundToggle(s) : soundSlot());
    cells.push(remove);
    row.append(...cells);
    listEl.append(row);
  }

  const n = state.sources.length;
  emptyEl.classList.toggle("hidden", n > 0);
  listEl.classList.toggle("hidden", n === 0);
  renderTabs();
  $("hint").textContent = state.recording
    ? t("bar.hintRecording")
    : t("bar.hintSources", { n });
  // A take outlives its last source: one can be dropped mid-recording, and the
  // button has to stay live so the take can still be stopped.
  $("rec-btn").disabled = n === 0 && !state.recording;
  // One live add at a time: opening a camera takes the better part of a
  // second, and a second pick would become a second source with a duplicate id.
  $("add-btn").disabled = addingId != null;
  // Failed overrides the hint and swaps in the diamond + log button (§4.5);
  // every repaint has to agree, or a render() would wipe the reason away.
  paintStatus();
}

// ── The journal panes ───────────────────────────────────────────────────────

const pad2 = (n) => String(n).padStart(2, "0");

const sameDay = (a, b) =>
  a.getFullYear() === b.getFullYear() &&
  a.getMonth() === b.getMonth() &&
  a.getDate() === b.getDate();

function dayLabel(ms) {
  const day = new Date(ms);
  const now = new Date();
  if (sameDay(day, now)) return t("day.today");
  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1);
  if (sameDay(day, yesterday)) return t("day.yesterday");
  const md = `${pad2(day.getMonth() + 1)}-${pad2(day.getDate())}`;
  return day.getFullYear() === now.getFullYear() ? md : `${day.getFullYear()}-${md}`;
}

function timeLabel(ms) {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

// Every take lands in <output root>\<take folder>, so repeating the profile
// path on each row is noise and the folder name is the only part worth
// reading. CSS can only cut the right side, which throws that part away.
function shortPath(dir) {
  const segments = dir.split(/[\\/]+/).filter(Boolean);
  const last = segments[segments.length - 1] || dir;
  const drive = /^[A-Za-z]:$/.exec(segments[0] || "");
  return drive ? `${drive[0]}\\…\\${last}` : last;
}

// A failed open answers a click the user just made, so the notice is the whole
// message — putting it in the log too would let a deleted folder fill the pane
// one click at a time.
//
// A double-click fires two clicks, and two opens land two Explorer windows on
// the same folder. One open per row is enough: the same dir within the window
// is the same intention. Different dirs are never throttled against each
// other, so browsing two takes quickly still opens both.
let lastOpen = { dir: null, at: 0 };
function openTake(dir) {
  const now = Date.now();
  if (dir === lastOpen.dir && now - lastOpen.at < 1500) return;
  lastOpen = { dir, at: now };
  invoke("open_take", { dir }).catch((e) => showNotice("error", uiErrorText(e)));
}

function renderTabs() {
  // No source count on the tab (2026-10-20): the pane and the status bar
  // already say it, and the badge just made the tab bar noisy.
  $("tab-history").classList.toggle("unread", journal.unread_takes > 0);
  const logTab = $("tab-log");
  logTab.classList.toggle("unread", journal.unread_log > 0);
  logTab.classList.toggle("warn", journal.unread_log_level === "warn");
  logTab.classList.toggle("error", journal.unread_log_level === "error");
}

// The trash on a row (history takes, log entries). Same two-step rule as
// Clear (`bindClear`): the first click arms, the second deletes — a delete
// that happens on one click is a delete that happens by accident. `label` is
// the aria sentence naming the row, `text`/`armedText` the two halves of the
// title, and `onConfirm` does the deed (a take also takes its folder with it,
// which is why the arm state is not optional).
function rowTrash({ label, text, armedText, onConfirm }) {
  const btn = document.createElement("button");
  btn.className = "row-del";
  btn.type = "button";
  const stamp = () => {
    // While armed the name says what the second click will do; `title` is the
    // only text an icon button has, so it carries both halves of the story.
    const words = btn.classList.contains("armed") ? armedText : text;
    btn.title = words;
    btn.setAttribute("aria-label", `${label} — ${words}`);
  };
  stamp();
  btn.innerHTML = TRASH_ICON;
  let armed = null;
  btn.addEventListener("click", (e) => {
    // A click on the trash is a delete, never an invitation to open the row.
    e.stopPropagation();
    if (!armed) {
      btn.classList.add("armed");
      stamp();
      armed = setTimeout(() => {
        armed = null;
        btn.classList.remove("armed");
        stamp();
      }, 3000);
      return;
    }
    clearTimeout(armed);
    armed = null;
    onConfirm()
      .then((view) => {
        journal = view;
        renderJournal();
        renderTabs();
      })
      .catch((e) => {
        btn.classList.remove("armed");
        stamp();
        showNotice("error", uiErrorText(e));
      });
  });
  return btn;
}

function takeDeleteButton(take) {
  return rowTrash({
    label: t("take.deleteLabel", { when: timeLabel(take.at) }),
    text: t("take.delete"),
    armedText: t("take.deleteArmed"),
    onConfirm: () => invoke("take_delete", { dir: take.dir }),
  });
}

function logDeleteButton(entry) {
  return rowTrash({
    label: t("log.deleteLabel", { when: timeLabel(entry.at) }),
    text: t("log.delete"),
    armedText: t("log.deleteArmed"),
    // By the stamp the entry carries, not its position: the list moves under
    // a row every time a new line lands, and a row that deletes "slot 3" one
    // second later deletes someone else.
    onConfirm: () => invoke("log_delete", { at: entry.at }),
  });
}

// Outline trash, same 16px / 1.4 family as SOURCE_ICONS (Phosphor `trash`).
const TRASH_ICON =
  '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M2.5 4.5h11M6 4.5V3a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1.5" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.2 4.5l.55 8.1a1.2 1.2 0 0 0 1.2 1.1h4.1a1.2 1.2 0 0 0 1.2-1.1l.55-8.1" stroke-linejoin="round"/><path d="M6.6 7v4.2M9.4 7v4.2" stroke-linecap="round"/></svg>';

function takeRow(take, fresh) {
  // A div with a button role, not a <button>: the row hosts the trash button
  // inside it, and a button inside a button is not HTML.
  const row = document.createElement("div");
  row.tabIndex = 0;
  row.setAttribute("role", "button");
  row.className = fresh ? "take fresh" : "take";
  row.title = take.dir;

  const dot = document.createElement("span");
  dot.className = take.failed > 0 ? "dot warn" : "dot ok";

  const when = document.createElement("span");
  when.className = "when";
  when.textContent = timeLabel(take.at);

  const stats = document.createElement("span");
  stats.className = "stats";
  const secs = take.seconds == null ? t("take.unknown") : `${take.seconds.toFixed(1)}s`;
  const mb = (take.bytes / (1024 * 1024)).toFixed(1);
  stats.textContent = `${secs} · ${mb} MB`;

  const path = document.createElement("span");
  path.className = "path";
  path.textContent = shortPath(take.dir);

  const open = document.createElement("span");
  open.className = "open";
  open.textContent = "⟶";

  row.append(dot, when, stats);
  if (fresh) {
    const badge = document.createElement("span");
    badge.className = "badge";
    badge.textContent = t("take.new");
    row.append(badge);
  }
  row.append(path, open, takeDeleteButton(take));
  row.addEventListener("click", () => openTake(take.dir));
  row.addEventListener("keydown", (e) => {
    // Enter/Space on the row's own focus opens the take; on the trash button
    // inside it, they are the button's own activation and must not bubble
    // into "open the take" alongside the delete.
    if (e.target !== row) return;
    if (e.key !== "Enter" && e.key !== " ") return;
    e.preventDefault();
    openTake(take.dir);
  });
  return row;
}

function logRow(entry) {
  const row = document.createElement("div");
  row.className = "entry";

  const dot = document.createElement("span");
  dot.className = entry.level === "error" ? "dot err" : "dot warn";

  const when = document.createElement("span");
  when.className = "when";
  when.textContent = timeLabel(entry.at);

  const body = document.createElement("div");
  body.className = "body";
  const what = document.createElement("div");
  what.className = "what";
  what.textContent = entry.text;
  body.append(what);

  if (entry.detail || entry.dir) {
    const why = document.createElement("div");
    why.className = "why";
    if (entry.detail) why.append(document.createTextNode(entry.detail));
    if (entry.dir) {
      const link = document.createElement("a");
      link.href = "#";
      link.textContent = t("log.openDir");
      link.addEventListener("click", (e) => {
        e.preventDefault();
        openTake(entry.dir);
      });
      why.append(link);
    }
    body.append(why);
  }

  row.append(dot, when, body, logDeleteButton(entry));
  return row;
}

function renderHistory() {
  const list = $("history-list");
  list.replaceChildren();
  const empty = journal.takes.length === 0;
  $("history-empty").classList.toggle("hidden", !empty);
  list.classList.toggle("hidden", empty);

  let day = null;
  for (const take of journal.takes) {
    const label = dayLabel(take.at);
    if (label !== day) {
      day = label;
      const head = document.createElement("div");
      head.className = "day";
      head.textContent = label;
      list.append(head);
    }
    list.append(takeRow(take, take.at === freshAt));
  }
}

function renderLog() {
  const list = $("log-list");
  list.replaceChildren();
  const empty = journal.log.length === 0;
  $("log-empty").classList.toggle("hidden", !empty);
  list.classList.toggle("hidden", empty);

  let day = null;
  for (const entry of journal.log) {
    const label = dayLabel(entry.at);
    if (label !== day) {
      day = label;
      const head = document.createElement("div");
      head.className = "day";
      head.textContent = label;
      list.append(head);
    }
    list.append(logRow(entry));
  }
}

function renderJournal() {
  renderHistory();
  renderLog();
}

// Re-read both lists. The pane that is open counts as read: a record that
// lands while you are looking at it must not leave a dot behind.
//
// `freshAt` is sampled before that happens, so opening the pane still shows
// which row is the new one — marking it read must not erase the marker that
// says why it is worth reading.
async function refreshJournal() {
  try {
    let view = await invoke("journal_get");
    freshAt = view.unread_takes > 0 && view.takes.length > 0 ? view.takes[0].at : null;
    if (activeTab === "history" && view.unread_takes > 0) {
      view = await invoke("journal_seen", { pane: "history" });
    } else if (activeTab === "log" && view.unread_log > 0) {
      view = await invoke("journal_seen", { pane: "log" });
    }
    journal = view;
    renderJournal();
    renderTabs();
  } catch (e) {
    console.error("journal_get", e);
  }
}

function selectTab(name) {
  activeTab = name;
  // The params bar and the transport only belong to the Record workspace;
  // style.css hides them on the other tabs off this attribute.
  document.body.dataset.tab = name;
  for (const tab of TAB_ORDER) {
    $("tab-" + tab).setAttribute("aria-selected", String(tab === name));
    $("pane-" + tab).classList.toggle("hidden", tab !== name);
  }
  if (name === "history" || name === "log") refreshJournal();
  // Remember the tab across restarts. Fire and forget: a failed write must
  // not interrupt someone clicking around the tabs.
  invoke("set_active_tab", { tab: name }).catch((e) =>
    console.error("set_active_tab", e),
  );
}

async function refreshSources() {
  const [windows, devices, mics] = await Promise.all([
    invoke("list_windows", { filter: "" }).catch((e) => {
      console.error("list_windows", e);
      return [];
    }),
    invoke("list_devices").catch((e) => {
      console.error("list_devices", e);
      return [];
    }),
    invoke("list_mics").catch((e) => {
      console.error("list_mics", e);
      return [];
    }),
  ]);
  state.windows = windows;
  state.devices = devices;
  state.mics = mics;
  state.sourcesLoaded = true;
  PROBE(`ipc-sources:${windows.length}w/${devices.length}d/${mics.length}m`);
  buildPicker();
  // A row's size comes from these lists (a window's live w×h), and the first
  // paint runs before they land — redraw so the size column fills in instead
  // of staying at "—". Safe here: both callers run outside a recording.
  render();
}

function pickButton(name, meta, onPick) {
  const btn = document.createElement("button");
  btn.className = "pick";
  const n = document.createElement("span");
  n.className = "name";
  n.textContent = name;
  const m = document.createElement("span");
  m.className = "meta";
  m.textContent = meta;
  btn.append(n, m);
  btn.addEventListener("click", onPick);
  pickerBody.append(btn);
  return btn;
}

function buildPicker() {
  pickerBody.replaceChildren();
  const groups = [];
  const items = [];

  const group = (label) => {
    const el = document.createElement("div");
    el.className = "group-label";
    el.textContent = label;
    pickerBody.append(el);
    const g = { el, items: [] };
    groups.push(g);
    return g;
  };
  const item = (g, name, meta, match, onPick, capture) => {
    const btn = pickButton(name, meta, onPick);
    // Already in the list: the same capture twice only produces two copies of
    // the same file, and a second row would have to be tuned separately.
    if (capture && state.sources.some((s) => captureKey(s.capture) === captureKey(capture))) {
      btn.disabled = true;
      btn.title = t("picker.addedTitle");
      btn.querySelector(".meta").textContent = t("picker.added", { meta });
    }
    items.push({ el: btn, match });
    g.items.push(btn);
    return btn;
  };

  const gDesktop = group(t("picker.desktop"));
  // The Windows backend captures one output at a time (WGC has no
  // virtual-desktop item), so it offers each monitor instead of one "whole
  // desktop" that would only ever be the first screen (2026-10-21 多屏). The
  // ffmpeg backend can span every output, so it keeps that row as well.
  const monitors = state.monitors || [];
  // The "whole desktop" row goes only when there are monitors to show in its
  // place: an empty list (enumeration failed) must not leave the group blank.
  if (state.backend !== "windows" || monitors.length === 0) {
    item(
      gDesktop,
      t("picker.desktopAll"),
      t("picker.allDisplays"),
      // Matches in either language: the search box is not a language test.
      (q) =>
        "desktop".includes(q) ||
        "桌面".includes(q) ||
        t("picker.desktop").toLowerCase().includes(q),
      () => addSource(uniqueId("screen"), { Desktop: { rect: null, crop: CROP() } }),
      { Desktop: { rect: null } },
    );
  }
  for (const m of monitors) {
    const rect = { x: m.x, y: m.y, w: m.w, h: m.h };
    item(
      gDesktop,
      t("label.monitor", { n: m.index }),
      `${m.w}×${m.h}`,
      (q) =>
        "desktop".includes(q) ||
        "display".includes(q) ||
        "monitor".includes(q) ||
        "桌面".includes(q) ||
        "显示器".includes(q),
      () => addSource(uniqueId("screen"), { Desktop: { rect, crop: CROP() } }),
      { Desktop: { rect } },
    );
  }

  const gWindows = group(t("picker.windows"));
  for (const w of state.windows) {
    const name = truncate(w.title, 48);
    const meta =
      state.backend === "windows"
        ? `${w.w}×${w.h} · ${t("picker.bgCapable")}`
        : `${w.w}×${w.h}`;
    item(
      gWindows,
      name,
      meta,
      (q) => w.title.toLowerCase().includes(q),
      () =>
        addSource(uniqueId("win"), {
          Window: { hwnd: w.hwnd, pid: w.pid, title: w.title, crop: CROP() },
        }),
      { Window: { hwnd: w.hwnd } },
    );
  }

  const gCameras = group(t("picker.cameras"));
  for (const d of state.devices) {
    const name = truncate(d.name, 48);
    // `live === false` = a registered filter with no output pin: a virtual
    // camera whose host app (e.g. the Mikey client) is not running. Say so
    // up front instead of failing at record time. Otherwise the meta names
    // the backends that can open it — dshow = ffmpeg, mf = windows.
    const dead = d.live === false;
    const backends =
      d.dshow && d.mf ? "DirectShow + MF" : d.mf ? "Media Foundation" : "DirectShow";
    const btn = item(
      gCameras,
      name,
      dead ? t("picker.noSignal") : backends,
      (q) => d.name.toLowerCase().includes(q),
      () => addSource(uniqueId("cam"), { Camera: { device: d.name, crop: CROP() } }),
      { Camera: { device: d.name } },
    );
    if (dead) btn.classList.add("no-signal");
  }

  // Sound sources. A mic is picked by name; the system is one row; a window's
  // sound is picked from the same window list as its picture — separate rows,
  // because a take can want one without the other.
  const gMics = group(t("picker.mics"));
  for (const m of state.mics) {
    const name = truncate(m.name, 48);
    item(
      gMics,
      name,
      t("picker.micMeta"),
      (q) =>
        m.name.toLowerCase().includes(q) ||
        "mic".includes(q) ||
        "microphone".includes(q) ||
        "麦克风".includes(q),
      () => addSource(uniqueId("mic"), { Mic: { device: m.name } }),
      { Mic: { device: m.name } },
    );
  }

  const gSound = group(t("picker.sound"));
  item(
    gSound,
    t("picker.systemAudio"),
    t("picker.systemAudioMeta"),
    (q) =>
      "system audio".includes(q) ||
      "audio".includes(q) ||
      "sound".includes(q) ||
      "系统声音".includes(q) ||
      "声音".includes(q) ||
      t("picker.systemAudio").toLowerCase().includes(q),
    () => addSource(uniqueId("sys-audio"), { SystemAudio: {} }),
    { SystemAudio: {} },
  );
  // Both are listed here on purpose, because "sound only, no picture" is a
  // different thing to ask for. What is *not* listed twice is a window's or the
  // desktop's own sound: that is an option on the picture source itself (see
  // `editor-sound`), so each thing is named once instead of asking the user to
  // pick the same window twice and hoping the two picks match.

  const none = document.createElement("div");
  none.className = "pick empty-row hidden";
  none.textContent = t("picker.none");
  pickerBody.append(none);

  pickerDom = { groups, items, none };
  applyPickerFilter();
}

// Filtering flips visibility instead of rebuilding: no re-created listeners,
// and an empty group header can never linger with its label showing.
function applyPickerFilter() {
  if (!pickerDom) return;
  const q = pickerSearch.value.trim().toLowerCase();
  let visible = 0;
  for (const it of pickerDom.items) {
    const show = it.match(q);
    it.el.classList.toggle("hidden", !show);
    if (show) visible += 1;
  }
  for (const g of pickerDom.groups) {
    g.el.classList.toggle("hidden", !g.items.some((b) => !b.classList.contains("hidden")));
  }
  pickerDom.none.classList.toggle("hidden", visible > 0 || !state.sourcesLoaded);
}

function openPicker() {
  // Mid-take this is the door to `Recording::add_source`: what is picked joins
  // the take that is running. Before GO it only plans the next one. There used
  // to be `if (state.recording) return` here — a leftover of the days when the
  // list was frozen at GO — and it made the whole mid-take path unreachable
  // from the panel while every layer under it was ready for it.
  //
  // The tab comes first: the picker is anchored to the add button's header, and
  // the tray's 添加源 can arrive with the panel on any tab. Opening a popover
  // over Settings would put it above a pane with no sources to add to.
  selectTab("sources");
  picker.classList.remove("hidden");
  pickerSearch.value = "";
  applyPickerFilter();
  pickerSearch.focus();
  // Render from the cached lists first so the picker opens instantly; the
  // refresh below rebuilds it when fresh data lands.
  refreshSources().catch((e) => console.error("refreshSources", e));
}

function closePicker() {
  picker.classList.add("hidden");
}

// ── The notice ──────────────────────────────────────────────────────────────
// One report at a time, pinned over the panel. A new one replaces the old one
// instead of stacking, and everything it says also lands in the log pane — so
// dismissing it costs nothing.

let noticeTimer = null;
const NOTICE_MS = 5000;

function hideNotice() {
  clearTimeout(noticeTimer);
  noticeTimer = null;
  $("notice").classList.add("hidden");
}

function showNotice(level, text) {
  $("notice-text").textContent = text;
  $("notice-dot").className = `dot ${level === "error" ? "err" : "warn"}`;
  $("notice").classList.remove("hidden");
  $("notice").classList.toggle("error", level === "error");
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(hideNotice, NOTICE_MS);
}

// A message worth interrupting for: shown now, kept in the log for later.
// `text` is the one-sentence conclusion the notice shows; `detail` is the
// technical half the log keeps.
function notify(level, text, detail = null, dir = null) {
  showNotice(level, text);
  invoke("log_push", { level, text, detail, dir })
    // Re-read rather than take log_push's snapshot: the pane that is open
    // right now must not get a dot for an entry it is already showing.
    .then(() => refreshJournal())
    .catch((e) => console.error("log_push", e));
}

function resetToIdle() {
  // The next take has its own timeline; these offsets belong to the last one.
  state.starts.clear();
  state.recording = false;
  state.paused = false;
  state.pauseLen = 0;
  state.countdown = false;
  state.finalizing = false;
  state.error = null;
  clearTimeout(doneTimer);
  tickLogged = false;
  // Idle means the clock shows zero, not the length of the take that just
  // ended: `tick` stops as soon as this runs, so nothing else would clear it.
  state.elapsed = 0;
  $("clock").textContent = clock(0);
  $("rec-btn").textContent = t("bar.rec");
  $("rec-btn").classList.remove("live");
  setParamsDisabled(false);
  render();
}

// A finished take speaks for itself in the history pane: the row appears with
// the folder, the size and the length, and the tab gets its dot. Only a take
// that lost a source needs to interrupt — that is not what the row would say.
function showSaved(result) {
  const mb = (result.sources.reduce((a, s) => a + s.bytes, 0) / (1024 * 1024)).toFixed(1);
  const secs = result.duration ? `${result.duration.toFixed(1)}s` : t("take.unknown");
  // Two different kinds of "not quite": a source that recorded nothing, and a
  // file whose length in the container disagrees with what take.json claims.
  // The second is not a failure — the footage is fine — but it is exactly the
  // thing the manifest exists to get right, so it is said out loud and kept in
  // the log beside the folder it belongs to.
  const lost = result.sources.filter((s) => s.error);
  const off = result.sources.filter((s) => s.warning);
  const lines = [];
  if (result.failed > 0) lines.push(t("notice.takePartial", { n: result.failed, secs, mb }));
  if (off.length > 0) lines.push(t("notice.lenMismatch", { n: off.length }));
  if (lines.length > 0) {
    const detail = [
      ...lost.map((s) => `${s.id}: ${codeText(s.error)}`),
      ...off.map((s) => `${s.id}: ${codeText(s.warning)}`),
    ].join("\n");
    notify("warn", lines.join(" · "), detail || null, result.dir);
  }
  refreshJournal();
}

// The bar's "done" beat (DESIGN §4.5): the take's length and size read for
// three seconds, then the bar falls back to idle on its own.
let doneTimer = null;
function showDone(result) {
  resetToIdle();
  const secs = result.duration ? clock(result.duration) : t("take.unknown");
  const mb = (result.sources.reduce((a, s) => a + s.bytes, 0) / (1024 * 1024)).toFixed(1);
  $("clock").textContent = result.duration ? clock(result.duration) : t("take.unknown");
  $("hint").textContent = t("bar.done", { secs, mb });
  clearTimeout(doneTimer);
  doneTimer = setTimeout(() => {
    $("clock").textContent = clock(0);
    render();
  }, 3000);
}

// The bar's failed state (DESIGN §4.5): diamond where the clock was, the
// reason on one line with the whole text in the title, and the log one click
// away. It stays until the next take — a reason is worth reading at the
// user's pace, and the ellipsis must never be the only copy of the text.
function showFailed(reason) {
  resetToIdle();
  state.error = reason;
  paintStatus();
}

// One place that says what the bar's right-hand text and slots read as. The
// idle/recording text is painted by render()/tick(); this owns the failed
// overrides and the two slots that come with them.
function paintStatus() {
  const fail = Boolean(state.error) && !state.recording && !state.finalizing;
  $("bar-fail-icon").hidden = !fail;
  $("logs-btn").hidden = !fail;
  $("clock").hidden = fail;
  const hint = $("hint");
  hint.classList.toggle("fail", fail);
  if (fail) {
    hint.textContent = state.error;
    hint.title = state.error;
  } else {
    hint.title = "";
  }
}

async function toggleRecording() {
  const btn = $("rec-btn");

  // The countdown owns the screen now (the panel is hidden, the veil is up).
  // Pressing the key again — or the button, if it were reachable — aborts the
  // take before anything is written.
  if (state.countdown) {
    invoke("overlay_cancel").catch((e) => console.error("overlay_cancel", e));
    return;
  }

  if (!state.recording) {
    btn.disabled = true;
    // Last take's report is stale the moment a new one starts; the log keeps
    // it either way.
    hideNotice();
    state.error = null;
    const dropped = await pruneSources();
    if (dropped) {
      render();
      notify("warn", prunedNotice(dropped), null);
    }
    if (state.sources.length === 0) {
      notify("warn", t("notice.noSources"), null);
      btn.disabled = false;
      return;
    }
    try {
      // `started` comes from the command, not the event: with the countdown
      // off the take is already running when this resolves (and `take:started`
      // can arrive first), so blindly setting the countdown flag afterwards
      // would freeze the clock for the whole take.
      // Reusing sessionParams is deliberate: an inline copy once forgot
      // `backend`, and serde's default quietly sent every take to the ffmpeg
      // backend while the panel said "Windows API".
      const started = await invoke("start_recording", {
        req: {
          ...sessionParams(),
          // The session-wide quality only covers a source without its own.
          quality: DEFAULT_QUALITY,
        },
      });
      // The take is prepared but not running: the veil is counting down and
      // this window is on its way out. `take:started` clears the flag when the
      // encoders are really up.
      state.recording = true;
      state.countdown = !started;
      state.elapsed = 0;
      btn.textContent = t("bar.stop");
      btn.classList.add("live");
      setParamsDisabled(true);
      render();
    } catch (e) {
      // The panel is still on screen — a prepare failure is reported here,
      // with the low beep that says "this did not start" (DESIGN §4.9).
      window.beep(220, 300);
      notify("error", uiErrorText(e));
      showFailed(uiErrorText(e));
    } finally {
      btn.disabled = false;
    }
    return;
  }

  btn.disabled = true;
  try {
    await invoke("stop_recording");
    // Completion is owned by `take:done` (it fires for every stop, this one
    // included): the bar shows its "done" beat and falls back to idle on its
    // own. The command's value is only here for the error path.
  } catch (e) {
    // A failed finalise is one of the codes Rust sends, so it reads in the
    // panel's language like every other failure.
    notify("error", uiErrorText(e));
    // Rust takes the recording out of its state before stop() can fail, so the
    // take is over either way — fall back to the failed bar state (take:failed
    // may not have fired: it is skipped when the panel is the command caller
    // and the stop screen is off).
    showFailed(uiErrorText(e));
  } finally {
    btn.disabled = false;
  }
}

async function tick() {
  // During the countdown the take is not running yet and this window is
  // hidden: polling would only show a clock that has not started. While the
  // take is finalising the clock is frozen at the button press (take:finalize
  // set it) and the bar shows the checklist count — a status poll would
  // zero both.
  if (!state.recording || state.countdown || state.finalizing) return;
  if (!tickLogged) {
    // Once per take: the first tick is the observable proof that the countdown
    // flag was cleared (a stuck flag freezes the clock for the whole take).
    tickLogged = true;
    PROBE("tick-first");
  }
  try {
    const s = await invoke("status");
    state.elapsed = s.elapsed;
    state.paused = s.paused;
    state.pauseLen = s.pause_len;
    paintTransport();
    // A source added mid-take reports its start a moment after it appears (the
    // first sample has to arrive first); this is where that late number lands.
    for (const live of s.sources) {
      if (live.start != null) state.starts.set(live.id, live.start);
    }
    $("clock").textContent = clock(s.elapsed);
    await dropUnusable(s.sources);
    $("hint").textContent = s.sources
      .map(
        (x) =>
          `${x.id} ${(x.bytes / (1024 * 1024)).toFixed(1)}MB${
            x.alive ? "" : " " + t("bar.sourceStopped")
          }`,
      )
      .join(" · ");
    // A failure the events did not announce (StatusInfo.error carries the
    // Rust-side last_error either way) still lands on the bar.
    if (s.error && !state.error) showFailed(s.error);
  } catch (e) {
    console.error("status", e);
  }
}

$("add-btn").addEventListener("click", openPicker);

// Pause is a button, not a setting: it does the thing now. The label only flips
// once the backend has actually stopped the clock — a failure says why and
// leaves the bar telling the truth about what the take is doing.
$("pause-btn").addEventListener("click", async () => {
  const going = !state.paused;
  $("pause-btn").disabled = true;
  try {
    await invoke(going ? "pause_recording" : "resume_recording");
    state.paused = going;
    if (!going) state.pauseLen = 0;
  } catch (e) {
    notify("warn", uiErrorText(e));
  }
  paintTransport();
});

// The popover's "添加源": it brought the panel back and asks for the picker.
// Mid-take that is the only way to grow a take from the tray, so the picker has
// to come up without a second click.
window.__TAURI__.event.listen("picker:open", () => openPicker());
$("backend").addEventListener("change", (e) => {
  state.backend = e.target.value;
  persist();
  buildPicker();
  render();
  PROBE(`backend-changed:${state.backend}`);
});

pickerSearch.addEventListener("input", applyPickerFilter);
pickerSearch.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closePicker();
});
$("rec-btn").addEventListener("click", toggleRecording);
// The failed state's own action (DESIGN §4.5): the log is where the notice's
// detail lines and the technical half went. In-app pane, not a folder — the
// log rows already link to a take's folder when one is involved.
$("logs-btn").addEventListener("click", () => selectTab("log"));

const TAB_ORDER = ["sources", "history", "log", "settings"];

for (const tab of TAB_ORDER) {
  const el = $("tab-" + tab);
  el.addEventListener("click", () => selectTab(tab));
  // A tablist is navigated with the arrow keys; without this the roles would
  // promise a keyboard model the panel does not have.
  el.addEventListener("keydown", (e) => {
    const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const next = TAB_ORDER[(TAB_ORDER.indexOf(activeTab) + step + TAB_ORDER.length) % TAB_ORDER.length];
    selectTab(next);
    $("tab-" + next).focus();
  });
}

$("notice-x").addEventListener("click", hideNotice);

// Two-step instead of a confirm() dialog: a native modal is the one thing this
// panel cannot style or test, and a click that silently does nothing is worse
// than a click that asks again.
function bindClear(pane) {
  const btn = $(`${pane}-clear`);
  // A function, not a string: the label follows the language, including
  // while an armed button is counting down. Each pane owns its keys — the
  // log pane once read the history ones, which only looked right while the
  // two said the same thing.
  const idle = () => t(`${pane}.clear`);
  let armed = null;
  btn.addEventListener("click", () => {
    if (!armed) {
      btn.textContent = t(`${pane}.clearArmed`);
      armed = setTimeout(() => {
        armed = null;
        btn.textContent = idle();
      }, 3000);
      return;
    }
    clearTimeout(armed);
    armed = null;
    btn.textContent = idle();
    invoke("journal_clear", { pane })
      .then((view) => {
        journal = view;
        renderJournal();
        renderTabs();
      })
      .catch((e) => console.error("journal_clear", e));
  });
}
bindClear("history");
bindClear("log");

// The global hotkey carries no parameters: it hands the decision back to the
// panel, which owns the sources, the parameters and the enabled rules — so
// pressing the key and clicking the button take the same path.
window.__TAURI__.event.listen("hotkey:toggle", () => {
  PROBE("hotkey:toggle");
  // While the chip is capturing, the key being pressed is the new binding —
  // the old one must not start a take underneath the capture.
  if (state.capturing) return;
  if (!$("rec-btn").disabled) toggleRecording();
});

// The stop, told in three events (B4): the same stream feeds the full-screen
// stop screen and this bar, so a stop reads the same whoever pressed it.
//
// `take:finalize` — the button was pressed; the timer freezes here.
window.__TAURI__.event.listen("take:finalize", (e) => {
  const p = e.payload || {};
  state.finalizing = true;
  state.finalized = 0;
  state.finalizeTotal = Number(p.total) || 0;
  state.elapsed = Number(p.elapsed) || 0;
  $("clock").textContent = clock(state.elapsed);
  $("hint").textContent = t("bar.writing", { n: 0, m: state.finalizeTotal });
  $("rec-btn").disabled = true;
  PROBE(`take:finalize total=${state.finalizeTotal}`);
});

// `take:source` — one file landed; the checklist count moves up.
window.__TAURI__.event.listen("take:source", () => {
  state.finalized += 1;
  if (state.finalizing) {
    $("hint").textContent = t("bar.writing", { n: state.finalized, m: state.finalizeTotal });
  }
});

// `take:done` — the take is over, whoever stopped it (the hotkey and the tray
// leave this window with no command response to read it from). No "saved"
// banner: the fresh history row is the success feedback; only a take that
// lost a source interrupts (showSaved), and the bar reads "done" for a beat.
window.__TAURI__.event.listen("take:done", (e) => {
  state.finalizing = false;
  showSaved(e.payload);
  showDone(e.payload);
});

// The countdown-off path hides this window before the take starts, so the
// "it started" beep is asked for here (DESIGN §4.6). The probe records the
// audio context state at that moment: a beep scheduled on a suspended context
// never sounds, and hiding the window right after is exactly how that happens.
window.__TAURI__.event.listen("take:beep", (e) => {
  PROBE(`take:beep-handled audio=${window.audioState()}`);
  window.beep(e.payload.hz, e.payload.ms);
});

// The veil's countdown reached GO and the encoders are up: the panel comes
// back from hiding and the clock starts for real.
window.__TAURI__.event.listen("take:started", () => {
  state.countdown = false;
});

// Esc during the countdown: nothing was written, so there is nothing to
// report — the panel just falls back to idle.
window.__TAURI__.event.listen("take:cancelled", () => {
  resetToIdle();
});

// The encoders refused to start after the countdown: the veil is already down
// and this window is back, with the reason to show.
window.__TAURI__.event.listen("take:failed", (e) => {
  notify("error", uiErrorText(e.payload));
  // The bar keeps the reason (diamond + one line + log button) after the
  // notice fades — the notice is the interruption, the bar is the receipt.
  showFailed(uiErrorText(e.payload));
});

// Language or palette changed — from this panel or anywhere else. The payload
// is the pair in force, so a window that missed one event is still correct.
window.__TAURI__.event.listen("ui:settings", (e) => {
  applySettings(e.payload);
});

// A take can start and stop outside this panel: `POST /record/start`, the
// `takekit record` CLI and the MCP tools all reach the same Rust side without
// going through a button here. `state.recording` used to be written only by
// `toggleRecording`, so such a take left the panel convinced it was idle — the
// bar said 开始录制 over a running take, no row showed its offset, and a pick
// from the picker went into the *next* session instead of joining this one.
// `status` is the truth; adopting it is the fix.
//
// The countdown and the finalise own these flags: the first has a take that is
// committed but not started, the second one that is already writing out, and
// `status` says "not recording" for both.
function adoptStatus(s) {
  if (!s || state.countdown || state.finalizing) return;
  state.recording = s.recording;
  state.paused = s.paused;
  state.pauseLen = s.pause_len;
  state.elapsed = s.elapsed;
  // Late offsets (a source added a moment ago whose first sample has only just
  // landed) arrive here too, so the chips are right on the first paint.
  for (const live of s.sources) {
    if (live.start != null) state.starts.set(live.id, live.start);
  }
  // The recording controls configure the *next* take and lock while one runs;
  // a state adopted from elsewhere has to lock them the same way.
  setParamsDisabled(s.recording);
}

// The agent's writes land here (docs/agent-control.md): the panel must show
// them the moment they happen, or the user is looking at a screen that quietly
// disagrees with what was just recorded. A full re-sync rather than a
// per-field diff on purpose — these events are rare (an agent acts, then
// stops), and a stale branch is exactly the bug this exists to prevent.
window.__TAURI__.event.listen("ctl:changed", async () => {
  try {
    const cfg = await invoke("config_get");
    // Rebuilt, not merged: a source the agent removed must actually leave.
    state.sources = [];
    applySaved(cfg);
    await pruneSources();
    // Same event covers "/record/start" and "/record/stop", so the take's own
    // state has to come along with the list.
    adoptStatus(await invoke("status").catch(() => null));
    render();
    renderSettingsState();
    await refreshJournal();
  } catch (e) {
    console.error("ctl:changed", e);
  }
});

function setParamsDisabled(off) {
  // Every recording control lives in the settings pane now (the params bar is
  // just the output path), but they configure the *next* take and stay locked
  // while one runs. Recording controls are selected by name: the
  // language/theme rows share .set-row but are UI settings and stay usable.
  for (const el of document.querySelectorAll(
    ".params input, .params button, #backend, #encoder, #marks-value, #countdown-value, #finalize-value, #raise-value, #hotkey-btn, #ffmpeg-path, #ffmpeg-browse",
  )) {
    el.disabled = off;
  }
}

function bindParams() {
  $("out-path").addEventListener("change", (e) => {
    state.outRoot = e.target.value.trim() || "takes";
    e.target.value = state.outRoot;
    persist();
  });
  $("out-browse").addEventListener("click", () => {
    // The native dialog is the one surface the webview's i18n cannot reach,
    // so its title travels into the command from here — where the dictionary
    // lives. Same key as the button's own aria/title.
    invoke("pick_folder", { title: t("params.outPick") })
      .then((dir) => {
        if (dir) {
          state.outRoot = dir;
          $("out-path").value = dir;
          persist();
        }
      })
      .catch((e) => console.error("pick_folder", e));
  });
  $("encoder").addEventListener("change", (e) => {
    state.encoder = e.target.value || null;
    persist();
  });
  // Not debounced through `persist`: the setting is applied to the running
  // process the moment it lands (the next prepare reads it), so a change is
  // its own command — like the language and theme rows.
  $("ffmpeg-path").addEventListener("change", (e) => {
    const path = e.target.value.trim();
    e.target.value = path;
    invoke("set_ffmpeg_path", { path }).catch((e) => console.error("set_ffmpeg_path", e));
  });
  $("ffmpeg-browse").addEventListener("click", () => {
    invoke("pick_file", { title: t("params.ffmpegPick") })
      .then((path) => {
        if (path) {
          $("ffmpeg-path").value = path;
          invoke("set_ffmpeg_path", { path }).catch((e) => console.error("set_ffmpeg_path", e));
        }
      })
      .catch((e) => console.error("pick_file", e));
  });
  $("marks-value").addEventListener("change", (e) => {
    state.marks = e.target.checked;
    persist();
  });
  $("raise-value").addEventListener("change", (e) => {
    state.raiseSources = e.target.checked;
    persist();
  });
  $("countdown-value").addEventListener("change", (e) => {
    if (e.target.checked) {
      state.countdownSecs = state.lastCountdownSecs || 3;
    } else {
      if (state.countdownSecs > 0) state.lastCountdownSecs = state.countdownSecs;
      state.countdownSecs = 0;
    }
    // Not part of the capture session: the countdown is its own setting, so it
    // is written straight through instead of riding along with save_session.
    invoke("set_countdown", { secs: state.countdownSecs }).catch((e) =>
      console.error("set_countdown", e),
    );
  });
  $("finalize-value").addEventListener("change", (e) => {
    state.finalizePanel = e.target.checked;
    // Same split from save_session as the countdown: the stop screen is not
    // part of the capture session either.
    invoke("set_finalize_panel", { on: state.finalizePanel }).catch((e) =>
      console.error("set_finalize_panel", e),
    );
  });
  // The switch owns a server, not a setting field: Rust starts or stops the
  // listener and writes the file (docs/agent-control.md).
  $("control-value").addEventListener("change", (e) => {
    state.control = e.target.checked;
    invoke("set_control", { on: state.control }).catch((e) =>
      console.error("set_control", e),
    );
  });
}
bindParams();

document.addEventListener("mousedown", (e) => {
  if (!picker.classList.contains("hidden")) {
    if (!picker.contains(e.target) && !e.target.closest("#add-btn")) closePicker();
  }
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    closePicker();
  }
});

// Start from what the last run left behind: the same sources, output directory,
// encoder and mouse-effect switch. Field names come from the Rust `Config`.
function applySaved(cfg) {
  const savedSources = cfg.sources || [];
  if (cfg.out_root) state.outRoot = cfg.out_root;
  $("out-path").value = state.outRoot;
  if (cfg.encoder) {
    state.encoder = cfg.encoder;
    $("encoder").value = cfg.encoder;
  }
  // Empty stays empty: the placeholder (PATH) is the lookup story, not text
  // the field owns.
  $("ffmpeg-path").value = cfg.ffmpeg_path || "";
  // Anything that is not "ffmpeg" reads as the Windows backend: an old config
  // has no field at all, and the in-process backend is the default going
  // forward.
  state.backend = cfg.backend === "ffmpeg" ? "ffmpeg" : "windows";
  $("backend").value = state.backend;
  state.marks = cfg.marks;
  $("marks-value").checked = state.marks;
  state.raiseSources = !!cfg.raise_sources;
  $("raise-value").checked = state.raiseSources;
  state.countdownSecs = typeof cfg.countdown_secs === "number" ? cfg.countdown_secs : 3;
  if (state.countdownSecs > 0) state.lastCountdownSecs = state.countdownSecs;
  $("countdown-value").checked = state.countdownSecs > 0;
  // An old config has no field: the stop screen defaults to on (DESIGN §4.6).
  state.finalizePanel = cfg.finalize_panel !== false;
  $("finalize-value").checked = state.finalizePanel;
  // The agent's door (docs/agent-control.md) defaults to open: a config from
  // before it existed comes up with the switch on, matching the Rust side.
  state.control = cfg.control !== false;
  $("control-value").checked = state.control;
  for (const s of savedSources) {
    state.sources.push({
      id: s.id,
      capture: s.capture,
      fps: s.fps,
      quality: s.quality,
      // A window that was set to record its own sound has to come back that
      // way; the field is picked out by name, so it is named here or lost.
      audio: Boolean(s.audio),
    });
    // Warm the engine probe for restored sources too. The picker preheats on
    // add, but a source that came back from config would otherwise leave the
    // first F9 after a restart paying the multi-second DDA probe — the take
    // feels like it hangs before the countdown even appears.
    preheatFor(s.capture);
  }
  // Language, palette and hotkey come from the same file. Rust already seeded
  // `data-theme` before the first paint (so an explicit choice does not flash
  // the other palette); this confirms it and fills in the strings.
  state.hotkey = cfg.hotkey || "F9";
  state.language = cfg.language === "en" ? "en" : "zh";
  state.theme = cfg.theme === "light" || cfg.theme === "dark" ? cfg.theme : "auto";
  applyTheme(state.theme);
  applyLanguageUi();
  renderSettingsState();
  // Land on the tab the user last had open (default: the first one).
  // config_get has already run the name through the one valid-tab list
  // (Rust side); re-checking it here would just be a second copy.
  selectTab(cfg.active_tab);
  // What the window is actually showing, not what the file said: the probe is
  // the only way to see the applied language, the resolved palette and the
  // chip's text without a screenshot.
  PROBE(
    `settings-applied lang=${state.language} theme=${
      document.documentElement.dataset.theme || "auto"
    } hotkey=${prettyHotkey(state.hotkey)}`,
  );
  PROBE(`ipc-config-applied:${savedSources.length}s`);
}

// ── Language and palette ───────────────────────────────────────────────────
// Rust owns both values (config.json): these commands validate them, write
// them and broadcast `ui:settings`, so the tray popover switches in the same
// moment. The panel applies what comes back rather than what it asked for —
// the file is the truth, and a value that failed to save must not look applied.

const settingsPanel = $("pane-settings");

function encoderAutoLabel() {
  return state.encoderName
    ? t("params.encoderAutoWith", { name: state.encoderName })
    : t("params.encoderAuto");
}

// Everything built from state, redrawn in the current language. The static
// text is setUiLang()'s job; this is the half that JS assembled.
function applyLanguageUi() {
  setUiLang(state.language);
  $("rec-btn").textContent = state.recording ? t("bar.stop") : t("bar.rec");
  $("encoder").options[0].textContent = encoderAutoLabel();
  renderHotkey();
  render();
  buildPicker();
  renderJournal();
  // The card and the crop workspace write their labels in JS rather than
  // through `data-i18n`, so a switch made while either is open would leave
  // them in the language they were born in — including the output size line,
  // which reads "输出 …" and then never changes.
  if (editor) {
    renderEditor();
    if (!$("crop").classList.contains("hidden")) renderCrop();
  }
}

function renderSettingsState() {
  $("lang-select").value = state.language;
  $("theme-select").value = state.theme;
}

function applySettings(info) {
  if (!info) return;
  if (info.theme) {
    state.theme = info.theme;
    applyTheme(info.theme);
  }
  if (info.language) state.language = info.language === "en" ? "en" : "zh";
  applyLanguageUi();
  renderSettingsState();
  PROBE(`settings-changed lang=${state.language} theme=${state.theme}`);
}

$("lang-select").addEventListener("change", (e) => {
  invoke("set_language", { language: e.target.value })
    .then(applySettings)
    .catch((err) => notify("error", uiErrorText(err)));
});

$("theme-select").addEventListener("change", (e) => {
  invoke("set_theme", { theme: e.target.value })
    .then(applySettings)
    .catch((err) => notify("error", uiErrorText(err)));
});

// ── The hotkey chip ────────────────────────────────────────────────────────
// Registration lives in Rust: `set_hotkey` re-registers through the plugin,
// writes the config, and puts the previous binding back when the new one is
// rejected. The panel only decides what combination the keys spell.

const chip = $("hotkey-btn");
const chipNote = $("hotkey-note");

function renderHotkey() {
  if (state.capturing) return;
  chip.textContent = prettyHotkey(state.hotkey);
}

function beginHotkeyCapture() {
  state.capturing = true;
  chip.classList.remove("error", "ok");
  chip.classList.add("capturing");
  chip.textContent = t("hotkey.capturing");
  chipNote.textContent = t("hotkey.cancelNote");
  // Capture phase: this must see the key before the panel's own Escape
  // handling or any focused control acts on it.
  window.addEventListener("keydown", captureHotkey, true);
}

function endHotkeyCapture() {
  state.capturing = false;
  chip.classList.remove("capturing");
  // Back to the standing hint — the slot stays filled so the row keeps its
  // height and the line reads as help rather than a gap.
  chipNote.textContent = t("params.hotkeyHint");
  window.removeEventListener("keydown", captureHotkey, true);
  renderHotkey();
}

// "Ctrl+KeyR": the names the registrar round-trips. The Windows key is left
// out on purpose — the OS reserves it, so offering it would only ever produce
// a registration that fails.
function hotkeyFromEvent(e) {
  const mods = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  return mods.concat(e.code).join("+");
}

function captureHotkey(e) {
  e.preventDefault();
  e.stopPropagation();
  if (e.key === "Escape") {
    endHotkeyCapture();
    return;
  }
  // A modifier on its own is half a shortcut: wait for the real key.
  if (/^(Control|Shift|Alt|Meta)/.test(e.code)) return;
  if (!e.code) return;
  const spec = hotkeyFromEvent(e);
  endHotkeyCapture();
  commitHotkey(spec);
}

function commitHotkey(spec) {
  invoke("set_hotkey", { hotkey: spec })
    .then((normalized) => {
      state.hotkey = normalized;
      renderHotkey();
      chip.classList.add("ok");
      setTimeout(() => chip.classList.remove("ok"), 1200);
    })
    .catch((err) => {
      // The old binding is still live: say why, and put its name back.
      chip.classList.add("error");
      setTimeout(() => chip.classList.remove("error"), 1600);
      notify("error", uiErrorText(err));
    });
}

chip.addEventListener("click", () => {
  // Clicking the chip again is the way out of a capture the user changed
  // their mind about (Esc does the same, but the pointer already landed).
  if (state.capturing) endHotkeyCapture();
  else beginHotkeyCapture();
});


// ── The source editor ──────────────────────────────────────────────────────
//
// A row is a summary; the editor is where a source's numbers are set. Two
// layers, because they answer different questions: the card says *which*
// source and what it will produce, and the crop workspace gives a 4K frame
// the room to be trimmed to the pixel.
//
// Nothing is written until 保存. Both layers edit `editor.draft`, and the list
// only learns about it at the end — a dialog you can back out of is the whole
// point of a dialog.

const MIN_OUT = 16; // source pixels; below this there is nothing left to see
const ZOOM_STEPS = [0.05, 0.1, 0.17, 0.25, 0.33, 0.5, 0.67, 1, 1.5, 2, 3, 4, 6, 8];
const EDGES = ["top", "bottom", "left", "right"];
// Which edges a handle drags. The data-edge values are compass points; the
// crop stores the four insets.
const HANDLE_EDGES = {
  nw: ["top", "left"],
  n: ["top"],
  ne: ["top", "right"],
  e: ["right"],
  se: ["bottom", "right"],
  s: ["bottom"],
  sw: ["bottom", "left"],
  w: ["left"],
};

// `editor` is null when nothing is open. Draft, size and picture all live
// here so a second open cannot inherit the first one's numbers.
let editor = null;
const view = { zoom: 1, panX: 0, panY: 0, hand: false };
let drag = null;
let spaceDown = false;
let limitTimer = null;

function cropOf(capture) {
  const c = capture.Desktop?.crop || capture.Window?.crop || capture.Camera?.crop;
  if (!c || isAudio(capture)) return CROP();
  return {
    top: Math.max(0, Math.round(c.top || 0)),
    bottom: Math.max(0, Math.round(c.bottom || 0)),
    left: Math.max(0, Math.round(c.left || 0)),
    right: Math.max(0, Math.round(c.right || 0)),
  };
}

// The capture with different edges. Identity (handle, device, title) is kept:
// only the four insets move. Sound has no edges and comes back unchanged,
// mirroring `Capture::with_crop` in the core.
function withCrop(capture, crop) {
  if (isAudio(capture)) return capture;
  if (capture.Desktop) return { Desktop: { rect: capture.Desktop.rect ?? null, crop } };
  if (capture.Window) return { Window: { ...capture.Window, crop } };
  return { Camera: { ...capture.Camera, crop } };
}

// Whether the crop can be edited at all: the size is what defines it. A
// failed picture grab still leaves a croppable frame.
function editorCroppable() {
  return Boolean(editor && editor.width > 0 && editor.height > 0);
}

// Rust sends prose for OS failures and a code for the ones we describe
// ourselves; a code has to come back in the user's language.
function noteText(note) {
  if (note === "preview.minimized") return t("preview.minimized");
  if (note === "preview.camera") return t("preview.camera");
  if (note === "preview.audio") return t("preview.audio");
  return note || t("editor.noStill");
}

function outputSize(crop, width, height) {
  return {
    w: Math.max(0, width - crop.left - crop.right),
    h: Math.max(0, height - crop.top - crop.bottom),
  };
}

// One edge, in source pixels, clamped so the box never collapses and never
// leaves the frame. `clamped` tells the caller the drag hit a wall.
function setEdge(crop, edge, value, width, height) {
  const opposite =
    edge === "left" ? "right" : edge === "right" ? "left" : edge === "top" ? "bottom" : "top";
  const span = edge === "left" || edge === "right" ? width : height;
  const max = Math.max(0, span - crop[opposite] - MIN_OUT);
  const wanted = Math.round(value);
  const kept = Math.max(0, Math.min(max, wanted));
  return { crop: { ...crop, [edge]: kept }, clamped: kept !== wanted };
}

// A whole drag, as one function: the pointer and the arrow keys both come
// through here, so a nudge and a drag can never disagree about the limits.
function applyDelta(base, edges, dx, dy, width, height) {
  if (!edges) {
    // The box itself: its width is fixed, so the room to move is exactly the
    // two insets on the axis — sliding right spends the right one.
    const nx = Math.max(-base.left, Math.min(base.right, dx));
    const ny = Math.max(-base.top, Math.min(base.bottom, dy));
    return {
      crop: {
        top: base.top + ny,
        bottom: base.bottom - ny,
        left: base.left + nx,
        right: base.right - nx,
      },
      clamped: nx !== dx || ny !== dy,
    };
  }
  let crop = { ...base };
  let clamped = false;
  for (const edge of edges) {
    const delta = edge === "top" ? dy : edge === "bottom" ? -dy : edge === "left" ? dx : -dx;
    const r = setEdge(crop, edge, crop[edge] + delta, width, height);
    crop = r.crop;
    clamped = clamped || r.clamped;
  }
  return { crop, clamped };
}

function flashLimit() {
  const box = $("crop-box");
  box.classList.add("limit");
  clearTimeout(limitTimer);
  limitTimer = setTimeout(() => box.classList.remove("limit"), 200);
}

// ── The card ───────────────────────────────────────────────────────────────

async function openEditor(id) {
  const source = state.sources.find((s) => s.id === id);
  if (!source || state.recording) return;
  editor = {
    id,
    name: labelFor(source.capture).name,
    // The draft is the only copy: nothing reaches the source until 保存.
    draft: cropOf(source.capture),
    width: 0,
    height: 0,
    image: null,
    note: null,
    loading: true,
  };
  $("editor-fps").value = String(source.fps);
  $("editor-quality").value = String(source.quality ?? DEFAULT_QUALITY);
  // Sound has no frame rate and no edges to trim: the fps half of the pair
  // and the whole crop section would only offer controls that do nothing.
  const audio = isAudio(source.capture);
  $("editor-fps").closest(".edit-field").classList.toggle("hidden", audio);
  $("editor-thumb").closest(".edit-section").classList.toggle("hidden", audio);
  // Only a picture that can carry sound shows the switch — a window's own
  // process tree, or the desktop's system sound — so it is hidden elsewhere
  // rather than offered and then ignored.
  const windowSound = canCarrySound(source);
  $("editor-sound-row").classList.toggle("hidden", !windowSound);
  // The switch reads "静音", so it is on when the sound is off: the row's
  // speaker and this checkbox are two views of one field, and they must
  // never disagree about which way round it is.
  $("editor-sound").checked = windowSound && !source.audio;
  $("editor").classList.remove("hidden");
  renderEditor();
  await loadPreview(source);
}

// Grab the still — or the reason there is none — for the open editor.
//
// Split out from `openEditor` because restoring a minimized window is a second
// way in: the same grab has to run again afterwards, against the same guard
// that stops a stale answer from landing in a different source's editor.
async function loadPreview(source) {
  const id = editor.id;
  editor.loading = true;
  renderEditor();
  const still = await invoke("source_preview", { capture: source.capture }).catch((e) => ({
    image: null,
    width: 0,
    height: 0,
    note: uiErrorText(e),
  }));
  // Closed, or opened on another source, while the grab was in flight.
  if (!editor || editor.id !== id) return;
  editor.loading = false;
  editor.width = still.width || 0;
  editor.height = still.height || 0;
  editor.image = still.image || null;
  editor.note = still.note || null;
  renderEditor();
  PROBE(`editor-still ${id} still=${editor.width}x${editor.height} ok=${Boolean(editor.image)}`);
}

// The window behind this source is minimized: bring it back so the crop can be
// drawn against a picture.
//
// A button, not something the editor does on open. Resolving a window restores
// it, so an editor that resolved eagerly would pull windows onto the screen
// every time one was clicked — and restoring one *does* hand it the keyboard
// focus (Windows semantics). The user asking is what makes that acceptable.
async function restoreSourceWindow() {
  if (!editor || editor.loading) return;
  const source = state.sources.find((s) => s.id === editor.id);
  if (!source) return;
  $("editor-restore").disabled = true;
  await invoke("restore_window", { capture: source.capture }).catch((e) => {
    console.error("restore_window", e);
    return false;
  });
  if (!editor || editor.id !== source.id) return;
  $("editor-restore").disabled = false;
  await loadPreview(source);
}

function renderEditor() {
  if (!editor) return;
  // Re-stamp the dialog's static text in the language in force *now*. The
  // global pass runs on a language change, but a dialog that survived one (or
  // a build that skipped a key) would otherwise show whatever it was born
  // with — and a half-translated dialog reads as a bug, not as a setting.
  applyI18n($("editor"));
  applyI18n($("crop"));
  const source = state.sources.find((s) => s.id === editor.id);
  $("editor-title").textContent = editor.name;
  $("editor-tile").innerHTML = source ? SOURCE_ICONS[kindOf(source.capture)] : "";
  const out = outputSize(editor.draft, editor.width || 0, editor.height || 0);
  // A draft whose edges leave nothing is one the writer drops: the card then
  // says "whole frame", which is what the file will hold. Printing `0 × 1392`
  // would describe a picture nothing can write.
  const draftFits = fitsIn(editor.draft, editor.width, editor.height);
  const size = editor.width
    ? draftFits
      ? t("editor.size", { w: out.w, h: out.h })
      : t("editor.sizeWhole")
    : t("editor.sizeUnknown");
  $("editor-sub").textContent = editor.width
    ? `${editor.width} × ${editor.height}`
    : t("editor.sizeUnknown");
  $("editor-size").textContent = editor.width ? (draftFits ? `${out.w} × ${out.h}` : t("crop.dropped")) : "—";
  const img = $("editor-img");
  img.classList.toggle("hidden", !editor.image);
  if (editor.image) img.src = editor.image;
  $("editor-thumb-empty").classList.toggle("hidden", Boolean(editor.image));
  $("editor-thumb-empty").textContent = editor.loading
    ? t("editor.loading")
    : noteText(editor.note);
  // Croppable only with a picture in hand (2026-10-23). A crop is four numbers
  // against the frame the take will record, and the numbers themselves are
  // always computable — but a user who cannot see what they are trimming is
  // guessing, and the one state that has no still (a minimized window) is the
  // one where the guess is least checkable. Measured, the earlier rule
  // ("size known ⇒ croppable") was admitting exactly that case.
  const cropable = Boolean(editor.image);
  $("editor-thumb").disabled = !cropable;
  // The one reason for a missing picture the user can fix from here. Shown as a
  // button in the footer, where the other reasons already live as text.
  const canRestore = !editor.image && editor.note === "preview.minimized";
  $("editor-restore").classList.toggle("hidden", !canRestore);
  $("editor-restore").disabled = !canRestore || editor.loading;
  // The reason lives on the picture area (it wraps there); the footer keeps
  // one short line. A sentence here is what squeezed 取消/保存 into two
  // one-character lines on a long source name (2026-10-21).
  $("editor-note").textContent = cropable ? size : "";
  // The tooltip carries the sentence, not the reason code: `editor.note` holds
  // "preview.minimized" and friends, and a code is not a tooltip. When the
  // sentence is already printed on the picture area, an empty tooltip is the
  // honest answer.
  // Cleared rather than set to the reason: `editor.note` holds a code
  // ("preview.minimized") and the sentence for it is already printed on the
  // picture area. A tooltip that repeats it, or worse shows the code, is noise.
  $("editor-thumb").title = "";
}

function closeEditor() {
  editor = null;
  $("editor").classList.add("hidden");
  $("crop").classList.add("hidden");
  delete document.body.dataset.view;
  // Even from crop mode — a take starting closes the editor under the
  // workspace, and the panel would otherwise stay screen-sized for good.
  invoke("exit_crop_mode").catch((e) => console.error("exit_crop_mode", e));
}

function saveEditor() {
  if (!editor) return;
  const source = state.sources.find((s) => s.id === editor.id);
  if (!source) {
    closeEditor();
    return;
  }
  source.fps = Number($("editor-fps").value) || source.fps;
  source.quality = Number($("editor-quality").value) || source.quality;
  // The switch only exists for a window; reading it on anything else would
  // write a flag the core refuses out loud. Checked means muted, so the field
  // is its negation.
  if (canCarrySound(source)) source.audio = !$("editor-sound").checked;
  // Without a picture there is nothing to have cropped: leave the edges as
  // they were rather than writing a draft against a size of zero.
  if (editor.width && editor.height) {
    let crop = { ...editor.draft };
    for (const edge of EDGES) {
      crop = setEdge(crop, edge, crop[edge], editor.width, editor.height).crop;
    }
    source.capture = withCrop(source.capture, crop);
  }
  const id = editor.id;
  persist();
  render();
  closeEditor();
  PROBE(
    `editor-saved ${id} fps=${source.fps} q=${source.quality} sound=${Boolean(source.audio)}`,
  );
}

// ── The crop workspace ─────────────────────────────────────────────────────

function stageSize() {
  const stage = $("crop-stage");
  return { w: stage.clientWidth, h: stage.clientHeight };
}

function fitCropView() {
  const { w, h } = stageSize();
  if (!editor || !editor.width || !editor.height || !w || !h) return;
  const z = Math.min((w - 24) / editor.width, (h - 24) / editor.height);
  view.zoom = Math.max(ZOOM_STEPS[0], Math.min(8, z));
  view.panX = Math.round((w - editor.width * view.zoom) / 2);
  view.panY = Math.round((h - editor.height * view.zoom) / 2);
}

function renderCrop() {
  if (!editor) return;
  const stage = $("crop-stage");
  const plane = $("crop-plane");
  const box = $("crop-box");
  const has = Boolean(editor.image);
  // The reason rides on the empty stage as a hint. `pointer-events: none` in
  // the stylesheet keeps it from swallowing the box's drags underneath.
  $("crop-none").classList.toggle("hidden", has);
  if (!has) $("crop-none").textContent = noteText(editor.note);
  stage.classList.toggle("hand", view.hand);
  $("tool-pan").setAttribute("aria-pressed", String(view.hand));
  $("zoom-value").textContent = `${Math.round(view.zoom * 100)}%`;
  $("crop-sub").textContent = editor.width
    ? t("crop.source", { w: editor.width, h: editor.height })
    : t("crop.noSource");
  // The plane is the frame the take will record — pixels or not, the crop box
  // sits in the same rectangle and the numbers move the same way.
  plane.classList.toggle("hidden", !editor.width);
  if (!editor.width) {
    $("crop-output").textContent = "—";
    return;
  }
  // The stage has its own <img>; the card's is only for the thumbnail.
  const img = $("crop-img");
  img.classList.toggle("hidden", !has);
  if (has && img.getAttribute("src") !== editor.image) img.src = editor.image;
  // Fitting in wants smoothing; zooming into the picture wants its pixels as
  // blocks. One class, one answer.
  plane.classList.toggle("pixelated", view.zoom >= 1);
  plane.style.width = `${editor.width * view.zoom}px`;
  plane.style.height = `${editor.height * view.zoom}px`;
  plane.style.transform = `translate(${view.panX}px, ${view.panY}px)`;
  const c = editor.draft;
  const out = outputSize(c, editor.width, editor.height);
  // A draft with no picture left in it is one the writer will drop, so the
  // toolbar says what the file will get rather than a size it cannot write.
  // The reason rides on the tooltip: the toolbar has no room for a sentence.
  const fits = fitsIn(c, editor.width, editor.height);
  // The box is the frame the take will record, and the dimming shadow is what
  // is thrown away. Edges that do not fit throw nothing away, so the box
  // covers the whole picture — a zero-width box would dim the entire stage and
  // read as a broken viewer rather than as "nothing is cropped".
  box.classList.toggle("dropped", !fits);
  box.style.left = `${(fits ? c.left : 0) * view.zoom}px`;
  box.style.top = `${(fits ? c.top : 0) * view.zoom}px`;
  box.style.width = `${(fits ? out.w : editor.width) * view.zoom}px`;
  box.style.height = `${(fits ? out.h : editor.height) * view.zoom}px`;
  for (const edge of EDGES) {
    const field = $(`edge-${edge}`);
    // The field being typed into keeps every keystroke; the others follow.
    if (document.activeElement !== field) field.value = String(Math.round(c[edge]));
  }
  // Re-labelled here rather than once at startup: switching language has to
  // reach these too, and there are eight of them.
  for (const handle of box.querySelectorAll(".crop-handle")) {
    const id = handle.dataset.edge;
    handle.setAttribute("aria-label", t(`crop.handle.${id}`));
  }
  const outLabel = $("crop-output");
  outLabel.textContent = fits ? `${out.w} × ${out.h}` : t("crop.dropped");
  outLabel.title = fits ? "" : t("crop.droppedHint");
}

function zoomAt(next, cx, cy) {
  const z = Math.max(ZOOM_STEPS[0], Math.min(8, next));
  if (Math.abs(z - view.zoom) < 1e-6) return;
  // Keep the pixel under the cursor under the cursor: pan is where the
  // picture's top-left corner sits, so it scales with the zoom about `cx`.
  view.panX = cx - (cx - view.panX) * (z / view.zoom);
  view.panY = cy - (cy - view.panY) * (z / view.zoom);
  view.zoom = z;
  renderCrop();
}

function stepZoom(dir) {
  const { w, h } = stageSize();
  let next;
  if (dir > 0) {
    next = ZOOM_STEPS.find((z) => z > view.zoom + 1e-6) || ZOOM_STEPS[ZOOM_STEPS.length - 1];
  } else {
    const below = ZOOM_STEPS.filter((z) => z < view.zoom - 1e-6);
    next = below.length ? below[below.length - 1] : ZOOM_STEPS[0];
  }
  zoomAt(next, w / 2, h / 2);
}

// Wait out a window resize: the layout is only true once the browser has
// reflowed, and measuring the stage a frame early fits the picture to the
// size the panel used to be.
async function nextLayout() {
  await new Promise((res) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      window.removeEventListener("resize", finish);
      res();
    };
    window.addEventListener("resize", finish);
    setTimeout(finish, 250);
  });
  // Two frames so the reflow is done — but never *only* frames: a hidden or
  // occluded window does not paint, and `requestAnimationFrame` would not
  // fire at all (headless runs hit the same thing).
  await Promise.race([
    new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))),
    new Promise((r) => setTimeout(r, 120)),
  ]);
}

async function openWorkspace() {
  if (!editorCroppable()) return;
  $("editor").classList.add("hidden");
  $("crop").classList.remove("hidden");
  document.body.dataset.view = "crop";
  // As much of the screen as the monitor can give: trimming a 4K frame to the
  // pixel is not a job for a 900x640 control strip. The panel grows to 90% of
  // the work area and goes back to the strip when the workspace closes.
  await invoke("enter_crop_mode").catch((e) => console.error("enter_crop_mode", e));
  await nextLayout();
  fitCropView();
  renderCrop();
  // The stage, not the box: arrows on the stage move the whole crop, and the
  // box's focus ring is for when the keyboard actually lands on it.
  $("crop-stage").focus({ preventScroll: true });
  PROBE("crop-open");
}

async function closeWorkspace() {
  // Nothing to go back to. A save, a cancel or a take starting can close the
  // card while the workspace is still animating out, and `editor !== mine`
  // below cannot tell that apart from its own editor (both are null) — the
  // card would come back empty, with the default title and no source.
  if (!editor) {
    $("crop").classList.add("hidden");
    delete document.body.dataset.view;
    await invoke("exit_crop_mode").catch((e) => console.error("exit_crop_mode", e));
    return;
  }
  const mine = editor;
  $("crop").classList.add("hidden");
  delete document.body.dataset.view;
  await invoke("exit_crop_mode").catch((e) => console.error("exit_crop_mode", e));
  await nextLayout();
  // The window is still shrinking while this runs, and the dialog is not
  // guaranteed to survive it: a save, a cancel or a take starting all close
  // it in the meantime, and bringing it back would be a dialog the user just
  // dismissed (this is how 保存 looked like it did nothing).
  if (editor !== mine) return;
  $("editor").classList.remove("hidden");
  renderEditor();
  $("editor-thumb").focus({ preventScroll: true });
}

function cropPointerDown(e) {
  if (!editorCroppable()) return;
  const handle = e.target.closest(".crop-handle");
  const box = e.target.closest(".crop-box");
  const stage = $("crop-stage");
  // Panning: the hand tool, the middle button, or space held down — the three
  // habits image editors share.
  if (e.button === 1 || view.hand || (spaceDown && e.button === 0)) {
    e.preventDefault();
    drag = { mode: "pan", x: e.clientX, y: e.clientY, panX: view.panX, panY: view.panY };
    stage.classList.add("panning");
    window.addEventListener("pointermove", cropPointerMove);
    window.addEventListener("pointerup", cropPointerUp, { once: true });
    return;
  }
  if (e.button !== 0 || (!handle && !box)) return;
  e.preventDefault();
  if (handle) handle.focus({ preventScroll: true });
  else box.focus({ preventScroll: true });
  drag = {
    mode: handle ? handle.dataset.edge : "move",
    edges: handle ? HANDLE_EDGES[handle.dataset.edge] : null,
    x: e.clientX,
    y: e.clientY,
    base: { ...editor.draft },
  };
  window.addEventListener("pointermove", cropPointerMove);
  window.addEventListener("pointerup", cropPointerUp, { once: true });
}

function cropPointerMove(e) {
  if (!drag || !editor) return;
  if (drag.mode === "pan") {
    view.panX = drag.panX + (e.clientX - drag.x);
    view.panY = drag.panY + (e.clientY - drag.y);
    renderCrop();
    return;
  }
  // Display pixels become source pixels here: the picture is a scaled
  // rendering of the frame, and the numbers are the frame's.
  const dx = (e.clientX - drag.x) / view.zoom;
  const dy = (e.clientY - drag.y) / view.zoom;
  const r = applyDelta(drag.base, drag.edges, dx, dy, editor.width, editor.height);
  editor.draft = r.crop;
  if (r.clamped) flashLimit();
  renderCrop();
}

function cropPointerUp() {
  window.removeEventListener("pointermove", cropPointerMove);
  $("crop-stage").classList.remove("panning");
  if (drag && drag.mode !== "pan" && editor) {
    // Landing on whole pixels: a crop is a count of pixels, and half of one
    // is a number the encoder cannot honour.
    const snapped = {};
    for (const edge of EDGES) snapped[edge] = Math.round(editor.draft[edge]);
    editor.draft = snapped;
    renderCrop();
  }
  drag = null;
}

function cropKeyDown(e) {
  if (!editor) return;
  const dir = {
    ArrowLeft: [-1, 0],
    ArrowRight: [1, 0],
    ArrowUp: [0, -1],
    ArrowDown: [0, 1],
  }[e.key];
  if (dir) {
    e.preventDefault();
    const step = e.shiftKey ? 10 : 1;
    const handle = e.target.closest?.(".crop-handle");
    const edges = handle ? HANDLE_EDGES[handle.dataset.edge] : null;
    const r = applyDelta(
      editor.draft,
      edges,
      dir[0] * step,
      dir[1] * step,
      editor.width,
      editor.height,
    );
    editor.draft = r.crop;
    if (r.clamped) flashLimit();
    renderCrop();
    return;
  }
  if (e.key === "Escape") {
    e.preventDefault();
    // Consumed here, or the window-level Esc (which closes the card) would
    // act on the same keystroke: `closeWorkspace` has already taken the
    // "workspace is up" flag down by the time the event reaches it, so one
    // Esc would take down both layers and drop the draft.
    e.stopPropagation();
    // Layered: the first Esc leaves the handle, the second leaves the
    // workspace. Esc that closed everything at once is how a careful crop
    // gets thrown away.
    const handle = e.target.closest?.(".crop-handle");
    if (handle) {
      $("crop-box").focus({ preventScroll: true });
      return;
    }
    closeWorkspace();
    return;
  }
  if (e.key === "Enter") {
    e.preventDefault();
    e.stopPropagation();
    closeWorkspace();
  }
}

// ── Wiring ─────────────────────────────────────────────────────────────────

function initEditor() {
  for (const [id, options, prefix] of [
    ["editor-fps", FPS_OPTIONS, ""],
    ["editor-quality", QUALITY_OPTIONS, "Q"],
  ]) {
    const sel = $(id);
    for (const v of options) {
      const opt = document.createElement("option");
      opt.value = String(v);
      opt.textContent = `${prefix}${v}`;
      sel.append(opt);
    }
  }
  $("editor-close").addEventListener("click", closeEditor);
  $("editor-cancel").addEventListener("click", closeEditor);
  // Clicking the dimmed area is a cancel. Only the backdrop itself: a click
  // that started inside the card and ended outside is not one.
  $("editor").addEventListener("click", (e) => {
    if (e.target === $("editor")) closeEditor();
  });
  window.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || !editor) return;
    // The workspace owns Esc while it is up (see cropKeyDown). Asked of the
    // element rather than of `body.dataset.view`: a workspace that closed on
    // this very keystroke has already cleared the flag.
    if (!$("crop").classList.contains("hidden")) return;
    closeEditor();
  });
  $("editor-save").addEventListener("click", saveEditor);
  $("editor-restore").addEventListener("click", restoreSourceWindow);
  $("editor-thumb").addEventListener("click", openWorkspace);
  $("crop-back").addEventListener("click", closeWorkspace);

  $("zoom-in").addEventListener("click", () => stepZoom(1));
  $("zoom-out").addEventListener("click", () => stepZoom(-1));
  $("view-fit").addEventListener("click", () => {
    fitCropView();
    renderCrop();
  });
  $("view-one").addEventListener("click", () => {
    const { w, h } = stageSize();
    zoomAt(1, w / 2, h / 2);
  });
  $("view-reset").addEventListener("click", () => {
    view.hand = false;
    fitCropView();
    renderCrop();
  });
  $("tool-pan").addEventListener("click", () => {
    view.hand = !view.hand;
    renderCrop();
  });
  $("crop-clear").addEventListener("click", () => {
    if (!editorCroppable()) return;
    editor.draft = CROP();
    renderCrop();
  });

  // Every handle is in the tab order: dragging is one way to move an edge,
  // and the keyboard is the other (WCAG 2.2 · Dragging Movements).
  for (const handle of document.querySelectorAll(".crop-handle")) {
    handle.tabIndex = 0;
  }

  const stage = $("crop-stage");
  stage.addEventListener("pointerdown", cropPointerDown);
  stage.addEventListener("keydown", cropKeyDown);
  stage.addEventListener(
    "wheel",
    (e) => {
      if (!editorCroppable()) return;
      e.preventDefault();
      const rect = stage.getBoundingClientRect();
      const factor = e.deltaY < 0 ? 1.12 : 1 / 1.12;
      zoomAt(view.zoom * factor, e.clientX - rect.left, e.clientY - rect.top);
    },
    { passive: false },
  );

  for (const edge of EDGES) {
    $(`edge-${edge}`).addEventListener("input", (e) => {
      if (!editorCroppable()) return;
      const v = Number(e.target.value);
      if (!Number.isFinite(v)) return;
      const r = setEdge(editor.draft, edge, v, editor.width, editor.height);
      editor.draft = r.crop;
      renderCrop();
      if (r.clamped) e.target.value = String(editor.draft[edge]);
    });
  }

  // Space is a held modifier here, the way it is in every image editor.
  window.addEventListener("keydown", (e) => {
    if (!editor || !document.body.dataset.view) return;
    if (e.code === "Space" && e.target === document.body) {
      spaceDown = true;
      e.preventDefault();
    }
  });
  window.addEventListener("keyup", (e) => {
    if (e.code === "Space") spaceDown = false;
  });
}

async function init() {
  PROBE("js-main-eval");
  initEditor();
  // The source rows come from the config alone. The device lists, the
  // environment probe and the journal all start at once and repaint what they
  // own when they land — the first paint used to wait behind the ffmpeg
  // encoder probe and the MF device walk, which left the Sources tab blank
  // for their whole duration.
  const sources = refreshSources().catch((e) => {
    console.error("refreshSources", e);
  });
  const env = invoke("env_info")
    .then((e) => {
      PROBE("ipc-env-ok:" + JSON.stringify(e));
      return e;
    })
    .catch((e) => {
      PROBE("ipc-env-fail:" + String(e).slice(0, 120));
      notify("error", String(e));
      return null;
    });
  // Nothing saved is not a failure: the panel just comes up empty.
  const saved = await invoke("config_get").catch((e) => {
    console.error("config_get", e);
    return null;
  });
  if (saved) applySaved(saved);
  // Before the first render: a source that went away while the panel was
  // closed should never show up at all.
  const dropped = await pruneSources();
  render();
  if (dropped) notify("warn", prunedNotice(dropped), null);
  const e = await env;
  if (e) {
    state.screen = e.screen;
    state.monitors = e.monitors || [];
    // The temp directory is only a first-run default: a directory the user
    // picked themselves outranks it.
    if (!saved || !saved.out_root) {
      if (e.default_out_root) {
        state.outRoot = e.default_out_root;
        $("out-path").value = state.outRoot;
      }
    }
    // Only the ffmpeg backend needs the binary: on the Windows backend a
    // missing ffmpeg is not an error, and crying wolf at every launch is how
    // an error notice stops being read.
    if (!e.ffmpeg) {
      if (state.backend === "ffmpeg") {
        notify("error", t("notice.noFfmpeg"), t("notice.noFfmpegDetail"));
      }
    } else if (e.encoder) {
      state.encoderName = e.encoder;
      $("encoder").options[0].textContent = encoderAutoLabel();
    }
    // The picker has one row per monitor. When the device walk beat the probe
    // it built without them; rebuild now that they are known. (The other
    // order — probe first — leaves `sourcesLoaded` false, and the pending
    // buildPicker gets the monitors for free.)
    if (state.sourcesLoaded) buildPicker();
    // Same reason as in `refreshSources`: a desktop row's size comes from
    // `state.screen` and its name from `state.monitors`, both of which only
    // exist now. Redraw the rows so they stop showing "—".
    render();
  }
  await sources;
  // Both list panes are restored before the first paint of them: opening a
  // tab must not show an empty list for a frame.
  await refreshJournal();
  PROBE("init-done");
}

// A setTimeout chain instead of setInterval: an interval keeps queueing ticks
// while the main thread is busy, and the backlog all fires at once afterwards.
async function tickLoop() {
  try {
    await tick();
  } finally {
    setTimeout(tickLoop, 250);
  }
}

tickLoop();
titleLoop();
init();
