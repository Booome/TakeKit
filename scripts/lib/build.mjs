// Which build a check runs against — and saying so out loud.
//
// Every check here drives `takekit-gui.exe` (and some, `takekit.exe`). The
// scripts used to hard-default to `target/debug`, which has one bad failure
// mode: a stale debug binary silently reproduces *last week's* behaviour.
// Measured 2026-10-28 — `check-midtake` reported
// `a-window-that-closes-is-stopped-without-the-panel` FAIL and
// `probe-source:vanished=false` on a tree whose watch code was fine. The panel
// being driven was a two-day-old debug build; the same script against
// `target/release` passed 15/15 with nothing changed but the path.
//
// A check that tests the wrong binary is worse than a check that refuses to
// run, because its verdict looks like a code verdict. So:
//
//   * `TK_GUI` / `TK_CLI` still win when set (isolation runs, A/B);
//   * otherwise the **freshest** of `target/release` and `target/debug` wins;
//   * the chosen binary and its timestamp are printed on every run;
//   * a build older than the newest source file is called out as stale.
//
// The staleness test is deliberately coarse (max mtime over the source and UI
// files, not a dependency graph): it only has to catch "this binary predates
// the edit under test", and it must not cost a build-graph implementation.
import { existsSync, readdirSync, statSync } from "node:fs";
import path from "node:path";

/// Newest mtime under the directories a binary is built from.
function newestSource(root) {
  let newest = 0;
  const walk = (dir) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (e.name === "target" || e.name === "node_modules" || e.name.startsWith(".")) continue;
      const full = path.join(dir, e.name);
      if (e.isDirectory()) {
        walk(full);
      } else if (/\.(rs|js|mjs|html|css|json|toml)$/.test(e.name)) {
        try {
          const t = statSync(full).mtimeMs;
          if (t > newest) newest = t;
        } catch {}
      }
    }
  };
  walk(path.join(root, "crates"));
  return newest;
}

let sources = null;

/// Absolute path to a binary, printing which one and warning when it is stale.
///
/// `envVar` is read first so a caller can point the whole script elsewhere with
/// one variable — that is how the release sweep runs, and how an A/B compares
/// two builds without editing anything.
export function pickBinary(root, exeName, envVar) {
  const override = process.env[envVar];
  const candidates = override
    ? [path.resolve(root, override)]
    : ["release", "debug"].map((p) => path.join(root, "target", p, exeName)).filter(existsSync);
  if (!candidates.length || !candidates.every(existsSync)) {
    const missing = candidates.filter((file) => !existsSync(file));
    console.error(
      (missing.length ? `${envVar} points at ${missing[0]}, which does not exist\n` : `no ${exeName} to run\n`) +
        `build one first\n` +
        `  cargo build --release -p takekit -p takekit-gui\n` +
        `  (or point ${envVar} at an existing one)`,
    );
    process.exit(2);
  }
  // Freshest wins. When the override names one, there is nothing to choose.
  const chosen = candidates
    .map((file) => ({ file, mtime: statSync(file).mtimeMs }))
    .sort((a, b) => b.mtime - a.mtime)[0];
  sources ??= newestSource(root);
  const stale = chosen.mtime < sources;
  const label = path.relative(root, chosen.file).replaceAll("\\", "/");
  console.log(
    `using ${label} (built ${new Date(chosen.mtime).toISOString().slice(0, 16).replace("T", " ")}Z)` +
      (stale ? "  ⚠ older than the newest source file — build it, or the verdict describes old code" : ""),
  );
  return chosen.file;
}
