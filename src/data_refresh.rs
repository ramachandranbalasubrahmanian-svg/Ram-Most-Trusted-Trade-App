//! Manual bulk data refresh — pull all pending candle/fundamental data for the
//! ~1500-stock universe into the parquet archive the Rust app reads, by running the
//! existing, tested Python pipeline (`daily_update.sh`) as a subprocess.
//!
//! INFRA-ONLY & GUARDED. This module only fetches market data into the parquet
//! archive (`config::data_root()`); it never touches signals, Confidence, scoring,
//! the journal, or an order. It REFUSES to start while the live intraday session is
//! active — during the NSE window (weekday 09:00–15:45 IST) OR whenever a
//! `ram_istp live` process is running — so a heavy ~20-min download can never
//! collide with live trading. At most one refresh runs at a time.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, NaiveTime, Timelike};
use chrono_tz::Tz;

use crate::config::IST;

/// State of the (at most one) running / most-recent manual refresh.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RefreshState {
    pub running: bool,
    pub started_ist: Option<String>,
    pub finished_ist: Option<String>,
    pub exit_code: Option<i32>,
    pub log_file: Option<String>,
}

pub type SharedRefresh = Arc<Mutex<RefreshState>>;

/// A fresh shared refresh-state handle for `AppState`.
pub fn shared() -> SharedRefresh {
    Arc::new(Mutex::new(RefreshState::default()))
}

/// Archive freshness + whether a refresh is blocked / running. Sent to the page.
#[derive(Debug, Default, serde::Serialize)]
pub struct DataStatus {
    pub archive_root: String,
    pub symbol_count: usize,
    pub last_minute_date: Option<String>,
    pub last_1day_date: Option<String>,
    pub last_daily_date: Option<String>,
    /// `Some(reason)` ⇒ the manual refresh button must be disabled.
    pub blocked_reason: Option<String>,
    pub refresh: RefreshState,
}

fn now_ist() -> DateTime<Tz> {
    chrono::Utc::now().with_timezone(&IST)
}

/// True if `now` is inside the NSE intraday window we protect: a weekday between
/// 09:00 and 15:45 IST (a touch either side of the 09:15–15:30 session). Pure +
/// testable; `block_reason` adds the live-process check on top.
pub fn within_intraday_window(now: DateTime<Tz>) -> bool {
    let weekday = now.weekday().number_from_monday(); // Mon=1 … Sun=7
    let t = now.time();
    let open = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let close = NaiveTime::from_hms_opt(15, 45, 0).unwrap();
    weekday <= 5 && t >= open && t <= close
}

/// True if a `ram_istp live` process is currently running (best-effort via pgrep).
/// The dashboard runs as `ram_istp serve …`, which does NOT match this pattern.
fn live_process_running() -> bool {
    Command::new("pgrep")
        .args(["-f", "ram_istp live"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Why the manual refresh is blocked right now, or `None` if it is safe to run.
pub fn block_reason() -> Option<String> {
    let now = now_ist();
    if within_intraday_window(now) {
        return Some(format!(
            "Intraday session window (Mon–Fri 09:00–15:45 IST) — it's {} now. Run the refresh after the close.",
            now.format("%a %H:%M")
        ));
    }
    if live_process_running() {
        return Some(
            "A live intraday process (`ram_istp live`) is running — stop it before refreshing data.".to_string(),
        );
    }
    None
}

/// Count `*.parquet` files directly under `dir`.
fn count_parquet(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "parquet").unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// Single-quote-escape a path for safe inlining into a DuckDB SQL literal.
fn sql_lit(p: &Path) -> String {
    p.to_string_lossy().replace('\'', "''")
}

/// Max `date` in a single parquet file, as `YYYY-MM-DD`.
fn max_date_in(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let conn = crate::storage_kernel::open_conn().ok()?;
    let q = format!(
        "SELECT CAST(MAX(date) AS DATE)::VARCHAR FROM read_parquet('{}')",
        sql_lit(path)
    );
    conn.query_row(&q, [], |r| r.get::<_, Option<String>>(0)).ok().flatten()
}

/// Last downloaded date for a per-symbol timeframe dir, sampled from RELIANCE
/// (falling back to the first parquet present) — a cheap freshness probe.
fn sample_max_date(root: &Path, tf_dir: &str) -> Option<String> {
    let dir = root.join(tf_dir);
    let mut path = dir.join("RELIANCE.parquet");
    if !path.exists() {
        path = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().map(|x| x == "parquet").unwrap_or(false))?;
    }
    max_date_in(&path)
}

/// Build the data status: archive freshness + block/running state.
pub fn status(root: &Path, state: &SharedRefresh) -> DataStatus {
    let symbol_count = count_parquet(&root.join("minute")).max(count_parquet(&root.join("1day")));
    let refresh = state.lock().map(|s| s.clone()).unwrap_or_default();
    DataStatus {
        archive_root: root.to_string_lossy().to_string(),
        symbol_count,
        last_minute_date: sample_max_date(root, "minute"),
        last_1day_date: sample_max_date(root, "1day"),
        last_daily_date: max_date_in(&root.join("nse_daily_all.parquet")),
        blocked_reason: block_reason(),
        refresh,
    }
}

/// Start the pipeline if it's not blocked and not already running. Spawns
/// `daily_update.sh` (incremental + resumable) detached, logging to
/// `<root>/logs/manual_refresh_<ts>.log`, and reaps it in a background thread.
/// Returns the log path, or an error string (block reason / already running /
/// spawn failure).
pub fn start_refresh(root: &Path, state: &SharedRefresh) -> Result<String, String> {
    if let Some(reason) = block_reason() {
        return Err(reason);
    }
    let script = root.join("scheduled_refresh.py");
    if !script.exists() {
        return Err(format!("pipeline wrapper not found at {}", script.display()));
    }

    let now = now_ist();
    let logs_dir = root.join("logs");
    std::fs::create_dir_all(&logs_dir).map_err(|e| format!("could not create logs dir: {e}"))?;
    let log_path = logs_dir.join(format!("manual_refresh_{}.log", now.format("%Y%m%d_%H%M%S")));

    {
        let mut st = state.lock().map_err(|_| "refresh state lock poisoned".to_string())?;
        if st.running {
            return Err("a data refresh is already running".to_string());
        }
        st.running = true;
        st.started_ist = Some(now.format("%Y-%m-%d %H:%M:%S").to_string());
        st.finished_ist = None;
        st.exit_code = None;
        st.log_file = Some(log_path.to_string_lossy().to_string());
    }

    let log_out = std::fs::File::create(&log_path).map_err(|e| format!("could not open log: {e}"))?;
    let log_err = log_out.try_clone().map_err(|e| format!("log clone failed: {e}"))?;

    // Run the FULL refresh through the wrapper: download -> rebuild all 7 edge maps ->
    // restart the dashboard, forced past the "already done today" marker. Launched with
    // the Framework python (deps + Full Disk Access); the wrapper sets the child PATH for
    // daily_update.sh itself.
    const FW_BIN: &str = "/Library/Frameworks/Python.framework/Versions/3.14/bin";
    const FW_PY: &str = "/Library/Frameworks/Python.framework/Versions/3.14/bin/python3";
    let mut cmd = Command::new(if Path::new(FW_PY).exists() { FW_PY } else { "python3" });
    cmd.arg("scheduled_refresh.py")
        .arg("--force")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        // Own process group, so the wrapper's final `launchctl kickstart -k` (which
        // restarts THIS dashboard) can't take the still-running pipeline down with it.
        .process_group(0);
    let path = std::env::var("PATH")
        .map(|p| format!("{FW_BIN}:{p}"))
        .unwrap_or_else(|_| format!("{FW_BIN}:/usr/bin:/bin"));
    cmd.env("PATH", path);
    // Forward market-data creds if the server has them (never logged). The pipeline
    // also reads <root>/.kite_token.json for the cached daily Kite token.
    for k in ["KITE_API_KEY", "KITE_API_SECRET", "KITE_ACCESS_TOKEN", "INDIANAPI_KEY"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                cmd.env(k, v);
            }
        }
    }

    let child = cmd.spawn().map_err(|e| {
        if let Ok(mut st) = state.lock() {
            st.running = false;
        }
        format!("could not start the pipeline: {e}")
    })?;

    // Reap in the background so the request returns immediately.
    let state2 = state.clone();
    std::thread::spawn(move || {
        let mut child = child;
        let code = child.wait().ok().and_then(|s| s.code());
        if let Ok(mut st) = state2.lock() {
            st.running = false;
            st.finished_ist = Some(now_ist().format("%Y-%m-%d %H:%M:%S").to_string());
            st.exit_code = code;
        }
    });

    Ok(log_path.to_string_lossy().to_string())
}

/// Tail the current/last refresh log (up to `max_bytes`); returns (text, running).
pub fn tail_log(state: &SharedRefresh, max_bytes: usize) -> Option<(String, bool)> {
    let (path, running) = {
        let s = state.lock().ok()?;
        (s.log_file.clone()?, s.running)
    };
    let data = std::fs::read(&path).ok()?;
    let start = data.len().saturating_sub(max_bytes);
    Some((String::from_utf8_lossy(&data[start..]).to_string(), running))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ist(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> DateTime<Tz> {
        IST.with_ymd_and_hms(y, m, d, hh, mm, 0).unwrap()
    }

    #[test]
    fn weekday_midsession_is_inside_window() {
        // 2026-06-29 is a Monday.
        assert!(within_intraday_window(ist(2026, 6, 29, 12, 0)));
        assert!(within_intraday_window(ist(2026, 6, 29, 9, 15)));
        assert!(within_intraday_window(ist(2026, 6, 29, 15, 30)));
    }

    #[test]
    fn weekday_after_close_is_outside_window() {
        assert!(!within_intraday_window(ist(2026, 6, 29, 16, 0)));
        assert!(!within_intraday_window(ist(2026, 6, 29, 8, 30)));
    }

    #[test]
    fn weekend_is_always_outside_window() {
        // 2026-06-27 Sat, 2026-06-28 Sun.
        assert!(!within_intraday_window(ist(2026, 6, 27, 12, 0)));
        assert!(!within_intraday_window(ist(2026, 6, 28, 12, 0)));
    }
}
