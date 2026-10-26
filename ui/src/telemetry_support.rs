use super::*;
use std::{path::PathBuf, sync::{Mutex, atomic::{AtomicBool, Ordering}}};
static STARTED: AtomicBool = AtomicBool::new(false);
static TRANSPORT: Mutex<()> = Mutex::new(());
static STATUS: Mutex<()> = Mutex::new(());

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) }
fn read(data: &Path) -> serde_json::Value { std::fs::read(data.join("telemetry-support.json")).ok().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_else(|| json!({})) }
fn write(data: &Path, value: &serde_json::Value) {
    let temp = data.join("telemetry-support.new");
    if serde_json::to_vec(value).ok().and_then(|v| std::fs::write(&temp, v).ok()).is_some() { let _ = std::fs::rename(temp, data.join("telemetry-support.json")); }
}
fn spool(data: &Path, body: &[u8]) -> Result<(), String> {
    let dir = data.join("telemetry-spool"); std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut files: Vec<_> = std::fs::read_dir(&dir).map_err(|e| e.to_string())?.filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|v| v == "json")).collect();
    files.sort_by_key(|e| e.file_name());
    let mut total: u64 = files.iter().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    while files.len() >= 50 || total + body.len() as u64 > 25 * 1024 * 1024 {
        if files.is_empty() { return Err("Лимит очереди диагностики".into()); }
        let file = files.remove(0); total = total.saturating_sub(file.metadata().map_or(0, |m| m.len())); std::fs::remove_file(file.path()).map_err(|e| e.to_string())?;
    }
    let milliseconds=SystemTime::now().duration_since(UNIX_EPOCH).map_or(0,|value|value.as_millis());
    let path = dir.join(format!("{milliseconds}-{}.json", Uuid::new_v4()));
    std::fs::write(path, body).map_err(|e| e.to_string())
}
fn backoff(state:&mut serde_json::Value,error:&str) {
    let failures=state["failures"].as_u64().unwrap_or(0).saturating_add(1).min(6);
    let retry=error.split("retry_after=").nth(1).and_then(|v|v.parse::<u64>().ok()).unwrap_or(0).min(86400);
    state["failures"]=json!(failures);state["retry_at"]=json!(now()+retry.max((60u64<<failures).min(3600)));
}
pub(super) fn deliver(data: &Path, body: &[u8], timeout: Duration) -> Result<serde_json::Value, String> {
    let Ok(_gate) = TRANSPORT.try_lock() else { spool(data, body)?; return Err("Отчёт сохранён в очереди".into()); };
    let mut state = read(data);
    if state["retry_at"].as_u64().unwrap_or(0) > now() { spool(data, body)?; return Err("Отчёт сохранён, повторная отправка отложена".into()); }
    match post_body(data, body, timeout) {
        Ok(result) => { state["retry_at"] = json!(0);state["failures"]=json!(0); write(data, &state); Ok(result) }
        Err(error) => { backoff(&mut state,&error); write(data, &state); spool(data, body)?; Err(error) }
    }
}
pub(super) fn start(data: &Path, runtime: &Path) {
    if !enabled() || cfg!(test) || STARTED.swap(true, Ordering::Relaxed) { return; }
    let data = data.to_path_buf(); let runtime = runtime.to_path_buf();
    std::thread::spawn(move || loop {
        let started=std::time::Instant::now();tick(&data, &runtime);
        std::thread::sleep(Duration::from_secs(600).saturating_sub(started.elapsed()));
    });
}
pub(super) fn feature_result(data:&Path,feature:&str,error:Option<String>) {
    if !enabled() || cfg!(test){return;}
    let data=data.to_path_buf();let feature=feature.to_owned();
    std::thread::spawn(move||{
        let Ok(_gate)=STATUS.lock()else{return;};
        let marker=data.join(format!("telemetry-{feature}-failure.json"));
        if let Some(error)=error {
            let mut previous:serde_json::Value=std::fs::read(&marker).ok().and_then(|v|serde_json::from_slice(&v).ok()).unwrap_or(json!({}));
            let count=previous["count"].as_u64().unwrap_or(0)+1;
            if previous["error"].as_str()==Some(error.as_str())&&now().saturating_sub(previous["sent_at"].as_u64().unwrap_or(0))<3600 {
                previous["count"]=json!(count);let _=std::fs::write(marker,previous.to_string());return;
            }
            let _=std::fs::write(marker,json!({"error":error,"count":0,"sent_at":now()}).to_string());
            let _=send(&data,vec![event("error",&feature,json!({"feature":feature,"source":"micnoize","message":error.chars().take(1000).collect::<String>(),"occurrences":count}))],Duration::from_secs(4));
        }else if std::fs::remove_file(marker).is_ok(){let _=send(&data,vec![event("support-recovery",&feature,json!({}))],Duration::from_secs(4));}
    });
}
fn tick(data: &Path, runtime: &Path) {
    {
        let Ok(_gate) = TRANSPORT.try_lock() else { return; }; let mut state = read(data);
        if state["retry_at"].as_u64().unwrap_or(0) <= now() {
            let mut pending: Vec<PathBuf> = std::fs::read_dir(data.join("telemetry-spool")).into_iter().flatten().filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|v| v == "json")).map(|e| e.path()).collect(); pending.sort();
            for path in pending.into_iter().take(5) {
                let Ok(body) = std::fs::read(&path) else { continue; };
                if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&body) && let Some(id)=value["diagnostic_id"].as_str() {
                    let current=format!("{}Z",chrono_free_utc(now()));
                    if state["diagnostic"]["id"].as_str()!=Some(id)||state["diagnostic"]["expires_at"].as_str().is_none_or(|v|v<=current.as_str()) {let _=std::fs::remove_file(path);continue;}
                }
                match post_body(data, &body, Duration::from_secs(4)){Ok(_)=>{let _=std::fs::remove_file(path);},Err(error)=>{backoff(&mut state,&error);break;}}
            }
        }
        write(data, &state);
    }
    let mut state = read(data);
    if state["version"].as_str() != Some(VERSION) || now().saturating_sub(state["summary_at"].as_u64().unwrap_or(0)) >= 86400 {
        state["version"]=json!(VERSION);state["summary_at"]=json!(now());write(data,&state);
        let _=send(data, vec![event("support-state", "current", json!({}))], Duration::from_secs(4));
    }
    state=read(data);
    if now().saturating_sub(state["hardware_at"].as_u64().unwrap_or(0)) >= 60 * 86400 {
        let hardware = state["prepared_hardware"].as_object().cloned().map(serde_json::Value::Object).or_else(hardware);
        if let Some(hardware) = hardware {
            state["prepared_hardware"] = hardware.clone(); write(data, &state);
            if send(data, vec![event("hardware", "snapshot", hardware)], Duration::from_secs(4)).is_ok() {
                state = read(data); state["hardware_at"] = json!(now()); state["prepared_hardware"] = serde_json::Value::Null; write(data, &state);
            }
        }
    }
    if let Ok(control) = send_with_control(data, vec![], Duration::from_secs(4), true, None) {
        let mut state = read(data); state["diagnostic"] = control["diagnostic"].clone(); write(data, &state);
        if let Some(id) = control["diagnostic"]["id"].as_str() {
            // Send a bounded snapshot only once per lease; subsequent status messages are incremental.
            if state["last_report"].as_str() != Some(id) {
                let mut events = report_events(data, runtime, runtime, "Диагностика по запросу");
                for event in &mut events { event["tag"] = json!("diagnostic"); }
                fit(&mut events);
                let _ = send_with_control(data, events, Duration::from_secs(8), false, Some(id));
                state = read(data); state["last_report"] = json!(id); write(data, &state);
            }
        }
    }
}

pub(super) fn status(data: &Path, message: &str, failed: bool, recovered: bool) {
    if !enabled() || cfg!(test) { return; }
    let data = data.to_path_buf(); let message = message.chars().take(1000).collect::<String>();
    std::thread::spawn(move || {
        let Ok(_gate)=STATUS.lock() else{return;};
        let control=read(&data);
        let status_dir=data.join("telemetry-status");let _=std::fs::create_dir_all(&status_dir);
        let mut state = read(&status_dir);
        if let (Some(id),Some(expires))=(control["diagnostic"]["id"].as_str(),control["diagnostic"]["expires_at"].as_str()) {
            let current=format!("{}Z",chrono_free_utc(now()));
            if expires>current.as_str() {
                if state["diagnostic_id"].as_str()!=Some(id){state["diagnostic_id"]=json!(id);state["diagnostic_bytes"]=json!(0);}
                let bytes=message.len() as u64+256;
                let used=state["diagnostic_bytes"].as_u64().unwrap_or(0);
                if used+bytes<=256*1024 {
                    state["diagnostic_bytes"]=json!(used+bytes);write(&status_dir,&state);
                    let _=send_with_control(&data,vec![event("diagnostic","status",json!({"message":message}))],Duration::from_secs(4),false,Some(id));
                }
            }
        }
        if failed {
            let fingerprint = hex::encode(Sha256::digest(message.as_bytes()));
            let previous = state["failure"]["fingerprint"].as_str() == Some(&fingerprint);
            let count = state["failure"]["count"].as_u64().unwrap_or(0) + 1;
            if previous && now().saturating_sub(state["failure"]["sent_at"].as_u64().unwrap_or(0)) < 3600 { state["failure"]["count"] = json!(count); write(&status_dir, &state); return; }
            state["failure"] = json!({"fingerprint":fingerprint,"sent_at":now(),"count":0}); write(&status_dir, &state);
            let _ = send(&data, vec![event("error", "engine", json!({"feature":"engine","source":"micnoize","message":message,"occurrences":if previous {count} else {1}}))], Duration::from_secs(4));
        } else if recovered && state["failure"].is_object() {
            state["failure"] = serde_json::Value::Null; write(&status_dir, &state);
            let _ = send(&data, vec![event("support-recovery", "engine", json!({}))], Duration::from_secs(4));
        }
    });
}
fn hardware() -> Option<serde_json::Value> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let script = include_str!("telemetry_hardware.ps1");
    let mut child = Command::new("powershell.exe").args(["-NoProfile","-NonInteractive","-Command",script]).creation_flags(0x08000000).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let started = std::time::Instant::now();
    loop { if child.try_wait().ok()?.is_some() { break; } else if started.elapsed() > Duration::from_secs(10) { let _ = child.kill(); let _ = child.wait(); return None; } std::thread::sleep(Duration::from_millis(50)); }
    let output = child.wait_with_output().ok()?; if !output.status.success() { return None; }
    let mut value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    value["hardware_hash"] = json!(hex::encode(Sha256::digest(serde_json::to_vec(&value).ok()?)));
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn retries_back_off_and_honor_the_server_delay(){
        let mut state=json!({});backoff(&mut state,"offline");let first=state["retry_at"].as_u64().unwrap();
        backoff(&mut state,"offline");assert!(state["retry_at"].as_u64().unwrap()>first);
        backoff(&mut state,"HTTP 429; retry_after=7200");assert!(state["retry_at"].as_u64().unwrap()>=now()+7200);
    }
    #[test] fn spool_is_bounded_without_touching_live_user_state(){
        let base=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.tmp");std::fs::create_dir_all(&base).unwrap();
        let root=base.join(format!("telemetry-test-{}",Uuid::new_v4()));std::fs::create_dir_all(&root).unwrap();
        for _ in 0..55{spool(&root,b"fixture").unwrap();}
        assert_eq!(std::fs::read_dir(root.join("telemetry-spool")).unwrap().count(),50);
        assert!(root.canonicalize().unwrap().starts_with(base.canonicalize().unwrap()));std::fs::remove_dir_all(root).unwrap();
    }
}
