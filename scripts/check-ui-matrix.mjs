// Language × theme matrix: every screen, in every language, in every theme.
// Two things a screenshot review misses and this catches:
//
//   1. a layout that pushes the page wider than the window (horizontal scroll)
//      or lets text spill over its neighbour instead of clipping politely;
//   2. text whose contrast against the surface it actually sits on falls under
//      WCAG AA — the background is walked up the tree because this UI paints on
//      containers, not on the text node.
//
// Screens: sources, editor, picker, settings × {zh, en} × {light, dark}.
// Screenshots land in target/checks/ui-matrix/ for the eyeball pass.
//
// Usage:  cargo build --workspace && node scripts/check-ui-matrix.mjs
// Close any running panel first (a second instance exits on sight).
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "..");
const GUI = path.join(ROOT, "target", "debug", "takekit-gui.exe");
const TK = path.join(ROOT, "target", "debug", "takekit.exe");
const OUT = path.join(ROOT, "target", "checks", "ui-matrix");
const APPDATA = path.join(OUT, "appdata");
const appDir = path.join(APPDATA, "TakeKit");
const CDP_PORT = 9239;

for (const bin of [GUI, TK]) {
  if (!existsSync(bin)) {
    console.error(`missing ${bin} — run: cargo build --workspace`);
    process.exit(2);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const say = (m) => console.log(`[${new Date().toISOString().slice(11, 19)}] ${m}`);
const results = [];
function record(name, ok, detail) {
  results.push({ name, ok, detail });
  say(`${ok ? "PASS" : "FAIL"}  ${name}  ${detail || ""}`);
}

try { rmSync(OUT, { recursive: true, force: true }); } catch {}
mkdirSync(appDir, { recursive: true });

// A window of our own, minimized — the one state where a window source has a
// size and no picture. Nothing in the panel can create it (no button minimizes
// anything), so the script brings its own: a WinForms form owns the handle, so
// seeding a source with that handle and this process's pid is exactly what the
// picker would have written.
const PROBE_PS1 = path.join(OUT, "probe.ps1");
const PROBE_HANDLE = path.join(OUT, "probe-handle.txt");
writeFileSync(
  PROBE_PS1,
  [
    "param([string]$HandleFile)",
    "Add-Type -AssemblyName System.Windows.Forms",
    "Add-Type -AssemblyName System.Drawing",
    "$f = New-Object System.Windows.Forms.Form",
    "$f.Text = 'TakeKit probe window'",
    "$f.Width = 420",
    "$f.Height = 300",
    "$f.BackColor = [System.Drawing.Color]::FromArgb(0, 214, 120)",
    "$f.Show()",
    "[System.Windows.Forms.Application]::DoEvents()",
    'Set-Content -Path $HandleFile -Value "$($f.Handle)"',
    "# Minimized only after the source exists, so the panel opens on the state",
    "# the user reported rather than on a window that is briefly up.",
    "Start-Sleep -Milliseconds 1500",
    "$f.WindowState = 'Minimized'",
    "$end = (Get-Date).AddSeconds(600)",
    "while ((Get-Date) -lt $end) {",
    "  [System.Windows.Forms.Application]::DoEvents()",
    "  Start-Sleep -Milliseconds 100",
    "}",
  ].join("\n"),
  "utf8"
);
const probe = spawn(
  "powershell",
  ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", PROBE_PS1, "-HandleFile", PROBE_HANDLE],
  { stdio: "ignore" }
);
let probeHwnd = 0;
for (let i = 0; i < 80 && !probeHwnd; i++) {
  await sleep(150);
  if (existsSync(PROBE_HANDLE)) {
    const n = Number(readFileSync(PROBE_HANDLE, "utf8").trim());
    if (n > 0) probeHwnd = n;
  }
}
if (!probeHwnd) {
  console.error("the probe window never reported its handle");
  process.exit(2);
}
say(`probe window ${probeHwnd} in pid ${probe.pid}`);

// Seed a spread of sources: a window with sound, a window muted, the desktop.
// `Desktop` is a struct variant — `null` is not a shape it deserialises, and
// one unreadable source takes the whole saved list down with it.
const listed = execFileSync(TK, ["list"], { encoding: "utf8" });
const wins = listed.split("\n")
  .map((l) => l.match(/^\s*\[(\d+)\]\s*(.*?)\s*\[[\d x, at]+\]\s*$/))
  .filter(Boolean)
  .map((m) => ({ hwnd: Number(m[1]), title: m[2] }))
  // The probe is seeded by handle below; leaving it in here would put a second
  // row on the same window and shift every other index out from under the
  // checks that assume the first row is somebody else's window.
  .filter((w) => w.hwnd && !/TakeKit probe window/.test(w.title));
if (wins.length === 0) {
  console.error("no windows to seed a source with");
  process.exit(2);
}
const NO_CROP = { top: 0, bottom: 0, left: 0, right: 0 };
// What a seeded row must end up calling itself: the caption the window has
// right now, not the one the config remembers.
//
// The probe window and not the user's: a real window's title changes while the
// suite runs (this very session retitles its window), so a fixture read once at
// start-up and compared four themes later is a flake waiting to happen — it
// failed exactly that way on 2026-10-23, in en-dark only. The probe's caption
// is set by this script and stays put; it is also minimized at that point,
// which is the harder case for a title read.
const LIVE_TITLE_PREFIX = "TakeKit probe window";
// `pid: 0` is what every config written before that field says — unknown owner,
// handle-only check. The picker always writes the real one; seeding 0 here
// keeps the old-config path exercised, and the probe below covers the real one.
const win = (i, audio, crop = NO_CROP, title = null) => ({
  id: `win${i}`,
  capture: {
    Window: {
      hwnd: wins[i % wins.length].hwnd,
      pid: 0,
      title: title ?? wins[i % wins.length].title,
      crop,
    },
  },
  fps: 30,
  quality: null,
  audio,
});
// A cropped source is the widest thing the size column ever has to print:
// "3440×1440 → 2160×720" is nearly twice the untrimmed string. It is seeded
// here because the column is a fixed width with an ellipsis, and a truncated
// size hides exactly the number the crop changed.
const CROP = { top: 40, bottom: 60, left: 80, right: 90 };
// Edges bigger than the frame itself. Nothing in the panel can produce this
// (the box is clamped to the picture), but `takekit record --crop`, POST
// /sources and a hand-edited config.json all can — and the writer drops such a
// crop and records the whole frame, so the row has to say the whole frame.
const OVER_CROP = { top: 0, bottom: 0, left: 2000, right: 2000 };
writeFileSync(path.join(appDir, "config.json"), JSON.stringify({
  sources: [
    // The first window is deliberately mislabelled: its stored title is a
    // snapshot the user would have left behind (an editor's other file, a
    // browser's other tab), and the row must show the live caption instead.
    win(0, true, NO_CROP, "STALE TITLE — 过期标题"),
    win(1, false),
    // Both crop rows point at things that give a still *now*: the fitting edges
    // sit on the window above, and the impossible edges sit on the desktop,
    // which always has one. Seeding them by window index used to land on
    // whatever the user had minimized — and a row with no picture cannot test a
    // crop at all (2026-10-23). Ids stay unique: two sources sharing one would
    // send the editor to the wrong one, and the crop it opened would be zeros.
    { ...win(0, false, CROP), id: "win0-crop" },
    { id: "desk-over", capture: { Desktop: { rect: null, crop: OVER_CROP } }, fps: 30, quality: null, audio: false },
    // The desktop carries a crop too, and its name is a label rather than a
    // window title — the two independent things the size column sits between.
    // It also carries sound: Windows has no per-monitor audio, so a desktop
    // source's sound is the whole machine's, and the row has to say so.
    { id: "desk", capture: { Desktop: { rect: null, crop: CROP } }, fps: 30, quality: null, audio: true },
    // A source that cannot carry sound has no speaker: a mic *is* the sound, and
    // offering to "record its sound into its own file" would be a second copy
    // of the same thing.
    { id: "mic", capture: { Mic: { device: "TakeKit test microphone" } }, fps: 30, quality: null, audio: false },
    // Minimized on purpose (see the probe above): the state where a source has
    // a size and no picture, and the only one that offers a way out. Its stored
    // title is a stale snapshot too, so the row also proves the live caption
    // refresh — against a caption this script controls, not the user's.
    {
      id: "probe",
      capture: { Window: { hwnd: probeHwnd, pid: probe.pid, title: "STALE TITLE — 过期标题", crop: NO_CROP } },
      fps: 30,
      quality: null,
      audio: false,
    },
  ],
  backend: "windows",
  marks: false,
  language: "zh",
  theme: "light",
}));

const gui = spawn(GUI, [], {
  env: {
    ...process.env,
    APPDATA,
    TAKEKIT_WEBVIEW_NO_SANDBOX: "1",
    WEBVIEW2_USER_DATA_FOLDER: path.join(OUT, "wv2"),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
  },
  detached: true,
  stdio: "ignore",
});
gui.unref();

const controlFile = path.join(appDir, "control.json");
let control = null;
for (let i = 0; i < 60; i++) {
  await sleep(500);
  if (existsSync(controlFile)) {
    try { control = JSON.parse(readFileSync(controlFile, "utf8")); if (control.port) break; } catch {}
  }
}
record("panel-start", Boolean(control?.port), `port ${control?.port}`);
if (!control?.port) {
  say("the panel did not come up — is another instance already running?");
  process.exit(1);
}

const resp = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
const list = JSON.parse(await resp.text());
const target =
  list.find((t) => t.type === "page" && /tauri\.localhost/.test(t.url || "")) ||
  list.find((t) => t.type === "page" && /TakeKit/i.test(t.title || ""));
if (!target) { record("cdp-target", false, "no panel target"); process.exit(1); }

const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; setTimeout(() => rej(new Error("ws timeout")), 8000); });
let id = 1;
const waits = new Map();
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  const w = waits.get(m.id);
  if (w) { waits.delete(m.id); w(m); }
};
const send = (method, params = {}) =>
  new Promise((res) => { const i = id++; waits.set(i, res); ws.send(JSON.stringify({ id: i, method, params })); });
const evalJs = async (expression) => {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) return { __error: r.result.exceptionDetails.text };
  return r.result?.result?.value;
};

// The audit runs inside the page: contrast needs the computed style and the
// real painted background, neither of which survives a trip to node.
const AUDIT = `(() => {
  const parse = (s) => {
    const m = String(s).match(/rgba?\\(([^)]+)\\)/);
    if (!m) return null;
    const p = m[1].split(",").map((x) => parseFloat(x));
    return { r: p[0], g: p[1], b: p[2], a: p.length > 3 ? p[3] : 1 };
  };
  const lum = (c) => {
    const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); };
    return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b);
  };
  const over = (fg, bg) => ({
    r: fg.r * fg.a + bg.r * (1 - fg.a),
    g: fg.g * fg.a + bg.g * (1 - fg.a),
    b: fg.b * fg.a + bg.b * (1 - fg.a),
    a: 1,
  });
  const bgOf = (el) => {
    let acc = null;
    let node = el;
    while (node && node !== document.documentElement) {
      const c = parse(getComputedStyle(node).backgroundColor);
      if (c && c.a > 0) acc = acc ? over(acc, c) : c;
      if (acc && acc.a >= 0.99) return acc;
      node = node.parentElement;
    }
    const root = parse(getComputedStyle(document.documentElement).backgroundColor) || { r: 255, g: 255, b: 255, a: 1 };
    return acc ? over(acc, root) : root;
  };
  const ratio = (a, b) => {
    const l1 = lum(a), l2 = lum(b);
    return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
  };
  const visible = (el) => {
    if (el.closest(".hidden")) return false;
    const r = el.getBoundingClientRect();
    const st = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && st.visibility !== "hidden" && st.display !== "none" && parseFloat(st.opacity) > 0.1;
  };
  const label = (el) => {
    const cls = typeof el.className === "string" && el.className ? "." + el.className.trim().split(/\\s+/)[0] : "";
    return el.tagName.toLowerCase() + cls;
  };
  const out = { low: [], spill: [], truncated: [], hScroll: 0, audited: 0 };
  for (const el of document.querySelectorAll("body *")) {
    if (!visible(el)) continue;
    const hasText = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
    if (!hasText) continue;
    out.audited += 1;
    const st = getComputedStyle(el);
    const fg = parse(st.color);
    const bg = bgOf(el);
    if (fg && bg) {
      const fgPainted = fg.a < 1 ? over(fg, bg) : fg;
      const cr = ratio(fgPainted, bg);
      const size = parseFloat(st.fontSize);
      const bold = Number(st.fontWeight) >= 600;
      const need = size >= 24 || (size >= 18.66 && bold) ? 3 : 4.5;
      if (cr < need) out.low.push({ el: label(el), text: el.textContent.trim().slice(0, 20), cr: +cr.toFixed(2), need });
    }
    const ox = st.overflowX;
    if (el.scrollWidth - el.clientWidth > 1) {
      if (ox === "visible" || ox === "auto" || ox === "scroll") {
        out.spill.push({ el: label(el), text: el.textContent.trim().slice(0, 20), sw: el.scrollWidth, cw: el.clientWidth, ox });
      } else {
        out.truncated.push({ el: label(el), text: el.textContent.trim().slice(0, 20) });
      }
    }
  }
  out.hScroll = document.documentElement.scrollWidth - document.documentElement.clientWidth;
  return JSON.stringify(out);
})()`;

const LANGS = ["zh", "en"];
const THEMES = ["light", "dark"];
// The editor's own state, as the crop checks need to see it: did the still
// land, is the crop entry open, is the way out offered.
const READ_EDITOR = `JSON.stringify({
  open: !document.getElementById("editor").classList.contains("hidden"),
  loading: editor ? Boolean(editor.loading) : null,
  image: editor ? Boolean(editor.image) : null,
  note: editor ? editor.note : null,
  size: editor ? [editor.width, editor.height] : null,
  thumbDisabled: document.getElementById("editor-thumb").disabled,
  restore: !document.getElementById("editor-restore").classList.contains("hidden"),
  reason: (document.getElementById("editor-thumb-empty").textContent || "").trim(),
  workspace: !document.getElementById("crop").classList.contains("hidden"),
})`;
async function editorState(tries = 25) {
  let st = {};
  for (let i = 0; i < tries; i++) {
    try {
      st = JSON.parse(await evalJs(READ_EDITOR));
    } catch {}
    if (st.loading === false) return st;
    await sleep(200);
  }
  return st;
}

const CLOSE_ALL = `(() => {
  // Only when open: the crop-back button is always in the DOM, and clicking
  // it with no editor used to bring the card back empty.
  if (!document.getElementById("crop").classList.contains("hidden")) document.getElementById("crop-back").click();
  if (!document.getElementById("picker").classList.contains("hidden") && typeof closePicker === "function") closePicker();
  if (!document.getElementById("editor").classList.contains("hidden")) document.getElementById("editor-close").click();
})()`;

// Every editor screen clicks a row and then measures what the card shows, so
// it has to wait for the grab. A still of a hardware-accelerated window can
// take a second, and a screenshot of "正在抓取画面…" proves nothing.
const WAIT_STILL = `for (let i = 0; i < 60 && (!editor || editor.loading); i++) await new Promise((r) => setTimeout(r, 150));`;

const SCREENS = [
  { name: "sources", click: `(() => { ${CLOSE_ALL}; document.getElementById("tab-sources").click(); })()` },
  { name: "editor", click: `(async () => { document.querySelector(".row").click(); ${WAIT_STILL} })()` },
  // The minimized window: size known, no picture, and therefore nothing to
  // crop against. This is the card the user reported as pointless to crop in.
  {
    name: "editor-minimized",
    click: `(async () => {
      document.getElementById("editor-close")?.click();
      const r = [...document.querySelectorAll(".row")].find((x) =>
        (x.querySelector(".row-name")?.textContent || "").includes("probe window"),
      );
      if (!r) return false;
      r.click();
      ${WAIT_STILL}
      return true;
    })()`,
  },
  // The cropped source's editor: its crop numbers are the densest row of
  // inputs in the app (four fields, three digits each).
  {
    name: "editor-crop",
    click: `(async () => {
      document.getElementById("editor-close")?.click();
      const r = [...document.querySelectorAll(".row")].find((x) => (x.querySelector(".row-geo")?.textContent || "").includes("→"));
      if (!r) return false;
      r.click();
      ${WAIT_STILL}
      return true;
    })()`,
  },
  // The crop workspace where the edges do fit: the box is smaller than the
  // picture and the dim around it is what gets cut.
  {
    name: "crop-workspace",
    click: `(async () => { ${WAIT_STILL}; document.getElementById("editor-thumb")?.click(); })()`,
  },
  // And the same card on edges that cannot fit: the box collapses to nothing,
  // so the output field is the only thing that can explain the file.
  {
    name: "editor-crop-dropped",
    click: `(async () => {
      ${CLOSE_ALL};
      const r = [...document.querySelectorAll(".row")].find((x) => {
        const g = x.querySelector(".row-geo");
        return g && g.title && !g.textContent.includes("→") && /裁剪超出画面|crop is larger/i.test(g.title);
      });
      if (!r) return false;
      r.click();
      ${WAIT_STILL}
      return true;
    })()`,
  },
  // The crop workspace is the third place that prints the output size, and the
  // one that also draws a box — with nothing left to keep, the box has no width.
  {
    name: "crop-workspace-dropped",
    click: `(async () => { ${WAIT_STILL}; document.getElementById("editor-thumb")?.click(); })()`,
  },
  { name: "picker", click: `(() => { ${CLOSE_ALL}; document.getElementById("add-btn").click(); })()` },
  { name: "settings", click: `(() => { ${CLOSE_ALL}; document.getElementById("tab-settings").click(); })()` },
];

// What the rows actually say, plus whether the size column had to clip.
// `scrollWidth > clientWidth` on a `.row-geo` means the ellipsis ate the
// right-hand side — which is the cropped size, the whole point of the column.
//
// It also measures the one thing a screenshot review keeps missing: how much
// air sits between the speaker and the first digit of "30fps · Q20". That cell
// was a fixed 88px holding a 58px string, so 30px of its own column became a
// gap in the middle of the row. Text width comes from a Range because the box
// is now sized by the text — if the box ever grows a fixed width again, these
// two numbers disagree and the check fails.
const READ_ROWS = `(() => {
  const textW = (el) => {
    if (!el || !el.firstChild) return 0;
    const r = document.createRange();
    r.selectNodeContents(el);
    return r.getBoundingClientRect().width;
  };
  return JSON.stringify([...document.querySelectorAll(".row")].map((row) => {
    const geo = row.querySelector(".row-geo");
    const sum = row.querySelector(".row-summary");
    const snd = row.querySelector(".row-sound");
    const sumTextW = textW(sum);
    const sumBox = sum ? sum.getBoundingClientRect() : null;
    const sndBox = snd ? snd.getBoundingClientRect() : null;
    const rm = row.querySelector(".row-remove");
    const rmBox = rm ? rm.getBoundingClientRect() : null;
    // The size column is what the frame rate follows now: the speaker moved to
    // the end of the row, so nothing sits between two numbers.
    const prev = geo;
    const prevBox = prev ? prev.getBoundingClientRect() : null;
    return {
      name: (row.querySelector(".row-name")?.textContent || "").trim(),
      geo: (geo?.textContent || "").trim(),
      title: (geo?.title || "").trim(),
      geoW: geo ? geo.clientWidth : 0,
      geoScroll: geo ? geo.scrollWidth : 0,
      textW: Math.round(textW(geo) * 10) / 10,
      sound: Boolean(snd),
      // Where the speaker and the × sit, to prove the columns do not slide when
      // a list mixes rows that can carry sound with rows that cannot.
      sumRight: sumBox ? Math.round(sumBox.right * 10) / 10 : 0,
      soundLeft: sndBox ? Math.round(sndBox.left * 10) / 10 : null,
      soundRight: sndBox ? Math.round(sndBox.right * 10) / 10 : null,
      removeLeft: rmBox ? Math.round(rmBox.left * 10) / 10 : 0,
      kind: row.dataset.kind || "",
      soundTitle: (snd?.title || "").trim(),
      soundOn: snd ? snd.getAttribute("aria-pressed") === "true" : null,
      summary: (sum?.textContent || "").trim(),
      // Phantom width inside the frame-rate cell (box minus its own text).
      summarySlack: sumBox ? Math.round((sumBox.width - sumTextW) * 10) / 10 : 0,
      // Distance from whatever precedes it to the first glyph.
      summaryGap: sumBox && prevBox ? Math.round((sumBox.right - sumTextW - prevBox.right) * 10) / 10 : 0,
      // The mid-take offset chip: where the file begins on the take's timeline.
      // Measured with the name it shares a cell with, because the chip is only
      // allowed to sit inside the flexible column — never between two numbers.
      startText: (row.querySelector(".row-start")?.textContent || "").trim(),
      startLeft: row.querySelector(".row-start")?.getBoundingClientRect().left ?? null,
      nameLeft: row.querySelector(".row-name")?.getBoundingClientRect().left ?? null,
    };
  }));
})()`;

await sleep(1500);
let worst = { cr: 99 };

for (const lang of LANGS) {
  for (const theme of THEMES) {
    const tag = `${lang}-${theme}`;
    await evalJs(`(() => {
      // applyLanguageUi() reads state.language, so setting the i18n table
      // alone would relabel nothing — the state is the switch.
      state.language = ${JSON.stringify(lang)};
      document.documentElement.dataset.theme = ${JSON.stringify(theme)};
      applyLanguageUi();
      return state.language;
    })()`);
    await sleep(800);

    // The rows first, and on the tab that shows them: the size column is a
    // fixed 160px with an ellipsis, so a cropped source is the one case where
    // the column can silently swallow the number it exists to show.
    await evalJs(`document.getElementById("tab-sources").click()`);
    await sleep(400);
    const rowsRaw = await evalJs(READ_ROWS);
    let rows = [];
    try { rows = JSON.parse(rowsRaw); } catch {}
    const cropped = rows.filter((r) => r.geo.includes("→"));
    // Two cropped rows are seeded — a window and the desktop — so the column is
    // measured against both a title and a label on either side of it.
    record(`${tag}/row-crop-shape`, cropped.length === 2, JSON.stringify(cropped.map((r) => r.geo)));
    for (const [i, row] of cropped.entries()) {
      const m = row.geo.match(/^(\d+)×(\d+)\s*→\s*(\d+)×(\d+)$/);
      const trim = m && Number(m[1]) - Number(m[3]) === CROP.left + CROP.right && Number(m[2]) - Number(m[4]) === CROP.top + CROP.bottom;
      record(`${tag}/row-crop-${i}-math`, Boolean(trim), `${row.geo} vs crop ${JSON.stringify(CROP)}`);
      record(`${tag}/row-crop-${i}-fits`, row.geoScroll <= row.geoW + 1, `text ${row.textW}px in ${row.geoW}px`);
    }
    // The speaker belongs to every picture that can carry sound — a window's
    // own process tree, the desktop's system sound — and to nothing else. A
    // mic needs none (it *is* the sound), and a camera has no route to one.
    const carriers = rows.filter((r) => r.kind === "window" || r.kind === "desktop");
    const silent = rows.filter((r) => r.kind !== "window" && r.kind !== "desktop" && r.sound);
    record(`${tag}/row-speaker-scope`,
      carriers.length > 0 && carriers.every((r) => r.sound) && silent.length === 0,
      `carriers ${carriers.map((r) => `${r.kind}${r.sound ? "" : "!"}`).join(",")} | off-scope ${silent.map((r) => r.kind).join(",") || "none"}`);
    const plain = rows.filter((r) => !r.geo.includes("→") && /\d+×\d+/.test(r.geo));
    record(`${tag}/row-plain-untrimmed`, plain.every((r) => !r.geo.includes("→")), `${plain.length} untrimmed rows`);
    // The over-crop row: a size, a minus sign, or a size plus a reason. Only
    // the last one matches what the file will contain.
    const droppedHint = lang === "zh" ? /裁剪超出画面/ : /crop is larger/i;
    const over = rows.find((r) => droppedHint.test(r.title));
    record(`${tag}/row-over-crop`, Boolean(over) && /^\d+×\d+$/.test(over.geo), JSON.stringify(over?.geo ?? rows.map((r) => r.geo)));

    // The frame-rate cell must not hold air of its own, and must follow the
    // size column at the row's 12px gap. A fixed width slips through every
    // other check here: nothing overflows, nothing truncates, nothing gets
    // wider than the window — it just looks like a hole in the row.
    const looseSummary = rows.filter((r) => r.summarySlack > 1 || r.summaryGap > 16);
    record(`${tag}/row-summary-tight`, looseSummary.length === 0,
      looseSummary.length
        ? looseSummary.map((r) => `${r.name}: slack ${r.summarySlack}px gap ${r.summaryGap}px`).join(" | ")
        : `slack ${Math.max(...rows.map((r) => r.summarySlack))}px gap ${Math.max(...rows.map((r) => r.summaryGap))}px`);

    // The speaker is the last cell of the line of facts, not one in the middle
    // of it: the frame rate has to end before the speaker begins.
    const misplaced = rows.filter((r) => r.sound && r.soundLeft !== null && r.soundLeft < r.sumRight - 0.5);
    record(`${tag}/row-speaker-last`, misplaced.length === 0,
      misplaced.length ? misplaced.map((r) => `${r.name}: fps ends ${r.sumRight}, speaker at ${r.soundLeft}`).join(" | ")
        : `speaker after fps on ${rows.filter((r) => r.sound).length} rows`);

    // Rows that cannot carry sound keep an empty slot, so the trailing columns
    // sit on one grid whatever the list is made of. Without the slot a window
    // row pushes the frame rate 38px left of a microphone row's — the numbers
    // stop forming columns, which is the whole point of the right-hand side.
    const spread = (key) => {
      const xs = rows.map((r) => r[key]).filter((x) => x > 0);
      return xs.length ? Math.round((Math.max(...xs) - Math.min(...xs)) * 10) / 10 : 0;
    };
    record(`${tag}/row-trailing-columns-line-up`,
      spread("sumRight") <= 1 && spread("removeLeft") <= 1,
      rows.map((r) => `${r.kind}: fps→${r.sumRight} ×→${r.removeLeft}`).join(" | "));

    // ── the offset chip, while a take is running ──────────────────────────
    // Recording is entered directly instead of started: a real take would film
    // the user's screen. What is being tested is the layout — a fourth thing on
    // the row is exactly how three number columns stop lining up — plus the one
    // number that matters: 32 s must read "0:32", not "0:00" and not "32".
    // `state.recording` is the switch that decides whether a row grows a chip;
    // the body class only styles it. Setting just the class would test a screen
    // the app never produces — and pass, because nothing would have moved.
    await evalJs(`(() => {
      state.recording = true;
      for (const s of state.sources) state.starts.set(s.id, 32);
      render();
    })()`);
    await sleep(300);
    const chipped = JSON.parse(await evalJs(READ_ROWS));
    const noChip = chipped.filter((r) => !r.startText);
    record(`${tag}/row-start-chip-present`, noChip.length === 0,
      noChip.length ? `missing on ${noChip.map((r) => r.name).join(", ")}` : `${chipped.length} rows chipped`);
    const wrongText = chipped.filter((r) => r.startText !== "0:32");
    record(`${tag}/row-start-chip-reads-mm-ss`, wrongText.length === 0,
      wrongText.length ? wrongText.map((r) => `${r.name}: "${r.startText}"`).join(" | ") : "all read 0:32");
    // Inside the name cell, to the left of the name: that is what keeps the
    // three number columns where they were.
    const misplacedChip = chipped.filter((r) => !(r.startLeft !== null && r.nameLeft !== null && r.startLeft < r.nameLeft));
    record(`${tag}/row-start-chip-leads-the-name`, misplacedChip.length === 0,
      misplacedChip.length ? misplacedChip.map((r) => `${r.name}: chip ${r.startLeft} name ${r.nameLeft}`).join(" | ")
        : `chip right edge before name on ${chipped.length} rows`);
    const spreadWhile = (key) => {
      const xs = chipped.map((r) => r[key]).filter((x) => x > 0);
      return xs.length ? Math.round((Math.max(...xs) - Math.min(...xs)) * 10) / 10 : 0;
    };
    record(`${tag}/row-start-chip-keeps-columns`, spreadWhile("sumRight") <= 1 && spreadWhile("removeLeft") <= 1,
      chipped.map((r) => `${r.kind}: fps→${r.sumRight} ×→${r.removeLeft}`).join(" | "));
    // Kept for the eyeball pass: this screen only exists mid-take, so no other
    // screenshot in the set shows the chip.
    {
      const shot = await send("Page.captureScreenshot", { format: "png" });
      writeFileSync(path.join(OUT, `${tag}-row-start-chip.png`), Buffer.from(shot.result.data, "base64"));
    }
    // The panel comes back mid-take now (the popover's 添加源), so the record
    // button has to say what pressing it will do. "开始录制" over a running
    // take would be both a lie and a stop button in disguise.
    const transport = await evalJs(`document.getElementById("rec-btn").textContent`);
    record(`${tag}/bar-says-stop-while-recording`,
      transport === (lang === "zh" ? "停止" : "Stop"), `"${transport}"`);
    const overflow = await evalJs(`document.documentElement.scrollWidth > document.documentElement.clientWidth`);
    record(`${tag}/row-start-chip-no-overflow`, overflow === false, `scrollWidth ${await evalJs("document.documentElement.scrollWidth")}`);
    // A source whose first sample has not arrived says so instead of guessing
    // zero — the number does not exist yet and "0:00" would be a lie.
    await evalJs(`(() => {
      for (const s of state.sources) state.starts.delete(s.id);
      render();
    })()`);
    await sleep(200);
    const pending = JSON.parse(await evalJs(READ_ROWS));
    record(`${tag}/row-start-chip-pending`, pending.every((r) => r.startText === "—"),
      pending.map((r) => r.startText).join(","));
    await evalJs(`(() => {
      state.recording = false;
      state.starts.clear();
      render();
    })()`);
    await sleep(200);

    // Sound is an option on any picture that can carry it — a window's own
    // process tree, and the desktop's system sound. Two rows here carry it, so
    // the speaker must appear on both, and the desktop's tooltip has to name
    // the thing a user would otherwise misread as "this monitor's audio".
    const speakers = rows.filter((r) => r.sound);
    const deskHint = lang === "zh" ? /整台电脑的声音/ : /all system sound/i;
    record(`${tag}/sound-on-window-and-desktop`, speakers.length >= 2,
      speakers.map((r) => `${r.name}=${r.soundOn}`).join(" | ") || "no speaker at all");
    record(`${tag}/desktop-sound-says-whole-machine`,
      speakers.some((r) => deskHint.test(r.soundTitle)),
      speakers.map((r) => r.soundTitle).join(" | "));

    // A name the user has moved past: the stored title is a snapshot, and the
    // row has to show what the window calls itself now. Both halves matter —
    // the stale string must be gone *and* the live one present, or a row that
    // simply blanked itself would pass.
    const stale = rows.filter((r) => r.name.includes("STALE"));
    const relabelled = rows.filter((r) => r.name.startsWith(LIVE_TITLE_PREFIX));
    record(`${tag}/row-title-follows-window`, stale.length === 0 && relabelled.length > 0,
      stale.length ? `still stale: ${stale.map((r) => r.name).join(" | ")}` : `live: ${relabelled.map((r) => r.name)[0] ?? "none"}`);

    for (const screen of SCREENS) {
      await evalJs(screen.click);
      await sleep(500);
      if (screen.name === "sources") {
        const st = await evalJs(`JSON.stringify({
          editorOpen: !document.getElementById("editor").classList.contains("hidden"),
          editorVar: editor ? { id: editor.id, name: editor.name, w: editor.width, draft: editor.draft } : null,
        })`);
        say(`    sources state: ${st}`);
      }
      const raw = await evalJs(AUDIT);
      let a;
      try { a = JSON.parse(raw); } catch { record(`${tag}/${screen.name}`, false, String(raw).slice(0, 120)); continue; }

      const shot = await send("Page.captureScreenshot", { format: "png" });
      writeFileSync(path.join(OUT, `${tag}-${screen.name}.png`), Buffer.from(shot.result.data, "base64"));

      for (const l of a.low) if (l.cr < worst.cr) worst = { cr: l.cr, ...l, where: `${tag}/${screen.name}` };
      const ok = a.low.length === 0 && a.spill.length === 0 && a.hScroll <= 0;
      record(`${tag}/${screen.name}`, ok,
        `text=${a.audited} low=${a.low.length} spill=${a.spill.length} trunc=${a.truncated.length} hScroll=${a.hScroll}`);
      if (a.low.length) say(`    low: ${a.low.slice(0, 6).map((l) => `${l.el} "${l.text}" ${l.cr}/${l.need}`).join(" | ")}`);
      if (a.spill.length) say(`    spill: ${a.spill.slice(0, 4).map((l) => `${l.el} ${l.sw}>${l.cw}`).join(" | ")}`);

      if (screen.name === "editor-minimized") {
        // The user's complaint, made into an assertion: a minimized window has
        // a size but no picture, so there is nothing to crop against — the
        // entry has to be shut and the way out ("恢复窗口") on screen.
        const st = await editorState();
        record(`${tag}/minimized-no-crop`,
          st.open === true && st.image === false && st.thumbDisabled === true,
          `image=${st.image} thumbDisabled=${st.thumbDisabled} size=${JSON.stringify(st.size)}`);
        record(`${tag}/minimized-restore-offered`, st.restore === true, `reason="${st.reason}"`);
        // And the picture must not be a door: clicking it with nothing to crop
        // against used to open the workspace on a blank stage.
        await evalJs(`document.getElementById("editor-thumb").click()`);
        const ws = JSON.parse(await evalJs(READ_EDITOR));
        record(`${tag}/minimized-crop-closed`, ws.workspace === false, `workspace=${ws.workspace}`);
      }
      if (screen.name === "crop-workspace") {
        // Fitting edges: a box strictly smaller than the picture, sitting
        // inside it. This is the pair to the dropped case below.
        const geo = await evalJs(`JSON.stringify({
          box: document.getElementById("crop-box").getBoundingClientRect(),
          plane: document.getElementById("crop-plane").getBoundingClientRect(),
          label: document.getElementById("crop-output").textContent,
        })`);
        let g = {};
        try { g = JSON.parse(geo); } catch {}
        const inside = g.box && g.plane && g.box.width > 0 && g.box.width < g.plane.width - 1 && g.box.left > g.plane.left - 1;
        record(`${tag}/workspace-crop-box-inside`, inside, `box=${Math.round(g.box?.width)} plane=${Math.round(g.plane?.width)} label="${g.label}"`);
      }
      if (screen.name === "editor-crop-dropped") {
        // The card has two places that name the output size: the badge on the
        // thumbnail and the one line above 取消/保存. Both have to say the
        // whole frame, because that is what the file will hold.
        const badge = await evalJs(`document.getElementById("editor-size").textContent`);
        const note = await evalJs(`document.getElementById("editor-note").textContent`);
        const whole = /整幅|whole frame/i;
        record(`${tag}/card-dropped-says-whole`, whole.test(badge || "") && whole.test(note || ""), `badge="${badge}" note="${note}"`);
        // The reason the thumbnail is empty is a sentence, and English says it
        // in twice the width: it used to run off the right edge.
        const empty = await evalJs(`(() => {
          const el = document.getElementById("editor-thumb-empty");
          return JSON.stringify({ sw: el.scrollWidth, cw: el.clientWidth, text: (el.textContent || "").slice(0, 30) });
        })()`);
        let e = {};
        try { e = JSON.parse(empty); } catch {}
        record(`${tag}/thumb-note-fits`, (e.sw || 0) <= (e.cw || 0) + 1, `${e.sw}/${e.cw}px "${e.text}"`);
      }
      if (screen.name === "crop-workspace-dropped") {
        const state = await evalJs(`JSON.stringify({
          open: !document.getElementById("crop").classList.contains("hidden"),
          label: document.getElementById("crop-output").textContent,
          box: document.getElementById("crop-box").getBoundingClientRect().width,
          plane: document.getElementById("crop-plane").getBoundingClientRect().width,
          dashed: document.getElementById("crop-box").classList.contains("dropped"),
        })`);
        let s = {};
        try { s = JSON.parse(state); } catch {}
        // The box covers the whole picture (a narrower one would dim the stage
        // and read as a broken viewer) and is dashed so it does not look like
        // a crop that will happen.
        const covers = Math.abs((s.box || 0) - (s.plane || 0)) <= 1;
        record(`${tag}/workspace-dropped-covers-all`, s.open && /整幅|whole frame/i.test(s.label || "") && covers && s.dashed,
          `open=${s.open} "${s.label}" box=${Math.round(s.box)} plane=${Math.round(s.plane)} dashed=${s.dashed}`);
      }
    }
  }
}

// The way out, once — the probe can only be restored once. Pressing 恢复窗口
// has to turn the same card into a croppable one without reopening it, which
// exercises the whole round trip: Rust unminimize → a fresh still → the UI rule.
say("restore round trip");
await evalJs(`(() => {
  state.language = "zh";
  document.documentElement.dataset.theme = "light";
  applyLanguageUi();
  ${CLOSE_ALL};
  const r = [...document.querySelectorAll(".row")].find((x) =>
    (x.querySelector(".row-name")?.textContent || "").includes("probe window"),
  );
  if (r) r.click();
})()`);
const beforeRestore = await editorState();
await evalJs(`document.getElementById("editor-restore").click()`);
let afterRestore = {};
for (let i = 0; i < 40; i++) {
  await sleep(250);
  afterRestore = JSON.parse(await evalJs(READ_EDITOR));
  if (afterRestore.image === true) break;
}
record("restore-window/turns-croppable",
  beforeRestore.thumbDisabled === true &&
    afterRestore.image === true &&
    afterRestore.thumbDisabled === false &&
    afterRestore.restore === false,
  `before image=${beforeRestore.image} disabled=${beforeRestore.thumbDisabled} → after image=${afterRestore.image} disabled=${afterRestore.thumbDisabled} offered=${afterRestore.restore}`);
{
  const shot = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(path.join(OUT, "zh-light-editor-restored.png"), Buffer.from(shot.result.data, "base64"));
}

// The picker is the only door that writes a real pid: a seeded config or an
// agent may carry 0 ("unknown owner"), but a window the user clicked must hand
// over both halves of its identity — otherwise the recycled-handle check exists
// everywhere except where sources actually come from.
const pids = JSON.parse(
  await evalJs(`JSON.stringify((state.windows || []).slice(0, 6).map((w) => w.pid))`)
);
record("picker/window-list-carries-pid",
  pids.length > 0 && pids.every((p) => Number.isInteger(p) && p > 0),
  `pids ${JSON.stringify(pids)}`);

if (worst.cr < 99) say(`\nworst contrast: ${worst.cr} on ${worst.el} "${worst.text}" at ${worst.where}`);
ws.close();
try { execFileSync("taskkill", ["/PID", String(gui.pid), "/T", "/F"], { stdio: "ignore" }); } catch {}
// The probe window is ours: take it down whether the run passed or not.
try { execFileSync("taskkill", ["/PID", String(probe.pid), "/T", "/F"], { stdio: "ignore" }); } catch {}
const fails = results.filter((r) => !r.ok);
say(`=== ${results.length - fails.length}/${results.length} screens clean ===`);
say(`screenshots: ${OUT}`);
process.exit(fails.length ? 1 : 0);
