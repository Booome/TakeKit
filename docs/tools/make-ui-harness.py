# Builds a throwaway page that runs the real panel scripts against a stubbed
# Tauri IPC, so the click paths can be exercised without an interactive
# desktop (a locked session cannot receive synthetic clicks).
#
# It is generated, never committed: the body comes from the shipped
# index.html, so it cannot drift away from the real markup.
#
#   python docs/tools/make-ui-harness.py
#   chrome --headless --dump-dom --virtual-time-budget=8000 file:///.../harness.html

import io
import os
import pathlib
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parents[2]
UI = ROOT / "crates" / "takekit-gui" / "ui"
OUT = pathlib.Path(os.environ.get("TEMP", "/tmp")) / "tk_harness.html"

STUB = """
<script>
  // ── stubbed IPC ──────────────────────────────────────────────────────────
  window.__CALLS__ = [];
  window.__LISTENERS__ = {};
  window.__TAKEKIT_LANG__ = "zh";
  window.__TAKEKIT_THEME__ = "auto";
  const cfg = {
    hotkey: "Ctrl+Shift+F8", countdown_secs: 3, finalize_panel: true,
    sources: [], out_root: "takes", encoder: null, marks: true,
    language: "zh", theme: "auto",
  };
  window.__TAURI__ = {
    core: {
      invoke(cmd, args) {
        window.__CALLS__.push([cmd, args || {}]);
        const table = {
          config_get: () => JSON.parse(JSON.stringify(cfg)),
          ui_settings: () => ({ language: cfg.language, theme: cfg.theme }),
          env_info: () => ({
            ffmpeg: "/x/ffmpeg", encoder: "h264_nvenc",
            screen: { x: 0, y: 0, w: 1920, h: 1080 },
            default_out_root: "/tmp",
          }),
          list_windows: () => [],
          list_devices: () => [],
          check_sources: () => [],
          journal_get: () => ({ takes: [], log: [], unread_takes: 0, unread_log: 0, unread_log_level: null }),
          status: () => ({ recording: false, elapsed: 0, sources: [], error: null }),
          set_language: (a) => {
            cfg.language = a.language;
            return { language: cfg.language, theme: cfg.theme };
          },
          set_theme: (a) => {
            cfg.theme = a.theme;
            return { language: cfg.language, theme: cfg.theme };
          },
          set_hotkey: (a) => {
            cfg.hotkey = a.hotkey;
            return a.hotkey;
          },
        };
        const f = table[cmd];
        // Unknown commands resolve instead of rejecting: this page exercises
        // the panel, not the backend.
        return Promise.resolve(f ? f(args) : undefined);
      },
    },
    event: {
      listen(name, cb) {
        window.__LISTENERS__[name] = cb;
        return Promise.resolve(() => {});
      },
    },
  };
</script>
"""

DRIVER = """
<script>
  // ── the scenario, written into the DOM for --dump-dom ────────────────────
  const out = [];
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  (async () => {
    await wait(400);
    const gear = document.getElementById("settings-btn");
    gear.click();
    out.push("gear.opened=" + !document.getElementById("settings").classList.contains("hidden"));

    document.querySelector('[data-lang="en"]').click();
    await wait(30);
    out.push("lang.html=" + document.documentElement.lang);
    out.push("lang.tab=" + document.querySelector("#tab-sources span").textContent);
    out.push("lang.rec=" + document.getElementById("rec-btn").textContent);
    out.push("lang.hint=" + document.getElementById("hint").textContent);

    document.querySelector('[data-theme-opt="dark"]').click();
    await wait(30);
    out.push("theme.attr=" + document.documentElement.dataset.theme);

    document.querySelector('[data-theme-opt="auto"]').click();
    await wait(30);
    out.push("theme.auto.attr=" + document.documentElement.dataset.theme);

    const chip = document.getElementById("hotkey-btn");
    out.push("chip.idle=" + chip.textContent);
    chip.click();
    out.push("chip.capturing=" + chip.classList.contains("capturing"));
    out.push("chip.note=" + document.getElementById("hotkey-note").textContent);
    // A combination that is NOT the current one: the chip has to end up with
    // what the registrar round-tripped, not with what it asked for.
    window.dispatchEvent(
      new KeyboardEvent("keydown", { code: "KeyJ", key: "j", altKey: true, bubbles: true }),
    );
    await wait(60);
    out.push("chip.after=" + chip.textContent);
    out.push("chip.note.after=[" + document.getElementById("hotkey-note").textContent + "]");
    out.push(
      "calls=" +
        JSON.stringify(
          window.__CALLS__.filter((c) => /^set_(language|theme|hotkey)$/.test(c[0])),
        ),
    );
    document.getElementById("result").textContent = out.join("\\n");
  })();
</script>
<pre id="result">pending</pre>
"""


def main():
    html = (UI / "index.html").read_text(encoding="utf-8")

    def absolute(match_attr: str, path: str) -> str:
        url = urllib.parse.urljoin(UI.as_uri() + "/", path)
        return f'{match_attr}="{url}"'

    for asset in ("style.css", "shared.js", "i18n.js", "beep.js", "main.js"):
        html = html.replace(f'href="{asset}"', absolute("href", asset))
        html = html.replace(f'src="{asset}"', absolute("src", asset))

    html = html.replace("<head>", "<head>\n" + STUB, 1)
    html = html.replace("</body>", DRIVER + "</body>", 1)

    OUT.write_text(html, encoding="utf-8")
    print(f"harness -> {OUT}")
    print(f"open with: chrome --headless --dump-dom --virtual-time-budget=8000 {OUT.as_uri()}")


if __name__ == "__main__":
    main()
