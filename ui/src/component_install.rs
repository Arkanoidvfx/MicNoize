//! Recoverable component placement. Downloads never hold the maintenance lock.
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::{Path, PathBuf}};

const TRANSACTION: &str = ".component-install";
const STAGE: &str = ".stage-rvc";
const NVIDIA: &str = "vendor/nvidia-afx-3.0.0";
const TAG: &str = "vendor/tag-2.0.0.1903-demo";
const RVC: &str = "vendor/vcclient-2.1.4-alpha";
const VOICES: &str = "vendor/vcclient-2.1.4-alpha/dist/main/model_dir";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Component { Core, Rvc, Models(String) }
impl Component {
    fn entries(&self) -> Result<Vec<String>, String> {
        Ok(match self {
            Self::Core => vec![NVIDIA.into(), TAG.into()],
            Self::Rvc => vec![RVC.into()],
            Self::Models(arch) if crate::components::MODEL_ARCHS.contains(&arch.as_str()) =>
                vec![format!("{}/{arch}", crate::components::MODELS_DIR)],
            Self::Models(_) => return Err("Неизвестная архитектура компонента".into()),
        })
    }
    fn validate(&self, root: &Path) -> Result<(), String> {
        for entry in self.entries()? {
            if !directory(&root.join(entry))? { return Err("Каталог компонента отсутствует".into()); }
        }
        let valid = match self {
            Self::Core => crate::components::core_installed(root),
            Self::Rvc => crate::components::rvc_installed(root),
            Self::Models(arch) => crate::components::models_installed(root, arch),
        };
        if valid { Ok(()) } else { Err(format!("Компонент в {} неполон или несовместим", root.display())) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum Phase { Applying, Committed }
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    root: PathBuf,
    component: Component,
    had_destination: Vec<bool>,
    // Recorded before any rename; also records whether an original RVC model_dir existed.
    preserved: Vec<String>,
    phase: Phase,
}

fn io(path: &Path, error: std::io::Error) -> String { format!("{}: {error}", path.display()) }
fn directory(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(format!("Ожидался обычный каталог: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io(path, e)),
    }
}
fn move_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to.parent().ok_or("У каталога нет родителя")?).map_err(|e| io(to, e))?;
    fs::rename(from, to).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))?;
    checkpoint();
    Ok(())
}
fn save(root: &Path, journal: &Journal) -> Result<(), String> {
    let path = root.join(TRANSACTION).join("journal.json");
    crate::maintenance::atomic(&path, &serde_json::to_vec(journal).map_err(|e| e.to_string())?)?;
    checkpoint();
    Ok(())
}
fn preserved_path(component: &Component, path: &str) -> bool {
    match component {
        Component::Rvc => path == VOICES,
        Component::Core => path.strip_prefix(&format!("{}/", crate::components::MODELS_DIR))
            .is_some_and(|name| !name.is_empty() && name.len() <= 64
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')),
        Component::Models(_) => false,
    }
}
fn read(root: &Path) -> Result<Option<Journal>, String> {
    let transaction = root.join(TRANSACTION);
    let path = transaction.join("journal.json");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if directory(&transaction)? && fs::read_dir(&transaction).map_err(|e| io(&transaction, e))?.next().is_some() {
                return Err(format!("Сохранены файлы установки без журнала: {}. Автоматическая очистка запрещена", transaction.display()));
            }
            return Ok(None);
        }
        Err(e) => return Err(io(&path, e)),
    };
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes).map_err(|e| io(&path, e))?;
    if bytes.len() > 16384 { return Err("Журнал компонента слишком велик".into()); }
    let j: Journal = serde_json::from_slice(&bytes).map_err(|e| io(&path, std::io::Error::other(e)))?;
    let entries = j.component.entries()?;
    if j.schema != 1 || j.root != fs::canonicalize(root).map_err(|e| io(root, e))?
        || j.had_destination.len() != entries.len()
        || j.preserved.iter().enumerate().any(|(i, p)| !preserved_path(&j.component, p)
            || j.preserved[..i].contains(p)
            || !entries.iter().enumerate().any(|(n, entry)| p.starts_with(&format!("{entry}/")) && j.had_destination[n])) {
        return Err("Недопустимое описание установки компонента; сохранённые файлы не изменены".into());
    }
    Ok(Some(j))
}

pub(crate) fn recover(root: &Path) -> Result<(), String> {
    let transaction = root.join(TRANSACTION);
    if !directory(&transaction)? || fs::read_dir(&transaction).map_err(|e| io(&transaction, e))?.next().is_none() { return Ok(()); }
    #[cfg(not(test))]
    let _lock = crate::maintenance::Lock::acquire()?;
    recover_locked(root)
}

pub(crate) fn prepare(root: &Path) -> Result<(), String> {
    #[cfg(not(test))]
    let _lock = crate::maintenance::Lock::acquire()?;
    recover_locked(root)?;
    if read(root)?.is_some() { return Err("Сначала необходимо завершить очистку предыдущего компонента".into()); }
    Ok(())
}

fn cleanup(root: &Path) -> Result<(), String> {
    for path in [root.join(TRANSACTION).join("backup"), root.join(STAGE)] {
        if directory(&path)? {
            // These fixed children are wholly owned by this validated transaction.
            fs::remove_dir_all(&path).map_err(|e| io(&path, e))?;
            checkpoint();
        }
    }
    let transaction = root.join(TRANSACTION);
    let temp = transaction.join("journal.tmp");
    match fs::remove_file(&temp) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(io(&temp, e)),
    }
    let path = transaction.join("journal.json");
    fs::remove_file(&path).map_err(|e| io(&path, e))?;
    checkpoint();
    Ok(())
}

fn recover_locked(root: &Path) -> Result<(), String> {
    let Some(j) = read(root)? else { return Ok(()); };
    if j.phase == Phase::Committed {
        j.component.validate(root)?;
        for path in &j.preserved {
            if !directory(&root.join(path))? { return Err(format!("Сохранённые модели отсутствуют: {path}")); }
        }
        if matches!(j.component, Component::Rvc) && !directory(&root.join(VOICES))? {
            return Err("Каталог голосов установленного RVC отсутствует".into());
        }
        if let Err(e) = cleanup(root) { crate::logs::note(root, &format!("Компонент установлен; очистка отложена: {e}")); }
        return Ok(());
    }
    let entries = j.component.entries()?;
    let backup = root.join(TRANSACTION).join("backup");
    let stage = root.join(STAGE);
    // Check every state before changing any path, including recovery interrupted halfway.
    for (entry, had) in entries.iter().zip(&j.had_destination) {
        let d = directory(&root.join(entry))?;
        let b = directory(&backup.join(entry))?;
        let s = directory(&stage.join(entry))?;
        if (*had && ((!b && !d) || (b && (d == s)))) || (!had && (b || (d && s))) {
            return Err(format!("Неоднозначное состояние {entry}; все копии сохранены"));
        }
    }
    for path in &j.preserved {
        let entry = entries.iter().find(|e| path.starts_with(&format!("{e}/"))).ok_or("Неизвестный каталог моделей")?;
        let original = if directory(&backup.join(entry))? { backup.join(path) } else { root.join(path) };
        let locations = [root.join(path), backup.join(path), stage.join(path)];
        let mut present = Vec::new();
        for candidate in locations { if directory(&candidate)? { present.push(candidate); } }
        if present.len() != 1 || (!directory(&backup.join(entry))? && present[0] != original) {
            return Err(format!("Неоднозначное расположение моделей {path}; все копии сохранены"));
        }
    }
    for (entry, had) in entries.iter().zip(&j.had_destination).rev() {
        let previous = backup.join(entry);
        if directory(&previous)? {
            for path in j.preserved.iter().filter(|p| p.starts_with(&format!("{entry}/"))) {
                if !directory(&backup.join(path))? {
                    let source = if directory(&stage.join(path))? { stage.join(path) } else { root.join(path) };
                    move_dir(&source, &backup.join(path))?;
                }
            }
            if directory(&root.join(entry))? { move_dir(&root.join(entry), &stage.join(entry))?; }
            move_dir(&previous, &root.join(entry))?;
        } else if !had && directory(&root.join(entry))? {
            move_dir(&root.join(entry), &stage.join(entry))?;
        }
    }
    cleanup(root)
}

pub(crate) fn place(root: &Path, component: Component) -> Result<(), String> {
    #[cfg(not(test))]
    let _lock = crate::maintenance::Lock::acquire()?;
    recover_locked(root)?;
    if read(root)?.is_some() { return Err("Сначала необходимо завершить очистку предыдущего компонента".into()); }
    let stage = root.join(STAGE);
    component.validate(&stage)?;
    let entries = component.entries()?;
    let mut had_destination = Vec::new();
    for entry in &entries { had_destination.push(directory(&root.join(entry))?); }
    let mut preserved = Vec::new();
    match &component {
        Component::Rvc => {
            if directory(&stage.join(VOICES))? { return Err("Архив runtime не должен содержать model_dir".into()); }
            if directory(&root.join(VOICES))? { preserved.push(VOICES.into()); }
            else { fs::create_dir_all(stage.join(VOICES)).map_err(|e| io(&stage, e))?; }
        }
        Component::Core => {
            let models = root.join(crate::components::MODELS_DIR);
            if directory(&models)? {
                for entry in fs::read_dir(&models).map_err(|e| io(&models, e))? {
                    let entry = entry.map_err(|e| io(&models, e))?;
                    let name = entry.file_name().into_string().map_err(|_| "Некорректное имя архитектуры моделей")?;
                    let path = format!("{}/{name}", crate::components::MODELS_DIR);
                    if !preserved_path(&component, &path) || !directory(&entry.path())? { return Err("Неизвестный файл в каталоге моделей; установка отменена".into()); }
                    if !directory(&stage.join(&path))? { preserved.push(path); }
                }
            }
        }
        Component::Models(_) => (),
    }
    preserved.sort();
    let transaction = root.join(TRANSACTION);
    fs::create_dir_all(&transaction).map_err(|e| io(&transaction, e))?;
    let mut j = Journal { schema: 1, root: fs::canonicalize(root).map_err(|e| io(root, e))?, component,
        had_destination, preserved, phase: Phase::Applying };
    save(root, &j)?;
    let result = (|| {
        let backup = transaction.join("backup");
        for (entry, had) in entries.iter().zip(&j.had_destination) {
            if *had { move_dir(&root.join(entry), &backup.join(entry))?; }
            for path in j.preserved.iter().filter(|p| p.starts_with(&format!("{entry}/"))) {
                move_dir(&backup.join(path), &stage.join(path))?;
            }
            move_dir(&stage.join(entry), &root.join(entry))?;
        }
        j.component.validate(root)?;
        j.phase = Phase::Committed;
        save(root, &j)
    })();
    if let Err(error) = result {
        return match recover_locked(root) {
            Ok(()) => Err(error),
            Err(recovery) => Err(format!("{error}; восстановление: {recovery}. Копии сохранены в {}", transaction.display())),
        };
    }
    recover_locked(root)
}

#[cfg(not(test))]
fn checkpoint() {}
#[cfg(test)]
thread_local! { static STEPS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, usize::MAX)) }; }
#[cfg(test)]
fn checkpoint() {
    STEPS.with(|state| { let (count, stop) = state.get(); state.set((count + 1, stop));
        if count + 1 == stop { std::process::exit(86); }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mnr-placement-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // The absolute UUID directory is allocated above, never a user-supplied path.
            assert!(self.0.is_absolute() && self.0.starts_with(std::env::temp_dir()));
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn put(root: &Path, name: &str, bytes: &[u8]) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn rvc_fixture(root: &Path) {
        put(root, &format!("{RVC}/old-runtime"), b"old");
        for (name, bytes) in [("3/voice.pth", &b"voice"[..]), ("3/params.json", b"metadata"), ("7/.mnr-name.txt", b"name")] {
            put(root, &format!("{VOICES}/{name}"), bytes);
        }
        put(&root.join(STAGE), &format!("{RVC}/dist/main/mnr_vcclient_server.exe"), b"new");
    }
    fn voices(root: &Path) {
        assert_eq!(fs::read(root.join(VOICES).join("3/voice.pth")).unwrap(), b"voice");
        assert_eq!(fs::read(root.join(VOICES).join("3/params.json")).unwrap(), b"metadata");
        assert_eq!(fs::read(root.join(VOICES).join("7/.mnr-name.txt")).unwrap(), b"name");
    }
    fn child(root: &Path, mode: &str, stop: usize) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "component_install::tests::checkpoint_child", "--nocapture"])
            .env("MNR_COMPONENT_TEST_ROOT", root)
            .env("MNR_COMPONENT_TEST_MODE", mode)
            .env("MNR_COMPONENT_TEST_STOP", stop.to_string())
            .output().unwrap()
    }
    fn steps(output: std::process::Output) -> usize {
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).lines().find_map(|s| s.strip_prefix("steps=")).unwrap().parse().unwrap()
    }
    #[test]
    fn checkpoint_child() {
        let Some(root) = std::env::var_os("MNR_COMPONENT_TEST_ROOT") else { return; };
        let root = PathBuf::from(root);
        let stop = std::env::var("MNR_COMPONENT_TEST_STOP").unwrap().parse().unwrap();
        STEPS.with(|s| s.set((0, stop)));
        match std::env::var("MNR_COMPONENT_TEST_MODE").unwrap().as_str() {
            "place" => place(&root, Component::Rvc).unwrap(),
            "core" => place(&root, Component::Core).unwrap(),
            "recover" => recover(&root).unwrap(),
            _ => panic!("unknown mode"),
        }
        STEPS.with(|s| println!("steps={}", s.get().0));
    }
    #[test]
    fn rvc_reinstall_and_each_process_interruption_preserve_voices() {
        let complete = Fixture::new(); rvc_fixture(&complete.0);
        let count = steps(child(&complete.0, "place", usize::MAX));
        voices(&complete.0);
        assert!(crate::components::rvc_installed(&complete.0));
        for stop in 1..=count {
            let fixture = Fixture::new(); rvc_fixture(&fixture.0);
            let output = child(&fixture.0, "place", stop);
            assert_eq!(output.status.code(), Some(86), "stop={stop}");
            let committed = read(&fixture.0).unwrap().is_none_or(|j| j.phase == Phase::Committed);
            recover(&fixture.0).unwrap(); recover(&fixture.0).unwrap();
            voices(&fixture.0);
            assert_eq!(crate::components::rvc_installed(&fixture.0), committed, "stop={stop}");
            assert_eq!(fixture.0.join(RVC).join("old-runtime").exists(), !committed);
        }
    }
    #[test]
    fn interruption_during_rollback_and_partial_cleanup_is_recoverable() {
        let baseline = Fixture::new(); rvc_fixture(&baseline.0);
        assert_eq!(child(&baseline.0, "place", 4).status.code(), Some(86));
        let count = steps(child(&baseline.0, "recover", usize::MAX));
        for stop in 1..=count {
            let fixture = Fixture::new(); rvc_fixture(&fixture.0);
            assert_eq!(child(&fixture.0, "place", 4).status.code(), Some(86));
            assert_eq!(child(&fixture.0, "recover", stop).status.code(), Some(86));
            recover(&fixture.0).unwrap(); recover(&fixture.0).unwrap();
            voices(&fixture.0);
            assert!(fixture.0.join(RVC).join("old-runtime").is_file());
        }
        let fixture = Fixture::new(); rvc_fixture(&fixture.0);
        assert_eq!(child(&fixture.0, "place", 5).status.code(), Some(86));
        // Commit is durable but deleting the old directory stopped partway through.
        assert_eq!(read(&fixture.0).unwrap().unwrap().phase, Phase::Committed);
        fs::remove_file(fixture.0.join(TRANSACTION).join("backup").join(RVC).join("old-runtime")).unwrap();
        recover(&fixture.0).unwrap(); voices(&fixture.0);
        assert!(read(&fixture.0).unwrap().is_none());
    }
    #[test]
    fn missing_second_core_entry_is_rejected_before_mutation_and_extra_models_survive() {
        let fixture = Fixture::new();
        put(&fixture.0, &format!("{NVIDIA}/bin/NVAudioEffects.dll"), b"old");
        put(&fixture.0, &format!("{TAG}/apidll/x64/tagapi.dll"), b"old");
        put(&fixture.0, &format!("{}/ada/denoiser_48k.trtpkg", crate::components::MODELS_DIR), b"ada1");
        put(&fixture.0, &format!("{}/ada/denoiser_v2_48k.trtpkg", crate::components::MODELS_DIR), b"ada2");
        let stage = fixture.0.join(STAGE);
        put(&stage, &format!("{NVIDIA}/bin/NVAudioEffects.dll"), b"new");
        assert!(place(&fixture.0, Component::Core).is_err());
        assert_eq!(fs::read(fixture.0.join(NVIDIA).join("bin/NVAudioEffects.dll")).unwrap(), b"old");
        put(&stage, &format!("{TAG}/apidll/x64/tagapi.dll"), b"new");
        place(&fixture.0, Component::Core).unwrap();
        assert!(crate::components::models_installed(&fixture.0, "ada"));
        assert_eq!(fs::read(fixture.0.join(crate::components::MODELS_DIR).join("ada/denoiser_48k.trtpkg")).unwrap(), b"ada1");
    }
    #[test]
    fn every_core_interruption_restores_both_entries_and_extra_models() {
        fn fixture(root: &Path, had_tag: bool) {
            put(root, &format!("{NVIDIA}/bin/NVAudioEffects.dll"), b"old");
            if had_tag { put(root, &format!("{TAG}/apidll/x64/tagapi.dll"), b"old"); }
            for file in ["denoiser_48k.trtpkg", "denoiser_v2_48k.trtpkg"] {
                put(root, &format!("{}/ada/{file}", crate::components::MODELS_DIR), b"ada");
            }
            put(&root.join(STAGE), &format!("{NVIDIA}/bin/NVAudioEffects.dll"), b"new");
            put(&root.join(STAGE), &format!("{TAG}/apidll/x64/tagapi.dll"), b"new");
        }
        for had_tag in [false, true] {
            let baseline = Fixture::new(); fixture(&baseline.0, had_tag);
            let count = steps(child(&baseline.0, "core", usize::MAX));
            for stop in 1..=count {
                let f = Fixture::new(); fixture(&f.0, had_tag);
                assert_eq!(child(&f.0, "core", stop).status.code(), Some(86));
                let committed = read(&f.0).unwrap().is_none_or(|j| j.phase == Phase::Committed);
                recover(&f.0).unwrap(); recover(&f.0).unwrap();
                assert_eq!(fs::read(f.0.join(NVIDIA).join("bin/NVAudioEffects.dll")).unwrap(), if committed { &b"new"[..] } else { b"old" });
                assert_eq!(f.0.join(TAG).exists(), committed || had_tag);
                assert_eq!(fs::read(f.0.join(crate::components::MODELS_DIR).join("ada/denoiser_48k.trtpkg")).unwrap(), b"ada");
            }
        }
    }
    #[test]
    fn orphan_corrupt_journal_and_unexpected_archive_models_are_not_deleted() {
        let fixture = Fixture::new(); rvc_fixture(&fixture.0);
        put(&fixture.0, ".component-install/backup/keep", b"only-copy");
        assert!(prepare(&fixture.0).is_err());
        put(&fixture.0, ".component-install/journal.json", b"{broken");
        assert!(recover(&fixture.0).is_err());
        assert_eq!(fs::read(fixture.0.join(".component-install/backup/keep")).unwrap(), b"only-copy");
        voices(&fixture.0);
        let other = Fixture::new(); rvc_fixture(&other.0);
        put(&other.0.join(STAGE), &format!("{VOICES}/foreign.pth"), b"foreign");
        assert!(place(&other.0, Component::Rvc).is_err());
        voices(&other.0);
    }
    #[test]
    fn locked_runtime_returns_error_without_deleting_models() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new(); rvc_fixture(&fixture.0);
        let handle = fs::OpenOptions::new().read(true).share_mode(1)
            .open(fixture.0.join(RVC).join("old-runtime")).unwrap();
        assert!(place(&fixture.0, Component::Rvc).is_err());
        voices(&fixture.0);
        assert_eq!(fs::read(fixture.0.join(RVC).join("old-runtime")).unwrap(), b"old");
        drop(handle);
        recover(&fixture.0).unwrap();
    }
    #[test]
    fn fresh_rvc_and_standalone_architecture_install() {
        let fixture = Fixture::new();
        put(&fixture.0.join(STAGE), &format!("{RVC}/dist/main/mnr_vcclient_server.exe"), b"new");
        place(&fixture.0, Component::Rvc).unwrap();
        assert!(fixture.0.join(VOICES).is_dir());
        let arch = format!("{}/blackwell", crate::components::MODELS_DIR);
        for file in ["denoiser_48k.trtpkg", "denoiser_v2_48k.trtpkg"] {
            put(&fixture.0.join(STAGE), &format!("{arch}/{file}"), b"new-model");
        }
        put(&fixture.0.join(STAGE), &format!("{arch}/version.txt"), b"3.0.0");
        place(&fixture.0, Component::Models("blackwell".into())).unwrap();
        assert!(crate::components::models_installed(&fixture.0, "blackwell"));
        assert!(place(&fixture.0, Component::Models("../invalid".into())).is_err());
    }
    #[test]
    fn ambiguous_models_and_missing_committed_models_preserve_all_copies() {
        let fixture = Fixture::new(); rvc_fixture(&fixture.0);
        assert_eq!(child(&fixture.0, "place", 4).status.code(), Some(86));
        let backup = fixture.0.join(TRANSACTION).join("backup");
        put(&backup, &format!("{VOICES}/duplicate.pth"), b"duplicate");
        assert!(recover(&fixture.0).is_err());
        voices(&fixture.0);
        assert!(backup.join(RVC).join("old-runtime").is_file());
        assert_eq!(fs::read(backup.join(VOICES).join("duplicate.pth")).unwrap(), b"duplicate");
        let committed = Fixture::new(); rvc_fixture(&committed.0);
        assert_eq!(child(&committed.0, "place", 5).status.code(), Some(86));
        fs::rename(committed.0.join(VOICES), committed.0.join("held-models")).unwrap();
        assert!(recover(&committed.0).is_err());
        assert!(committed.0.join(TRANSACTION).join("backup").join(RVC).join("old-runtime").is_file());
        assert_eq!(fs::read(committed.0.join("held-models/3/voice.pth")).unwrap(), b"voice");
    }
    #[test]
    fn empty_completed_transaction_is_a_startup_noop() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join(TRANSACTION)).unwrap();
        recover(&fixture.0).unwrap();
        assert!(fs::read_dir(fixture.0.join(TRANSACTION)).unwrap().next().is_none());
        assert!(!fixture.0.join(STAGE).exists());
    }
}
