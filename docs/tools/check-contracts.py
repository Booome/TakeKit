#!/usr/bin/env python3
"""Cross-layer contract check for TakeKit — commands, events and i18n keys.

Three sets of names have to agree across the Rust/JS/HTML boundary, and each
has drifted at least once: a dead `take:stopped` event kept being emitted
with no listener, `overlay_hide` sat registered with no caller, and the
i18n dictionaries grew keys nothing asks for. All three are invisible to
`cargo test` — the app just quietly carries the extra name.

Run from the repository root:  python docs/tools/check-contracts.py
Exit code 0 = every name lines up. Cross-platform, stdlib only.

The checks:
  1. Tauri commands — defined == registered in `invoke_handler` == invoked
     from the UI.
  2. Events — emitted from Rust == listened to in the UI. `emit_to` takes a
     label first, so the event name is matched as the last quoted argument.
  3. i18n — every key the UI asks for exists in both dictionaries, and every
     dictionary key is asked for somewhere (HTML `data-i18n*`, a `t()` call,
     a Rust `UiError` code, or a `t(`prefix${…}` template whose prefix the
     key extends).
  4. Agent control surface (`docs/agent-control.md`) — the HTTP routes, the
     MCP tools and the `takekit ctl` subcommands are three doors onto the same
     capabilities, and each door must carry every capability (and nothing that
     is not one).
"""

from __future__ import annotations

import re
import sys
from functools import lru_cache
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GUI_SRC = ROOT / "crates" / "takekit-gui" / "src"
UI = ROOT / "crates" / "takekit-gui" / "ui"

failures: list[str] = []


def fail(msg: str) -> None:
    failures.append(msg)


@lru_cache(maxsize=1)
def rust_sources() -> list[Path]:
    return sorted((ROOT / "crates").glob("*/src/*.rs"))


# Three checks walk the Rust sources (commands, events, i18n), and each used
# to re-read every file from disk. The corpus is small but the walk is not
# free, and a stale read inside one run is impossible anyway — the check does
# not write anything. Read once, share.
_text_cache: dict[Path, str] = {}


def read_cached(path: Path) -> str:
    text = _text_cache.get(path)
    if text is None:
        text = path.read_text(encoding="utf-8")
        _text_cache[path] = text
    return text


def js_sources() -> list[Path]:
    return sorted(UI.glob("*.js"))


def html_sources() -> list[Path]:
    return sorted(UI.glob("*.html"))


# ── 1. Tauri commands ────────────────────────────────────────────────────────


def check_commands() -> None:
    defined: set[str] = set()
    for path in rust_sources():
        text = read_cached(path)
        # `#[tauri::command]` sits directly above its fn, but a doc comment
        # of any length can sit between them. Each attribute's own chunk is
        # therefore searched for the first top-level fn, not a fixed window
        # after the attribute: a long doc comment silently drops the command
        # from `defined`, which then reads as a phantom registration.
        for chunk in text.split("#[tauri::command]")[1:]:
            found = re.search(
                r"(?m)^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)", chunk
            )
            if found:
                defined.add(found.group(1))

    main = (GUI_SRC / "main.rs").read_text(encoding="utf-8")
    handler = re.search(r"generate_handler!\[([\s\S]*?)\]", main)
    registered: set[str] = set()
    if handler:
        registered = {n.strip() for n in handler.group(1).split(",") if n.strip()}
    else:
        fail("commands: no invoke_handler(generate_handler![...]) found in main.rs")

    invoked: set[str] = set()
    for path in js_sources():
        text = path.read_text(encoding="utf-8")
        # A leading literal — `invoke("state")`.
        invoked |= set(re.findall(r"\binvoke\(\s*[\"'](\w+)[\"']", text))
        # Both arms of a ternary — `invoke(going ? "pause_recording" :
        # "resume_recording")`. Reading only the leading literal saw neither
        # arm, so the pair was reported as "never invoked" while the button
        # worked. Names are not filtered against the command list afterwards:
        # `invoked - registered` is what catches a misspelt name, and dropping
        # the unknown ones here would quietly retire that check.
        for m in re.finditer(
            r"\binvoke\(\s*[^\"')]*\?\s*[\"'](\w+)[\"']\s*:\s*[\"'](\w+)[\"']", text
        ):
            invoked |= {m.group(1), m.group(2)}

    for name in sorted(defined - registered):
        fail(f"commands: `{name}` is #[tauri::command] but not in invoke_handler")
    for name in sorted(registered - defined):
        fail(f"commands: `{name}` is in invoke_handler but no such command exists")
    for name in sorted(invoked - registered):
        fail(f"commands: UI invokes `{name}` but it is not registered")
    for name in sorted(registered - invoked):
        # Not necessarily dead (a command may exist for scripting), but it is
        # worth a line: the UI is the only consumer this app has.
        print(f"note: command `{name}` is registered but never invoked from the UI")


# ── 2. Events ────────────────────────────────────────────────────────────────


def check_events() -> None:
    emitted: set[str] = set()
    for path in rust_sources():
        text = read_cached(path)
        # `emit("x", …)` names the event first; `emit_to(label, "x", …)`
        # names a window label first and the event second. Both forms are
        # matched, the label never being mistaken for the event.
        emitted |= set(re.findall(r"\bemit\(\s*[\"']([\w:-]+)[\"']", text))
        emitted |= set(
            re.findall(r"\bemit_to\(\s*[^,]+,\s*[\"']([\w:-]+)[\"']", text)
        )

    listened: set[str] = set()
    for path in js_sources():
        listened |= set(
            re.findall(r"\blisten\(\s*[\"']([\w:-]+)[\"']", path.read_text(encoding="utf-8"))
        )

    for name in sorted(emitted - listened):
        fail(f"events: Rust emits `{name}` but no UI listens for it")
    for name in sorted(listened - emitted):
        fail(f"events: UI listens for `{name}` but Rust never emits it")


# ── 3. i18n ──────────────────────────────────────────────────────────────────


def dictionaries() -> tuple[set[str], set[str]]:
    text = (UI / "i18n.js").read_text(encoding="utf-8")
    lines = text.split("\n")
    try:
        zh_start = next(i for i, l in enumerate(lines) if l.strip() == "zh: {")
        en_start = next(i for i, l in enumerate(lines) if l.strip() == "en: {")
        zh_block = "\n".join(lines[zh_start + 1 : en_start])
        en_block = "\n".join(lines[en_start + 1 :])
        # The en block ends at the dictionary's closing brace; the file
        # continues with helper code whose quoted words are not keys.
        en_block = en_block[: en_block.index("\n};") + 3]
    except (StopIteration, ValueError):
        # A restructured dictionary is a loud failure with a sentence, not a
        # traceback from three frames down.
        fail("i18n: could not find the zh:/en: dictionary blocks in i18n.js")
        return set(), set()
    # `-` 也是键名字符：`warn.len-mismatch` 这类键含连字符，旧正则把它们整条漏掉，
    # 「zh/en 对等」与「有人问过」两道断言对它们完全无效。
    key = re.compile(r"[\"']([A-Za-z0-9_.\-]+)[\"']\s*:")
    return set(key.findall(zh_block)), set(key.findall(en_block))


# Keys the UI asks for through a name built at run time, where the interpolated
# part comes *first* — `t(`${pane}.clearArmed`)` — so there is no literal prefix
# to match on. Listed by hand: adding one is a deliberate act, and the list is
# short on purpose.
ASKED_BY_VARIABLE = {"history.clearArmed", "log.clearArmed"}


def check_i18n() -> None:
    zh, en = dictionaries()
    if zh != en:
        for k in sorted(zh - en):
            fail(f"i18n: key `{k}` is in zh but not en")
        for k in sorted(en - zh):
            fail(f"i18n: key `{k}` is in en but not zh")

    # Asked for literally…
    asked: set[str] = set()
    for path in html_sources() + js_sources():
        text = path.read_text(encoding="utf-8")
        asked |= set(re.findall(r"data-i18n(?:-\w+)?=\"([\w.]+)\"", text))
        # `\bt(` keeps createElement("button") from reading as a lookup.
        asked |= set(re.findall(r"\bt\(\s*[\"']([\w.]+)[\"']", text))
    # …or by name from Rust (UiError codes the UI translates on arrival)…
    for path in rust_sources():
        text = read_cached(path)
        asked |= set(re.findall(r"UiError::(?:new|with_detail)\(\s*\"([\w.\-]+)\"", text))
    # …or through a template: `t(`crop.handle.${id}`)` covers every
    # `crop.handle.*` key, so a key is used when one of those prefixes
    # starts it.
    prefixes: set[str] = set()
    for path in js_sources():
        text = path.read_text(encoding="utf-8")
        for body in re.findall(r"\bt\(\s*`([^`]*)`", text):
            prefix = body.split("${")[0]
            # 模板一开头就插值（`t(`${pane}.clearArmed`)`）时前缀是空串，而任何字符串
            # 都以空串开头 —— 收进来就等于让「没人问过」这条断言永远通过。
            if prefix:
                prefixes.add(prefix)

    # Every quoted string in the UI and Rust, minus the dictionary itself.
    mentioned = "\n".join(
        p.read_text(encoding="utf-8")
        for p in html_sources() + js_sources() + rust_sources()
        if p.name != "i18n.js"
    )

    def is_asked(key: str) -> bool:
        if key in asked or key in ASKED_BY_VARIABLE:
            return True
        if any(key.startswith(p) for p in prefixes):
            return True
        # …or handed to `t()` through a variable (`const k = on ? "row.soundOn" :
        # "row.soundOff"; t(k)`). A static reader cannot follow that, but a key
        # that appears as a quoted string anywhere in the UI or Rust is clearly
        # not orphaned — only keys nothing mentions at all get reported.
        return bool(re.search(r"[\"']%s[\"']" % re.escape(key), mentioned))

    for key in sorted(asked - zh):
        # A key asked for but absent everywhere would render as the key
        # itself; `createElement`-style false positives cannot reach here
        # because of the `\bt(` guard.
        fail(f"i18n: `{key}` is asked for but missing from both dictionaries")
    for key in sorted(k for k in zh if not is_asked(k)):
        fail(f"i18n: dictionary key `{key}` is never asked for")


# ── 4. Agent control surface ─────────────────────────────────────────────────

# capability -> (route path in control.rs, MCP tool, `takekit ctl` subcommand).
# One row per capability: adding one means a new row here and a line in
# docs/agent-control.md, which is the point — the table is the contract.
CONTROL: list[tuple[str, str, str, str]] = [
    ("state", "/state", "takekit_state", "State"),
    ("devices", "/devices", "takekit_list_devices", "Devices"),
    ("add source", "/sources", "takekit_add_source", "Add"),
    ("remove source", "/sources/", "takekit_remove_source", "Remove"),
    ("replace sources", "/sources", "takekit_set_sources", "SetSources"),
    ("settings", "/config", "takekit_set_settings", "Config"),
    ("record add source", "/record/sources", "takekit_record_add_source", "AddLive"),
    ("record pause", "/record/pause", "takekit_pause_recording", "Pause"),
    ("record resume", "/record/resume", "takekit_resume_recording", "Resume"),
    ("start", "/record/start", "takekit_start_recording", "Start"),
    ("stop", "/record/stop", "takekit_stop_recording", "Stop"),
    ("history", "/history", "takekit_history", "History"),
    ("log", "/log", "takekit_log", "Log"),
    ("clear log", "/log", "takekit_clear_log", "ClearLog"),
    ("delete log entry", "/log/", "takekit_delete_log", "DeleteLog"),
    ("delete take", "/history/", "takekit_delete_take", "Delete"),
]


def check_control() -> None:
    control = (GUI_SRC / "control.rs").read_text(encoding="utf-8")
    cli = ROOT / "crates" / "takekit-cli" / "src"
    mcp = (cli / "mcp.rs").read_text(encoding="utf-8")
    ctl = (cli / "ctl.rs").read_text(encoding="utf-8")

    # Route literals: `(Method::Get, "/state")` and the two prefix matches
    # (`p.starts_with("/sources/")`). Anything else would be a route this
    # table does not know about.
    routes = set(re.findall(r'Method::\w+,\s*"(/[^"]+)"', control))
    routes |= set(re.findall(r'starts_with\("(/[^"]+/)"\)', control))

    # MCP tools twice over: declared in `tool_list` and handled in
    # `tools_call`. A tool only half-wired fails one of the two.
    declared = set(re.findall(r'"name":\s*"(takekit_\w+)"', mcp))
    dispatched = set(re.findall(r'"(takekit_\w+)"\s*=>', mcp))

    # `takekit ctl` subcommand variants, taken from the enum body only.
    body = ctl.split("pub enum Ctl", 1)[1].split("\n}\n", 1)[0]
    variants = set(re.findall(r"\n    (\w+)", body))

    expected_routes = {route for _, route, _, _ in CONTROL}
    expected_tools = {tool for _, _, tool, _ in CONTROL}
    expected_subs = {sub for _, _, _, sub in CONTROL}

    for capability, route, tool, sub in CONTROL:
        if route not in routes:
            fail(f"control: no route `{route}` in control.rs ({capability})")
        if tool not in declared:
            fail(f"control: MCP tool `{tool}` not declared ({capability})")
        if tool not in dispatched:
            fail(f"control: MCP tool `{tool}` declared but never dispatched ({capability})")
        if sub not in variants:
            fail(f"control: CLI subcommand `{sub}` missing from ctl.rs ({capability})")

    for route in sorted(routes - expected_routes):
        fail(f"control: route `{route}` is not in the contract table")
    for tool in sorted(declared - expected_tools):
        fail(f"control: MCP tool `{tool}` is not in the contract table")
    for tool in sorted(dispatched - expected_tools):
        fail(f"control: MCP dispatch handles `{tool}` but nothing declares it")
    for sub in sorted(variants - expected_subs):
        fail(f"control: CLI subcommand `{sub}` is not in the contract table")


def main() -> int:
    check_commands()
    check_events()
    check_i18n()
    check_control()
    for line in failures:
        print(f"FAIL {line}")
    if failures:
        print(f"\n{len(failures)} contract problem(s)")
        return 1
    print("contracts OK: commands, events, i18n keys and the control surface all line up")
    return 0


if __name__ == "__main__":
    sys.exit(main())
