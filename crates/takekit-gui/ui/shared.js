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
