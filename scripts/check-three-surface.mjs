// Three-surface parity: a feature is not done until CLI, GUI and MCP all have
// it — and "have it" means a real call reaches the real code path, not that a
// field exists somewhere in a schema.
//
// What this covers: the MCP half end to end (stdio server → HTTP → panel →
// config) and the CLI's flags. The GUI half is `gui-sound.mjs`-style checks
// driven over CDP; the take-level proof is a real recording (see
// docs/api-recording.md).
//
// Usage:  cargo build --workspace && node scripts/check-three-surface.mjs
// A panel already running blocks this (and every GUI check): the isolated
// APPDATA below gives a fresh control port but the single-instance mutex is
// machine-wide, so the launch exits on sight. `lib/panel-guard.mjs` refuses up
// front and names the pid, instead of failing at the port nine steps later.
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { createInterface } from "node:readline";
import path from "node:path";
import { assertNoPanel, startPanel } from "./lib/panel-guard.mjs";
import { pickBinary } from "./lib/build.mjs";

const ROOT = path.resolve(import.meta.dirname, "..");
const GUI = pickBinary(ROOT, "takekit-gui.exe", "TK_GUI");
const TK = pickBinary(ROOT, "takekit.exe", "TK_CLI");
const OUT = path.join(ROOT, "target", "checks", "three-surface");
const APPDATA = path.join(OUT, "appdata");
const appDir = path.join(APPDATA, "TakeKit");

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

// `control.json` is the only file here that can poison a run, and it is the only
// one deleted: the port it names belongs to a process from an earlier run, so
// every request below would go there and report on a panel this script never
// started. A whole-tree delete is not available — the host's bulk-delete guard
// refuses more than 50 files at once (`SAFE_DELETE_BULK_CONFIRM_REQUIRED`), and
// swallowing that failure is how the stale port survived in the first place.
try {
  rmSync(path.join(appDir, "control.json"), { force: true });
} catch (e) {
  console.error(`could not remove the stale control.json: ${e.message}`);
  process.exit(2);
}
mkdirSync(appDir, { recursive: true });
writeFileSync(path.join(appDir, "config.json"), JSON.stringify({ sources: [], backend: "windows", marks: false }));

await assertNoPanel();

let guiPid = 0;
const guiLaunch = () => {
  const child = spawn(GUI, [], {
    env: { ...process.env, APPDATA, TAKEKIT_WEBVIEW_NO_SANDBOX: "1", WEBVIEW2_USER_DATA_FOLDER: path.join(OUT, "wv2") },
    detached: true,
    stdio: "ignore",
  });
  child.unref();
  guiPid = child.pid;
  return child;
};

// Retried: this runs after other checks that kill their panel, and a panel that
// cannot start says nothing about the three surfaces (see `startPanel`).
const boot = await startPanel(guiLaunch, appDir);
const control = boot.control;
// The pid is half of it: a control.json left behind points at a port this
// process never opened, and every call below would then be somebody else's
// panel answering. Same guard as check-ui-matrix / check-midtake.
record("panel-start", Boolean(control?.port) && control.pid === guiPid,
  `port ${control?.port} pid ${control?.pid} (ours ${guiPid})`);
if (!control?.port || control.pid !== guiPid) {
  say("the panel did not come up under this pid — is another instance running, or a stale control.json?");
  process.exit(1);
}

// A real window title fragment for kind=window. The panel's own window is on
// that list and is not recordable, so pick the first one that is not ours.
const listed = execFileSync(TK, ["list"], { encoding: "utf8" });
const windows = listed.split("\n")
  .map((l) => l.match(/^\s*\[(\d+)\]\s*(.*?)\s*\[[\d x, at]+\]\s*$/))
  .filter(Boolean)
  .map((m) => ({ hwnd: Number(m[1]), title: m[2] }))
  .filter((w) => w.hwnd && !/takekit/i.test(w.title));
const winTitle = windows[0]?.title || "";
record("window-available", Boolean(winTitle), JSON.stringify(winTitle));
if (!winTitle) {
  say("no recordable window on screen — open any app and rerun");
  try { execFileSync("taskkill", ["/PID", String(guiPid), "/T", "/F"], { stdio: "ignore" }); } catch {}
  process.exit(2);
}
const fragment = winTitle.split(/[-—|·]/)[0].trim().slice(0, 24);

const mcp = spawn(TK, ["mcp"], { env: { ...process.env, APPDATA }, stdio: ["pipe", "pipe", "pipe"] });
const rl = createInterface({ input: mcp.stdout });
const pending = [];
rl.on("line", (line) => { const w = pending.shift(); if (w) w(line); });
const rpc = (payload) =>
  new Promise((res, rej) => {
    const timer = setTimeout(() => rej(new Error(`timeout waiting for ${payload.method}`)), 20000);
    pending.push((line) => { clearTimeout(timer); res(JSON.parse(line)); });
    mcp.stdin.write(JSON.stringify(payload) + "\n");
  });

let nextId = 1;
const call = async (name, args) => {
  const reply = await rpc({ jsonrpc: "2.0", id: nextId++, method: "tools/call", params: { name, arguments: args } });
  return { error: reply.result?.isError === true, text: reply.result?.content?.[0]?.text ?? "" };
};

try {
  const init = await rpc({ jsonrpc: "2.0", id: nextId++, method: "initialize", params: { protocolVersion: "2024-11-05" } });
  record("mcp-initialize", init.result?.serverInfo?.name === "takekit", JSON.stringify(init.result?.serverInfo));

  const list = await rpc({ jsonrpc: "2.0", id: nextId++, method: "tools/list" });
  const tools = list.result?.tools || [];
  const addTool = tools.find((t) => t.name === "takekit_add_source");
  const props = Object.keys(addTool?.inputSchema?.properties || {});
  record("mcp-schema-has-sound", props.includes("sound"), props.join(","));
  record("mcp-schema-mentions-window-sound", /sound/.test(addTool?.description || ""),
    (addTool?.description || "").slice(0, 60));

  const withSound = await call("takekit_add_source", { kind: "window", target: fragment, sound: true });
  record("mcp-add-window-with-sound", !withSound.error, withSound.text.slice(0, 90));
  await sleep(400);

  const muted = await call("takekit_add_source", { kind: "window", target: fragment, sound: false });
  record("mcp-add-same-window-muted", !muted.error, muted.text.slice(0, 60));
  await sleep(400);

  // The desktop carries sound too, and it is not "its own": Windows has no
  // per-monitor audio, so this is the whole machine's loopback. Same field,
  // same file — the only thing that differs is which target is opened.
  const deskSound = await call("takekit_add_source", { kind: "desktop", sound: true });
  record("mcp-add-desktop-with-sound", !deskSound.error, deskSound.text.slice(0, 90));
  await sleep(400);

  const state = await call("takekit_state", {});
  const snapshot = JSON.parse(state.text);
  const audio = (snapshot.sources || []).map((s) => s.audio);
  record("mcp-sound-reaches-source", audio[0] === true, `sources audio=${JSON.stringify(audio)}`);
  record("mcp-muted-reaches-source", audio[1] === false, `sources audio=${JSON.stringify(audio)}`);
  record("mcp-sound-reaches-desktop", audio[2] === true, `sources audio=${JSON.stringify(audio)}`);
  const desk = (snapshot.sources || [])[2];
  record("desktop-source-keeps-its-capture", Boolean(desk && desk.capture && desk.capture.Desktop),
    JSON.stringify(desk?.capture));

  // `takekit_record_add_source` is the door to the take that is running. No
  // take is running here (and starting one would film the user's screen), so
  // what is proved is the wiring: schema → stdio → HTTP → the handler that
  // knows this is not the saved list. A 409 naming the state is exactly right;
  // a 404 or an unknown tool would mean the three surfaces drifted.
  const liveTool = tools.find((t) => t.name === "takekit_record_add_source");
  record("mcp-live-add-schema", Boolean(liveTool) && /start/.test(liveTool?.description || ""),
    (liveTool?.description || "").slice(0, 60));
  const live = await call("takekit_record_add_source", { kind: "desktop" });
  record("mcp-live-add-without-a-take", Boolean(live.error), live.text.slice(0, 120));
} finally {
  mcp.stdin.end();
  mcp.kill();
  try { execFileSync("taskkill", ["/PID", String(guiPid), "/T", "/F"], { stdio: "ignore" }); } catch {}
}

// The CLI half: the flag has to exist and be documented, or the three
// surfaces have drifted again.
const help = execFileSync(TK, ["record", "--help"], { encoding: "utf8" });
record("cli-window-sound-flag", /--window-sound/.test(help), "record --help");
record("cli-desktop-sound-flag", /--desktop-sound/.test(help), "record --help");
// `ctl add-live` posts the same spec as `ctl add`, so it has to carry the same
// sound switch — it shipped without it once (2026-10-27), which made "add a
// window with its sound" work before a take and not during one.
const liveHelp = execFileSync(TK, ["ctl", "add-live", "--help"], { encoding: "utf8" });
record("cli-addlive-sound-flag", /--sound/.test(liveHelp), "ctl add-live --help");

const fails = results.filter((r) => !r.ok);
say(`\n=== ${results.length - fails.length}/${results.length} passed ===`);
for (const f of fails) say(`FAIL ${f.name}: ${f.detail}`);
process.exit(fails.length ? 1 : 0);
