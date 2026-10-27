// The stop screen, checked in its own window.
//
// Nothing else can see it: the veil is a third document, it is only up while a
// take is being stopped or read, and reaching that state honestly needs a real
// recording. Which is exactly how it shipped with a two-second life, a second
// tone on the way out, and no way to leave it early — three things a screenshot
// review of the panel cannot show, because the panel is not what is on screen.
//
// So this drives the veil's page directly over CDP. What is checked:
//
//   1. the page still loads with i18n (it grew one line of prose, and a broken
//      script there takes the countdown down with it);
//   2. that line is in the panel's language, and follows a language change;
//   3. the stop screen lays out — hint on screen, inside the window, not
//      clipped, and clear of the checklist above it;
//   4. a click is what reaches Rust (`overlay:dismiss-click` in the probe log),
//      and a click during the countdown is not;
//   5. `take:done` is what arms it, so the two cannot drift apart.
//
// Usage:  cargo build --workspace && node scripts/check-stop-screen.mjs
// Close any running panel first (a second instance exits on sight).
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import path from "node:path";
import { assertNoPanel } from "./lib/panel-guard.mjs";
import { pickBinary } from "./lib/build.mjs";

const ROOT = path.resolve(import.meta.dirname, "..");
// `TK_GUI` points the check at another build; see check-tray-layout.mjs.
const GUI = pickBinary(ROOT, "takekit-gui.exe", "TK_GUI");
const OUT = path.join(ROOT, "target", "checks", "stop-screen");
const APPDATA = path.join(OUT, "appdata");
const appDir = path.join(APPDATA, "TakeKit");
const CDP_PORT = 9242;

if (!existsSync(GUI)) {
  console.error(`missing ${GUI} — run: cargo build --workspace`);
  process.exit(2);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const say = (m) => console.log(`[${new Date().toISOString().slice(11, 19)}] ${m}`);
const results = [];
function record(name, ok, detail) {
  results.push({ name, ok, detail });
  say(`${ok ? "PASS" : "FAIL"}  ${name}  ${detail || ""}`);
}

// Whether another panel is in the way is `assertNoPanel`'s question, asked
// right before the spawn below — one implementation for all five scripts
// instead of a fourth copy of the same `tasklist` call.

// Only the probe log goes: the rest of this tree (the WebView2 profile, the
// screenshots) is reused or overwritten, and a whole-tree delete is what the
// host's bulk-delete guard refuses (see check-ui-matrix, 2026-10-27).
try {
  rmSync(path.join(OUT, "probe.log"), { force: true });
} catch {}
mkdirSync(appDir, { recursive: true });
writeFileSync(
  path.join(appDir, "config.json"),
  JSON.stringify(
    { hotkey: "F9", pause_hotkey: "F10", countdown: 3, finalize_panel: true, language: "zh", theme: "light" },
    null,
    2,
  ),
);

let guiPid = 0;
const killTree = (pid) => {
  if (!pid) return;
  try { execFileSync("taskkill", ["/PID", String(pid), "/T", "/F"], { stdio: "ignore" }); } catch {}
};
process.on("exit", () => killTree(guiPid));

const probeLog = path.join(OUT, "probe.log");
await assertNoPanel();

const gui = spawn(GUI, [], {
  env: {
    ...process.env,
    APPDATA,
    TAKEKIT_PROBE_LOG: probeLog,
    // Raises the veil at startup so it can be photographed; see main.rs.
    TAKEKIT_OVERLAY_DEMO: "1",
    TAKEKIT_WEBVIEW_NO_SANDBOX: "1",
    WEBVIEW2_USER_DATA_FOLDER: path.join(OUT, "wv2"),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
  },
  detached: true,
  stdio: "ignore",
});
gui.unref();
guiPid = gui.pid;

// ── CDP ────────────────────────────────────────────────────────────────────
// The port answers before the targets do, so this waits for the veil's document
// by name (the same lesson as check-tray-layout).
let list = null;
let target = null;
for (let i = 0; i < 80 && !target; i += 1) {
  try {
    const resp = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
    list = JSON.parse(await resp.text());
    target = list.find((t) => /overlay\.html/.test(t.url || ""));
  } catch {
    list = null;
  }
  if (!target) await sleep(200);
}
if (!list) {
  record("cdp-port", false, `no debug port on ${CDP_PORT} after 16s (WebView2 did not start)`);
  process.exit(1);
}
if (!target) {
  record("cdp-target/overlay", false, `no veil target; saw ${list.map((t) => t.url).join(", ")}`);
  process.exit(1);
}
record("cdp-target/overlay", true, target.url);

const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => {
  ws.onopen = res;
  ws.onerror = rej;
  setTimeout(() => rej(new Error("ws timeout")), 8000);
});
let id = 1;
const waits = new Map();
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  const w = waits.get(m.id);
  if (w) { waits.delete(m.id); w(m); }
};
const send = (method, params = {}, timeoutMs = 10_000) =>
  new Promise((res) => {
    const i = id++;
    const timer = setTimeout(() => { waits.delete(i); res({ __timeout: true }); }, timeoutMs);
    waits.set(i, (m) => { clearTimeout(timer); res(m); });
    ws.send(JSON.stringify({ id: i, method, params }));
  });
const evalJs = async (expression, awaitPromise = false) => {
  const r = await send("Runtime.evaluate", { expression, awaitPromise, returnByValue: true });
  if (r.__timeout) return undefined;
  if (r.result?.exceptionDetails) return { __error: r.result.exceptionDetails.text };
  return r.result?.result?.value;
};
const evalJson = async (expression, awaitPromise = false) => {
  const raw = await evalJs(expression, awaitPromise);
  try { return JSON.parse(raw) ?? {}; } catch { return {}; }
};
const readProbe = () => {
  try { return readFileSync(probeLog, "utf8"); } catch { return ""; }
};
const probeCount = (needle) => readProbe().split(needle).length - 1;

// ── 1. The page is whole ───────────────────────────────────────────────────
// The target exists before its document does — the same race that had the panel
// matrix reading an empty DOM — so this waits for the nodes rather than for the
// URL.
const READ_PAGE = `JSON.stringify({
  big: !!document.getElementById("big"),
  sub: !!document.getElementById("sub"),
  list: !!document.getElementById("list"),
  foot: !!document.getElementById("foot"),
  // i18n.js is new on this page: without it the one line of prose has no text
  // and setUiLang throws on the first language change.
  hasT: typeof t === "function",
  hasSetLang: typeof setUiLang === "function",
  lang: document.documentElement.lang,
})`;
let page = {};
for (let i = 0; i < 40; i += 1) {
  page = await evalJson(READ_PAGE);
  if (page.big && page.foot && page.hasT) break;
  await sleep(200);
}
record(
  "overlay/page-whole",
  page.big && page.sub && page.list && page.foot && page.hasT && page.hasSetLang,
  JSON.stringify(page),
);

// ── 2. The stop screen, painted the way take:done paints it ───────────────
// The listeners themselves cannot be fired from here (they are Tauri events),
// so this performs what `take:finalize` + one `take:source` + `take:done` do to
// the DOM, then measures the result. `stopScreen` is the page's own flag — the
// half that decides whether a click means anything.
const PAINT = (lang) => `(async () => {
  await window.__TAURI__.core.invoke("set_language", { language: "${lang}" });
  const big = document.getElementById("big");
  const sub = document.getElementById("sub");
  const list = document.getElementById("list");
  const foot = document.getElementById("foot");
  big.textContent = "STOP";
  sub.textContent = "00:01:24";
  list.replaceChildren();
  const row = document.createElement("div");
  row.className = "row";
  for (const [cls, text] of [["row-id", "win"], ["row-num", "00:01:24"], ["row-num", "1920×1130"], ["row-num", "8.2 MB"]]) {
    const span = document.createElement("span");
    span.className = cls;
    span.textContent = text;
    row.append(span);
  }
  list.append(row);
  // What the take:done listener does.
  foot.hidden = false;
  stopScreen = true;
  await new Promise((r) => setTimeout(r, 150));
  const f = foot.getBoundingClientRect();
  const l = list.getBoundingClientRect();
  const cs = getComputedStyle(foot);
  return JSON.stringify({
    text: foot.textContent.trim(),
    lang: document.documentElement.lang,
    inside: f.bottom <= innerHeight && f.left >= 0 && f.right <= innerWidth,
    // Clipped prose is the failure this line cannot afford: it is the only
    // instruction on the screen.
    clipped: foot.scrollWidth > foot.clientWidth + 1,
    fixed: cs.position === "fixed",
    gapAbove: Math.round(f.top - l.bottom),
    belowList: f.top >= l.bottom - 0.5,
  });
})()`;

const WANT = {
  zh: "点击任意位置关闭 · 1 分钟后自动关闭",
  en: "Click anywhere to close · closes itself after a minute",
};

// The photograph first: the demo lowers the veil 5s after startup, and a hidden
// window has nothing to capture. The whole virtual screen, scaled down: the
// summary is centred and the way out is pinned to the bottom edge, so a crop of
// either half would miss the point of the other.
{
  await evalJs(PAINT("zh"), true);
  const metrics = await send("Page.getLayoutMetrics");
  const view = metrics.result?.cssLayoutViewport ?? metrics.result?.layoutViewport ?? {};
  const vw = Math.round(view.clientWidth ?? 1200);
  const vh = Math.round(view.clientHeight ?? 700);
  const r = await send("Page.captureScreenshot", {
    format: "png",
    clip: { x: 0, y: 0, width: vw, height: vh, scale: Math.min(1, 1000 / vw) },
    captureBeyondViewport: false,
  });
  if (r.result?.data) writeFileSync(path.join(OUT, "stop-screen.png"), Buffer.from(r.result.data, "base64"));
}

for (const lang of ["zh", "en"]) {
  const m = await evalJson(PAINT(lang), true);
  record(`stop-screen/${lang}/says-how-to-leave`, m.text === WANT[lang], `"${m.text}" (lang=${m.lang})`);
  record(
    `stop-screen/${lang}/hint-lays-out`,
    m.inside === true && m.clipped === false && m.fixed === true,
    `inside=${m.inside} clipped=${m.clipped} fixed=${m.fixed}`,
  );
  // Under the checklist, not on top of it: a hint over the numbers reads as
  // part of the last row's failure.
  record(`stop-screen/${lang}/hint-clears-the-list`, m.belowList === true, `gap ${m.gapAbove}px above the hint`);
  record(`stop-screen/${lang}/language-followed`, m.lang === (lang === "en" ? "en" : "zh-CN"), m.lang);
}

// ── 3. The click reaches Rust ──────────────────────────────────────────────
// `stopScreen` is true at this point (PAINT left it armed). A pointerdown is
// what the page listens for; the probe line is what proves the invoke left.
const before = probeCount("overlay:dismiss-click");
await evalJs(`(() => {
  window.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
  return "sent";
})()`);
await sleep(400);
record(
  "stop-screen/click-dismisses",
  probeCount("overlay:dismiss-click") === before + 1,
  `${probeCount("overlay:dismiss-click")} dismiss-click probe(s) after ${before}`,
);

// …and a click during the countdown is not a dismissal: the veil is
// click-through then, and the page must agree even if a synthetic event lands.
const armed = await evalJs(`(() => {
  stopScreen = false;
  const n = ${before};
  window.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
  return n;
})()`);
await sleep(400);
record(
  "stop-screen/click-means-nothing-in-a-countdown",
  probeCount("overlay:dismiss-click") === armed + 1,
  `${probeCount("overlay:dismiss-click")} total, expected ${armed + 1}`,
);

// ── 4. take:done is still what arms it ────────────────────────────────────
// Read per handler, not by proximity: the first version searched "within 600
// characters of take:finalize" for the disarm, which made a longer comment in
// that handler look like a regression (2026-10-27, the slot-reservation
// comment). A handler is what it does, wherever in it the line sits.
const overlayJs = readFileSync(path.join(ROOT, "crates", "takekit-gui", "ui", "overlay.js"), "utf8");
const handlerOf = (src, ev) => {
  const at = src.indexOf(`listen("${ev}"`);
  if (at < 0) return "";
  const end = src.indexOf("\n});", at);
  return src.slice(at, end < 0 ? undefined : end);
};
const doneHandler = handlerOf(overlayJs, "take:done");
const finalizeHandler = handlerOf(overlayJs, "take:finalize");
const sourceHandler = handlerOf(overlayJs, "take:source");
const doneArms = doneHandler.includes("stopScreen = true") && doneHandler.includes("foot.hidden = false");
const finalizeDisarms =
  finalizeHandler.includes("stopScreen = false") && !finalizeHandler.includes("stopScreen = true");
record(
  "stop-screen/armed-by-done-only",
  doneArms && finalizeDisarms,
  `doneArms=${doneArms} finalizeDisarms=${finalizeDisarms}`,
);
// The reservation is a contract between three files: finalize renders the slots,
// take:source looks one up by class, and the stylesheet gives an empty row a
// line's height. A renamed class breaks the middle link silently — the rows
// still appear, they just start moving the screen again.
const overlayCss = readFileSync(path.join(ROOT, "crates", "takekit-gui", "ui", "overlay.css"), "utf8");
record(
  "stop-screen/slots-are-a-shared-name",
  finalizeHandler.includes('"row pending"') &&
    sourceHandler.includes('querySelector(".row.pending")') &&
    /#list \.row \{[^}]*min-height/.test(overlayCss),
  `renders=${finalizeHandler.includes('"row pending"')} fills=${sourceHandler.includes('querySelector(".row.pending")')} sized=${/#list \.row \{[^}]*min-height/.test(overlayCss)}`,
);

// ── 5. The checklist fills without moving STOP ────────────────────────────
// The veil centres its children, so a list that grows one row at a time walks
// STOP and the timer upward by half a row per file — one flicker per source,
// which is exactly what the user reported. `take:finalize` carries `total`, so
// the height is known before the first file lands and the rows can fill slots
// instead of adding to them.
//
// This drives the page's own listeners. `event.emit` goes out to Rust and comes
// back as a broadcast, which is the same route the take itself takes — so the
// handlers under test are the ones that run in a real stop, not a copy of them.
const ROWS = 4;
const canEmit = await evalJs(`typeof (window.__TAURI__ && window.__TAURI__.event && window.__TAURI__.event.emit) === "function"`);
let still = {};
if (canEmit) {
  still = await evalJson(
    `(async () => {
      const big = document.getElementById("big");
      const sub = document.getElementById("sub");
      const list = document.getElementById("list");
      const foot = document.getElementById("foot");
      const wait = (ms) => new Promise((r) => setTimeout(r, ms));
      const round = (n) => Math.round(n * 10) / 10;
      const emit = (ev, payload) => window.__TAURI__.event.emit(ev, payload);
      await emit("take:finalize", { dir: "", elapsed: 84, total: ${ROWS} });
      await wait(300);
      const base = { big: big.getBoundingClientRect().top, sub: sub.getBoundingClientRect().top, h: list.getBoundingClientRect().height };
      const drift = [];
      const heights = [];
      const subDrift = [];
      for (let i = 0; i < ${ROWS}; i += 1) {
        await emit("take:source", { id: "s" + i, seconds: 84, bytes: 8388608, width: 1920, height: 1080 });
        await wait(140);
        drift.push(round(big.getBoundingClientRect().top - base.big));
        subDrift.push(round(sub.getBoundingClientRect().top - base.sub));
        heights.push(round(list.getBoundingClientRect().height));
      }
      await emit("take:done", {});
      await wait(150);
      return JSON.stringify({
        drift,
        subDrift,
        heights,
        base: round(base.h),
        rows: list.children.length,
        ids: [...list.children].map((r) => (r.querySelector(".row-id") || {}).textContent || ""),
        stop: big.textContent,
        footShown: !foot.hidden,
      });
    })()`,
    true,
  );
}
const flat = (a, limit) => Array.isArray(a) && a.length === ROWS && a.every((d) => Math.abs(d) <= limit);
record(
  "stop-screen/rows-land-without-moving-stop",
  canEmit && still.rows === ROWS && flat(still.drift, 1) && flat(still.subDrift, 1) && flat(still.heights.map((h) => h - still.base), 1),
  `canEmit=${canEmit} rows=${still.rows} drift=${JSON.stringify(still.drift)} heights=${JSON.stringify(still.heights)} base=${still.base}`,
);
// …and the slots are what got filled, in order, rather than four new rows bolted
// on the end: the ids are the tell.
record(
  "stop-screen/slots-fill-in-order",
  canEmit && JSON.stringify(still.ids) === JSON.stringify(["s0", "s1", "s2", "s3"]),
  `ids=${JSON.stringify(still.ids)}`,
);
record(
  "stop-screen/stop-and-arming-survive-a-real-finalize",
  canEmit && still.stop === "STOP" && still.footShown === true,
  `${still.stop} / foot=${still.footShown}`,
);

// What the reservation looks like half-filled: a four-source take with one file
// landed. Worth a photograph because the reserved rows paint nothing — if they
// ever paint something (a border, a placeholder glyph) this is where it shows.
if (canEmit) {
  await evalJs(
    `(async () => {
      const wait = (ms) => new Promise((r) => setTimeout(r, ms));
      await window.__TAURI__.event.emit("take:finalize", { dir: "", elapsed: 84, total: 4 });
      await wait(250);
      await window.__TAURI__.event.emit("take:source", { id: "win", seconds: 84, bytes: 8388608, width: 1920, height: 1130 });
      await wait(250);
      return "painted";
    })()`,
    true,
  );
  const metrics = await send("Page.getLayoutMetrics");
  const view = metrics.result?.cssLayoutViewport ?? metrics.result?.layoutViewport ?? {};
  const vw = Math.round(view.clientWidth ?? 1200);
  const vh = Math.round(view.clientHeight ?? 700);
  const r = await send("Page.captureScreenshot", {
    format: "png",
    clip: { x: 0, y: 0, width: vw, height: vh, scale: Math.min(1, 1000 / vw) },
    captureBeyondViewport: false,
  });
  if (r.result?.data) {
    writeFileSync(path.join(OUT, "stop-screen-slots.png"), Buffer.from(r.result.data, "base64"));
  }
}

// ── 6. The four columns sit on three vertical lines ───────────────────────
// This is the whole reason the checklist is a grid: the rows used to be flex
// boxes, so each one measured its own columns. A 14-character id pushed that
// row's numbers 30px right, a missing resolution pulled them back left, and the
// result was three rows with three different rhythms — read as noise right when
// the user is checking whether the take came out whole (2026-10-27).
//
// The fixture is built to fail the old layout on purpose: names from 3 to 14
// characters, one audio source with no resolution at all, one value in GB and
// one duration past an hour.
if (canEmit) {
  const aligned = await evalJson(
    `(async () => {
      const list = document.getElementById("list");
      const wait = (ms) => new Promise((r) => setTimeout(r, ms));
      const emit = (ev, payload) => window.__TAURI__.event.emit(ev, payload);
      await emit("take:finalize", { dir: "", elapsed: 84, total: 4 });
      await wait(250);
      const sources = [
        { id: "mic", seconds: 84, bytes: 104857, width: 0, height: 0 },
        { id: "workbuddy", seconds: 84, bytes: 1048576, width: 3440, height: 1392 },
        { id: "windowsterminal", seconds: 3720, bytes: 2147483648, width: 1212, height: 742 },
        { id: "chrome", seconds: 0, bytes: 0, width: 0, height: 0, error: "encoder stopped (0x80070005)" },
      ];
      for (const s of sources) {
        await emit("take:source", s);
        await wait(140);
      }
      await emit("take:done", {});
      await wait(150);
      const rows = [...list.querySelectorAll(".row")].filter((r) => r.querySelector(".row-id"));
      const edges = rows.map((r) =>
        [...r.children].map((c) => {
          const b = c.getBoundingClientRect();
          return [Math.round(b.left * 10) / 10, Math.round(b.right * 10) / 10];
        }),
      );
      // A right-aligned number is pinned by its RIGHT edge; the place a string
      // starts is a property of the string, not of the column (measured the
      // wrong way first and got 192px of "misalignment" out of a layout that
      // was already correct). Names are pinned by the left edge, and the failure
      // row has no numbers at all, so it is measured on its own.
      const spread = (vals) =>
        vals.length && vals.every((v) => typeof v === "number")
          ? Math.round((Math.max(...vals) - Math.min(...vals)) * 10) / 10
          : null;
      const full = edges.filter((e) => e.length === 4);
      const perCol = [1, 2, 3].map((i) => spread(full.map((e) => e[i][1])));
      const colSpread = perCol.every((v) => v !== null) ? Math.max(...perCol) : null;
      const nameLeft = spread(edges.map((e) => e[0] && e[0][0]));
      const cells = rows.map((r) => [...r.children].map((c) => c.textContent));
      const audio = cells.find((c) => c[0] === "mic") || [];
      const long = cells.find((c) => c[0] === "windowsterminal") || [];
      const failed = cells.find((c) => c[0] === "chrome") || [];
      const listBox = list.getBoundingClientRect();
      return JSON.stringify({
        rows: rows.length,
        nameLeft,
        colSpread,
        perCol,
        cells,
        audioRes: audio[2],
        longTime: long[1],
        longSize: long[3],
        failedText: failed[1] || "",
        failedCells: failed.length,
        // The name column gives up characters, never alignment.
        inside: rows.every((r) => r.getBoundingClientRect().right <= listBox.right + 0.5),
      });
    })()`,
    true,
  );

  record(
    "stop-screen/columns-line-up",
    aligned.rows === 4 &&
      aligned.inside === true &&
      // `typeof`, not just `<=`: an empty measurement serialises to null, and
      // `null <= 0.5` is true — an empty list would pass its own alignment test.
      typeof aligned.colSpread === "number" &&
      aligned.colSpread <= 0.5 &&
      typeof aligned.nameLeft === "number" &&
      aligned.nameLeft <= 0.5,
    `rows=${aligned.rows} inside=${aligned.inside} names ${aligned.nameLeft}px apart, numbers ${aligned.colSpread}px apart per column ${JSON.stringify(aligned.perCol)}`,
  );
  // Every cell says something. A blank resolution was what pulled its row's
  // size column left; the dash is the fix, and the formats are the contract —
  // `shortClock` (shared.js) everywhere, so `m:ss` and `h:mm:ss` past an hour.
  record(
    "stop-screen/every-cell-says-something",
    aligned.audioRes === "—" && aligned.longTime === "1:02:00" && aligned.longSize === "2.0 GB",
    `audio-res="${aligned.audioRes}" 3720s="${aligned.longTime}" 2GiB="${aligned.longSize}"`,
  );
  // A failed source has no numbers, so the reason spans the row — and it keeps
  // the cross with it instead of leaving it on a column of its own.
  record(
    "stop-screen/failure-takes-the-row",
    aligned.failedCells === 2 && /^✗ /.test(aligned.failedText),
    `${aligned.failedCells} cell(s): "${String(aligned.failedText).slice(0, 40)}"`,
  );

  // The photograph: the same four rows, one of each shape.
  const metrics = await send("Page.getLayoutMetrics");
  const view = metrics.result?.cssLayoutViewport ?? metrics.result?.layoutViewport ?? {};
  const vw = Math.round(view.clientWidth ?? 1200);
  const vh = Math.round(view.clientHeight ?? 700);
  const r = await send("Page.captureScreenshot", {
    format: "png",
    clip: { x: 0, y: 0, width: vw, height: vh, scale: Math.min(1, 1000 / vw) },
    captureBeyondViewport: false,
  });
  if (r.result?.data) {
    writeFileSync(path.join(OUT, "stop-screen-columns.png"), Buffer.from(r.result.data, "base64"));
  }
}

ws.close();
const fails = results.filter((r) => !r.ok);
say(`=== ${results.length - fails.length}/${results.length} checks clean ===`);
say(`screenshots: ${OUT}`);
process.exit(fails.length ? 1 : 0);
