// A TakeKit panel the user already has open stops every GUI check here dead.
//
// `tauri-plugin-single-instance` names its Windows mutex `{identifier}-sim`,
// with the identifier taken from tauri.conf.json (platform_impl/windows.rs,
// L66-75). Its builder exposes only `dbus_id`, which is Linux-only, so there is
// no way to give a check script a key of its own: one panel per machine,
// whoever started it. The one launched here therefore exits on sight.
//
// What that looks like from the outside is `panel-start port undefined pid
// undefined` about fifty seconds later — a description of a symptom three steps
// away from its cause. Hence this: say it up front, in one line, with the
// command that fixes it.
//
// Deliberately not killing it. A running panel holds somebody's configured
// source list (config.json does not keep sources), and a check script is not a
// reason to throw that away. `TK_GUI` isolation does not help either: APPDATA
// and the WebView2 profile are per-script, the mutex is not.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

const EXE = "takekit-gui.exe";

/// Pids of running panels — any of them, since we have not spawned ours yet.
///
/// `tasklist` because it is the one that is always there; CSV so the parse does
/// not depend on column padding. A filter that matches nothing exits non-zero
/// and prints a sentence instead of a row, so failure has to mean "none" rather
/// than an exception escaping into the caller.
export function runningPanels() {
  let out = "";
  try {
    out = execFileSync("tasklist", ["/FI", `IMAGENAME eq ${EXE}`, "/NH", "/FO", "CSV"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
  } catch {
    return [];
  }
  const pids = [];
  for (const line of out.split(/\r?\n/)) {
    const m = /^"[^"]*","(\d+)"/.exec(line.trim());
    if (m) pids.push(Number(m[1]));
  }
  return pids;
}

/// Wait for the field to clear, then exit before doing anything expensive.
///
/// The wait is not politeness: a panel that the previous check just `taskkill`ed
/// is still listed for a moment, and `taskkill` returns before the process is
/// gone. Running two checks back to back (`check-midtake && check-pause`) hit
/// that window every time and reported a panel that was already dying — measured
/// 2026-10-27, `control port undefined pid undefined (ours 41304)` with no
/// process left by the time the failure was read.
///
/// Ten seconds, not four: the same failure came back when five checks ran in a
/// row (2026-10-27), because the previous panel's WebView2 children outlive the
/// `taskkill /T` that took the parent. Waiting is cheap; a false red is not.
export async function assertNoPanel({ graceMs = 10_000, stepMs = 200 } = {}) {
  const until = Date.now() + graceMs;
  let pids = runningPanels();
  while (pids.length && Date.now() < until) {
    await new Promise((r) => setTimeout(r, stepMs));
    pids = runningPanels();
  }
  if (!pids.length) return;
  console.error(
    `a TakeKit panel is already running (pid ${pids.join(", ")}).\n` +
      `The single-instance mutex is global — one panel per machine, whoever started it — so the one this\n` +
      `script launches would exit on sight, and every assertion after that would describe the wrong window.\n` +
      `Close it, or:  taskkill /PID ${pids[0]} /T /F`,
  );
  process.exit(2);
}

/// Launch a panel and wait for the control port it writes, retrying.
///
/// `assertNoPanel` only proves no *process* is listed; that is not the same as
/// "the previous one has let go". Between `taskkill` returning and the last
/// handle closing there is a window in which the new process loses the
/// single-instance mutex, or cannot open the WebView2 profile directory the old
/// one's children still hold — it then exits without ever writing
/// `control.json`, and the check reports `port undefined pid undefined` (a
/// description of a symptom three steps from its cause). Measured 2026-10-27 on
/// `... && check-pause` inside a six-check sweep, after the guard had already
/// been lengthened twice: the wait is not the fix, a second try is.
///
/// `launch` is the caller's own spawn, because each check needs its own env
/// (`TAKEKIT_OVERLAY_DEMO`, CDP ports, a profile dir). It returns `{ pid,
/// control }` — `control` is null when every attempt failed, and the caller
/// decides what that means for its own report.
export async function startPanel(launch, appDir, { attempts = 3, waitMs = 20_000, stepMs = 300 } = {}) {
  const file = path.join(appDir, "control.json");
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    const child = launch();
    const pid = child.pid;
    const until = Date.now() + waitMs;
    while (Date.now() < until) {
      await new Promise((r) => setTimeout(r, stepMs));
      try {
        const control = JSON.parse(readFileSync(file, "utf8"));
        // The pid is half of it: a control.json left behind points at a port
        // whose process is gone, and every request would land there.
        if (control.port && control.pid === pid) return { pid, control };
      } catch {}
      // Died on the way up (mutex/profile). No point waiting out the clock.
      if (child.exitCode !== null || child.signalCode !== null) break;
    }
    try {
      execFileSync("taskkill", ["/PID", String(pid), "/T", "/F"], { stdio: "ignore" });
    } catch {}
    if (attempt < attempts) await new Promise((r) => setTimeout(r, 1500));
  }
  return { pid: null, control: null };
}
