// Pause a take of a window that keeps repainting — the one case the writer's
// frame loop gets wrong, and the one no screenshot can see.
//
// The capture never stops during a pause; a window that keeps painting keeps
// delivering frames stamped past the freeze. Writing those opened a fresh file
// per frame and, with nothing open, reopened the take's *first* file over
// itself: a 5-second pause left sixteen zero-second segments and a first file
// holding one frame, and the panel said `1 个文件的实测长度与 take.json 不符`
// (measured 2026-10-27, `workbuddy-part2 … part17`).
//
// The fixture is a form that changes colour every 30 ms. A still window is the
// reason this hid for so long: with nothing repainting, no frame arrives during
// the pause and the bug never fires.
//
// One caveat, measured 2026-10-27: this fixture does *not* reproduce the frame
// storm by itself. WGC still delivers nothing for it during a pause — with the
// fix reverted this script stayed 7/7 — so what it holds down is the rest of
// the pause path (one file per cut, every file matching its record, every
// recorded file on disk), not the storm. The decisive test for the storm is
// `frames_that_arrive_during_a_pause_are_dropped` in takekit-media, which hands
// the writer the frames directly: with the guard removed it reports twelve
// segments for one pause instead of two.
//
// Usage:  cargo build --workspace && node scripts/check-pause.mjs
//         TK_GUI=target/release/takekit-gui.exe node scripts/check-pause.mjs
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync, readdirSync } from "node:fs";
import path from "node:path";
import { assertNoPanel, startPanel } from "./lib/panel-guard.mjs";
import { pickBinary } from "./lib/build.mjs";

const ROOT = path.resolve(import.meta.dirname, "..");
const GUI = pickBinary(ROOT, "takekit-gui.exe", "TK_GUI");
const OUT = path.join(ROOT, "target", "checks", "pause");
const APPDATA = path.join(OUT, "appdata");
const appDir = path.join(APPDATA, "TakeKit");
// A frame and a half, the same tolerance the manifest is judged by
// (`takekit_core::take::length_mismatch`); anything past it is the panel's own
// warning, so this would be asserting against itself.
const TOLERANCE = 0.15;
/// The popover is checked over CDP, like the stop screen: it is its own
/// document and nothing else can see it.
const CDP_PORT = 9251;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const say = (m) => console.log(`[${new Date().toISOString().slice(11, 19)}] ${m}`);
const results = [];
function record(name, ok, detail) {
  results.push({ name, ok, detail });
  say(`${ok ? "PASS" : "FAIL"}  ${name}  ${detail || ""}`);
}

if (!existsSync(GUI)) {
  console.error(`missing ${GUI} — run: cargo build --workspace`);
  process.exit(2);
}
mkdirSync(appDir, { recursive: true });
rmSync(path.join(appDir, "control.json"), { force: true });

// ── the fixture: a window that will not sit still ─────────────────────────
const ps1 = path.join(OUT, "fixture.ps1");
writeFileSync(
  ps1,
  [
    "param([string]$HandleFile, [string]$Caption)",
    "Add-Type -AssemblyName System.Windows.Forms",
    "Add-Type -AssemblyName System.Drawing",
    "$f = New-Object System.Windows.Forms.Form",
    "$f.Text = $Caption",
    "$f.Width = 480",
    "$f.Height = 320",
    "$f.BackColor = [System.Drawing.Color]::FromArgb(0, 200, 120)",
    // On top, or the panel (or anything else) covers it — and a fully covered
    // window is one WGC stops delivering frames for, which is exactly the
    // condition that made this bug look untriggerable.
    "$f.TopMost = $true",
    "$f.Show()",
    "[System.Windows.Forms.Application]::DoEvents()",
    'Set-Content -Path $HandleFile -Value "$($f.Handle)"',
    "$i = 0",
    "$end = (Get-Date).AddSeconds(120)",
    "while ((Get-Date) -lt $end) {",
    "  $i++",
    "  $f.BackColor = [System.Drawing.Color]::FromArgb(($i * 7) % 255, 200, 120)",
    "  [System.Windows.Forms.Application]::DoEvents()",
    "  Start-Sleep -Milliseconds 30",
    "}",
  ].join("\n"),
  "utf8",
);

const handleFile = path.join(OUT, `handle-${process.pid}-${Date.now()}.txt`);
const fixture = spawn(
  "powershell",
  ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ps1, "-HandleFile", handleFile, "-Caption", "TakeKit pause fixture"],
  { stdio: "ignore" },
);
let hwnd = 0;
for (let i = 0; i < 60 && !hwnd; i += 1) {
  await sleep(200);
  try {
    hwnd = Number(readFileSync(handleFile, "utf8").trim()) || 0;
  } catch {}
}
if (!hwnd) {
  console.error("the fixture window never appeared");
  process.exit(2);
}

writeFileSync(
  path.join(appDir, "config.json"),
  JSON.stringify({
    sources: [
      {
        id: "fixture",
        capture: {
          Window: {
            hwnd,
            pid: fixture.pid,
            title: "TakeKit pause fixture",
            crop: { top: 0, bottom: 0, left: 0, right: 0 },
          },
        },
        fps: 30,
        quality: null,
        audio: false,
      },
    ],
    out_root: path.join(OUT, "takes"),
    backend: "windows",
    marks: false,
    language: "zh",
    theme: "light",
    countdown_secs: 0,
  }),
);

await assertNoPanel();
let guiPid = 0;
const killTree = () => {
  for (const pid of [guiPid, fixture.pid]) {
    if (!pid) continue;
    try {
      execFileSync("taskkill", ["/PID", String(pid), "/T", "/F"], { stdio: "ignore" });
    } catch {}
  }
};
process.on("exit", killTree);

const guiLaunch = () => {
  const child = spawn(GUI, [], {
    env: {
      ...process.env,
      APPDATA,
      TAKEKIT_WEBVIEW_NO_SANDBOX: "1",
      // The popover is a third surface that has to say "paused" too, and it
      // cannot be opened from a script — the demo flag raises it at startup
      // (main.rs). Its own tick keeps it current from there.
      TAKEKIT_TRAY_DEMO: "1",
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

// Retried: this check runs after another one that just killed its panel, and a
// panel that cannot start says nothing about pauses (see `startPanel`).
const boot = await startPanel(guiLaunch, appDir);
const control = boot.control;
record(
  "panel-start",
  Boolean(control?.port) && control.pid === guiPid,
  `control port ${control?.port} pid ${control?.pid} (ours ${guiPid})`,
);
if (!control?.port || control.pid !== guiPid) process.exit(1);

const call = async (p, body) => {
  const r = await fetch(`http://127.0.0.1:${control.port}${p}`, {
    method: "POST",
    headers: { Authorization: `Bearer ${control.token}`, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await r.text();
  if (!r.ok) throw new Error(`${p} ${r.status} ${text}`);
  return text ? JSON.parse(text) : {};
};

// ── CDP, for the popover ───────────────────────────────────────────────────
// Opened before the take starts: a WebSocket handshake inside the two seconds
// the pause lasts would eat most of them.
async function cdp(match) {
  let target = null;
  for (let i = 0; i < 60 && !target; i += 1) {
    try {
      const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
      target = JSON.parse(await r.text()).find((t) => match.test(t.url || ""));
    } catch {}
    if (!target) await sleep(200);
  }
  if (!target) return null;
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
    if (w) {
      waits.delete(m.id);
      w(m);
    }
  };
  const send = (method, params = {}) =>
    new Promise((res) => {
      const i = id++;
      const timer = setTimeout(() => {
        waits.delete(i);
        res({ __timeout: true });
      }, 10_000);
      waits.set(i, (m) => {
        clearTimeout(timer);
        res(m);
      });
      ws.send(JSON.stringify({ id: i, method, params }));
    });
  const evalJs = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, returnByValue: true });
    if (r.__timeout) return undefined;
    if (r.result?.exceptionDetails) return { __error: r.result.exceptionDetails.text };
    return r.result?.result?.value;
  };
  const evalJson = async (expression) => {
    try {
      return JSON.parse(await evalJs(expression)) ?? {};
    } catch {
      return {};
    }
  };
  return { ws, send, evalJson };
}

const trayCdp = await cdp(/tray\.html/);

// ── record, pause in the middle, resume ───────────────────────────────────
await call("/record/start", { countdown_secs: 0 });
await sleep(1200);
const paused = await call("/record/pause");
record("pause-accepted", paused.paused !== false, JSON.stringify(paused));

// ── the pause has to be visible, not just true ────────────────────────────
// Reported 2026-10-27: 暂停的时候托盘图标、托盘菜单和主界面的提示都不充分.
// The panel hides itself for the length of a take, so while it runs the popover
// is the only surface where a pause is on screen at all — a frozen clock and
// the word 录制中 is the same picture as an encoder that hung.
//
// 1.4s, not 700ms: the note counts the pause, and at 700ms it read `0:00` —
// a number that would look just as right if the counter never moved at all
// (2026-10-27, the first run of this check was too early to tell).
await sleep(1400);
let trayLook = {};
if (trayCdp) {
  trayLook = await trayCdp.evalJson(`(() => {
    const el = (id) => document.getElementById(id);
    const cs = (id) => getComputedStyle(el(id));
    const note = el("pause-note");
    return JSON.stringify({
      state: el("state").textContent.trim(),
      note: note.textContent.trim(),
      noteShown: !note.classList.contains("hidden"),
      dot: cs("dot").backgroundColor,
      clock: cs("clock").color,
      stateColor: cs("state").color,
      pausedClass: document.body.classList.contains("paused"),
      liveClass: document.body.classList.contains("live"),
      key: el("pause").textContent.trim(),
      visible: document.visibilityState,
    });
  })()`);
}
record(
  "popover-says-paused",
  trayLook.state === "已暂停" && trayLook.pausedClass === true && trayLook.liveClass === false,
  `state="${trayLook.state}" paused=${trayLook.pausedClass} live=${trayLook.liveClass} visible=${trayLook.visible}`,
);
// The two things three characters cannot carry: for how long, and that the
// stretch is not in the file — which is the question a pause raises. Asserted
// as "at least one second on the clock", not as a format: a note that never
// counted would match any regex.
const noteTime = /^这段 (\d+):(\d\d) 不计入录制$/.exec(trayLook.note || "");
const noteSecs = noteTime ? Number(noteTime[1]) * 60 + Number(noteTime[2]) : -1;
record(
  "popover-counts-it-and-says-it-is-not-recorded",
  trayLook.noteShown === true && noteSecs >= 1,
  `"${trayLook.note}" (${noteSecs}s)`,
);
// Amber is this popover's word for "still here, not writing": the dot, the
// clock and the state have to agree, or the header reads as two states at once.
record(
  "popover-turns-amber-together",
  Boolean(trayLook.dot) && trayLook.dot === trayLook.clock && trayLook.clock === trayLook.stateColor,
  `dot=${trayLook.dot} clock=${trayLook.clock} state=${trayLook.stateColor}`,
);
record("popover-offers-the-way-back", trayLook.key === "继续", `key="${trayLook.key}"`);

// A photograph of the one surface that was still saying "Recording" during a
// pause. Worth keeping: the amber is a 8px dot and a 15px number at real size.
if (trayCdp && trayLook.visible === "visible") {
  const m = await trayCdp.send("Page.getLayoutMetrics");
  const box = m.result?.cssLayoutViewport ?? m.result?.layoutViewport ?? {};
  const shot = await trayCdp.send("Page.captureScreenshot", {
    format: "png",
    clip: { x: 0, y: 0, width: box.clientWidth || 220, height: box.clientHeight || 200, scale: 3 },
    captureBeyondViewport: false,
  });
  if (shot.result?.data) {
    writeFileSync(path.join(OUT, "popover-paused.png"), Buffer.from(shot.result.data, "base64"));
  }
}

await sleep(1000);
await call("/record/resume");
await sleep(1200);
const stopped = await call("/record/stop");
const dir = stopped.dir || "";
const manifest = JSON.parse(readFileSync(path.join(dir, "take.json"), "utf8"));
const src = manifest.sources[0];
const segs = src.segments || [];

record(
  "no-mismatch-warning",
  stopped.sources?.[0]?.warning == null,
  `warning ${JSON.stringify(stopped.sources?.[0]?.warning)}`,
);
record(
  "one-file-per-cut",
  segs.length === 2,
  `segments ${segs.length}: ${segs
    .map((s) => `${s.file}@${s.duration.toFixed(2)}/${s.probed?.toFixed(2) ?? "-"}`)
    .join(" ")}`,
);
const worst = segs.reduce((m, s) => {
  const off = s.probed === undefined ? 0 : Math.abs(s.probed - s.duration);
  return Math.max(m, off);
}, 0);
record(
  "each-file-matches-its-record",
  segs.every((s) => s.probed !== undefined) && worst <= TOLERANCE,
  `worst offset ${worst.toFixed(4)}s over ${segs.length} file(s)`,
);
const onDisk = new Set(readdirSync(dir));
record(
  "every-recorded-file-exists",
  segs.every((s) => onDisk.has(s.file)),
  segs.filter((s) => !onDisk.has(s.file)).map((s) => s.file).join(",") || "all present",
);
// A split numbers **every** file, the first included. Leaving the first one
// plain next to `-part2` reads as "the file without a number holds the whole
// recording" — the opposite of what a split means (2026-10-27, user's reading).
const parts = segs.map((s) => s.file);
record(
  "every-part-is-numbered",
  parts.length === 2 && /-part1\./.test(parts[0]) && /-part2\./.test(parts[1]),
  parts.join(" ") || "no names",
);
// …and the source-level `file` points at the first part, so a reader that only
// wants "the file for this source" still gets one that exists.
record(
  "the-source-file-is-the-first-part",
  parts.length > 0 && src.file === parts[0],
  `file=${src.file} parts[0]=${parts[0]}`,
);
record(
  "the-pause-is-banked",
  (manifest.paused ?? 0) > 1.5,
  `paused ${manifest.paused}s of ${manifest.duration}s`,
);

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} passed`);
killTree();
process.exit(failed ? 1 : 0);
