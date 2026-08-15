use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Open (or create/append to) the debug log at `~/.cache/listen_to_it/debug.log`
/// and record a session-start marker. Safe to call at most once, before any
/// `log!` calls — until this runs, `log!` is a silent no-op.
pub fn init() -> std::io::Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(home).join(".cache").join("listen_to_it");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("debug.log");
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let _ = LOG_FILE.set(Mutex::new(file));
    log(&format!(
        "──── listen_to_it started (pid {}, v{}) ────",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    ));
    Ok(path)
}

pub fn log(msg: &str) {
    let Some(mutex) = LOG_FILE.get() else { return };
    let Ok(mut file) = mutex.lock() else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let (y, mo, d, h, mi, s) = civil_from_unix(now.as_secs() as i64);
    let millis = now.subsec_millis();
    let _ = writeln!(file, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{millis:03}Z  {msg}");
    let _ = file.flush();
}

/// UTC calendar date/time from a Unix timestamp (Howard Hinnant's
/// `civil_from_days` algorithm) — avoids pulling in a chrono/time dependency
/// just for log timestamps.
fn civil_from_unix(epoch_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch_secs.div_euclid(86400);
    let secs_of_day = epoch_secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let h = (secs_of_day / 3600) as u32;
    let mi = ((secs_of_day % 3600) / 60) as u32;
    let s = (secs_of_day % 60) as u32;
    (y, m, d, h, mi, s)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Append a formatted line to the debug log. No-op until `logging::init()`
/// has run.
#[macro_export]
macro_rules! logline {
    ($($arg:tt)*) => {
        $crate::logging::log(&format!($($arg)*))
    };
}
