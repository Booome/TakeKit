//! The external control surface (`docs/agent-control.md`): a loopback HTTP
//! server so an AI agent — or a script, or the `takekit ctl` CLI — can drive
//! the panel while it runs.
//!
//! Design in one paragraph: the server owns **no** logic. Every route ends in
//! the same functions the panel's own `invoke()` calls land in (`take::begin`,
//! `config::Config::update`, `journal`), so there is exactly one state machine
//! and one source list. It listens on `127.0.0.1:0` (the OS picks the port),
//! requires a per-run bearer token, and writes `{port, token, pid}` to
//! `control.json` next to `config.json` so clients can find it. The threat
//! model is "the same user is trusted" — the token stops drive-by calls from
//! web pages and stray processes, not from malware already running as the
//! user. Every write mirrors into the panel (`ctl:changed`), so an agent can
//! never change something the user cannot see.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tauri::Emitter;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use takekit_core::source::{Backend, Capture, Crop, ScreenRect, Source};

use crate::config;
use crate::journal;
use crate::proto::{StartRequest, UiError};
use crate::take;

/// The session-wide quality a start request falls back on, the same number the
/// panel's `DEFAULT_QUALITY` sends.
const DEFAULT_QUALITY: u8 = 20;
/// Same for a source's frame rate when the convenience form does not say.
const DEFAULT_FPS: u32 = 30;
/// A request body bigger than this is refused rather than buffered: the
/// largest legitimate one is a full source list, nowhere near this.
const MAX_BODY: u64 = 2 * 1024 * 1024;

/// `%APPDATA%\TakeKit\control.json` — beside `config.json`, same ACLs. The
/// directory is `fsx::app_dir()`, the one definition the CLI client reads the
/// same file through.
pub fn control_path() -> PathBuf {
    takekit_core::fsx::app_dir().join("control.json")
}

struct Running {
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Start the server if it is not already up. Returns the port it landed on.
///
/// Idempotent on purpose: the settings switch can flip on an already-running
/// server (a hand-edited config, a double click) and that is not an error.
pub fn start(app: tauri::AppHandle) -> Result<u16, String> {
    let mut guard = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(running) = guard.as_ref() {
        return Ok(running.port);
    }

    let server = Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| "the listener is not on an IP socket".to_string())?
        .port();
    let token = fresh_token();
    let server = Arc::new(server);

    write_discovery(port, &token)?;

    let thread = {
        let server = Arc::clone(&server);
        let token = token.clone();
        std::thread::spawn(move || {
            while let Ok(request) = server.recv() {
                // Handled inline, one request at a time. A take's stop takes
                // as long as it takes whatever; the clients are agents talking
                // in sequence and nobody is paging a control API at 60Hz.
                //
                // A panic in one handler must not take the door down for
                // every request after it: the routes reach into the panel's
                // own state, and a bug there is exactly when the caller
                // still needs an answer. The request is consumed either way,
                // so a panicking handler costs that one client a closed
                // connection instead of costing the rest a hang.
                let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    respond(&app, &token, request)
                }));
                if handled.is_err() {
                    crate::probe("control:handler-panicked");
                }
            }
        })
    };

    *guard = Some(Running {
        server,
        thread: Some(thread),
        port,
    });
    Ok(port)
}

/// Stop the server if it is up. Safe to call when it is not.
pub fn stop() {
    let running = {
        let mut guard = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    if let Some(mut running) = running {
        // The discovery file goes first: a client that finds it a moment after
        // the server is gone must see "panel not running", not a dead port.
        let _ = std::fs::remove_file(control_path());
        running.server.unblock();
        if let Some(thread) = running.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 32 random bytes as hex. Per run, never persisted beyond `control.json`:
/// the next start gets a new one, so a leaked token expires with the process.
fn fresh_token() -> String {
    let mut bytes = [0u8; 32];
    // getrandom reads the OS CSPRNG and cannot fail on Windows; the fallback
    // is for the impossible case rather than an error path anyone can hit.
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (nanos >> (i % 16 * 4)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_discovery(port: u16, token: &str) -> Result<(), String> {
    write_discovery_at(&control_path(), port, token)
}

/// The discovery write, split out so the "creates its own directory" half
/// is testable against a scratch path.
fn write_discovery_at(path: &std::path::Path, port: u16, token: &str) -> Result<(), String> {
    // This can be the **first file the panel ever writes**: `setup` runs
    // before any window interaction, and `write_atomic` expects the
    // directory to exist (config and journal create theirs on the way in).
    // Without this a fresh machine's control surface never comes up — the
    // switch is on by default and the door stays shut, with nothing but the
    // probe log to say so. Found by an E2E on an isolated APPDATA.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = json!({
        "port": port,
        "token": token,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string();
    takekit_core::fsx::write_atomic(path, &body).map_err(|e| e.to_string())
}

// ── request plumbing ────────────────────────────────────────────────────────

/// An error on its way out as JSON: HTTP status, stable code, English message.
/// The codes are the machine face (`docs/agent-control.md` §6.2) — the panel's
/// human-facing `err.*` sentences are a different, translated surface.
type Fail = (u16, String, String);

fn respond(app: &tauri::AppHandle, token: &str, mut request: Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url, String::new()),
    };

    let authed = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|given| given.trim() == token);

    let outcome = if !authed {
        Err(fail(
            401,
            "unauthorized",
            "missing or wrong bearer token; read it from control.json",
        ))
    } else {
        let body = read_body(&mut request);
        match body {
            Err(msg) => Err(bad(msg)),
            Ok(body) => route(app, &method, &path, &query, body),
        }
    };

    // The agent's failures leave a line (docs/agent-control.md §6.4): the
    // HTTP answer reaches the caller, but the user looking at the log later
    // has nothing unless the panel writes it down here. Reads stay quiet (a
    // poll is not an event), and so do bad tokens (a scan is not a story).
    if let Err((status, code, message)) = &outcome {
        if trail_worthy(&method, *status) {
            let entry = journal::LogRecord {
                at: journal::now_ms(),
                level: trail_level(*status),
                text: format!("control: {method} {path} failed ({code})"),
                detail: Some(message.clone()),
                dir: None,
            };
            let _ = journal::Journal::update(|j| j.push_log(entry));
        }
    }

    let (status, payload) = match outcome {
        Ok(value) => (200, value),
        Err((status, code, message)) => (status, json!({ "code": code, "message": message })),
    };

    let text = payload.to_string();
    let header =
        Header::from_bytes("Content-Type", "application/json").expect("literal header is valid");
    let _ = request.respond(
        Response::from_string(text)
            .with_status_code(StatusCode(status))
            .with_header(header),
    );
}

fn read_body(request: &mut Request) -> Result<Value, String> {
    let mut buf = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Ok(Value::Null);
    }
    // A bad body is a client mistake, not a crash; serde's first sentence is
    // the useful half of the answer.
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

fn route(
    app: &tauri::AppHandle,
    method: &Method,
    path: &str,
    query: &str,
    body: Value,
) -> Result<Value, Fail> {
    match (method, path) {
        (Method::Get, "/state") => Ok(snapshot(app)),
        (Method::Get, "/devices") => Ok(devices(query)),
        (Method::Get, "/history") => Ok(history_view()),
        (Method::Get, "/log") => log_view(query),
        (Method::Post, "/sources") => add_source(app, body),
        (Method::Put, "/sources") => set_sources(app, body),
        (Method::Delete, p) if p.starts_with("/sources/") => {
            let index = p["/sources/".len()..]
                .parse::<usize>()
                .map_err(|_| bad("index must be a number from /state"))?;
            remove_source(app, index)
        }
        (Method::Patch, "/config") => patch_config(app, body),
        (Method::Post, "/record/start") => record_start(app, body),
        (Method::Post, "/record/stop") => record_stop(app),
        (Method::Delete, "/log") => clear_log(app, body),
        (Method::Delete, p) if p.starts_with("/log/") => delete_log(app, &p["/log/".len()..], body),
        (Method::Delete, p) if p.starts_with("/history/") => {
            delete_take(app, &p["/history/".len()..], body)
        }
        _ => Err(fail(404, "not-found", format!("no route {method} {path}"))),
    }
}

/// The one constructor every error goes through, so the shape (status, stable
/// code, English sentence) cannot drift apart between call sites.
fn fail(status: u16, code: &str, message: impl Into<String>) -> Fail {
    (status, code.to_string(), message.into())
}

fn bad(message: impl Into<String>) -> Fail {
    fail(400, "bad-request", message)
}

fn not_found(message: impl Into<String>) -> Fail {
    fail(404, "not-found", message)
}

/// The panel redraws from this: after every write the agent can see, the
/// window is told what moved. `what` is one of `sources` / `config` /
/// `history` / `status` — the payload is a label, not a filter: the listener
/// re-syncs the whole panel, which is the point (a stale pane is the bug
/// this exists to prevent).
fn mirror(app: &tauri::AppHandle, what: &str) {
    let _ = app.emit("ctl:changed", json!({ "what": what }));
}

// ── snapshot ────────────────────────────────────────────────────────────────

/// Everything an agent needs to answer "what is going on": the settings, the
/// source list with stable indices, the live take, the history.
///
/// Shapes are the ones already on disk (`config.json`, `journal.json`) plus an
/// `index` field per source — deliberately no second schema to keep in sync.
fn snapshot(app: &tauri::AppHandle) -> Value {
    let cfg = config::Config::load();
    let status = take::status(app);
    let journal = journal::Journal::load();

    let sources: Vec<Value> = cfg
        .sources
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("index".to_string(), json!(i));
            }
            v
        })
        .collect();

    json!({
        "app": { "version": env!("CARGO_PKG_VERSION") },
        "config": {
            "hotkey": cfg.hotkey,
            "countdown_secs": cfg.countdown_secs,
            "finalize_panel": cfg.finalize_panel,
            "out_root": cfg.out_root,
            // `null` = PATH lookup, like `PATCH /config` takes it.
            "ffmpeg_path": cfg.ffmpeg_path,
            // The same lowercase names `PATCH /config` accepts, `null` =
            // auto — one vocabulary in both directions.
            "encoder": serde_json::to_value(cfg.encoder).unwrap_or(Value::Null),
            "backend": cfg.backend,
            "marks": cfg.marks,
            "raise_sources": cfg.raise_sources,
            "language": cfg.language,
            "theme": cfg.theme,
            "control": cfg.control,
        },
        "sources": sources,
        "status": {
            "recording": status.recording,
            "elapsed": status.elapsed,
            "error": status.error,
            "live": status.sources,
        },
        "history": {
            "takes": journal.takes,
            "log": journal.log,
        },
    })
}

fn history_view() -> Value {
    let journal = journal::Journal::load();
    json!({ "takes": journal.takes, "log": journal.log })
}

// ── log ─────────────────────────────────────────────────────────────────────

/// `GET /log` — the warning/error register on its own, for when a take came
/// out wrong and the question is "why". `/history` carries the same entries
/// tucked beside the takes; this is the door named after the thing.
///
/// `?level=warn|error` narrows it. An unknown word is a typo, not "all".
fn log_view(query: &str) -> Result<Value, Fail> {
    let level = match query_param(query, "level") {
        None => None,
        Some(text) => Some(parse_level(&text).ok_or_else(|| bad("level must be warn or error"))?),
    };
    let journal = journal::Journal::load();
    let entries: Vec<&journal::LogRecord> = journal
        .log
        .iter()
        .filter(|e| level.is_none_or(|want| e.level == want))
        .collect();
    Ok(json!(entries))
}

fn parse_level(text: &str) -> Option<journal::Level> {
    match text {
        "warn" => Some(journal::Level::Warn),
        "error" => Some(journal::Level::Error),
        _ => None,
    }
}

/// `DELETE /log` — wipe the register (the panel's own clear button does the
/// same). Destructive like the take delete, so the same `"confirm": true`.
fn clear_log(app: &tauri::AppHandle, body: Value) -> Result<Value, Fail> {
    if body.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err(bad(
            "clearing the log needs {\"confirm\": true} in the body",
        ));
    }
    let _ = journal::Journal::update(|j| j.clear(journal::Pane::Log));
    mirror(app, "history");
    Ok(snapshot(app))
}

/// `DELETE /log/{at}` — one line out of the register, matched by the `at`
/// stamp it carries (what `/log` hands out and the row's own delete button
/// sends). The history's single delete is this shape's sibling; the same
/// `"confirm": true` applies, and a stamp nothing carries is a 404 rather
/// than a quiet pass.
fn delete_log(app: &tauri::AppHandle, at_text: &str, body: Value) -> Result<Value, Fail> {
    if body.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err(bad(
            "deleting a log entry needs {\"confirm\": true} in the body",
        ));
    }
    let at: u64 = percent_decode(at_text)
        .parse()
        .map_err(|_| bad("the id is the entry's `at` stamp (unix ms), from /log"))?;
    let mut removed = false;
    let _ = journal::Journal::update(|j| removed = j.remove_log(at));
    if !removed {
        return Err(not_found(format!(
            "no log entry with at={at}; /log lists them (they may have aged out)"
        )));
    }
    mirror(app, "history");
    Ok(snapshot(app))
}

/// Whether a failed request earns a line in the log. Mutations only: a
/// polling `GET /state` failing is noise, and a rejected token is an attacker
/// or a stale copy of `control.json` — neither is a story about this session.
fn trail_worthy(method: &Method, status: u16) -> bool {
    status != 401 && !matches!(method, Method::Get)
}

/// Server-side trouble is an error, a client-side refusal a warning.
fn trail_level(status: u16) -> journal::Level {
    if status >= 500 {
        journal::Level::Error
    } else {
        journal::Level::Warn
    }
}

// ── devices ─────────────────────────────────────────────────────────────────

/// What can be added, as the pickers see it. `kind` narrows it (`window` /
/// `desktop` / `camera` / `mic` / `system-audio` / `window-audio`); anything
/// else returns all four groups.
///
/// Each group is built only when it is asked for: the camera and microphone
/// walk is the slow one, and an agent listing windows should not pay for it.
/// The two sound-only kinds pick from the same lists as their picture twins
/// (`window-audio` chooses a window; `system-audio` chooses nothing at all).
fn devices(query: &str) -> Value {
    let kind = query_param(query, "kind").unwrap_or_default();
    match kind.as_str() {
        "window" | "windows" | "window-audio" | "window_audio" => json!(window_group()),
        "desktop" | "monitor" | "screen" => json!(desktop_group()),
        "camera" => json!(camera_group()),
        "mic" | "microphone" => json!(mic_group()),
        "system-audio" | "system_audio" => json!([]),
        _ => json!({
            "windows": window_group(),
            "desktops": desktop_group(),
            "cameras": camera_group(),
            "mics": mic_group(),
        }),
    }
}

/// Top-level windows only (the panel's own are noise to an agent), with the
/// rectangle flattened: `hwnd` is the identity, the rest is where it is.
fn window_group() -> Vec<Value> {
    takekit_core::source::list_windows("", Some(std::process::id()))
        .into_iter()
        .map(|w| {
            json!({
                "hwnd": w.hwnd,
                "pid": w.pid,
                "title": w.title,
                "x": w.rect.x, "y": w.rect.y, "w": w.rect.w, "h": w.rect.h,
            })
        })
        .collect()
}

fn desktop_group() -> Vec<Value> {
    takekit_media::monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|m| json!({ "index": m.index, "x": m.x, "y": m.y, "w": m.w, "h": m.h }))
        .collect()
}

fn camera_group() -> Vec<Value> {
    // The merged enumeration (dshow ∪ MF) the panel's picker also shows, so
    // an agent and the user look at the same list. Each entry carries the
    // backends that can open it (`dshow` = the ffmpeg backend, `mf` = the
    // windows backend) and the DirectShow liveness read: `live: false` is a
    // registered virtual camera whose host app is off — start the host app
    // instead of discovering it at record time.
    takekit_core::devices::cameras(takekit_core::ffmpeg::locate().as_deref())
        .into_iter()
        .map(|c| {
            json!({
                "name": c.name,
                "live": c.live,
                "dshow": c.dshow,
                "mf": c.mf,
                "link": c.link,
            })
        })
        .collect()
}

fn mic_group() -> Vec<Value> {
    takekit_media::devices::list()
        .mics
        .iter()
        .map(|m| json!({ "name": m.name, "endpoint": m.endpoint }))
        .collect()
}

/// One query pair's value, percent-decoded — the CLI client escapes what it
/// writes (`client::url_escape`), so a space arrives as `%20` and has to
/// come back out before it is matched against the known words.
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

// ── sources ─────────────────────────────────────────────────────────────────

/// `POST /sources`. Two shapes, one route:
///
/// * the raw `Source` from `config.json` (`{"id", "capture", "fps", ...}`) —
///   what the panel itself stores, accepted verbatim;
/// * a convenience spec (`{"kind": "window", "target": "微信"}`) resolved here,
///   because "the WeChat window" is what an agent hears and `hwnd` is what the
///   recorder needs. Ambiguous or missing targets fail with the candidates
///   attached rather than guessing.
fn add_source(app: &tauri::AppHandle, body: Value) -> Result<Value, Fail> {
    let cfg = config::Config::load();
    if recording_now(app) {
        return Err(conflict("the take is recording; the source list is frozen"));
    }

    let source = if body.get("capture").is_some() {
        let source: Source = serde_json::from_value(body).map_err(|e| bad(e.to_string()))?;
        check_new_source(&source, &cfg.sources)?;
        source
    } else {
        resolve_spec(&body, &cfg.sources)?
    };

    config::Config::update(|c| c.sources.push(source)).map_err(internal)?;
    mirror(app, "sources");
    Ok(snapshot(app))
}

fn set_sources(app: &tauri::AppHandle, body: Value) -> Result<Value, Fail> {
    if recording_now(app) {
        return Err(conflict("the take is recording; the source list is frozen"));
    }
    let sources: Vec<Source> = serde_json::from_value(body).map_err(|e| bad(e.to_string()))?;
    // The whole list at once: every id has to be usable and they have to be
    // pairwise unique — the same rule the one-at-a-time door holds.
    for (i, s) in sources.iter().enumerate() {
        if !safe_source_id(&s.id) {
            return Err(bad(format!(
                "source id {:?} is not a usable file name: letters, digits, '-' and '_' only",
                s.id
            )));
        }
        if sources[..i].iter().any(|other| other.id == s.id) {
            return Err(conflict(format!(
                "source id {:?} appears twice; ids name the output files, so they must be unique",
                s.id
            )));
        }
    }
    config::Config::update(|c| c.sources = sources).map_err(internal)?;
    mirror(app, "sources");
    Ok(snapshot(app))
}

/// A source `id` is a file name — `win` becomes `win.mkv` inside the take
/// folder — so the raw-JSON doors have to hold the line the generated ids
/// hold by construction. The picker only ever makes `win` / `screen` / `cam`
/// plus a numeric suffix; anything with a separator, a `..` or a dot either
/// escapes the take folder or makes an encoder write outside it.
fn safe_source_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// The one-at-a-time door's guard: a usable id that is not already in the
/// list. Two sources sharing an id write the same file and break the
/// per-source stop, which finds its target by id.
fn check_new_source(source: &Source, existing: &[Source]) -> Result<(), Fail> {
    if !safe_source_id(&source.id) {
        return Err(bad(format!(
            "source id {:?} is not a usable file name: letters, digits, '-' and '_' only",
            source.id
        )));
    }
    if existing.iter().any(|s| s.id == source.id) {
        return Err(conflict(format!(
            "source id {:?} is already in the list; ids name the output files, so they must be unique",
            source.id
        )));
    }
    Ok(())
}

fn remove_source(app: &tauri::AppHandle, index: usize) -> Result<Value, Fail> {
    if recording_now(app) {
        return Err(conflict("the take is recording; the source list is frozen"));
    }
    let cfg = config::Config::load();
    if index >= cfg.sources.len() {
        return Err(not_found(format!(
            "no source at index {index}; /state lists them"
        )));
    }
    config::Config::update(|c| {
        c.sources.remove(index);
    })
    .map_err(internal)?;
    mirror(app, "sources");
    Ok(snapshot(app))
}

/// The two bounded numbers, as pure functions so the bounds are testable
/// without a panel. Bounded rather than cast: `as u32` wraps, and a take at
/// four billion frames per second is a typo, not a request.
fn bounded_fps(body: &Value) -> Result<u32, Fail> {
    match body.get("fps").and_then(Value::as_u64) {
        None => Ok(DEFAULT_FPS),
        Some(fps) if (1..=240).contains(&fps) => Ok(fps as u32),
        Some(fps) => Err(bad(format!("fps must be 1..=240, got {fps}"))),
    }
}

/// `None` when absent — the session-wide quality applies — and out of range
/// is a refusal, never a wrap into a different number.
fn bounded_quality(body: &Value) -> Result<Option<u8>, Fail> {
    match body.get("quality").and_then(Value::as_u64) {
        None => Ok(None),
        Some(q) if q <= 51 => Ok(Some(q as u8)),
        Some(q) => Err(bad(format!(
            "quality must be 0..=51 (CRF-flavoured), got {q}"
        ))),
    }
}

/// Turn the convenience spec into a `Source`, or say exactly why not.
fn resolve_spec(body: &Value, existing: &[Source]) -> Result<Source, Fail> {
    let kind = body.get("kind").and_then(Value::as_str).ok_or_else(|| {
        bad("need `kind` (window/desktop/camera/mic/system-audio/window-audio) or a raw `capture`")
    })?;
    let fps = bounded_fps(body)?;
    let quality = bounded_quality(body)?;
    let crop = match body.get("crop") {
        None | Some(Value::Null) => Crop::default(),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| bad(e.to_string()))?,
    };

    let mut source = match kind {
        "window" | "window-audio" => {
            // Picture or just the sound of that window's process tree: the
            // same title match and the same ambiguity refusal, only the
            // capture differs — so one arm keeps both honest.
            let target = body
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("a window source needs `target` (a title substring)"))?;
            let matches = takekit_core::source::list_windows(target, Some(std::process::id()));
            match matches.len() {
                0 => Err(not_found(format!(
                    "no window title contains {target:?}; {} windows visible: {}",
                    takekit_core::source::list_windows("", Some(std::process::id())).len(),
                    title_digest()
                ))),
                1 => {
                    let w = &matches[0];
                    let capture = if kind == "window" {
                        Capture::Window {
                            hwnd: w.hwnd,
                            pid: w.pid,
                            title: w.title.clone(),
                            crop,
                        }
                    } else {
                        Capture::WindowAudio {
                            hwnd: w.hwnd,
                            pid: w.pid,
                            title: w.title.clone(),
                        }
                    };
                    Ok(Source::new(
                        unique_id(existing, if kind == "window" { "win" } else { "win-audio" }),
                        capture,
                        fps,
                    ))
                }
                _ => Err(fail(
                    409,
                    "ambiguous",
                    format!(
                        "{} windows match {target:?}: {}",
                        matches.len(),
                        matches
                            .iter()
                            .map(|w| format!("{:?} (hwnd {})", w.title, w.hwnd))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            }
        }
        "desktop" | "monitor" | "screen" => {
            // No monitor = the whole virtual desktop, the same thing the
            // panel's "entire desktop" row adds.
            let capture = match body.get("monitor").and_then(Value::as_u64) {
                None => Capture::desktop(None),
                Some(index) => {
                    let rect = takekit_media::monitors()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|m| m.index == index as usize)
                        .map(|m| ScreenRect {
                            x: m.x,
                            y: m.y,
                            w: m.w,
                            h: m.h,
                        })
                        .ok_or_else(|| {
                            bad(format!(
                                "no monitor #{index}; /devices?kind=desktop lists them"
                            ))
                        })?;
                    Capture::desktop(Some(rect))
                }
            };
            Ok(Source::new(unique_id(existing, "desktop"), capture, fps))
        }
        "camera" => {
            let target = body
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("a camera source needs `target` (a name substring)"))?;
            let list = takekit_media::devices::list();
            let hits: Vec<_> = list
                .cameras
                .iter()
                .filter(|c| c.name.contains(target) || c.link.contains(target))
                .collect();
            match hits.len() {
                1 => Ok(Source::new(
                    unique_id(existing, "cam"),
                    Capture::Camera {
                        device: hits[0].name.clone(),
                        crop,
                    },
                    fps,
                )),
                0 => Err(not_found(format!(
                    "no camera name or link contains {target:?}"
                ))),
                n => Err(fail(
                    409,
                    "ambiguous",
                    format!(
                        "{n} cameras match {target:?}: {}",
                        hits.iter()
                            .map(|c| c.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            }
        }
        "mic" | "microphone" => {
            let target = body
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("a mic source needs `target` (a name substring)"))?;
            let list = takekit_media::devices::list();
            let hits: Vec<_> = list
                .mics
                .iter()
                .filter(|m| m.name.contains(target) || m.endpoint.contains(target))
                .collect();
            match hits.len() {
                1 => Ok(Source::new(
                    unique_id(existing, "mic"),
                    Capture::Mic {
                        device: hits[0].name.clone(),
                    },
                    fps,
                )),
                0 => Err(not_found(format!(
                    "no microphone name or endpoint contains {target:?}"
                ))),
                n => Err(fail(
                    409,
                    "ambiguous",
                    format!(
                        "{n} microphones match {target:?}: {}",
                        hits.iter()
                            .map(|m| m.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            }
        }
        "system-audio" => {
            // No target and no device pick: the default render endpoint is
            // the one thing a WASAPI loopback can mean.
            Ok(Source::new(
                unique_id(existing, "sys-audio"),
                Capture::SystemAudio {},
                fps,
            ))
        }
        other => Err(bad(format!(
            "unknown kind {other:?}; window, desktop, camera, mic, system-audio and window-audio are the six"
        ))),
    }?;
    // The per-source quality override, only when asked for: `None` means the
    // session-wide quality, which is what the panel's own `addSource` writes.
    source.quality = quality;
    // "sound": true puts this source's sound into its own file — for a window
    // its process tree, for the desktop everything the speakers play (Windows
    // has no per-monitor sound, so "this screen's sound" is the whole
    // machine's). `kind: window-audio` / `system-audio` stay for recording just
    // the sound, with no picture to put it in.
    if matches!(
        source.capture,
        Capture::Window { .. } | Capture::Desktop { .. }
    ) {
        source.audio = body.get("sound").and_then(Value::as_bool).unwrap_or(false);
    }
    Ok(source)
}

/// An id that survives being a file name and does not collide with the ones
/// already in the list (`win`, `win-2`, `win-3`, …).
fn unique_id(existing: &[Source], base: &str) -> String {
    let taken: Vec<&str> = existing.iter().map(|s| s.id.as_str()).collect();
    if !taken.contains(&base) {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    base.to_string()
}

/// A one-line list of window titles, for the "nothing matched" error. Truncated:
/// an error that dumps two hundred titles helps nobody.
fn title_digest() -> String {
    let titles: Vec<String> = takekit_core::source::list_windows("", Some(std::process::id()))
        .into_iter()
        .map(|w| w.title)
        .filter(|t| !t.trim().is_empty())
        .take(12)
        .collect();
    titles.join(" | ")
}

// ── config ──────────────────────────────────────────────────────────────────

/// `PATCH /config`: partial by design — only the keys present are touched, and
/// unknown keys are refused so a typo cannot silently do nothing.
///
/// Recording owns the capture setup while it runs: encoder, backend, marks,
/// raise and out_root are frozen (409). The UI knobs (hotkey, countdown, stop
/// screen, language, theme) stay live because they affect the next take or the
/// surface, not the take in flight.
///
/// Everything is validated **before** anything is written: a bad value leaves
/// the config exactly as it was, and a hotkey Windows refuses keeps the old
/// one working.
fn patch_config(app: &tauri::AppHandle, body: Value) -> Result<Value, Fail> {
    let obj = body
        .as_object()
        .ok_or_else(|| bad("body must be a JSON object of settings"))?;
    if obj.is_empty() {
        return Err(bad("nothing to patch"));
    }

    const KNOWN: &[&str] = &[
        "hotkey",
        "countdown_secs",
        "finalize_panel",
        "out_root",
        "ffmpeg_path",
        "encoder",
        "backend",
        "marks",
        "raise_sources",
        "language",
        "theme",
    ];
    for key in obj.keys() {
        if !KNOWN.contains(&key.as_str()) {
            return Err(bad(format!(
                "unknown setting {key:?}; the {} are {}",
                KNOWN.len(),
                KNOWN.join(", ")
            )));
        }
    }
    const FROZEN_WHILE_RECORDING: &[&str] = &[
        "out_root",
        "ffmpeg_path",
        "encoder",
        "backend",
        "marks",
        "raise_sources",
    ];
    if recording_now(app)
        && obj
            .keys()
            .any(|k| FROZEN_WHILE_RECORDING.contains(&k.as_str()))
    {
        return Err(conflict(
            "the take is recording; capture settings are frozen until it stops",
        ));
    }

    // Validate first, apply second — see the doc comment. Strict on purpose:
    // a silently coerced value (a string "yes" reading as true, a float
    // countdown reading as 0) is a setting that changed without anyone
    // asking, which is the failure this ordering exists to prevent.
    if let Some(v) = obj.get("encoder") {
        let ok = v.is_null() || matches!(v.as_str(), Some("nvenc" | "qsv" | "amf" | "x264"));
        if !ok {
            return Err(bad(
                "encoder must be nvenc, qsv, amf, x264, or null for auto",
            ));
        }
    }
    if let Some(v) = obj.get("backend") {
        // The two names, exactly: the config file's serde rejects anything
        // else, so this door must not be the lenient one.
        if !matches!(v.as_str(), Some("windows") | Some("ffmpeg")) {
            return Err(bad("backend must be \"windows\" or \"ffmpeg\""));
        }
    }
    for key in ["countdown_secs"] {
        if let Some(v) = obj.get(key) {
            // The whole domain the config file and `set_countdown` hold is a
            // u8. Out of range is a refusal, never a clamp into 255: a
            // coerced value is a setting that changed without anyone
            // asking, which is the failure this ordering exists to prevent.
            match v.as_u64() {
                Some(n) if n <= u8::MAX as u64 => {}
                _ => {
                    return Err(bad(format!(
                        "{key} must be a whole number of seconds from 0 to 255 (0 = off)"
                    )));
                }
            }
        }
    }
    for key in ["finalize_panel", "marks", "raise_sources"] {
        if let Some(v) = obj.get(key) {
            if !v.is_boolean() {
                return Err(bad(format!("{key} must be true or false")));
            }
        }
    }
    for key in ["out_root", "hotkey"] {
        if let Some(v) = obj.get(key) {
            if v.as_str().is_none() {
                return Err(bad(format!("{key} must be a string")));
            }
        }
    }
    if let Some(v) = obj.get("ffmpeg_path") {
        // `null` and "" both mean "back to the PATH lookup"; a string is the
        // path. Anything else is a refusal, like every other key here.
        let ok = v.is_null() || v.as_str().is_some();
        if !ok {
            return Err(bad("ffmpeg_path must be a string or null"));
        }
    }

    // The registrar is asked before the file is written: a combination Windows
    // refuses must leave the old one working and the config agreeing with it.
    // The normalized spelling rides into the one update below, so the hotkey
    // is never persisted by a write of its own.
    let mut hotkey = None;
    if let Some(v) = obj.get("hotkey") {
        let wanted = v.as_str().unwrap_or_default().to_string();
        match take::install_hotkey(app, &wanted) {
            Ok(normalized) => hotkey = Some(normalized),
            Err(e) => return Err(ui_fail(&e)),
        }
    }

    let mut language_changed = false;
    let mut theme_changed = false;
    config::Config::update(|cfg| {
        if let Some(hotkey) = hotkey {
            cfg.hotkey = hotkey;
        }
        if let Some(v) = obj.get("countdown_secs") {
            // Validated above to be 0..=255, so the cast is exact.
            cfg.countdown_secs = v.as_u64().unwrap_or(0) as u8;
        }
        if let Some(v) = obj.get("finalize_panel") {
            cfg.finalize_panel = v.as_bool().unwrap_or(true);
        }
        if let Some(v) = obj.get("out_root") {
            cfg.out_root = v.as_str().map(str::to_string);
        }
        if let Some(v) = obj.get("ffmpeg_path") {
            let path = v.as_str().map(str::to_string).filter(|s| !s.is_empty());
            takekit_core::ffmpeg::set_path(path.as_deref().map(std::path::PathBuf::from));
            cfg.ffmpeg_path = path;
        }
        if let Some(v) = obj.get("encoder") {
            cfg.encoder = match v.as_str() {
                Some("nvenc") => Some(takekit_core::encoder::Encoder::Nvenc),
                Some("qsv") => Some(takekit_core::encoder::Encoder::Qsv),
                Some("amf") => Some(takekit_core::encoder::Encoder::Amf),
                Some("x264") => Some(takekit_core::encoder::Encoder::X264),
                _ => None,
            };
        }
        if let Some(v) = obj.get("backend") {
            cfg.backend = match v.as_str() {
                Some("ffmpeg") => Backend::Ffmpeg,
                _ => Backend::Windows,
            };
        }
        if let Some(v) = obj.get("marks") {
            cfg.marks = v.as_bool().unwrap_or(true);
        }
        if let Some(v) = obj.get("raise_sources") {
            cfg.raise_sources = v.as_bool().unwrap_or(false);
        }
        if let Some(v) = obj.get("language") {
            let lang = if v.as_str() == Some("en") { "en" } else { "zh" };
            language_changed = lang != cfg.language;
            cfg.language = lang.to_string();
        }
        if let Some(v) = obj.get("theme") {
            let theme = match v.as_str() {
                Some("light") => "light",
                Some("dark") => "dark",
                _ => "auto",
            };
            theme_changed = theme != cfg.theme;
            cfg.theme = theme.to_string();
        }
    })
    .map_err(internal)?;

    // Every window is told, like `set_language` / `set_theme` do: the tray is
    // a second document and must not keep the old strings.
    if language_changed || theme_changed {
        let cfg = config::Config::load();
        let _ = app.emit(
            "ui:settings",
            json!({
                "language": cfg.effective_language(),
                "theme": cfg.effective_theme(),
            }),
        );
    }
    mirror(app, "config");
    Ok(snapshot(app))
}

// ── recording ───────────────────────────────────────────────────────────────

fn recording_now(app: &tauri::AppHandle) -> bool {
    take::status(app).recording
}

/// `POST /record/start`. The take runs on the current setup (what `/state`
/// shows) — set the sources first, then start. The countdown is the one in
/// the settings; `0` starts immediately.
fn record_start(app: &tauri::AppHandle, body: Value) -> Result<Value, Fail> {
    let cfg = config::Config::load();
    let quality = bounded_quality(&body)?.unwrap_or(DEFAULT_QUALITY);

    let req = StartRequest {
        sources: cfg.sources.clone(),
        out_root: cfg.out_root.clone().unwrap_or_else(|| "takes".to_string()),
        quality,
        marks: cfg.marks,
        encoder: cfg.encoder,
        backend: cfg.backend,
        raise_sources: cfg.raise_sources,
    };
    if req.sources.is_empty() {
        return Err(fail(
            400,
            "no-sources",
            "add a source first; /state shows the list",
        ));
    }

    match take::begin(app, req) {
        Ok(started) => {
            // The panel's own start hides the window and raises the veil; the
            // mirror is what redraws the bar as recording.
            mirror(app, "status");
            Ok(json!({
                "started": started,
                "countdown_secs": cfg.countdown_secs,
            }))
        }
        Err(e) => Err(ui_fail(&e)),
    }
}

/// `POST /record/stop`. Blocks until the files are closed — the answer is the
/// take that landed, directory included.
fn record_stop(app: &tauri::AppHandle) -> Result<Value, Fail> {
    // `announce`: nobody in the panel pressed stop, so the panel needs the
    // event to learn the take is over — the same reason the tray asks for
    // one. Success is unaffected (`take:done` goes either way); a *failed*
    // stop is exactly the case where the user saw no button and must still
    // see the reason.
    match take::stop(app, true) {
        Ok(result) => {
            mirror(app, "history");
            Ok(serde_json::to_value(result).unwrap_or(Value::Null))
        }
        Err(e) => {
            // The failed take still landed in the history (stop records it),
            // so the panes are resynced even though the request failed.
            mirror(app, "history");
            Err(ui_fail(&e))
        }
    }
}

// ── history ─────────────────────────────────────────────────────────────────

/// `DELETE /history/{id}` where `id` is the take folder's name (the `dir` in
/// `/state` with its parent stripped). Destructive on purpose, so the body has
/// to say `"confirm": true` — an agent that misreads a schema deletes nothing.
fn delete_take(app: &tauri::AppHandle, id: &str, body: Value) -> Result<Value, Fail> {
    if body.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err(bad("deleting a take needs {\"confirm\": true} in the body"));
    }
    let journal = journal::Journal::load();
    let wanted = percent_decode(id);
    let hit = journal
        .takes
        .iter()
        .find(|t| t.dir == wanted || folder_name(&t.dir) == wanted)
        .cloned();
    let Some(record) = hit else {
        return Err(not_found(format!(
            "no take named {wanted:?}; /history lists them"
        )));
    };

    // Same order and same guard as the panel's `take_delete`: the folder is
    // removed first and `take.json` inside it is what proves the path was one
    // of ours.
    takekit_core::take::delete_dir(&record.dir).map_err(internal)?;
    let _ = journal::Journal::update(|j| {
        j.takes.retain(|t| t.dir != record.dir);
        for e in &mut j.log {
            if e.dir.as_deref() == Some(record.dir.as_str()) {
                e.dir = None;
            }
        }
    });
    mirror(app, "history");
    Ok(snapshot(app))
}

fn folder_name(dir: &str) -> String {
    std::path::Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Minimal URL-unescape for a path segment (`%5C` and friends). Agents
/// quote; browsers quote; so must we.
///
/// Byte-level on purpose: a URL is bytes, so a stray `%` can sit next to
/// anything — including a multi-byte character. Slicing the input as a
/// `str` would panic on that char boundary; passing the bytes through
/// instead lets the caller's own parse (`.parse::<u64>()`, a name match)
/// reject the malformed value with the normal answer.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = hex_byte(bytes[i + 1], bytes[i + 2]) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Two hex digits as one byte (`%20` → space), either case; `None` when
/// either half is not a digit, which leaves the `%` literal.
fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    fn digit(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    Some(digit(hi)? << 4 | digit(lo)?)
}

// ── errors ──────────────────────────────────────────────────────────────────

fn conflict(message: impl Into<String>) -> Fail {
    fail(409, "conflict", message)
}

fn internal(message: impl ToString) -> Fail {
    fail(500, "internal", message.to_string())
}

/// A `UiError` on the machine face: the code stays (`err.no-sources` is
/// stable and greppable), the sentence becomes English. The panel translates
/// the same code into zh/en for people; agents read this one.
fn ui_fail(err: &UiError) -> Fail {
    let status = match err.code.as_str() {
        "err.already-recording"
        | "err.countdown"
        | "err.starting"
        | "err.not-recording"
        | "err.nothing-pending" => 409,
        "err.no-sources" => 400,
        _ => 500,
    };
    let known = match err.code.as_str() {
        "err.starting" => "a start is already in flight",
        "err.already-recording" => "a take is already recording",
        "err.countdown" => "the countdown veil is still up",
        "err.no-sources" => "no sources to record",
        "err.no-ffmpeg" => "ffmpeg not found and this take needs it",
        "err.prepare-failed" => "the capture engine could not be prepared",
        "err.overlay" => "the countdown overlay window is missing",
        "err.not-recording" => "no take is recording",
        "err.nothing-pending" => "no take is waiting to start",
        "err.save" => "the settings file could not be written",
        "err.internal" => "internal error",
        // `diagnose` codes: the detail is the device name, so the sentence
        // can name it exactly the way `diagnose::explain` does for the CLI.
        "err.cam-no-signal" | "err.cam-not-mf" | "err.device-gone" | "err.cam-open"
        | "err.ffmpeg-died" => return fail(500, &err.code, diagnose_explain(err)),
        _ => "request failed",
    };
    let message = match &err.detail {
        Some(detail) => format!("{known}: {detail}"),
        None => known.to_string(),
    };
    fail(status, &err.code, message)
}

/// A `diagnose` code back into its English sentence for the machine face,
/// with the device name slotted in.
fn diagnose_explain(err: &UiError) -> String {
    let wire = match &err.detail {
        Some(detail) => format!("{}|{detail}", err.code),
        None => err.code.clone(),
    };
    takekit_core::diagnose::explain(&wire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_ids_do_not_collide_with_the_list() {
        let s = |id: &str| Source::new(id, Capture::desktop(None), 30);
        assert_eq!(unique_id(&[], "win"), "win");
        assert_eq!(unique_id(&[s("win")], "win"), "win-2");
        assert_eq!(unique_id(&[s("win"), s("win-2")], "win"), "win-3");
        assert_eq!(unique_id(&[s("win"), s("win-3")], "win"), "win-2");
    }

    #[test]
    fn the_bearer_token_is_hex_of_a_sane_length() {
        let token = fresh_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(fresh_token(), token);
    }

    #[test]
    fn folder_name_strips_the_path() {
        assert_eq!(
            folder_name(r"D:\takes\2026-10-21-22-30-00"),
            "2026-10-21-22-30-00"
        );
        assert_eq!(folder_name("2026-10-21-22-30-00"), "2026-10-21-22-30-00");
    }

    #[test]
    fn percent_decode_unescapes_quoted_segments() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("2026-10-21-22-30-00"), "2026-10-21-22-30-00");
    }

    #[test]
    fn a_stray_percent_beside_a_multibyte_character_is_not_a_panic() {
        // A URL is bytes: `%` can sit next to anything, including inside a
        // multi-byte character. Slicing the input as a str would panic on
        // the char boundary; the bytes pass through and the caller's parse
        // rejects the malformed value with the normal answer.
        assert_eq!(percent_decode("a%2é"), "a%2é");
        assert_eq!(percent_decode("é%20"), "é ");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%2"), "%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn ui_errors_keep_their_code_and_gain_an_english_sentence() {
        let (status, code, message) = ui_fail(&UiError::new("err.not-recording"));
        assert_eq!(status, 409);
        assert_eq!(code, "err.not-recording");
        assert!(message.contains("no take is recording"));

        let (status, _, message) = ui_fail(&UiError::with_detail("err.prepare-failed", "boom"));
        assert_eq!(status, 500);
        assert!(message.ends_with("boom"));
    }

    #[test]
    fn log_levels_parse_only_the_two_names() {
        assert_eq!(parse_level("warn"), Some(journal::Level::Warn));
        assert_eq!(parse_level("error"), Some(journal::Level::Error));
        assert_eq!(parse_level("info"), None);
        assert_eq!(parse_level(""), None);
    }

    #[test]
    fn only_failed_mutations_leave_a_trail() {
        // Reads are a poll, bad tokens are a scan; a mutation that failed is
        // the one thing worth finding in the log later.
        assert!(trail_worthy(&Method::Post, 409));
        assert!(trail_worthy(&Method::Delete, 500));
        assert!(!trail_worthy(&Method::Get, 500));
        assert!(!trail_worthy(&Method::Post, 401));
    }

    #[test]
    fn a_query_value_comes_back_out_of_its_escaping() {
        // The CLI client percent-escapes what it writes; the server has to
        // undo it before the word is matched.
        assert_eq!(
            query_param("kind=window", "kind").as_deref(),
            Some("window")
        );
        assert_eq!(
            query_param("kind=win%20dow", "kind").as_deref(),
            Some("win dow")
        );
        assert_eq!(
            query_param("a=1&level=error", "level").as_deref(),
            Some("error")
        );
        assert_eq!(query_param("kind=window", "level"), None);
        // An empty value is a value: `?kind=` asks for a kind nobody spelled.
        assert_eq!(query_param("kind=", "kind").as_deref(), Some(""));
    }

    #[test]
    fn only_picker_shaped_ids_survive_the_raw_door() {
        // An id is a file name inside the take folder: the picker's shapes
        // pass, anything that could escape or collide does not.
        assert!(safe_source_id("win"));
        assert!(safe_source_id("win-2"));
        assert!(safe_source_id("cam_3"));
        assert!(!safe_source_id(""));
        assert!(!safe_source_id(".."));
        assert!(!safe_source_id("../evil"));
        assert!(!safe_source_id("a/b"));
        assert!(!safe_source_id("a\\b"));
        assert!(!safe_source_id("win.mkv"));
        assert!(!safe_source_id("win mkv"));
        assert!(!safe_source_id(&"w".repeat(65)));
    }

    #[test]
    fn a_raw_source_cannot_take_an_id_the_list_already_has() {
        let s = |id: &str| Source::new(id, Capture::desktop(None), 30);
        let taken = [s("win")];
        let fresh = s("cam");
        let clash = s("win");

        assert!(check_new_source(&fresh, &taken).is_ok());
        let err = check_new_source(&clash, &taken).unwrap_err();
        assert_eq!(err.0, 409, "{err:?}");
        // And an unusable id is a 400 even when it is unique.
        let nasty = s("../evil");
        assert_eq!(check_new_source(&nasty, &taken).unwrap_err().0, 400);
    }

    #[test]
    fn the_two_numbers_are_bounded_not_wrapped() {
        let absent = json!({});
        assert_eq!(bounded_fps(&absent).unwrap(), DEFAULT_FPS);
        assert_eq!(bounded_quality(&absent).unwrap(), None);

        let good = json!({ "fps": 60, "quality": 16 });
        assert_eq!(bounded_fps(&good).unwrap(), 60);
        assert_eq!(bounded_quality(&good).unwrap(), Some(16));

        // 256 as u8 wraps to 0 and 4_000_000_000 as u32 wraps to garbage:
        // both are refusals here, which is the point of the bounds.
        let wrapped = json!({ "fps": 4_000_000_000u64, "quality": 256u64 });
        assert!(bounded_fps(&wrapped).is_err());
        assert!(bounded_quality(&wrapped).is_err());

        // Zero fps is not a frame rate; zero quality is a valid (lossless) one.
        let zero = json!({ "fps": 0, "quality": 0 });
        assert!(bounded_fps(&zero).is_err());
        assert_eq!(bounded_quality(&zero).unwrap(), Some(0));
    }

    #[test]
    fn server_trouble_is_an_error_and_refusals_are_warnings() {
        assert_eq!(trail_level(500), journal::Level::Error);
        assert_eq!(trail_level(503), journal::Level::Error);
        assert_eq!(trail_level(409), journal::Level::Warn);
        assert_eq!(trail_level(400), journal::Level::Warn);
    }

    #[test]
    fn the_discovery_file_names_the_port_and_token() {
        let path = std::env::temp_dir().join("takekit-control-test.json");
        let body = json!({"port": 1234, "token": "ab", "pid": 7, "version": "0.1.0"});
        takekit_core::fsx::write_atomic(&path, &body.to_string()).unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["port"], 1234);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_discovery_write_creates_the_directory_it_lands_in() {
        // The panel can be writing its first file ever when this runs
        // (setup precedes any window interaction), and the app directory
        // does not exist yet on a fresh machine. Regression test for the
        // control surface coming up dead there.
        let dir = std::env::temp_dir().join("takekit-control-fresh-dir/nested");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("takekit-control-fresh-dir"));
        let path = dir.join("control.json");

        write_discovery_at(&path, 4321, "cafe").unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["port"], 4321);
        assert_eq!(back["token"], "cafe");
        assert_eq!(back["pid"], std::process::id());
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("takekit-control-fresh-dir"));
    }
}
