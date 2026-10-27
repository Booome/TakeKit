// The mid-take door, driven for real.
//
// `check-ui-matrix` sets `state.recording = true` in the page and then clicks
// 添加源. That proves the picker renders; it proves nothing about the path a
// user takes, and on 2026-10-24 that gap shipped a panel whose picker was
// locked during a take while 151 screenshots stayed green. This runs the real
// thing end to end: a real take, started from *outside* the panel (the control
// surface — the same thing a CLI or an MCP agent does), then the real button,
// then a real pick, then a real stop.
//
// It records nothing of the user's: the only source is a window this script
// creates and owns (a solid green form), which is also the fixture the matrix
// uses for a window that has a size and no picture.
//
// Usage:  cargo build --workspace && node scripts/check-midtake.mjs
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import path from "node:path";
import { assertNoPanel, startPanel } from "./lib/panel-guard.mjs";
import { pickBinary } from "./lib/build.mjs";

const ROOT = path.resolve(import.meta.dirname, "..");
const GUI = pickBinary(ROOT, "takekit-gui.exe", "TK_GUI");
const TK = pickBinary(ROOT, "takekit.exe", "TK_CLI");
const OUT = path.join(ROOT, "target", "checks", "midtake");
const APPDATA = path.join(OUT, "appdata");
const appDir = path.join(APPDATA, "TakeKit");
const CDP_PORT = 9241;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const say = (m) => console.log(`[${new Date().toISOString().slice(11, 19)}] ${m}`);
const results = [];
function record(name, ok, detail) {
  results.push({ name, ok, detail });
  say(`${ok ? "PASS" : "FAIL"}  ${name}  ${detail || ""}`);
}

for (const bin of [GUI, TK]) {
  if (!existsSync(bin)) {
    console.error(`missing ${bin} — run: cargo build --workspace`);
    process.exit(2);
  }
}
// A stale tree here is not cosmetic: control.json from a previous instance
// points at a port nobody is listening on, and this script then spends a
// minute reporting on a window it never started (2026-10-27 — the failures read
// like a broken `take::begin` and were neither).
//
// Only the two files that cause it are deleted, because the whole tree cannot
// be: the host's bulk-delete guard refuses anything over 50 files
// (`SAFE_DELETE_BULK_CONFIRM_REQUIRED`, measured at 194 here), so a full wipe
// fails on every run after the first — and a fatal error on a *cleanup* step is
// a check that can never pass twice. `probe-handle.txt` is fatal for its own
// reason: an old HWND is not a window, so the seeded source renders dead and
// the mid-take assertions describe the wrong screen. Recordings from earlier
// runs and `wv2/` are left to be overwritten and reused.
for (const stale of [path.join(appDir, "control.json")]) {
  try {
    rmSync(stale, { force: true });
  } catch (e) {
    console.error(`could not remove the stale ${stale}: ${e.message}`);
    process.exit(2);
  }
  if (existsSync(stale)) {
    console.error(`a stale ${stale} survived — is a previous run still up?`);
    process.exit(2);
  }
}
mkdirSync(appDir, { recursive: true });

// The probe log appends across runs, so a line from an earlier run would pass
// this run's assertion for it. Everything read from it is taken after
// `probeFrom`, the length it had before this panel started.
const probeLog = path.join(OUT, "probe.log");
const probeFrom = (() => {
  try {
    return readFileSync(probeLog).length;
  } catch {
    return 0;
  }
})();
const probeSinceStart = () => {
  try {
    return readFileSync(probeLog, "utf8").slice(probeFrom);
  } catch {
    return "";
  }
};

// ── the fixture window ─────────────────────────────────────────────────────
// Visible, not minimized: frames have to flow for the take to have content,
// and a minimized window records a still (the matrix's case, not this one).
const PROBE_PS1 = path.join(OUT, "probe.ps1");
// The handle file cannot have a fixed name. The read loop below polls every
// 200 ms while the fixture takes about a second to start, so a leftover from an
// earlier run gets read *first*, long before the new window writes its own —
// and the script then seeds a source pointing at a dead handle. What that looks
// like is `check_sources` silently dropping it, `/record/start` answering
// `no-sources`, and a run that reads like the mid-take door is broken. Measured
// on 2026-10-27: the log printed hwnd 131668896 while the file ended up holding
// 12191704 — the value it read was two runs old. A nonce cannot be stale.
const PROBE_HANDLE = path.join(OUT, `probe-handle-${process.pid}-${Date.now()}.txt`);
writeFileSync(PROBE_PS1, [
  "param([string]$HandleFile, [string]$Caption)",
  "Add-Type -AssemblyName System.Windows.Forms",
  "Add-Type -AssemblyName System.Drawing",
  "$f = New-Object System.Windows.Forms.Form",
  "$f.Text = $Caption",
  "$f.Width = 480",
  "$f.Height = 320",
  "$f.BackColor = [System.Drawing.Color]::FromArgb(0, 200, 120)",
  "$f.Show()",
  "[System.Windows.Forms.Application]::DoEvents()",
  'Set-Content -Path $HandleFile -Value "$($f.Handle)"',
  "$end = (Get-Date).AddSeconds(300)",
  "while ((Get-Date) -lt $end) {",
  "  [System.Windows.Forms.Application]::DoEvents()",
  "  Start-Sleep -Milliseconds 100",
  "}",
].join("\n"), "utf8");
// Two windows: one is the take's first source, the other is what the picker
// picks. Both belong to this script, so a take that records them films nothing
// of the user's — the reason the matrix never starts a real take at all.
const fixtures = [];
for (const [i, caption] of ["TakeKit midtake fixture", "TakeKit midtake pick"].entries()) {
  const handleFile = `${PROBE_HANDLE}.${i}`;
  const script = `${PROBE_PS1}.${i}.ps1`;
  writeFileSync(script, readFileSync(PROBE_PS1, "utf8"), "utf8");
  const proc = spawn("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script, "-HandleFile", handleFile, "-Caption", caption], { stdio: "ignore" });
  let hwnd = 0;
  for (let w = 0; w < 50 && !hwnd; w++) {
    await sleep(200);
    try { hwnd = Number(readFileSync(handleFile, "utf8").trim()) || 0; } catch {}
  }
  if (!hwnd) { console.error(`the fixture window "${caption}" never appeared`); process.exit(2); }
  fixtures.push({ hwnd, pid: proc.pid, caption, proc });
  say(`fixture "${caption}" hwnd ${hwnd}`);
}
const probeHwnd = fixtures[0].hwnd;

// ── a session that starts from one source ──────────────────────────────────
writeFileSync(path.join(appDir, "config.json"), JSON.stringify({
  sources: [{
    id: "fix",
    capture: { Window: { hwnd: probeHwnd, pid: fixtures[0].pid, title: "TakeKit midtake fixture", crop: { top: 0, bottom: 0, left: 0, right: 0 } } },
    fps: 30, quality: null, audio: false,
  }],
  out_root: path.join(OUT, "takes"),
  backend: "windows",
  marks: false,
  language: "zh",
  theme: "light",
  countdown_secs: 0,
}));

await assertNoPanel();

let guiPid = 0;
const guiLaunch = () => {
  const child = spawn(GUI, [], {
    env: {
      ...process.env,
      APPDATA,
      TAKEKIT_WEBVIEW_NO_SANDBOX: "1",
      // The probe log is what tells "the Rust watch stopped it" apart from
      // "the capture gave up on its own" — `alive: false` is the same either
      // way, and only one of them is the behaviour this check is about.
      TAKEKIT_PROBE_LOG: probeLog,
      WEBVIEW2_USER_DATA_FOLDER: path.join(OUT, "wv2"),
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
    },
    detached: true,
    stdio: "ignore",
  });
  child.unref();
  guiPid = child.pid;
  return child;
};
// Killed on the way out, like the other checks: a panel left behind keeps its
// control.json, and the next run then posts its requests into the old port.
process.on("exit", () => {
  try { execFileSync("taskkill", ["/PID", String(guiPid), "/T", "/F"], { stdio: "ignore" }); } catch {}
});

// Retried: this runs right after other checks that kill their panel, and a
// panel that cannot start says nothing about adding a source (see `startPanel`).
const boot = await startPanel(guiLaunch, appDir);
const control = boot.control;
// The pid is half the check: a control port that is not this process's means
// every request below lands somewhere else, and every assertion then reports on
// a window that was never opened.
record(
  "panel-start",
  Boolean(control?.port) && control.pid === guiPid,
  `control port ${control?.port} pid ${control?.pid} (ours ${guiPid})`,
);
if (!control?.port || control.pid !== guiPid) process.exit(1);
const CTL = `http://127.0.0.1:${control.port}`;
const AUTH = { Authorization: `Bearer ${control.token}` };

// Wait for the panel's own document, by name — not for the port, and not for
// "any page target". The veil window is created before the panel, so during
// start-up its target still reads about:blank, and a fallback that takes the
// first page in the list drives the wrong window and reports a panel that
// answers null to everything (2026-10-27).
let list = null;
let target = null;
for (let i = 0; i < 60 && !target; i += 1) {
  try {
    list = JSON.parse(await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).text());
    target = list.find((t) => t.type === "page" && /tauri\.localhost\/(index\.html)?$/.test(t.url || ""));
  } catch {
    list = null;
  }
  if (!target) await sleep(250);
}
record("cdp-target", Boolean(target), target?.url ?? list?.map((t) => t.url).join(", "));
if (!target) process.exit(1);

const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let id = 1;
const waits = new Map();
ws.onmessage = (e) => { const m = JSON.parse(e.data); const w = waits.get(m.id); if (w) { waits.delete(m.id); w(m); } };
const send = (method, params = {}, timeoutMs = 15000) => new Promise((res) => {
  const i = id++;
  const timer = setTimeout(() => { waits.delete(i); res({ __timeout: true }); }, timeoutMs);
  waits.set(i, (m) => { clearTimeout(timer); res(m); });
  ws.send(JSON.stringify({ id: i, method, params }));
});
const js = async (expression) => {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.__timeout) return null;
  if (r.result?.exceptionDetails) return null;
  return r.result?.result?.value;
};

await sleep(1500);

// ── the take, started the way an agent starts it ───────────────────────────
const started = await fetch(`${CTL}/record/start`, { method: "POST", headers: AUTH, body: "{}" })
  .then((r) => r.json()).catch(() => null);
record("take-started-outside-the-panel", started?.started === true, JSON.stringify(started));

// The panel was not the caller, so this is the whole question: does it notice?
let adopted = false;
for (let i = 0; i < 40 && !adopted; i++) {
  await sleep(500);
  adopted = (await js("(() => state.recording)()")) === true;
}
record("panel-adopts-a-take-it-did-not-start", adopted, `state.recording=${adopted}`);

const bar = await js(`document.getElementById("rec-btn").textContent.trim()`);
record("bar-says-stop", bar === "停止", `"${bar}"`);

const chips = await js(`JSON.stringify([...document.querySelectorAll("#source-list .row")].map((r) => ({
  name: r.querySelector(".row-name")?.textContent,
  start: r.querySelector(".row-start")?.textContent,
})))`);
const rows = JSON.parse(chips || "[]");
record("every-row-shows-its-offset", rows.length > 0 && rows.every((r) => r.start && r.start !== "—"),
  rows.map((r) => `${r.name}=${r.start}`).join(" | "));

// ── the door ───────────────────────────────────────────────────────────────
const opened = await js(`(() => {
  document.getElementById("add-btn").click();
  const p = document.getElementById("picker");
  return JSON.stringify({
    open: !p.classList.contains("hidden"),
    picks: p.querySelectorAll(".pick").length,
    enabled: [...p.querySelectorAll(".pick")].filter((b) => !b.disabled).length,
  });
})()`);
const picker = JSON.parse(opened || "{}");
record("add-source-opens-mid-take", picker.open === true, `open=${picker.open} picks=${picker.picks}`);
record("picker-offers-something-to-pick", (picker.enabled ?? 0) > 0, `enabled=${picker.enabled}`);

// Kept for the eyeball pass: this screen only exists mid-take, so nothing else
// in the repo shows a list over a running take.
{
  const shot = await send("Page.captureScreenshot", { format: "png" }, 25000);
  if (shot.result?.data) writeFileSync(path.join(OUT, "picker-open.png"), Buffer.from(shot.result.data, "base64"));
}

// ── a pick that actually joins the take ────────────────────────────────────
const picked = await js(`(() => {
  const all = [...document.querySelectorAll("#picker .pick")].filter((x) => !x.disabled);
  const b = all.find((x) => /midtake pick/.test(x.querySelector(".name")?.textContent || "")) || all[0];
  const before = state.sources.length;
  b.click();
  return JSON.stringify({ name: b.querySelector(".name")?.textContent, before });
})()`);
const pick = JSON.parse(picked || "{}");
let joined = 0;
for (let i = 0; i < 30 && joined < 2; i++) {
  await sleep(500);
  joined = Number(await js(`window.__TAURI__.core.invoke("status").then((s) => s.sources.length).catch(() => 0)`)) || 0;
}
record("the-pick-joins-the-running-take", joined === 2, `${pick.name}: ${pick.before} → live ${joined}`);
const secondChip = await js(`(() => {
  const rows = [...document.querySelectorAll("#source-list .row")];
  const last = rows[rows.length - 1];
  return JSON.stringify({ n: rows.length, start: last?.querySelector(".row-start")?.textContent });
})()`);
const after = JSON.parse(secondChip || "{}");
record("the-new-row-gets-an-offset", after.n === 2 && Boolean(after.start), JSON.stringify(after));

// ── a source that goes away is stopped by Rust, not by the panel ───────────
//
// The panel hides itself for the whole take (it would otherwise be in the
// recording) and Chromium throttles — then freezes — a hidden window's timers,
// which is why the health check cannot live there: a window closed at 0:05
// would keep having the desktop behind it recorded until 1:05, and nothing
// about the resulting clip would say it was wrong.
//
// Reproducing that needs the panel genuinely out of the loop, which takes two
// things, and skipping either one makes this assertion pass with the Rust watch
// deleted:
//
//   * the page is **frozen** (`Page.setWebLifecycleState`), because attaching
//     DevTools is itself what keeps a hidden page's timers running — the panel's
//     own 250 ms tick would otherwise win the race every time;
//   * the polling below goes through the **control port**, because the page is
//     now frozen.
//
// `alive: false` on its own proves nothing (a capture whose window vanished can
// also give up by itself); the `source:vanished` probe line is the one only the
// watch writes.
const liveBefore =
  (await fetch(`${CTL}/state`, { headers: AUTH }).then((r) => r.json()).catch(() => null))?.status
    ?.live?.map((x) => x.id) ?? [];
await send("Page.setWebLifecycleState", { state: "frozen" });
try {
  fixtures[1].proc.kill();
} catch {}
let died = null;
for (let i = 0; i < 16 && !died; i += 1) {
  await sleep(250);
  const live =
    (await fetch(`${CTL}/state`, { headers: AUTH }).then((r) => r.json()).catch(() => null))?.status
      ?.live ?? [];
  const gone = live.find((x) => !x.alive);
  if (gone) died = gone.id;
}
let saidSo = false;
try {
  saidSo = /source:vanished/.test(probeSinceStart());
} catch {}
record(
  "a-window-that-closes-is-stopped-without-the-panel",
  Boolean(died) && saidSo,
  `live ${JSON.stringify(liveBefore)} → stopped ${died || "none within 4s"}, probe-source:vanished=${saidSo}`,
);

const shot = await send("Page.captureScreenshot", { format: "png" }, 25000);
if (shot.result?.data) writeFileSync(path.join(OUT, "mid-take.png"), Buffer.from(shot.result.data, "base64"));

// ── and it all lands in the manifest ───────────────────────────────────────
const stopped = await fetch(`${CTL}/record/stop`, { method: "POST", headers: AUTH, body: "{}" })
  .then((r) => r.json()).catch(() => null);
record("take-stopped", Boolean(stopped?.dir), stopped?.dir || JSON.stringify(stopped).slice(0, 120));
if (stopped?.dir) {
  let manifest = null;
  for (let i = 0; i < 30 && !manifest; i++) {
    await sleep(300);
    try { manifest = JSON.parse(readFileSync(path.join(stopped.dir, "take.json"), "utf8")); } catch {}
  }
  const withStart = (manifest?.sources || []).filter((s) => typeof s.start === "number");
  record("manifest-records-both-starts", (manifest?.sources || []).length === 2 && withStart.length === 2,
    (manifest?.sources || []).map((s) => `${s.id}@${s.start}`).join(" | "));
  const late = (manifest?.sources || []).find((s) => s.start > 0);
  record("the-added-source-starts-later", Boolean(late), late ? `${late.id} at ${late.start}s` : "none has start > 0");
  // A take that was never paused has one file per source and no `-part` in any
  // name: the number is only there to line up a split, so it must not appear
  // where there is nothing to line up (2026-10-27).
  const names = (manifest?.sources || []).flatMap((s) => [s.file, ...(s.segments || []).map((g) => g.file)]);
  record("an-uncut-source-keeps-its-plain-name", names.length > 0 && names.every((n) => !/-part\d/.test(n)),
    names.join(" ") || "no names");
}

const failed = results.filter((r) => !r.ok);
say(`\n${results.length - failed.length}/${results.length} passed${failed.length ? ` — FAILED: ${failed.map((f) => f.name).join(", ")}` : ""}`);
for (const f of fixtures) { try { f.proc.kill(); } catch {} }
try { execFileSync("taskkill", ["/F", "/IM", "takekit-gui.exe"], { stdio: "ignore" }); } catch {}
process.exit(failed.length ? 1 : 0);
