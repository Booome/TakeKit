// TakeKit control panel.
//
// No bundler and no npm: Tauri injects window.__TAURI__.core, so invoke() is
// available directly. Source specs mirror the serde shape of
// takekit_core::source::{Source, Capture, Crop} — an externally tagged enum,
// so `{"Window": {...}}`, `{"Camera": {...}}`, `{"Desktop": {"rect": null}}`.
//
// `invoke`, `PROBE`, `$` and `clock` come from shared.js, which loads first.

const state = {
  sources: [],
  windows: [],
  devices: [],
  outRoot: "takes",
  encoder: null,
  marks: true,
  // Seconds of the full-screen countdown before a take starts; 0 = off. The
  // switch only flips between "the seconds remembered here" and off, so
  // toggling it twice does not lose a value B5's number input once allows.
  countdownSecs: 3,
  lastCountdownSecs: 3,
  recording: false,
  // True between "begin accepted" and the veil saying GO: the panel is hidden
  // and the take is not running yet. F9 again in that window cancels it.
  countdown: false,
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

// A remembered source carries no display text — only what the core needs — so
// the row labels are derived back from the capture spec.
function labelFor(capture) {
  if (capture.Desktop) return { name: t("label.desktop"), meta: "ddagrab / gdigrab" };
  if (capture.Window) {
    const live = state.windows.find((x) => x.hwnd === capture.Window.hwnd);
    return {
      name: truncate(capture.Window.title, 48),
      meta: live ? t("label.windowMeta", { w: live.w, h: live.h }) : t("label.window"),
    };
  }
  return { name: truncate(capture.Camera.device, 48), meta: t("label.camera") };
}

// What makes two sources the same source: the handle, the device, or the
// desktop. Not the label — two windows can share a title.
function captureKey(capture) {
  if (capture.Desktop) {
    return capture.Desktop.rect ? `d:${JSON.stringify(capture.Desktop.rect)}` : "d:all";
  }
  if (capture.Window) return `w:${capture.Window.hwnd}`;
  return `c:${capture.Camera.device}`;
}

// The capture setup, which is what the panel remembers between runs. Field
// names stay snake_case: Tauri does not rename them.
function sessionParams() {
  return {
    sources: state.sources.map(({ id, capture, fps, quality }) => ({ id, capture, fps, quality })),
    out_root: state.outRoot,
    encoder: state.encoder,
    marks: state.marks,
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

function addSource(id, capture, name, meta) {
  state.sources.push({ id, capture, fps: DEFAULT_FPS, quality: DEFAULT_QUALITY, name, meta });
  preheatFor(capture);
  persist();
  render();
  closePicker();
}

// Warm the DDA probe for this source's geometry so start_recording does not
// have to pay the multi-second probe. Fire-and-forget: a failed preheat only
// means start pays the probe itself.
function preheatFor(capture) {
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

function sourceSelect(label, options, prefix, current, apply, disabled) {
  const sel = document.createElement("select");
  sel.className = "row-param";
  sel.setAttribute("aria-label", label);
  sel.disabled = disabled;
  for (const v of options) {
    const opt = document.createElement("option");
    opt.value = String(v);
    opt.textContent = `${prefix}${v}`;
    opt.selected = v === current;
    sel.append(opt);
  }
  sel.addEventListener("change", (e) => apply(Number(e.target.value)));
  return sel;
}

function render() {
  listEl.replaceChildren();

  for (const s of state.sources) {
    const row = document.createElement("div");
    row.className = "row";

    const dot = document.createElement("span");
    dot.className = "dot";

    // Derived on every render rather than stored: switching language has to
    // relabel the rows that are already in the list.
    const label = labelFor(s.capture);
    const main = document.createElement("div");
    main.className = "row-main";
    const name = document.createElement("div");
    name.className = "row-name";
    name.textContent = label.name;
    const meta = document.createElement("div");
    meta.className = "row-meta";
    meta.textContent = label.meta;
    main.append(name, meta);

    const fpsSel = sourceSelect(
      t("row.fps", { name: label.name }),
      FPS_OPTIONS,
      "",
      s.fps,
      (v) => {
        s.fps = v;
        persist();
      },
      state.recording,
    );
    const qualitySel = sourceSelect(
      t("row.quality", { name: label.name }),
      QUALITY_OPTIONS,
      "Q",
      s.quality,
      (v) => {
        s.quality = v;
        persist();
      },
      state.recording,
    );

    const remove = document.createElement("button");
    remove.className = "row-remove";
    remove.title = t("row.remove");
    remove.textContent = "×";
    remove.disabled = state.recording;
    remove.addEventListener("click", (e) => {
      e.stopPropagation();
      removeSource(s.id);
    });

    row.append(dot, main, fpsSel, qualitySel, remove);
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
function openTake(dir) {
  invoke("open_take", { dir }).catch((e) => showNotice("error", String(e)));
}

function renderTabs() {
  $("count").textContent = state.sources.length > 0 ? String(state.sources.length) : "";
  $("tab-history").classList.toggle("unread", journal.unread_takes > 0);
  const logTab = $("tab-log");
  logTab.classList.toggle("unread", journal.unread_log > 0);
  logTab.classList.toggle("warn", journal.unread_log_level === "warn");
  logTab.classList.toggle("error", journal.unread_log_level === "error");
}

function takeRow(take, fresh) {
  const row = document.createElement("button");
  row.type = "button";
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
  row.append(path, open);
  row.addEventListener("click", () => openTake(take.dir));
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

  row.append(dot, when, body);
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
  for (const tab of ["sources", "history", "log"]) {
    $("tab-" + tab).setAttribute("aria-selected", String(tab === name));
    $("pane-" + tab).classList.toggle("hidden", tab !== name);
  }
  if (name !== "sources") refreshJournal();
}

async function refreshSources() {
  const [windows, devices] = await Promise.all([
    invoke("list_windows", { filter: "" }).catch((e) => {
      console.error("list_windows", e);
      return [];
    }),
    invoke("list_devices").catch((e) => {
      console.error("list_devices", e);
      return [];
    }),
  ]);
  state.windows = windows;
  state.devices = devices;
  state.sourcesLoaded = true;
  PROBE(`ipc-sources:${windows.length}w/${devices.length}d`);
  buildPicker();
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
  };

  const gDesktop = group(t("picker.desktop"));
  item(
    gDesktop,
    t("picker.desktopAll"),
    t("picker.allDisplays"),
    // Matches in either language: the search box is not a language test.
    (q) =>
      "desktop".includes(q) || "桌面".includes(q) || t("picker.desktop").toLowerCase().includes(q),
    () =>
      addSource(
        uniqueId("screen"),
        { Desktop: { rect: null } },
        t("picker.desktopAll"),
        "ddagrab / gdigrab",
      ),
    { Desktop: { rect: null } },
  );

  const gWindows = group(t("picker.windows"));
  for (const w of state.windows) {
    const name = truncate(w.title, 48);
    item(
      gWindows,
      name,
      `${w.w}×${w.h}`,
      (q) => w.title.toLowerCase().includes(q),
      () =>
        addSource(
          uniqueId("win"),
          { Window: { hwnd: w.hwnd, title: w.title, crop: CROP() } },
          name,
          t("label.windowMeta", { w: w.w, h: w.h }),
        ),
      { Window: { hwnd: w.hwnd } },
    );
  }

  const gCameras = group(t("picker.cameras"));
  for (const d of state.devices) {
    const name = truncate(d, 48);
    item(
      gCameras,
      name,
      "DirectShow",
      (q) => d.toLowerCase().includes(q),
      () =>
        addSource(
          uniqueId("cam"),
          { Camera: { device: d, crop: CROP() } },
          name,
          t("label.camera"),
        ),
      { Camera: { device: d } },
    );
  }

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
  if (state.recording) return;
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
  state.recording = false;
  state.countdown = false;
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
  if (result.failed > 0) {
    const detail = result.sources
      .filter((s) => s.error)
      .map((s) => `${s.id}: ${s.error}`)
      .join("\n");
    notify(
      "warn",
      t("notice.takePartial", { n: result.failed, secs, mb }),
      detail || null,
      result.dir,
    );
  }
  refreshJournal();
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
      const started = await invoke("start_recording", {
        req: {
          // fps and quality are per source; the session-wide quality only
          // covers a source that somehow has none.
          sources: state.sources.map(({ id, capture, fps, quality }) => ({
            id,
            capture,
            fps,
            quality,
          })),
          out_root: state.outRoot,
          quality: DEFAULT_QUALITY,
          encoder: state.encoder,
          marks: state.marks,
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
    } finally {
      btn.disabled = false;
    }
    return;
  }

  btn.disabled = true;
  try {
    const res = await invoke("stop_recording");
    showSaved(res);
  } catch (e) {
    // A failed finalise is one of the codes Rust sends, so it reads in the
    // panel's language like every other failure.
    notify("error", uiErrorText(e));
  } finally {
    // Rust takes the recording out of its state before stop() can fail, so the
    // take is over either way — reset the UI even when the invoke rejected.
    resetToIdle();
    btn.disabled = false;
  }
}

async function tick() {
  // During the countdown the take is not running yet and this window is
  // hidden: polling would only show a clock that has not started.
  if (!state.recording || state.countdown) return;
  if (!tickLogged) {
    // Once per take: the first tick is the observable proof that the countdown
    // flag was cleared (a stuck flag freezes the clock for the whole take).
    tickLogged = true;
    PROBE("tick-first");
  }
  try {
    const s = await invoke("status");
    state.elapsed = s.elapsed;
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
  } catch (e) {
    console.error("status", e);
  }
}

$("add-btn").addEventListener("click", openPicker);
pickerSearch.addEventListener("input", applyPickerFilter);
pickerSearch.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closePicker();
});
$("rec-btn").addEventListener("click", toggleRecording);

const TAB_ORDER = ["sources", "history", "log"];

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
  // while an armed button is counting down.
  const idle = () => t("history.clear");
  let armed = null;
  btn.addEventListener("click", () => {
    if (!armed) {
      btn.textContent = t("history.clearArmed");
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

// A take can be stopped from the tray popover, which leaves this window with
// neither the response nor a reason to still say "recording".
window.__TAURI__.event.listen("take:stopped", (e) => {
  resetToIdle();
  showSaved(e.payload);
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
  resetToIdle();
});

// Language or palette changed — from this panel or anywhere else. The payload
// is the pair in force, so a window that missed one event is still correct.
window.__TAURI__.event.listen("ui:settings", (e) => {
  applySettings(e.payload);
});

function setParamsDisabled(off) {
  for (const el of document.querySelectorAll(".params input, .params select, .params button")) {
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
    invoke("pick_folder")
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
  $("marks-value").addEventListener("change", (e) => {
    state.marks = e.target.checked;
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
}
bindParams();

document.addEventListener("mousedown", (e) => {
  if (!picker.classList.contains("hidden")) {
    if (!picker.contains(e.target) && !e.target.closest("#add-btn")) closePicker();
  }
  if (!settingsPanel.classList.contains("hidden")) {
    if (!settingsPanel.contains(e.target) && !settingsBtn.contains(e.target)) closeSettings();
  }
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    closePicker();
    closeSettings();
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
  state.marks = cfg.marks;
  $("marks-value").checked = state.marks;
  state.countdownSecs = typeof cfg.countdown_secs === "number" ? cfg.countdown_secs : 3;
  if (state.countdownSecs > 0) state.lastCountdownSecs = state.countdownSecs;
  $("countdown-value").checked = state.countdownSecs > 0;
  for (const s of savedSources) {
    const l = labelFor(s.capture);
    state.sources.push({
      id: s.id,
      capture: s.capture,
      fps: s.fps,
      quality: s.quality,
      name: l.name,
      meta: l.meta,
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

const settingsPanel = $("settings");
const settingsBtn = $("settings-btn");

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
}

function renderSettingsState() {
  for (const b of settingsPanel.querySelectorAll("[data-lang]")) {
    b.setAttribute("aria-pressed", String(b.dataset.lang === state.language));
  }
  for (const b of settingsPanel.querySelectorAll("[data-theme-opt]")) {
    b.setAttribute("aria-pressed", String(b.dataset.themeOpt === state.theme));
  }
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

function openSettings() {
  settingsPanel.classList.remove("hidden");
  settingsBtn.setAttribute("aria-expanded", "true");
  // The picker and this card share a corner; two open popovers would overlap
  // into something that reads as a bug.
  closePicker();
}

function closeSettings() {
  settingsPanel.classList.add("hidden");
  settingsBtn.setAttribute("aria-expanded", "false");
}

function toggleSettings() {
  if (settingsPanel.classList.contains("hidden")) openSettings();
  else closeSettings();
}

settingsBtn.addEventListener("click", toggleSettings);

for (const b of settingsPanel.querySelectorAll("[data-lang]")) {
  b.addEventListener("click", () => {
    invoke("set_language", { language: b.dataset.lang })
      .then(applySettings)
      .catch((e) => notify("error", uiErrorText(e)));
  });
}

for (const b of settingsPanel.querySelectorAll("[data-theme-opt]")) {
  b.addEventListener("click", () => {
    invoke("set_theme", { theme: b.dataset.themeOpt })
      .then(applySettings)
      .catch((e) => notify("error", uiErrorText(e)));
  });
}

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
  chipNote.textContent = "";
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

async function init() {
  PROBE("js-main-eval");
  // Parallel on purpose: the source lists do not depend on the environment
  // probe, and the picker is usable as soon as they land — gating them behind
  // the encoder probe would leave the picker empty for its first second.
  const sources = refreshSources().catch((e) => {
    console.error("refreshSources", e);
  });
  // Nothing saved is not a failure: the panel just comes up empty.
  const saved = await invoke("config_get").catch((e) => {
    console.error("config_get", e);
    return null;
  });
  try {
    const env = await invoke("env_info");
    PROBE("ipc-env-ok:" + JSON.stringify(env));
    state.screen = env.screen;
    // The temp directory is only a first-run default: a directory the user
    // picked themselves outranks it.
    if (!saved || !saved.out_root) {
      if (env.default_out_root) {
        state.outRoot = env.default_out_root;
        $("out-path").value = state.outRoot;
      }
    }
    if (!env.ffmpeg) {
      notify("error", t("notice.noFfmpeg"), t("notice.noFfmpegDetail"));
    } else if (env.encoder) {
      state.encoderName = env.encoder;
      $("encoder").options[0].textContent = encoderAutoLabel();
    }
  } catch (e) {
    PROBE("ipc-env-fail:" + String(e).slice(0, 120));
    notify("error", String(e));
  }
  await sources;
  if (saved) applySaved(saved);
  // Before the first render: a source that went away while the panel was
  // closed should never show up at all.
  const dropped = await pruneSources();
  render();
  if (dropped) notify("warn", prunedNotice(dropped), null);
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
init();
