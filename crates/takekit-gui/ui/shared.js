// Helpers every page needs: the panel, the veil and the popover.
//
// No bundler and no npm: classic scripts share one global lexical
// environment, so what is declared here is visible to the scripts loaded
// after it. It lives in one file because three copies of `PROBE`, `$` and
// `clock` had already started to drift.

const { invoke } = window.__TAURI__.core;

// PROBE-ONLY: checkpoint helper. Remove with the Rust `ui_ping` command.
const PROBE = (stage) => invoke("ui_ping", { stage }).catch(() => {});

const $ = (id) => document.getElementById(id);

// Palette: `data-theme` on <html> overrides the system preference, which is
// what the stylesheets fall back to. "auto" is written too — the attribute
// then just does not match the explicit rules, and the media query decides.
function applyTheme(mode) {
  const value = mode === "light" || mode === "dark" ? mode : "auto";
  document.documentElement.dataset.theme = value;
}

function clock(seconds) {
  const total = Math.floor(seconds);
  const h = String(Math.floor(total / 3600)).padStart(2, "0");
  const m = String(Math.floor((total % 3600) / 60)).padStart(2, "0");
  const s = String(total % 60).padStart(2, "0");
  return `${h}:${m}:${s}`;
}

// The same duration where a leading `00:` only takes up room: `1:24`, and
// `1:02:00` once it is past an hour (`62:03` is not a duration anyone reads).
// The bar's clock always shows hours because it sits in a fixed column; these
// are the places where the number is prose — a source's start chip, a row on
// the stop checklist, the popover's paused note. All three had their own copy
// before 2026-10-27, and one of them disagreed about the hour case.
function shortClock(seconds) {
  const total = Math.max(0, Math.floor(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}
