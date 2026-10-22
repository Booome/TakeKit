//! `takekit mcp` — the Model Context Protocol face of the control surface
//! (`docs/agent-control.md`): a stdio JSON-RPC server that hands agents
//! thirteen typed tools instead of a URL and a token to remember.
//!
//! Hand-rolled on purpose. The protocol over stdio is newline-delimited
//! JSON-RPC with three methods worth serving (`initialize`, `tools/list`,
//! `tools/call`); pulling in an SDK to get that would outweigh the panel.
//! Everything behind the tools is the same HTTP client `takekit ctl` uses —
//! one state machine, three doors.

use std::io::BufRead;

use serde_json::{Value, json};

use crate::client::Client;

const PROTOCOL: &str = "2024-11-05";

pub fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            // Not JSON: the protocol has no way to say so without an id, and
            // inventing one is worse than staying quiet.
            continue;
        };
        if let Some(response) = handle(&request) {
            println!(
                "{}",
                serde_json::to_string(&response).map_err(|e| e.to_string())?
            );
            use std::io::Write;
            std::io::stdout().flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// One request in, one response out (or none, for notifications).
fn handle(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) get silence: the only one that matters is
    // `notifications/initialized`, which has nothing to answer.
    let id = id?;

    let result = match method {
        "initialize" => json!({
            // Echoing the client's version is what the spec asks for when it
            // is one we speak; 2024-11-05 is the widely deployed baseline.
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL),
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "takekit",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_list() }),
        "tools/call" => match tools_call(&params) {
            Ok(value) => value,
            Err(message) => json!({
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            }),
        },
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("no method {method:?}") },
            }));
        }
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tools_call(params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call needs a name".to_string())?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let client = Client::discover()?;
    let outcome = match name {
        "takekit_state" => client.get("/state"),
        "takekit_list_devices" => {
            let path = match args.get("kind").and_then(Value::as_str) {
                Some(kind) => format!("/devices?kind={}", crate::client::url_escape(kind)),
                None => "/devices".to_string(),
            };
            client.get(&path)
        }
        "takekit_add_source" => {
            // The convenience shape the server resolves: kind + target (+ the
            // optional trim and rates). Nulls left out so the server's own
            // defaults apply.
            let mut body = json!({});
            for key in ["kind", "target", "monitor", "crop", "fps", "quality"] {
                if let Some(value) = args.get(key) {
                    if !value.is_null() {
                        body[key] = value.clone();
                    }
                }
            }
            client.post("/sources", body)
        }
        "takekit_remove_source" => {
            let index = args
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("remove needs an index")?;
            client.delete(&format!("/sources/{index}"), Value::Null)
        }
        "takekit_set_sources" => {
            let sources = args
                .get("sources")
                .cloned()
                .ok_or("set_sources needs a sources array")?;
            client.put("/sources", sources)
        }
        "takekit_set_settings" => {
            let mut body = json!({});
            for key in [
                "hotkey",
                "countdown_secs",
                "finalize_panel",
                "out_root",
                "encoder",
                "backend",
                "marks",
                "raise_sources",
                "language",
                "theme",
            ] {
                if let Some(value) = args.get(key) {
                    body[key] = value.clone();
                }
            }
            client.patch("/config", body)
        }
        "takekit_start_recording" => {
            let mut body = json!({});
            if let Some(quality) = args.get("quality") {
                if !quality.is_null() {
                    body["quality"] = quality.clone();
                }
            }
            client.post("/record/start", body)
        }
        "takekit_stop_recording" => client.post("/record/stop", json!({})),
        "takekit_history" => client.get("/history"),
        "takekit_log" => {
            let path = match args.get("level").and_then(Value::as_str) {
                Some(level) => format!("/log?level={}", crate::client::url_escape(level)),
                None => "/log".to_string(),
            };
            client.get(&path)
        }
        "takekit_clear_log" => {
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err(
                    "takekit_clear_log wipes the register: pass \"confirm\": true to mean it"
                        .to_string(),
                );
            }
            client.delete("/log", json!({ "confirm": true }))
        }
        "takekit_delete_log" => {
            let at = args
                .get("at")
                .and_then(Value::as_u64)
                .ok_or("delete_log needs the entry's `at` stamp (from takekit_log)")?;
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err(
                    "takekit_delete_log removes the row: pass \"confirm\": true to mean it"
                        .to_string(),
                );
            }
            client.delete(&format!("/log/{at}"), json!({ "confirm": true }))
        }
        "takekit_delete_take" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .ok_or("delete_take needs an id")?;
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err(
                    "takekit_delete_take is destructive: pass \"confirm\": true to mean it"
                        .to_string(),
                );
            }
            // Escaped like a query value: a folder name is machine-made and
            // URL-safe, but the id arrives from the agent's arguments.
            let path = format!("/history/{}", crate::client::url_escape(id));
            client.delete(&path, json!({ "confirm": true }))
        }
        other => return Err(format!("no tool {other:?}")),
    };

    match outcome {
        Ok(value) => Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
            }],
            "isError": false,
        })),
        // A failed call is still a successful tool call with an error inside:
        // that is what the protocol's `isError` is for.
        Err(message) => Ok(json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        })),
    }
}

/// The thirteen tools, with the shapes an agent fills in. Names and descriptions
/// are what the model reads to pick one, so they say the action, not the
/// endpoint.
fn tool_list() -> Vec<Value> {
    let object = |properties: Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    };
    let prop = |description: &str, ty: &str| json!({ "description": description, "type": ty });

    vec![
        json!({
            "name": "takekit_state",
            "description": "Full TakeKit snapshot: settings, sources (with indices), \
                            live recording status, history. Call before anything else.",
            "inputSchema": object(json!({}), &[]),
        }),
        json!({
            "name": "takekit_list_devices",
            "description": "List what can be recorded: windows (hwnd + title), desktops, \
                            cameras, microphones. `system-audio` records everything the \
                            speakers play and needs no device; `window-audio` picks from \
                            the windows list.",
            "inputSchema": object(
                json!({ "kind": prop("window | desktop | camera | mic | window-audio | system-audio; the four groups when omitted", "string") }),
                &[],
            ),
        }),
        json!({
            "name": "takekit_add_source",
            "description": "Add one source. Windows and cameras are matched by a name/title \
                            substring; a unique hit is added, several hits are listed back. \
                            mics match a device name; system-audio needs no target. \
                            The panel shows the source the moment it is added.",
            "inputSchema": object(
                json!({
                    "kind": prop("window | desktop | camera | mic | system-audio | window-audio", "string"),
                    "target": prop("window title, camera name or mic name substring (unused for system-audio)", "string"),
                    "monitor": prop("monitor number for kind=desktop (see list_devices)", "number"),
                    "crop": prop("edges to trim: {top,bottom,left,right} in pixels (pictures only)", "object"),
                    "fps": prop("frames per second, default 30", "number"),
                    "quality": prop("per-source quality override", "number"),
                }),
                &["kind"],
            ),
        }),
        json!({
            "name": "takekit_remove_source",
            "description": "Remove a source by its index from takekit_state.",
            "inputSchema": object(json!({ "index": prop("source index from takekit_state", "number") }), &["index"]),
        }),
        json!({
            "name": "takekit_set_sources",
            "description": "Replace the whole source list with the given array (the raw \
                            Source shape from takekit_state's sources field).",
            "inputSchema": object(
                json!({ "sources": json!({ "description": "array of sources", "type": "array" }) }),
                &["sources"],
            ),
        }),
        json!({
            "name": "takekit_set_settings",
            "description": "Change settings; only the keys given are touched. Capture \
                            settings are frozen while a take is recording.",
            "inputSchema": object(
                json!({
                    "hotkey": prop("global shortcut, e.g. F9", "string"),
                    "countdown_secs": prop("countdown before recording, 0 = off", "number"),
                    "finalize_panel": prop("full-screen stop checklist", "boolean"),
                    "out_root": prop("where takes are written", "string"),
                    "encoder": prop("nvenc | qsv | amf | x264 | null for auto", "string"),
                    "backend": prop("windows | ffmpeg", "string"),
                    "marks": prop("draw click effects into the recording", "boolean"),
                    "raise_sources": prop("bring windows to the front before recording", "boolean"),
                    "language": prop("zh | en", "string"),
                    "theme": prop("auto | light | dark", "string"),
                }),
                &[],
            ),
        }),
        json!({
            "name": "takekit_start_recording",
            "description": "Start a take on the current sources. With a countdown configured \
                            the full-screen countdown runs first (started=false in the reply); \
                            the encoders start when it reaches GO.",
            "inputSchema": object(
                json!({ "quality": prop("quality for this take, default 20", "number") }),
                &[],
            ),
        }),
        json!({
            "name": "takekit_stop_recording",
            "description": "Stop the take. The reply reports the folder every file landed in.",
            "inputSchema": object(json!({}), &[]),
        }),
        json!({
            "name": "takekit_history",
            "description": "The take history and the warning/error log.",
            "inputSchema": object(json!({}), &[]),
        }),
        json!({
            "name": "takekit_log",
            "description": "The warning/error register alone — the first place to look \
                            when a take came out wrong or a source died mid-recording. \
                            Includes lines the control surface wrote about its own failed \
                            operations.",
            "inputSchema": object(
                json!({ "level": prop("warn | error; all entries when omitted", "string") }),
                &[],
            ),
        }),
        json!({
            "name": "takekit_clear_log",
            "description": "Wipe the warning/error register. Destructive; needs confirm=true.",
            "inputSchema": object(
                json!({ "confirm": json!({ "description": "must be true", "type": "boolean" }) }),
                &["confirm"],
            ),
        }),
        json!({
            "name": "takekit_delete_log",
            "description": "Delete one log entry by its `at` stamp (from takekit_log). \
                            The other entries stay; a stamp nothing carries is an error, \
                            not a silent pass. Needs confirm=true.",
            "inputSchema": object(
                json!({
                    "at": prop("the entry's at stamp (unix ms) from takekit_log", "number"),
                    "confirm": json!({ "description": "must be true", "type": "boolean" }),
                }),
                &["at", "confirm"],
            ),
        }),
        json!({
            "name": "takekit_delete_take",
            "description": "Delete one take — its folder on disk and its history row. \
                            Destructive; needs confirm=true.",
            "inputSchema": object(
                json!({
                    "id": prop("take folder name from takekit_history", "string"),
                    "confirm": json!({ "description": "must be true", "type": "boolean" }),
                }),
                &["id", "confirm"],
            ),
        }),
    ]
}
