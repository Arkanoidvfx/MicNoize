use velopack::{UpdateCheck, UpdateManager, sources::{GithubSource, HttpSource}};

const REPOSITORY: &str = "https://github.com/Arkanoidvfx/MicNoize";

#[derive(Clone, Debug)]
pub enum Status {
    Current,
    Ready(String),
    Unavailable(String),
}

fn manager() -> Result<UpdateManager, String> {
    UpdateManager::new(GithubSource::new(REPOSITORY, None, false), None, None)
        .map_err(|e| e.to_string())
}

pub fn check_and_download() -> Status {
    let status=check_and_download_inner();
    if let Ok(paths)=crate::paths::Paths::resolve(){
        match &status {
            Status::Unavailable(error)=>crate::telemetry::record(&paths.data,"update-check-error",serde_json::json!({"message":error})),
            _=>crate::telemetry::record(&paths.data,"check-none",serde_json::json!({})),
        }
    }
    status
}
fn check_and_download_inner() -> Status {
    let policy = match release_policy() { Ok(value) => value, Err(error) => return Status::Unavailable(error) };
    // The restart's takeover follows this percent; a package already on disk reports nothing.
    let downloaded = &crate::tacho::restart::DOWNLOADED;
    downloaded.store(0, std::sync::atomic::Ordering::Relaxed);
    let manager = match if let Some(base) = policy.release_base_url.as_deref() {
        UpdateManager::new(HttpSource::new(base), None, None).map_err(|e| e.to_string())
    } else { manager() } {
        Ok(manager) => manager,
        // Development/portable builds have no Velopack locator and must remain usable.
        Err(error) => return Status::Unavailable(error),
    };
    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(update)) => {
            let version = update.TargetFullRelease.Version.to_string();
            if !policy.permits(&version) { return Status::Current; }
            if let Err(error)=crate::maintenance::preserve_current_package_for_download(){return Status::Unavailable(error);}
            let (progress, reports) = std::sync::mpsc::channel::<i16>();
            std::thread::scope(|scope| {
                scope.spawn(move || reports.iter().for_each(|p| downloaded.store(p.clamp(0, 100) as u8, std::sync::atomic::Ordering::Relaxed)));
                manager.download_updates(&update, Some(progress))
            })
                .map_err(|e| e.to_string())
                .inspect(|_| downloaded.store(100, std::sync::atomic::Ordering::Relaxed))
                .and_then(|_| crate::maintenance::verify_download(&update.TargetFullRelease))
                .map(|_| Status::Ready(version))
                .unwrap_or_else(Status::Unavailable)
        }
        Ok(_) => Status::Current,
        Err(error) => Status::Unavailable(error.to_string()),
    }
}

pub fn prepare() -> Result<(), String> {
    let manager = manager()?;
    let update = manager
        .get_update_pending_restart()
        .ok_or("Скачанное обновление не найдено")?;
    if !release_policy()?.permits(&update.Version.to_string()) { return Err("Версия выше разрешённой в политике обновлений".into()); }
    crate::maintenance::prepare(&update)
}

/// `window`: where the watcher shows the update window (see `maintenance::apply`), or Velopack's
/// own dialog when `None`.
pub fn apply_and_restart(intent:Option<crate::ResumeIntent>,window:Option<String>,background:bool) -> Result<(), String> {
    let pending=manager()?.get_update_pending_restart().ok_or("Скачанное обновление не найдено")?;
    if !release_policy()?.permits(&pending.Version.to_string()){return Err("Версия выше разрешённой в политике обновлений".into());}
    crate::maintenance::apply_prepared(intent,window,background)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ReleasePolicy { channel: String, mode: String, pinned_version: Option<String>, release_base_url: Option<String> }
fn version_parts(value: &str) -> Option<[u32;3]> {
    let values: Vec<_> = value.split('.').map(str::parse::<u32>).collect::<Result<_,_>>().ok()?;
    values.try_into().ok()
}
impl ReleasePolicy {
    fn valid(&self) -> bool {
        self.channel == "win-x64-stable-v2" && match self.mode.as_str() {
            "auto" => self.pinned_version.is_none() && self.release_base_url.is_none(),
            "pinned" => self.pinned_version.as_deref().and_then(version_parts).is_some() && self.release_base_url.as_deref().is_some_and(|url| {
                url.strip_prefix("https://github.com/Arkanoidvfx/MicNoize/releases/download/").is_some_and(|tag| tag.strip_prefix('v').unwrap_or(tag) == self.pinned_version.as_deref().unwrap_or("") )
            }), _ => false,
        }
    }
    fn permits(&self, version: &str) -> bool {
        version_parts(version).is_some_and(|value| self.mode == "auto" || self.pinned_version.as_deref().and_then(version_parts).is_some_and(|limit| value <= limit))
    }
}
fn release_policy() -> Result<ReleasePolicy,String> {
    let path = crate::paths::Paths::resolve()?.data.join("release-policy.json");
    let result = (|| {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(4))).build().into();
        let text=agent.get("https://micnoize-telemetry.arkanoidvfx.workers.dev/v1/update-policy?channel=win-x64-stable-v2").call().map_err(|e|e.to_string())?
            .body_mut().read_to_string().map_err(|e|e.to_string())?;
        serde_json::from_str::<ReleasePolicy>(&text).map_err(|e|e.to_string())
    })();
    if let Ok(policy) = result && policy.valid() {
        if let Ok(json)=serde_json::to_vec(&policy) {let temp=path.with_extension("new");if std::fs::write(&temp,json).is_ok(){let _=std::fs::rename(temp,&path);}}
        return Ok(policy);
    }
    let cached:ReleasePolicy=std::fs::read(&path).ok().and_then(|v|serde_json::from_slice(&v).ok()).ok_or("Политика обновлений недоступна. Проверка отложена.")?;
    if cached.valid(){Ok(cached)}else{Err("Сохранённая политика обновлений недействительна".into())}
}

/// Explicit developer check: use the same verifier/transaction against a local full package.
pub fn check_local_package(path:&std::path::Path)->Result<(),String> {
    use velopack::locator::{auto_locate_app_manifest,find_local_full_packages,LocationContext};
    let location=auto_locate_app_manifest(LocationContext::FromCurrentExe).map_err(|e|e.to_string())?;
    let path=path.canonicalize().map_err(|e|e.to_string())?;
    let packages=location.get_packages_dir();
    if path.parent()!=Some(packages.canonicalize().map_err(|e|e.to_string())?.as_path()){return Err("Check package must be inside this installation's packages directory".into());}
    let (_,manifest)=find_local_full_packages(&packages).into_iter().find(|(candidate,_)|candidate.canonicalize().ok().as_ref()==Some(&path)).ok_or("Full package manifest missing")?;
    let asset=velopack::VelopackAsset{FileName:path.file_name().and_then(|s|s.to_str()).ok_or("Invalid package filename")?.into(),Version:manifest.version.to_string(),..Default::default()};
    crate::maintenance::prepare(&asset)?;
    apply_and_restart(Some(crate::ResumeIntent{microphone:false,headphones:false,monitor:0,full_monitor:false}),None,false)
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test] fn ceiling_is_numeric_and_does_not_allow_foreign_urls() {
        let mut p=ReleasePolicy{channel:"win-x64-stable-v2".into(),mode:"pinned".into(),pinned_version:Some("0.4.9".into()),release_base_url:Some("https://github.com/Arkanoidvfx/MicNoize/releases/download/v0.4.9".into())};
        assert!(p.valid()); assert!(p.permits("0.4.8")); assert!(p.permits("0.4.9")); assert!(!p.permits("0.4.10")); assert!(!p.permits("garbage"));
        p.release_base_url=Some("https://other.example/v0.4.9".into());assert!(!p.valid());
    }
}
