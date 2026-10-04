use velopack::{UpdateCheck, UpdateManager, sources::GithubSource};

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
    let manager = match manager() {
        Ok(manager) => manager,
        // Development/portable builds have no Velopack locator and must remain usable.
        Err(error) => return Status::Unavailable(error),
    };
    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(update)) => {
            let version = update.TargetFullRelease.Version.to_string();
            if let Err(error)=crate::maintenance::preserve_current_package_for_download(){return Status::Unavailable(error);}
            manager
                .download_updates(&update, None)
                .map_err(|e| e.to_string())
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
    crate::maintenance::prepare(&update)
}

/// `window`: where the watcher shows the update window (see `maintenance::apply`), or Velopack's
/// own dialog when `None`.
pub fn apply_and_restart(intent:Option<crate::ResumeIntent>,window:Option<String>) -> Result<(), String> {crate::maintenance::apply_prepared(intent,window)}

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
    apply_and_restart(Some(crate::ResumeIntent{microphone:false,headphones:false,monitor:0,full_monitor:false}),None)
}
