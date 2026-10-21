//! The HTTP client every agent-facing command shares: it finds the running
//! panel through `control.json` and speaks the loopback protocol described in
//! `docs/agent-control.md`.

use std::path::PathBuf;

use serde_json::Value;

/// Talks to the panel's control surface.
pub struct Client {
    base: String,
    token: String,
}

impl Client {
    /// Find the running panel via `%APPDATA%\TakeKit\control.json` — the file
    /// the server writes at start and removes at stop. A missing file means
    /// the panel is not running (or its control switch is off), which is the
    /// one answer worth saying plainly.
    pub fn discover() -> Result<Client, String> {
        let path = control_path();
        let text = std::fs::read_to_string(&path).map_err(|_| {
            "panel not running: no control.json (start TakeKit, and check the \
             control switch in its settings)"
                .to_string()
        })?;
        let info: Value =
            serde_json::from_str(&text).map_err(|e| format!("control.json is unreadable: {e}"))?;
        // The file a crashed run leaves behind is the case that actually
        // happens: its port is dead. Saying "not running" then beats saying
        // "unreachable" — the one answer worth saying plainly. A pid reused
        // by someone else falls through to the connection, which refuses.
        // The switch being off leaves the same shape: the panel removed the
        // file when it closed the door, so a copy that outlived it is stale
        // in exactly the same way.
        let pid = info.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
        if pid != 0 && !takekit_core::proc::alive(pid) {
            return Err(
                "panel not running: control.json is stale (the process it names is gone, \
                 or the control switch is off)"
                    .to_string(),
            );
        }
        let port = info
            .get("port")
            .and_then(Value::as_u64)
            .ok_or_else(|| "control.json has no port".to_string())?;
        let token = info
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| "control.json has no token".to_string())?
            .to_string();
        Ok(Client {
            base: format!("http://127.0.0.1:{port}"),
            token,
        })
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        self.send("GET", path, None)
    }

    pub fn send(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let request = ureq::request(method, &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token));
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        match response {
            Ok(response) => response
                .into_json::<Value>()
                .map_err(|e| format!("the panel answered with something that is not JSON: {e}")),
            Err(ureq::Error::Status(status, response)) => {
                // The server always answers with {code, message}; the status
                // code is the rough kind, the body is the sentence.
                let body = response.into_string().unwrap_or_default();
                let detail = serde_json::from_str::<Value>(&body).ok();
                let code = detail
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let message = detail
                    .as_ref()
                    .and_then(|d| d.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or(body.trim());
                Err(format!("{code}: {message} (HTTP {status})"))
            }
            Err(ureq::Error::Transport(e)) => {
                Err(format!("panel unreachable at {}: {e}", self.base))
            }
        }
    }

    /// The same `send`, but a body is expected on every verb that has one.
    pub fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.send("POST", path, Some(body))
    }

    pub fn put(&self, path: &str, body: Value) -> Result<Value, String> {
        self.send("PUT", path, Some(body))
    }

    pub fn patch(&self, path: &str, body: Value) -> Result<Value, String> {
        self.send("PATCH", path, Some(body))
    }

    pub fn delete(&self, path: &str, body: Value) -> Result<Value, String> {
        self.send("DELETE", path, Some(body))
    }
}

/// `%APPDATA%\TakeKit\control.json` — the same directory the panel's
/// `config.json` lives in (`fsx::app_dir()` is the single definition both
/// sides resolve through).
fn control_path() -> PathBuf {
    takekit_core::fsx::app_dir().join("control.json")
}

/// Percent-escape one URL component — a query value (`?kind=`, `?level=`)
/// or a path segment (`/history/{id}`), where the unreserved set survives
/// and everything else is escaped. The values are enum words, but they
/// arrive as strings an agent composed: a space or a `#` must not become a
/// broken URL, and the server decodes what this writes.
pub(crate) fn url_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_value_escapes_what_a_url_cannot_carry() {
        // The server decodes with the same alphabet in mind; a space or a
        // `#` must survive the trip without breaking the URL.
        assert_eq!(url_escape("window"), "window");
        assert_eq!(url_escape("win dow"), "win%20dow");
        assert_eq!(url_escape("a#b&c=d"), "a%23b%26c%3Dd");
        assert_eq!(url_escape("微型"), "%E5%BE%AE%E5%9E%8B");
    }

    #[test]
    fn a_path_segment_escapes_the_same_way() {
        // The take-folder id travels in the path, not the query — same
        // alphabet, same reason. A machine-made name is untouched.
        assert_eq!(url_escape("2026-10-21-22-30-00"), "2026-10-21-22-30-00");
        assert_eq!(url_escape("a b"), "a%20b");
    }
}
