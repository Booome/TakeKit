# TakeKit

**English** | [简体中文](README.zh-CN.md)

Record several sources at once into one take — a window or the whole desktop, plus a camera, a
microphone and the sound the machine plays — so the files come out already in sync.

> Status: **early (0.1.0)**, Windows only, no installer yet (build from source).
> It is used daily by its author, but treat the CLI flags as unstable.

## Why

Screen recordings are usually assembled by hand: grab the screen with one tool, the camera with
another, then line them up in an editor. TakeKit treats a recording the way a film crew treats a
take — every source starts and stops together, so the files already line up.

Each source gets **its own encoder**: one ffmpeg child per source on the ffmpeg backend, one
in-process writer thread per source on the Windows-API backend. That is not a shortcut, it is the
point. A single ffmpeg fed several inputs picks the input to read next by timestamp, which starves
the desktop grabber down to about half the requested frame rate. Separate encoders keep every
source at its own rate.

## Features

- **Sources**: a window (by handle or by title), the whole desktop, DirectShow cameras,
  microphones, the system sound (WASAPI loopback), and one window's own sound.
- **One take, one clock.** All sources share a single time origin, so cross-source duration
  differences stay within a frame or two — no alignment step afterwards.
- **A take can grow while it records.** Add a window, screen, camera or sound source mid-take and
  it joins immediately: its file begins at its own first frame, and that instant goes into the
  manifest, so nothing downstream has to guess where it belongs.
- **A take can be paused, and the pause is cut out.** The clock stops, every source closes the file
  it is writing, and the resume opens the next one on the exact frame the last ended. What comes out
  is clips that already sit next to each other on a timeline — nothing to find and trim. Windows
  backend only.
- **Two capture backends.**
  - *Windows APIs* (WGC + Media Foundation), fully in-process: keeps recording a window even while
    it sits behind other windows. The panel uses this by default.
  - *ffmpeg*: one child process per source. The CLI uses this by default; it is the compatibility
    fallback.
- **Hardware encoding when it actually works**: NVENC → QSV → AMF → libx264. Every candidate is
  probed with a one-frame encode first, so a machine that advertises an encoder it cannot use still
  gets a working take.
- **A desktop panel** (Tauri 2 + WebView2): a source list with still previews, per-source cropping,
  a countdown with a beep, a tray icon, take history, a log tab, global hotkey, and
  Chinese/English × light/dark themes.
- **Mouse effects**: click ripples and drag trails, from [MouseShow](https://github.com/Booome/MouseShow),
  linked as a library rather than launched as a separate process.
- **Ctrl-C finalises instead of killing.** The file is closed properly and the manifest is written,
  so an interrupted take is still a usable take.
- **An agent surface**, because not everyone wants to click: a loopback HTTP control service inside
  the panel, a `takekit ctl` client, and an MCP server over stdio.

## Requirements

| | |
|---|---|
| OS | Windows 10 (1809 or newer) or Windows 11, x64. Developed and tested on Windows 11 |
| To build | Rust 1.85+ (edition 2024), Visual Studio C++ build tools |
| Panel | WebView2 runtime (already present on Windows 11 and most Windows 10 installs) |
| ffmpeg | On `PATH` — **only** for the ffmpeg backend; the Windows-API backend does not use it |

## Build

```bash
cargo build --release -p takekit -p takekit-gui
# target/release/takekit.exe      — CLI
# target/release/takekit-gui.exe  — panel
```

`cargo test --workspace` runs the suite. The panel's UI is embedded into the binary at compile
time, so any change under `crates/takekit-gui/ui/` needs a rebuild to take effect.

## Usage — the panel

Run `takekit-gui.exe`, then:

1. Add sources (window / desktop / camera / microphone), and crop a source by clicking its preview.
2. Press **F9** (the default hotkey) or the record button. A 3-second countdown runs; **Esc**
   cancels it.
3. Press **F9** again to stop. The take lands in `%TEMP%\<timestamp>\` unless you set another
   output folder in settings.

Each take holds one file per source plus a `take.json` manifest. History lives in the panel; a row
opens its folder.

Settings cover the hotkey, countdown length, language, theme, the ffmpeg path, and whether the
agent control service is on.

## Usage — the CLI

```bash
# what can I record?
takekit list
takekit list "VirtualBox"        # filter windows by title

# one window + a camera + a microphone, 60 seconds
takekit record --window "VirtualBox" --cam "My Camera" --mic "Microphone (USB)" --duration 60

# whole desktop with sound, cropped 33px from the top, until you press Enter
takekit record --desktop --desktop-sound --crop 33,0,0,0
```

`--window` takes either a window handle (`--window "#12345"`) or a title substring. The take goes
to `takes/<timestamp>/` by default (`-o` changes the root).

### `takekit record`

| Flag | Meaning |
|---|---|
| `--window <TITLE_OR_#HWND>` | Record one window |
| `--desktop` | Record the whole desktop instead of a window |
| `--windows` | Capture through the Windows APIs instead of ffmpeg children (records a covered window; one monitor) |
| `--crop T,B,L,R` | Crop inset in pixels, applied per source |
| `--cam <DEVICE>` | Also record a video device (see `takekit list`) |
| `--mic <DEVICE>` | Also record a microphone (see `takekit media --list`) |
| `--system-audio` | Also record what the speakers play, as its own file |
| `--window-sound` | The window's own sound, written into the same file as its picture (needs `--window`) |
| `--desktop-sound` | The desktop's sound, into the desktop's file (the whole machine — Windows has no per-monitor audio; **not with `--window`**) |
| `--encoder auto\|x264\|nvenc\|qsv\|amf` | `auto` probes and picks the best that actually works |
| `--quality 0..=51` | Roughly CRF for x264, CQ/QP for hardware encoders; 0 = lossless (default 20, out of range refused before the take folder exists) |
| `--fps <N>` | Capture rate, default 30 |
| `-d, --duration <SECS>` | Stop after N seconds, otherwise press Enter |
| `-o, --out <DIR>` | Output root, default `takes` |
| `--marks true\|false` | Draw click ripples and drag trails into the picture, default `true` |
| `--ffmpeg <PATH>` | Use a specific ffmpeg instead of the `PATH` lookup |

### `takekit media`

The same idea through the Windows APIs only, with no ffmpeg child at all:

```bash
takekit media --list                       # what the APIs can see
takekit media --window 0x1234 --window-audio 0x1234 --duration 30
takekit media --screen 1 --system-audio --fps 60 --bitrate 20000
```

`--cfr` makes the picture sample at `--fps` instead of writing only on change. The file is
fixed-frame-rate either way (the encoder repeats frames to fill a gap); this flag only decides
whether a slot shows the picture as it was at that moment or the last one that arrived.

### `takekit ctl` and `takekit mcp`

`ctl` drives a **running panel** (add sources, change settings, start, stop, read history, delete a
take) and prints JSON — handy from a script:

```bash
takekit ctl state
takekit ctl add --kind window --target "#12345"
takekit ctl start
takekit ctl stop
```

`mcp` speaks MCP over stdio so an agent can do the same sixteen things with typed tools
(`takekit_state`, `takekit_list_devices`, `takekit_add_source`, `takekit_start_recording`, …).
Point an MCP client at `takekit mcp` and it works; the panel must be running. The control service
listens on `127.0.0.1` only, on a random port, and requires a per-run bearer token that the panel
writes to `%APPDATA%\TakeKit\control.json`.

## What a take looks like

```
takes/2026-10-18-11-31-33/
├─ win.mkv
├─ cam.mkv
└─ take.json
```

```json
{
  "encoder": "h264_nvenc",
  "hardware": true,
  "started_at": "2026-10-18-11-31-33",
  "duration": 60.0,
  "start_clock": "frames",
  "sources": [
    { "id": "win", "file": "win.mkv", "start": 0.0, "duration": 60.0,
      "source": { "id": "win", "capture": { "Window": { "hwnd": 133890, "pid": 4321,
                                           "title": "VirtualBox",
                                           "crop": { "top": 0, "bottom": 0, "left": 0, "right": 0 } } },
                  "fps": 30, "quality": null, "audio": false } },
    { "id": "cam", "file": "cam.mkv", "start": 0.0, "duration": 60.0,
      "source": { "id": "cam", "capture": { "Camera": { "device": "My Camera",
                                           "crop": { "top": 0, "bottom": 0, "left": 0, "right": 0 } } },
                  "fps": 30, "quality": null, "audio": false } }
  ]
}
```

`start_clock` says what the offsets were measured against (`frames` = the file's own first sample,
exact; `spawn` = encoder launch, the ffmpeg backend's best look); `paused` appears only for a take
that was paused, `frames` on a source only on the Windows backend.

The manifest records which encoder ran and each file's own duration, so a mismatch is visible instead
of being discovered in the editor. The container follows the backend — `.mp4` on the Windows APIs,
`.mkv` on ffmpeg — and a sound-only source is always an `.mp4`, even inside an `.mkv` take.

## How it works

```
crates/
├─ takekit-core/    every decision: session lifecycle, sources, encoder choice, take/manifest, ffmpeg
├─ takekit-media/   Windows API capture: WGC screen/window, Media Foundation camera and mic,
│                   WASAPI loopback, MF SinkWriter (H.264 + AAC into MP4)
├─ takekit-cli/     thin shell, binary name `takekit`
└─ takekit-gui/     Tauri 2 panel: commands, control service, journal, tray, overlay, UI
```

- The core decides, the front ends only ask. GUI and CLI share one implementation; there is no
  second state machine.
- The Windows-API backend runs everything in one process on one clock — that is what keeps sources
  aligned without a post-production step.
- Window identity is the window handle plus its owning process, not the title: window titles change
  (a browser tab, an editor's open file) and handle numbers get recycled.

## Docs

| What | Where |
|---|---|
| Design contract: screens, components, persistence, keyboard | [`DESIGN.md`](DESIGN.md) *(Chinese)* |
| Development notes: architecture map, implementation contracts, probes, trap list | [`docs/development.md`](docs/development.md) *(Chinese)* |
| The Windows-API capture backend in detail | [`docs/api-recording.md`](docs/api-recording.md) *(Chinese)* |
| The agent control surface: HTTP routes ↔ MCP tools ↔ CLI | [`docs/agent-control.md`](docs/agent-control.md) *(Chinese)* |

## Limitations

- **Windows only.** Too much of it is Win32/WGC/WASAPI to port casually.
- **Desktop sound is the whole machine.** WASAPI loopback is per playback endpoint, not per
  monitor; recording "this monitor's audio" is not a thing the OS offers.
- **The `--windows` desktop path records one monitor** — the first, or the one the crop region sits
  on.
- **No installer and no code signing**, so Windows SmartScreen will complain about the binary.
- **Camera capture over Media Foundation is the least-tested path**: it needs a device that
  enumerates through MF, which virtual cameras often do not.

## License

MIT. See [LICENSE](LICENSE).
