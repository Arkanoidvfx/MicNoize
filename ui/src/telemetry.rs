use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

// Mic Noize has its own Worker and its own D1 database; the Moment Player Worker keeps
// accepting `app_id = mic_noize` only as a fallback while the new one is being deployed.
const ENDPOINT: &str = "https://micnoize-telemetry.arkanoidvfx.workers.dev/v1/ingest";
const VERSION: &str = env!("CARGO_PKG_VERSION");
static SESSION: OnceLock<(Uuid, u64)> = OnceLock::new();

fn secret() -> &'static str {
    option_env!("MNR_TELEMETRY_SECRET").unwrap_or("")
}

fn enabled() -> bool {
    !secret().is_empty() && std::env::var_os("MNR_DISABLE_TELEMETRY").is_none()
}

pub fn record(data: &Path, name: &str, details: serde_json::Value) {
    if !enabled() {
        return;
    }
    let data = data.to_path_buf();
    let name = name.to_owned();
    std::thread::spawn(move || {
        let _ = send(&data, vec![event("app-lifecycle", &name, details)], Duration::from_secs(4));
    });
}

/// Sent on the UI thread while quitting, so it is capped short: a stalled connection used to
/// freeze the window for good (ureq has no timeout by default) and only killing the process helped.
pub fn record_blocking(data: &Path, name: &str, details: serde_json::Value) {
    if enabled() {
        let _ = send(data, vec![event("app-lifecycle", name, details)], Duration::from_millis(1500));
    }
}

/// The tail of every runtime log plus the user's own note, sent as `error` events so they land
/// in the same operator dashboard as crashes. One request: the ingest limits batches, not fields.
pub fn report(data: &Path, runtime: &Path, note: &str) -> Result<String, String> {
    if !enabled() {
        return Err("Отправка отчётов доступна только в установленной версии".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut events = report_events(data, runtime, exe.parent().ok_or("Нет папки приложения")?, note);
    events.push(error_event("report-environment", &format!(
        "Windows: {}\ntar: {}",
        crate::logs::command("cmd.exe", &["/d", "/c", "ver"]),
        crate::logs::command(crate::components::tar(), &["--version"]),
    )));
    let count = events.len();
    send(data, events, Duration::from_secs(15))?;
    Ok(format!("Отчёт отправлен ({count})"))
}

fn report_events(data: &Path, runtime: &Path, app: &Path, note: &str) -> Vec<serde_json::Value> {
    let mut events = vec![error_event("user-report", note)];
    let mut files: Vec<_> = [
        "app.log", "nvafx.log", "tag-host.log", "tag-host.previous.log",
        "tag-endpoint.log", "sessions.log", "tag-headphones.log",
    ].into_iter().map(|name| (name, runtime.join("results").join(name))).collect();
    files.push(("component-unpack.log", runtime.join(".download-rvc/rvc-runtime.tar.log")));
    files.push(("rust-ui-error.log", data.join("Logs/rust-ui-error.log")));
    files.push(("rust-ui-panic.log", data.join("Logs/rust-ui-panic.log")));
    // Before Components/vendor exists, the scheduled host's projectRoot() falls back to
    // the installation parent. Keep these early failures even after core has been installed.
    if let Some(parent) = app.parent().filter(|p| *p != runtime) {
        files.push(("startup/tag-host.log", parent.join("results/tag-host.log")));
        files.push(("startup/tag-host.previous.log", parent.join("results/tag-host.previous.log")));
        files.push(("startup/tag-endpoint.log", parent.join("results/tag-endpoint.log")));
    }
    let mut inventory = Vec::new();
    for (name, path) in files {
        let status = match crate::logs::read_tail(&path, 8192) {
            Ok(text) if text.trim().is_empty() => "empty".to_owned(),
            Ok(text) => {
                events.push(error_event(name, &text));
                let modified = path.metadata().and_then(|m| m.modified()).ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| format!("{}Z", chrono_free_utc(d.as_secs())))
                    .unwrap_or_else(|| "unknown".into());
                format!("included; modified={modified}")
            }
            Err(e) => format!("unavailable: {:?}; os={:?}", e.kind(), e.raw_os_error()),
        };
        inventory.push(format!("{name}: {status}"));
    }
    events.push(error_event("report-files", &inventory.join("\n")));
    events
}

/// The worker's JS schema limits UTF-16 units, not Unicode scalar values or UTF-8 bytes.
fn error_event(name: &str, text: &str) -> serde_json::Value {
    let tail: String = text.chars().rev().scan(0, |len, ch| {
        *len += ch.len_utf16();
        (*len <= 2000).then_some(ch)
    }).collect::<Vec<_>>().into_iter().rev().collect();
    event(
        "error",
        name,
        json!({
            "exception_type": name,
            "message": text.lines().next_back().unwrap_or("").chars().scan(0, |len, ch| {
                *len += ch.len_utf16();
                (*len <= 1000).then_some(ch)
            }).collect::<String>(),
            "stack_top": tail,
            "source": "micnoize",
        }),
    )
}

fn event(tag: &str, name: &str, details: serde_json::Value) -> serde_json::Value {
    json!({"tag": tag, "name": name, "data": details})
}

fn support_label(user: &str, computer: &str) -> Option<String> {
    let user = user.trim();
    let computer = computer.trim();
    if user.is_empty() || computer.is_empty() {
        return None;
    }
    let label: String = format!("{user}@{computer}")
        .chars()
        .filter(|ch| !ch.is_control())
        .scan(0, |len, ch| {
            *len += ch.len_utf16();
            (*len <= 96).then_some(ch)
        })
        .collect();
    Some(label)
}

fn send(data: &Path, events: Vec<serde_json::Value>, timeout: Duration) -> Result<(), String> {
    let install_path = data.join("install-id.txt");
    let install_id = std::fs::read_to_string(&install_path)
        .ok()
        .and_then(|s| Uuid::parse_str(s.trim()).ok())
        .unwrap_or_else(|| {
            let id = Uuid::new_v4();
            let _ = std::fs::write(&install_path, id.to_string());
            id
        });
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let (session, started_at) = SESSION.get_or_init(|| (Uuid::new_v4(), timestamp));
    let iso = format!("{}Z", chrono_free_utc(timestamp));
    let started_iso = format!("{}Z", chrono_free_utc(*started_at));
    let events: Vec<serde_json::Value> = events
        .into_iter()
        .map(|mut e| {
            e["ts"] = json!(iso);
            e
        })
        .collect();
    let body = serde_json::to_vec(&json!({
        "app_id": "mic_noize",
        "session_id": session.to_string(),
        "session_started_at": started_iso,
        "app_version": VERSION,
        "support_identity": support_label(
            &std::env::var("USERNAME").unwrap_or_default(),
            &std::env::var("COMPUTERNAME").unwrap_or_default(),
        ).map(|label| json!({"enabled": true, "source": "windows", "label": label})),
        "events": events
    }))
    .map_err(|e| e.to_string())?;
    let body_hash = hex::encode(Sha256::digest(&body));
    let signed = format!("{install_id}|{timestamp}|2|{body_hash}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret().as_bytes()).map_err(|e| e.to_string())?;
    mac.update(signed.as_bytes());
    let hmac = hex::encode(mac.finalize().into_bytes());
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into();
    agent
        .post(ENDPOINT)
        .header("Content-Type", "application/json")
        .header("X-App-Id", "mic_noize")
        .header("X-Install-Id", &install_id.to_string())
        .header("X-App-Version", VERSION)
        .header("X-Schema-Version", "2")
        .header("X-Timestamp", &timestamp.to_string())
        .header("X-Hmac", &hmac)
        .send(&body)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// UTC formatting without another time dependency; telemetry accepts ISO-8601.
pub(crate) fn chrono_free_utc(seconds: u64) -> String {
    const SECONDS_PER_DAY: u64 = 86_400;
    let days = seconds / SECONDS_PER_DAY;
    let rem = seconds % SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_include_early_host_unpack_errors_and_missing_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.tmp/report-test").join(std::process::id().to_string());
        let runtime = root.join("Components");
        let app = root.join("install/current");
        std::fs::create_dir_all(runtime.join("results")).unwrap();
        std::fs::create_dir_all(runtime.join(".download-rvc")).unwrap();
        std::fs::create_dir_all(root.join("install/results")).unwrap();
        std::fs::create_dir_all(root.join("Logs")).unwrap();
        let app_log = runtime.join("results/app.log");
        std::fs::write(&app_log, format!("{}\nlast failure 😀", "old log\n".repeat(10_000))).unwrap();
        std::fs::write(runtime.join("results/nvafx.log"), b"").unwrap();
        std::fs::write(root.join("install/results/tag-host.log"), "TAG API missing before core install").unwrap();
        std::fs::write(runtime.join(".download-rvc/rvc-runtime.tar.log"), b"bad byte \xff\nAccess denied").unwrap();
        std::fs::write(root.join("Logs/rust-ui-error.log"), "TAG host initializing or waiting for driver").unwrap();
        let tail = crate::logs::read_tail(&app_log, 8192).unwrap();
        assert!(tail.len() <= 8192 && tail.ends_with("last failure 😀"));
        let events = report_events(&root, &runtime, &app, "state=0 driver=false core=false");
        let get = |name| events.iter().find(|e| e["name"] == name).unwrap()["data"]["stack_top"].as_str().unwrap();
        assert!(get("app.log").ends_with("last failure 😀"));
        assert!(get("startup/tag-host.log").contains("TAG API missing"));
        assert!(get("component-unpack.log").contains("Access denied"));
        assert!(get("rust-ui-error.log").contains("waiting for driver"));
        assert!(get("report-files").contains("tag-host.log: unavailable:"));
        assert!(get("report-files").contains("nvafx.log: empty"));
        assert!(get("report-files").contains("startup/tag-host.log: included; modified="));
        assert_eq!(get("user-report"), "state=0 driver=false core=false");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn report_fields_fit_worker_utf16_limits() {
        let event = error_event("app.log", &format!("{}END", "😀".repeat(3000)));
        let tail = event["data"]["stack_top"].as_str().unwrap();
        let message = event["data"]["message"].as_str().unwrap();
        assert!(tail.ends_with("END") && tail.encode_utf16().count() <= 2000);
        assert_eq!(message.encode_utf16().count(), 1000);
    }

    #[test]
    fn unix_epoch_formats_as_utc() {
        assert_eq!(chrono_free_utc(0), "1970-01-01T00:00:00");
        assert_eq!(chrono_free_utc(1_767_225_600), "2026-01-01T00:00:00");
    }

    #[test]
    fn support_label_matches_worker_limit() {
        assert_eq!(
            support_label(" Alice ", " PC ").as_deref(),
            Some("Alice@PC")
        );
        assert_eq!(support_label("Ali\nce", "PC").as_deref(), Some("Alice@PC"));
        assert_eq!(support_label("", "PC"), None);
        assert_eq!(support_label(&"a".repeat(100), "PC").unwrap().len(), 96);
        assert_eq!(
            support_label(&"😀".repeat(50), "PC")
                .unwrap()
                .encode_utf16()
                .count(),
            96
        );
    }
}
