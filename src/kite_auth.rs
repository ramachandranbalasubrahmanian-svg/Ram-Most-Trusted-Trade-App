//! Kite Connect login bridge for the `/kite` page. Shells out to the Framework
//! python helper (`1500-Stocks-Parquest/kite_web_login.py`), which reuses the
//! tested `kiteconnect` library and the exact `.kite_token.json` format the
//! download pipeline reads.
//!
//! SECURITY: this never returns or logs the API secret or the access token. The
//! helper writes the token only to `.kite_token.json` (mode 600); the handlers
//! surface status (connected / valid-today / which creds are configured) and the
//! public login URL. The request_token is single-use and passed via STDIN (not a
//! CLI arg / env var, so it never appears in the process list).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The Full-Disk-Access-granted interpreter that has `kiteconnect` installed.
const FRAMEWORK_PY: &str = "/Library/Frameworks/Python.framework/Versions/3.14/bin/python3";

fn python() -> &'static str {
    if Path::new(FRAMEWORK_PY).exists() { FRAMEWORK_PY } else { "python3" }
}

fn helper_path(root: &Path) -> PathBuf {
    root.join("kite_web_login.py")
}

/// Run `kite_web_login.py <cmd>` (optionally feeding `stdin_data`), parse its JSON.
fn run(root: &Path, cmd: &str, stdin_data: Option<&str>) -> serde_json::Value {
    let helper = helper_path(root);
    if !helper.exists() {
        return serde_json::json!({"error": "kite_web_login.py not found in the archive folder"});
    }
    let mut c = Command::new(python());
    // cwd = the archive root; pass the BASENAME (not the root-relative path,
    // already validated above) to avoid double-pathing under current_dir(root).
    c.arg("kite_web_login.py")
        .arg(cmd)
        .current_dir(root)
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return serde_json::json!({"error": format!("could not start the Kite helper: {e}")}),
    };
    if let Some(data) = stdin_data {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(data.as_bytes());
            // si drops here → stdin closes → the helper's read() gets EOF.
        }
    }
    match child.wait_with_output() {
        Ok(o) => serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|_| serde_json::json!({"error": "the Kite helper returned unexpected output"})),
        Err(e) => serde_json::json!({"error": format!("the Kite helper failed: {e}")}),
    }
}

/// Token status: connected / valid-today / which creds are configured. No secrets.
pub fn status(root: &Path) -> serde_json::Value {
    run(root, "status", None)
}

/// The Kite Connect login URL (contains the public api_key, never the secret).
pub fn login_url(root: &Path) -> serde_json::Value {
    run(root, "url", None)
}

/// Exchange a single-use request_token for an access token + cache it. An optional
/// `api_secret` (used + persisted to .env when one isn't configured yet) is passed
/// via STDIN as JSON, never as an arg/env var. Returns `{ok, message}`; the token
/// and secret are written to disk, never returned here.
pub fn exchange(root: &Path, request_token: &str, api_secret: Option<&str>) -> serde_json::Value {
    let payload = match api_secret {
        Some(s) if !s.trim().is_empty() => serde_json::json!({"request_token": request_token, "api_secret": s}),
        _ => serde_json::json!({"request_token": request_token}),
    };
    run(root, "exchange", Some(&payload.to_string()))
}
