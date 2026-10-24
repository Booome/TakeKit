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

const ROOT = path.resolve(import.meta.dirname, "..");
const GUI = path.join(ROOT, "target", "debug", "takekit-gui.exe");
const TK = path.join(ROOT, "target", "debug", "takekit.exe");
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
try { rmSync(OUT, { recursive: true, force: true }); } catch {}
mkdirSync(appDir, { recursive: true });

// ── the fixture window ─────────────────────────────────────────────────────
// Visible, not minimized: frames have to flow for the take to have content,
// and a minimized window records a still (the matrix's case, not this one).
const PROBE_PS1 = path.join(OUT, "probe.ps1");
const PROBE_HANDLE = path.join(OUT, "probe-handle.txt");
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

let control = null;
const controlFile = path.join(appDir, "control.json");
for (let i = 0; i < 60; i++) {
  await sleep(500);
  if (existsSync(controlFile)) {
    try { control = JSON.parse(readFileSync(controlFile, "utf8")); if (control.port) break; } catch {}
  }
}
record("panel-start", Boolean(control?.port), `control port ${control?.port}`);
if (!control?.port) process.exit(1);
const CTL = `http://127.0.0.1:${control.port}`;
const AUTH = { Authorization: `Bearer ${control.token}` };

let list = null;
for (let i = 0; i < 50 && !list; i += 1) {
  try { list = JSON.parse(await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).text()); } catch { await sleep(200); }
}
const target = list?.find((t) => t.type === "page" && /tauri\.localhost\/$/.test(t.url || "")) ||
  list?.find((t) => t.type === "page");
record("cdp-target", Boolean(target), target?.url);
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
}

const failed = results.filter((r) => !r.ok);
say(`\n${results.length - failed.length}/${results.length} passed${failed.length ? ` — FAILED: ${failed.map((f) => f.name).join(", ")}` : ""}`);
for (const f of fixtures) { try { f.proc.kill(); } catch {} }
try { execFileSync("taskkill", ["/F", "/IM", "takekit-gui.exe"], { stdio: "ignore" }); } catch {}
process.exit(failed.length ? 1 : 0);
