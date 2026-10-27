// The tray popover, measured in its own window.
//
// Nothing in check-ui-matrix.mjs can see this: the popover is a second window
// with its own document, it is not open while the panel is on screen, and it
// only opens on a click on the tray icon. Which is how it shipped with its
// second row of keys half cut off by the taskbar, and then — after the flex
// rules that were meant to split the keys into two rows did the opposite — with
// four keys squeezed onto one line and their labels wrapped inside 50px.
//
// So this drives it directly: TAKEKIT_TRAY_DEMO opens the popover at startup,
// CDP measures the cards in the real window, and every language × theme is
// checked against the same rules:
//
//   1. four keys, two rows, two per row;
//   2. no key outside the card, no label outside its key;
//   3. the card needs no scroll — the window was sized to it, so content that
//      does not fit is clipped, not scrolled;
//   4. the card's bottom padding survives (a cut-off card loses it first).
//
// Usage:  cargo build --workspace && node scripts/check-tray-layout.mjs
// Close any running panel first (a second instance exits on sight).
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import path from "node:path";
import { assertNoPanel } from "./lib/panel-guard.mjs";
import { pickBinary } from "./lib/build.mjs";

const ROOT = path.resolve(import.meta.dirname, "..");
// `TK_GUI` points the check at another build — the release binary is the one
// the user runs, so "green on debug" is not the same claim as "green on what
// ships".
const GUI = pickBinary(ROOT, "takekit-gui.exe", "TK_GUI");
const OUT = path.join(ROOT, "target", "checks", "tray-layout");
const APPDATA = path.join(OUT, "appdata");
const appDir = path.join(APPDATA, "TakeKit");
const CDP_PORT = 9241;
/// What the card's own padding should leave under the last row of keys.
const BOTTOM_PAD = 8;

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

// One instance per app id: ours would hand its argv to the running one and
// exit, and the run would report on a window it never opened. That check lives
// in `assertNoPanel`, called right before the spawn below.

// Only the files that would poison the run are deleted, and the rest of the
// tree is left alone: a whole-tree delete is what the host's bulk-delete guard
// refuses once the WebView2 profile is in the way (see check-ui-matrix,
// 2026-10-27). Screenshots are overwritten anyway.
for (const stale of [path.join(appDir, "control.json")]) {
  try {
    rmSync(stale, { force: true });
  } catch (e) {
    console.error(`could not remove the stale ${stale}: ${e.message}`);
    process.exit(2);
  }
}
mkdirSync(appDir, { recursive: true });

// The idle popover is the state that was reported, and it needs no session: no
// sources, one line of text, both rows of keys.
writeFileSync(
  path.join(appDir, "config.json"),
  JSON.stringify(
    {
      hotkey: "F9",
      pause_hotkey: "F10",
      countdown: 0,
      finalize_panel: false,
      language: "zh",
      theme: "light",
      sound: true,
    },
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
    // Opens the popover 1.5s after the panel is up; see main.rs.
    TAKEKIT_TRAY_DEMO: "1",
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
// The debug port answers long before the page targets do — the first list is
// `about:blank` and nothing else — so this waits for the tray document by name
// rather than for the port.
let list = null;
let target = null;
for (let i = 0; i < 80 && !target; i += 1) {
  try {
    const resp = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
    list = JSON.parse(await resp.text());
    target = list.find((t) => /tray\.html/.test(t.url || ""));
  } catch {
    list = null;
  }
  if (!target) await sleep(250);
}
if (!list) {
  record("cdp-port", false, `no debug port on ${CDP_PORT} after 20s (WebView2 did not start)`);
  process.exit(1);
}
if (!target) {
  record("cdp-target/tray", false, `no tray target; saw ${list.map((t) => t.url).join(", ")}`);
  process.exit(1);
}
record("cdp-target/tray", true, target.url);

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
  if (r.__timeout) return { __timeout: true };
  if (r.result?.exceptionDetails) return { __error: r.result.exceptionDetails.text };
  return r.result?.result?.value;
};
const evalJson = async (expression, fallback = {}) => {
  const raw = await evalJs(expression);
  try {
    const v = JSON.parse(raw);
    return v ?? fallback;
  } catch {
    return fallback;
  }
};

// ── What the popover is, in numbers ────────────────────────────────────────
// Everything is read from the live window: a card that is merely "not clipped
// in the screenshot" is not the same as one whose keys are all inside it.
const MEASURE = `JSON.stringify((() => {
  const card = document.querySelector(".card");
  if (!card) return { error: "no card" };
  const cr = card.getBoundingClientRect();
  const keys = [...document.querySelectorAll(".acts button")];
  const boxes = keys.map((b) => {
    const r = b.getBoundingClientRect();
    return {
      id: b.id,
      text: (b.textContent || "").trim(),
      y: Math.round(r.top),
      w: Math.round(r.width),
      h: Math.round(r.height),
      // A label that does not fit its own key.
      overflows: b.scrollWidth > b.clientWidth + 1 || b.scrollHeight > b.clientHeight + 1,
      // A key the card (and therefore the window) cut off.
      outside: r.bottom > cr.bottom + 0.5 || r.right > cr.right + 0.5 ||
               r.top < cr.top - 0.5 || r.left < cr.left - 0.5,
    };
  });
  const tops = [...new Set(boxes.map((b) => b.y))].sort((a, b) => a - b);
  const bottom = boxes.length ? Math.max(...boxes.map((b) => b.y + b.h)) : 0;
  return {
    visible: document.visibilityState === "visible",
    keys: boxes.length,
    rows: tops.length,
    perRow: tops.map((t) => boxes.filter((b) => b.y === t).length),
    keyWidths: boxes.map((b) => b.w),
    labels: boxes.map((b) => b.text),
    overflows: boxes.filter((b) => b.overflows).map((b) => b.id),
    outside: boxes.filter((b) => b.outside).map((b) => b.id),
    // The card scrolls when the window is smaller than its content — the
    // window has no scrollbar of its own, so this is the clipped first.
    spills: card.scrollHeight > card.clientHeight + 1 || card.scrollWidth > card.clientWidth + 1,
    hScroll: document.documentElement.scrollWidth > innerWidth + 1,
    windowW: innerWidth,
    windowH: innerHeight,
    gapUnderKeys: Math.round(cr.bottom - bottom),
  };
})())`;

// Visible, and settled: the page measures itself on every tick, and a
// measurement taken while the window is still growing reports the old window.
async function settled() {
  let last = {};
  for (let i = 0; i < 40; i += 1) {
    const now = await evalJson(MEASURE);
    if (now.visible && now.keys === 4 && !now.error) {
      if (last.windowH === now.windowH && last.gapUnderKeys === now.gapUnderKeys) return now;
      last = now;
    }
    await sleep(250);
  }
  return last;
}

const shot = async (name) => {
  const r = await send("Page.captureScreenshot", { format: "png" });
  if (!r.result?.data) return;
  writeFileSync(path.join(OUT, `${name}.png`), Buffer.from(r.result.data, "base64"));
};

const first = await settled();
if (!first.visible || first.error) {
  record("tray/opens", false, `popover not visible after 10s (probe: ${readProbe()})`);
  process.exit(1);
}

// ── Every language × theme ─────────────────────────────────────────────────
// Long labels are the whole risk here: the two rows have to survive "Show
// panel" as well as "显示主窗口", and the card has to grow with them.
const COMBOS = [
  ["zh", "light"],
  ["zh", "dark"],
  ["en", "light"],
  ["en", "dark"],
];

for (const [lang, theme] of COMBOS) {
  const switched = await evalJs(
    `(async () => {
       await window.__TAURI__.core.invoke("set_language", { language: "${lang}" });
       await window.__TAURI__.core.invoke("set_theme", { theme: "${theme}" });
       return "ok";
     })()`,
    true,
  );
  if (switched !== "ok") {
    record(`tray/${lang}-${theme}`, false, `could not switch: ${JSON.stringify(switched)}`);
    continue;
  }
  // Two settled reads: the switch re-renders the labels, the page re-measures,
  // and the window grows or shrinks to match.
  await sleep(400);
  const m = await settled();

  const layoutOk = m.rows === 2 && m.perRow.length === 2 && m.perRow.every((n) => n === 2);
  record(
    `tray/${lang}-${theme}/two-rows-of-two`,
    layoutOk,
    `${m.rows} row(s) ${JSON.stringify(m.perRow)} widths ${JSON.stringify(m.keyWidths)}`,
  );
  record(
    `tray/${lang}-${theme}/nothing-clipped`,
    m.overflows.length === 0 && m.outside.length === 0 && !m.spills && !m.hScroll,
    `labelsOut=${JSON.stringify(m.overflows)} keysOut=${JSON.stringify(m.outside)} ` +
      `cardSpills=${m.spills} hScroll=${m.hScroll}`,
  );
  // A card cut off by the window loses its bottom padding first — the same
  // symptom as the screenshot that started this: the keys looked complete and
  // the window ended through them.
  record(
    `tray/${lang}-${theme}/bottom-padding`,
    m.gapUnderKeys >= BOTTOM_PAD,
    `${m.gapUnderKeys}px under the keys in a ${m.windowW}×${m.windowH} window`,
  );
  await shot(`${lang}-${theme}`);
}

ws.close();
const fails = results.filter((r) => !r.ok);
say(`=== ${results.length - fails.length}/${results.length} checks clean ===`);
say(`screenshots: ${OUT}`);
process.exit(fails.length ? 1 : 0);

function readProbe() {
  try {
    return readFileSync(probeLog, "utf8").trim().split("\n").slice(-6).join(" | ");
  } catch {
    return "no probe log";
  }
}
