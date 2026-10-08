//! A durable update record lives outside Velopack's replaceable `current` directory.
//! Velopack replaces the entire UI/host package; a retained package provides rollback.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{ffi::c_char, fs::{self, File}, io::{Read, Write}, path::{Path, PathBuf}, process::Command, os::windows::{process::CommandExt, io::AsRawHandle}};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "MicNoize.RecoverUpdate";
// Published runtime-core-v2, older than the later source change deleting TAG defaults.
const LEGACY_HOST_SHA256:&str="0D196EEFF1CFDD6CC2A6E2EC89866A4577019D19DC24A8F100AFE499974BC726";
static RESUME:std::sync::Mutex<Option<crate::ResumeIntent>>=std::sync::Mutex::new(None);
pub fn take_resume()->Option<crate::ResumeIntent>{RESUME.lock().ok()?.take()}
unsafe extern "C" {
    fn mnr_replace_file(from: *const u8, fl: u32, to: *const u8, tl: u32) -> i32;
    fn mnr_tag_stop_host(error: *mut c_char, capacity: u32) -> i32;
    fn mnr_tag_repair_lines(error: *mut c_char, capacity: u32) -> i32;
    fn mnr_tag_legacy_host(stop:i32,error:*mut c_char,capacity:u32)->i32;
    fn mnr_tag_remove_task(error:*mut c_char,capacity:u32)->i32;
    fn mnr_tag_autostart(mode:i32,error:*mut c_char,capacity:u32)->i32;
    fn mnr_refresh_host(error: *mut c_char, capacity: u32) -> i32;
    fn mnr_tag_task_enabled(mode: i32, error: *mut c_char, capacity: u32) -> i32;
}
#[link(name="kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const u8, owned: i32, name: *const u16) -> isize;
    fn WaitForSingleObject(handle: isize, timeout: u32) -> u32;
    fn ReleaseMutex(handle: isize) -> i32;
    fn CloseHandle(handle: isize) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetCurrentProcess() -> isize;
    fn GetLastError()->u32;
    fn GetProcessTimes(handle: isize, created: *mut u64, exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
}
pub struct Lock(isize);
impl Lock {
    pub fn acquire() -> Result<Self, String> {
        Self::wait(0)
    }
    fn wait(timeout: u32) -> Result<Self, String> {
        let name: Vec<u16> = "Local\\MicNoize.Maintenance.v1\0".encode_utf16().collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let wait = if handle != 0 { unsafe { WaitForSingleObject(handle, timeout) } } else { u32::MAX };
        if wait != 0 && wait != 0x80 {
            if handle != 0 { unsafe { CloseHandle(handle); } }
            return Err("Обновление или восстановление устройства уже выполняется".into());
        }
        Ok(Self(handle))
    }
}
impl Drop for Lock { fn drop(&mut self) { unsafe { ReleaseMutex(self.0); CloseHandle(self.0); } } }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub version: String,
    protocol: u32,
    host_build: u32,
    ui_sha256: String,
    host_sha256: String,
}
impl Bundle {
    fn validate(&self) -> Result<(), String> {
        if self.version.is_empty() || self.version.len() > 128 || self.protocol == 0 || self.host_build == 0
            || [&self.ui_sha256, &self.host_sha256].iter().any(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err("Неверное описание комплекта UI/хоста".into());
        }
        Ok(())
    }
    fn matches(&self, directory: &Path) -> Result<(), String> {
        self.validate()?;
        check_hash(&directory.join("MicNoize.exe"), &self.ui_sha256)?;
        check_hash(&directory.join("mic_tag_host.exe"), &self.host_sha256)
    }
    fn installed(&self,root:&Path)->Result<(),String> {
        use velopack::locator::{auto_locate_app_manifest,LocationContext};
        self.matches(&root.join("current"))?;
        let location=auto_locate_app_manifest(LocationContext::FromSpecifiedRootDir(root.to_path_buf(),None)).map_err(|e|format!("Метаданные установки {}: {e}",root.display()))?;
        if location.get_manifest_id()!="MicNoize" || location.get_manifest_version().to_string()!=self.version{return Err("Метаданные установки не соответствуют комплекту UI/хоста".into());}
        Ok(())
    }
}
fn hash(reader: &mut impl Read) -> Result<String, String> {
    let mut hash = Sha256::new(); let mut block = [0u8; 65536];
    loop { let n = reader.read(&mut block).map_err(|e| e.to_string())?; if n == 0 { break; } hash.update(&block[..n]); }
    Ok(hex::encode(hash.finalize()))
}
fn check_hash(path: &Path, expected: &str) -> Result<(), String> {
    let actual = hash(&mut File::open(path).map_err(|e| format!("{}: {e}", path.display()))?)?;
    if !actual.eq_ignore_ascii_case(expected) { return Err(format!("Контрольная сумма не совпала: {}", path.display())); }
    Ok(())
}
fn archive_entry(names:&[String],name:&str)->Result<String,String> {
    let found:Vec<_>=names.iter().filter(|n|n.as_str()==name || n.ends_with(&format!("/{name}"))).collect();
    if found.len()!=1 || found[0].split('/').any(|part|part==".." || part.contains('\\')) {
        return Err(format!("Пакет не содержит однозначный {name}; нужен полный совместимый комплект Mic Noize"));
    }
    Ok(found[0].clone())
}
pub fn package(path: &Path, version: &str) -> Result<Bundle, String> {
    let mut archive = zip::ZipArchive::new(File::open(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if archive.len()>4096{return Err("Слишком много файлов в пакете приложения".into());}
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    let entry = |name: &str| archive_entry(&names,name);
    let mut data = Vec::new();
    archive.by_name(&entry("micnoize-bundle.json")?).map_err(|e| e.to_string())?.take(8193).read_to_end(&mut data).map_err(|e| e.to_string())?;
    if data.len() > 8192 { return Err("Описание комплекта слишком велико".into()); }
    let bundle: Bundle = serde_json::from_slice(&data).map_err(|e| e.to_string())?; bundle.validate()?;
    if bundle.version != version { return Err("Версия пакета не совпадает с версией UI/хоста".into()); }
    for (name, expected, limit) in [("MicNoize.exe", &bundle.ui_sha256, 512*1024*1024), ("mic_tag_host.exe", &bundle.host_sha256, 64*1024*1024)] {
        let mut file = archive.by_name(&entry(name)?).map_err(|e| e.to_string())?;
        if file.size() > limit || !hash(&mut file)?.eq_ignore_ascii_case(expected) { return Err(format!("Повреждён файл {name} в пакете")); }
    }
    Ok(bundle)
}
fn signature_path(runtime:&Path,package:&str)->PathBuf {runtime.join(".update").join(format!("{package}.sig.json"))}
fn check_update_name(version:&str,package:&str)->Result<(),String> {
    let semver=version.split('.').count()==3 && version.split('.').all(|part|!part.is_empty() && part.len()<=9 && part.bytes().all(|b|b.is_ascii_digit()));
    if !semver || package!=format!("MicNoize-{version}-win-x64-stable-v2-full.nupkg") {return Err("Неверное имя пакета обновления".into());}
    Ok(())
}
/// Every file Velopack installs from the full package (all of `lib/`, its launcher and updater
/// included) must match the payload the release key signed, and no signed file may be missing.
fn check_signed_files(package:&Path,version:&str,payload:&str)->Result<(),String> {
    let mut lines=payload.lines();
    if lines.next()!=Some(format!("MicNoize {version}").as_str()) {return Err("Подпись относится к другой версии обновления".into());}
    let mut signed=std::collections::HashMap::new();
    for line in lines {
        let (sha,name)=line.split_once("  ").ok_or("Неверная строка подписи обновления")?;
        if sha.len()!=64 || !sha.bytes().all(|b|b.is_ascii_hexdigit()) || signed.insert(name,sha).is_some() {return Err("Неверная строка подписи обновления".into());}
    }
    let mut archive=zip::ZipArchive::new(File::open(package).map_err(|e|format!("Открытие пакета {}: {e}",package.display()))?).map_err(|e|e.to_string())?;
    if archive.len()>4096 {return Err("Слишком много файлов в пакете приложения".into());}
    for index in 0..archive.len() {
        let mut file=archive.by_index(index).map_err(|e|e.to_string())?;
        let name=file.name().replace('\\',"/");
        if name.starts_with('/') || name.contains(':') || name.split('/').any(|part|part=="..") {return Err(format!("Недопустимый путь в пакете обновления: {name}"));}
        if name.ends_with('/') || !name.get(..4).is_some_and(|prefix|prefix.eq_ignore_ascii_case("lib/")) {continue;}
        let expected=signed.remove(name.as_str()).ok_or_else(||format!("Файл обновления не подписан: {name}"))?;
        if file.size()>512*1024*1024 || !hash(&mut file)?.eq_ignore_ascii_case(expected) {return Err(format!("Файл обновления не совпал с подписью: {name}"));}
    }
    if let Some(name)=signed.keys().next() {return Err(format!("В обновлении нет подписанного файла: {name}"));}
    Ok(())
}
/// Fails closed: an update applies only with a valid release signature over all installed files.
fn verify_update(package:&Path,version:&str,envelope:&str,key:[u8;32])->Result<(),String> {
    check_update_name(version,package.file_name().and_then(|n|n.to_str()).unwrap_or_default())?;
    if envelope.len()>1<<20 {return Err("Подпись обновления слишком велика".into());}
    let payload=crate::components::signed_payload(envelope,key).map_err(|_|String::from("Подпись обновления не прошла проверку"))?;
    check_signed_files(package,version,&payload)
}
/// After a download: fetches the update's signature from its release and checks the package. The
/// checked signature stays in `.update`, so an offline apply can check the same package again.
pub fn verify_download(candidate:&velopack::VelopackAsset)->Result<(),String> {
    use velopack::locator::{auto_locate_app_manifest,LocationContext};
    check_update_name(&candidate.Version,&candidate.FileName)?;
    let location=auto_locate_app_manifest(LocationContext::FromCurrentExe).map_err(|e|e.to_string())?;
    let runtime=crate::paths::Paths::resolve()?.runtime_root().to_path_buf();
    let envelope=crate::components::update_signature(&candidate.Version,&candidate.FileName)?;
    verify_update(&location.get_packages_dir().join(&candidate.FileName),&candidate.Version,&envelope,crate::components::release_key()?)?;
    fs::create_dir_all(runtime.join(".update")).map_err(|e|e.to_string())?;
    atomic(&signature_path(&runtime,&candidate.FileName),envelope.as_bytes())
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyBackup {
    install:PathBuf,
    version:String,
    package_name:String,
    package_sha256:String,
    ui_sha256:String,
    host_sha256:Option<String>,
    host_login:[Option<String>;2],
    #[serde(default)]
    app_login:Option<String>,
}
fn legacy_ui_hash(path:&Path)->Result<Option<String>,String> {
    let mut archive=zip::ZipArchive::new(File::open(path).map_err(|e|format!("Открытие файла {}: {e}",path.display()))?).map_err(|e|e.to_string())?;
    if archive.len()>4096{return Err("Слишком много файлов в старом пакете".into());}
    let names:Vec<String>=archive.file_names().map(str::to_owned).collect();
    if names.iter().any(|n|n=="micnoize-bundle.json" || n.ends_with("/micnoize-bundle.json")){return Ok(None);}
    let mut ui=archive.by_name(&archive_entry(&names,"MicNoize.exe")?).map_err(|e|e.to_string())?;
    if ui.size()>512*1024*1024{return Err("Старое приложение превышает допустимый размер".into());}
    hash(&mut ui).map(Some)
}
fn legacy_login(name:&str)->Result<Option<String>,String> {
    #[link(name="advapi32")]
    unsafe extern "system" {fn RegGetValueW(key:isize,path:*const u16,name:*const u16,flags:u32,kind:*mut u32,data:*mut u8,size:*mut u32)->i32;}
    let key:Vec<u16>="Software\\Microsoft\\Windows\\CurrentVersion\\Run\0".encode_utf16().collect();
    let name:Vec<u16>=name.encode_utf16().chain(Some(0)).collect();
    let mut bytes=0;
    let read=|data:*mut u8,bytes:&mut u32|unsafe{RegGetValueW(0x80000001u32 as i32 as isize,key.as_ptr(),name.as_ptr(),2,std::ptr::null_mut(),data,bytes)};
    let result=read(std::ptr::null_mut(),&mut bytes);
    if result==2{return Ok(None);}
    if result!=0 || !(2..=65536).contains(&bytes) || bytes%2!=0{return Err(format!("Не удалось сохранить старый автозапуск: {result}"));}
    let mut text=vec![0u16;bytes as usize/2];
    let result=read(text.as_mut_ptr().cast(),&mut bytes);
    if result!=0 || bytes as usize>text.len()*2{return Err(format!("Старый автозапуск изменился во время чтения: {result}"));}
    let count=text.iter().position(|c|*c==0).ok_or("Неверная строка старого автозапуска")?;
    String::from_utf16(&text[..count]).map(Some).map_err(|e|e.to_string())
}
fn retain_legacy(install:&Path,runtime:&Path,previous:&Path,version:&str,login:[Option<String>;2])->Result<LegacyBackup,String> {
    let files=runtime.join(".update/legacy");fs::create_dir_all(&files).map_err(|e|e.to_string())?;
    let record=files.join("backup.json");
    if record.exists() {
        let mut data=Vec::new();File::open(&record).map_err(|e|e.to_string())?.take(65537).read_to_end(&mut data).map_err(|e|e.to_string())?;
        if data.len()>65536{return Err("Слишком большое описание старой установки".into());}
        let saved:LegacyBackup=serde_json::from_slice(&data).map_err(|e|e.to_string())?;
        if saved.install!=install{return Err("Сохранённая старая версия принадлежит другой установке".into());}
        check_hash(&files.join("previous.nupkg"),&saved.package_sha256)?;
        if legacy_ui_hash(&files.join("previous.nupkg"))?.as_ref()!=Some(&saved.ui_sha256){return Err("Сохранённое старое приложение повреждено".into());}
        if let Some(hash)=&saved.host_sha256{check_hash(&files.join("mic_tag_host.exe"),hash)?;}
        return Ok(saved);
    }
    let ui_sha256=legacy_ui_hash(previous)?.ok_or("Ожидался старый пакет без отдельного хоста")?;
    copy_synced(previous,&files.join("previous.nupkg"))?;
    if legacy_ui_hash(&files.join("previous.nupkg"))?.as_ref()!=Some(&ui_sha256){return Err("Старый пакет изменился во время сохранения".into());}
    let host=runtime.join("bin/mic_tag_host.exe");
    let host_sha256=if host.is_file(){copy_synced(&host,&files.join("mic_tag_host.exe"))?;Some(hash(&mut File::open(files.join("mic_tag_host.exe")).map_err(|e|e.to_string())?)?)}else{None};
    let saved=LegacyBackup{install:install.to_path_buf(),version:version.into(),package_name:previous.file_name().and_then(|n|n.to_str()).ok_or("Неверное имя старого пакета")?.into(),package_sha256:hash(&mut File::open(files.join("previous.nupkg")).map_err(|e|e.to_string())?)?,ui_sha256,host_sha256,host_login:login,app_login:if cfg!(test){None}else{legacy_login("MicNoize")?}};
    // Publish last: interrupted copies are retried before Velopack may clean packages.
    let data=serde_json::to_vec(&saved).map_err(|e|e.to_string())?;
    if data.len()>65536{return Err("Слишком большое описание старой установки".into());}
    atomic(&record,&data)?;Ok(saved)
}
pub fn preserve_legacy()->Result<(),String> {
    use velopack::locator::{auto_locate_app_manifest,find_local_full_packages,LocationContext};
    if std::env::args().any(|arg|arg.starts_with("--veloapp-") || arg=="--recover-update"){return Ok(());}
    let Ok(location)=auto_locate_app_manifest(LocationContext::FromCurrentExe) else{return Ok(());};
    let paths=crate::paths::Paths::resolve()?;let runtime=paths.runtime_root();
    if runtime.join(".update/journal.json").exists(){return Ok(());}
    let _lock=Lock::acquire()?;
    let mut previous=find_local_full_packages(&location.get_packages_dir()).into_iter()
        .filter(|(_,m)|m.id=="MicNoize" && m.version<location.get_manifest_version()).collect::<Vec<_>>();
    previous.sort_by(|(_,a),(_,b)|b.version.cmp(&a.version));
    if let Some((path,manifest))=previous.first() && legacy_ui_hash(path)?.is_some() {
        retain_legacy(&location.get_root_dir(),runtime,path,&manifest.version.to_string(),[legacy_login("MicNoize.TagHost")?,legacy_login("MicNoiseReducer.TagHost")?])?;
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
enum Phase { Prepared, Applying, RollingBack, Complete }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    transaction: String,
    install: PathBuf,
    runtime: PathBuf,
    phase: Phase,
    before: Bundle,
    after: Bundle,
    previous_hash: String,
    candidate_hash: String,
    updater_hash: String,
    task_enabled: bool,
    #[serde(default)]
    applier: Option<(u32,u64)>,
    #[serde(default)]
    rollback_attempts: u8,
    #[serde(default)]
    resume:Option<crate::ResumeIntent>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    legacy:Option<LegacyBackup>,
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp).map_err(|e| format!("Создание {}: {e}", temp.display()))?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?; drop(file);
    let from = temp.to_str().ok_or("Invalid temporary path")?; let to = path.to_str().ok_or("Invalid journal path")?;
    if unsafe { mnr_replace_file(from.as_ptr(), from.len() as u32, to.as_ptr(), to.len() as u32) } == 0 { return Err("Не удалось сохранить этап обновления".into()); }
    Ok(())
}
fn save(journal: &Journal) -> Result<(), String> { atomic(&journal.runtime.join(".update/journal.json"), &serde_json::to_vec(journal).map_err(|e| e.to_string())?) }
fn copy_synced(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to).map_err(|e| format!("Копирование {} -> {}: {e}", from.display(), to.display()))?;
    File::options().write(true).open(to).and_then(|f| f.sync_all()).map_err(|e| format!("Сохранение {}: {e}", to.display()))
}
fn retained_package(runtime:&Path,version:&str)->PathBuf {
    runtime.join(".update").join(format!("previous-{version}.nupkg"))
}
fn current_package(packages:&Path,runtime:&Path,version:&str)->Result<PathBuf,String> {
    use velopack::locator::find_local_full_packages;
    find_local_full_packages(packages).into_iter()
        .find(|(_,m)|m.id=="MicNoize" && m.version.to_string()==version).map(|(path,_)|path)
        .or_else(||retained_package(runtime,version).is_file().then(||retained_package(runtime,version)))
        .ok_or("Предыдущий полный пакет отсутствует: безопасный откат недоступен".into())
}
fn preserve_current_package(root:&Path,packages:&Path,runtime:&Path,version:&str)->Result<(),String> {
    use velopack::locator::find_local_full_packages;
    let retained=retained_package(runtime,version);
    if retained.is_file() && package(&retained,version).and_then(|b|b.installed(root)).is_ok(){return Ok(());}
    let source=find_local_full_packages(packages).into_iter()
        .find(|(_,m)|m.id=="MicNoize" && m.version.to_string()==version)
        .ok_or("Полный пакет текущей версии отсутствует; безопасный откат недоступен")?.0;
    package(&source,version)?.installed(root)?;
    fs::create_dir_all(retained.parent().unwrap()).map_err(|e|e.to_string())?;
    let source_hash=hash(&mut File::open(&source).map_err(|e|e.to_string())?)?;
    let pending=retained.with_extension("partial");copy_synced(&source,&pending)?;
    check_hash(&pending,&source_hash)?;
    package(&pending,version)?.installed(root)?;
    if retained.exists(){fs::remove_file(&retained).map_err(|e|e.to_string())?;}
    fs::rename(&pending,&retained).map_err(|e|e.to_string())
}
pub fn preserve_current_package_for_download()->Result<(),String> {
    use velopack::locator::{auto_locate_app_manifest,LocationContext};
    let _lock=Lock::acquire()?;
    let location=auto_locate_app_manifest(LocationContext::FromCurrentExe).map_err(|e|e.to_string())?;
    let runtime=crate::paths::Paths::resolve()?.runtime_root().to_path_buf();
    preserve_current_package(&location.get_root_dir(),&location.get_packages_dir(),&runtime,&location.get_manifest_version().to_string())
}
fn native(call: impl FnOnce(*mut c_char, u32) -> i32, minimum: i32) -> Result<i32, String> {
    let mut error = [0u8; 4096]; let result = call(error.as_mut_ptr().cast(), error.len() as u32);
    if result < minimum { return Err(String::from_utf8_lossy(&error[..error.iter().position(|c| *c == 0).unwrap_or(error.len())]).into_owned()); }
    Ok(result)
}
fn task(mode: i32) -> Result<bool, String> { native(|e,n| unsafe { mnr_tag_task_enabled(mode,e,n) },0).map(|n| n != 0) }
fn stop() -> Result<(), String> { native(|e,n| unsafe { mnr_tag_stop_host(e,n) },1).map(|_| ()) }
fn ready_with_retry(require_device:bool,mut check:impl FnMut()->Result<(),String>,mut wait:impl FnMut(u64))->Result<(),String> {
    if !require_device {return Ok(());}
    let mut result=check();
    for seconds in crate::RECOVERY_DELAYS {
        if !result.as_ref().is_err_and(|error|crate::transient_device_failure(error)){return result;}
        wait(seconds);result=check();
    }
    result
}
fn verify_ready(runtime:&Path,allow_setup:bool)->Result<(),String> {
    // Paired UI/host files were validated by installed() before this call. Core and the
    // machine-wide driver are installed by the UI; rollback cannot restore either of them.
    // Legacy migration still needs a live endpoint to validate its explicit line transfer.
    let require_device=!allow_setup || (crate::components::core_installed(runtime) && crate::components::driver_installed());
    if !require_device {
        crate::logs::note(runtime,"Комплект UI/хоста проверен; core или TAG-драйвер отсутствует. Открываем интерфейс для завершения установки устройства.");
    }
    ready_with_retry(require_device,||native(|e,n|unsafe{mnr_refresh_host(e,n)},1).map(|_|()),|seconds|{
        crate::logs::note(runtime,&format!("Проверка комплекта: ожидаем устройство, повтор через {seconds} с"));
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    })
}
fn run_recovery(exe: Option<&Path>, runtime: &Path) -> Result<(), String> {
    let value = exe.map(|exe| format!("\"{}\" --recover-update \"{}\"", exe.display(), runtime.display()));
    write_run(RUN_NAME, value.as_deref())
        .map_err(|e| format!("Автозапуск восстановления {RUN_NAME}: {e}"))
}
fn folder(j: &Journal) -> PathBuf { j.runtime.join(".update").join(&j.transaction) }
fn legacy_guard()->Result<Lock,String> {
    let name:Vec<u16>="Local\\MicNoize.SingleInstance\0".encode_utf16().collect();
    let handle=unsafe{CreateMutexW(std::ptr::null(),1,name.as_ptr())};
    let existed=unsafe{GetLastError()}==183;
    if handle==0 || existed {
        if handle!=0{unsafe{CloseHandle(handle);}}
        return Err("Закройте Mic Noize перед переходом со старой версии".into());
    }
    Ok(Lock(handle))
}
fn write_run(name:&str,value:Option<&str>)->Result<(),String> {
    #[link(name="advapi32")]
    unsafe extern "system" {
        fn RegCreateKeyExW(root:isize,path:*const u16,reserved:u32,class:*mut u16,options:u32,access:u32,security:*const u8,key:*mut isize,disposition:*mut u32)->i32;
        fn RegSetValueExW(key:isize,name:*const u16,reserved:u32,kind:u32,data:*const u8,size:u32)->i32;
        fn RegDeleteValueW(key:isize,name:*const u16)->i32;
        fn RegCloseKey(key:isize)->i32;
    }
    let path:Vec<u16>=RUN_KEY.trim_start_matches("HKCU\\").encode_utf16().chain(Some(0)).collect();
    let name:Vec<u16>=name.encode_utf16().chain(Some(0)).collect();let mut key=0;
    let result=unsafe{RegCreateKeyExW(0x80000001u32 as i32 as isize,path.as_ptr(),0,std::ptr::null_mut(),0,2,std::ptr::null(),&mut key,std::ptr::null_mut())};
    if result!=0{return Err(format!("Не удалось открыть автозапуск: {result}"));}
    let result=if let Some(value)=value {
        let text:Vec<u16>=value.encode_utf16().chain(Some(0)).collect();
        unsafe{RegSetValueExW(key,name.as_ptr(),0,1,text.as_ptr().cast(),(text.len()*2) as u32)}
    }else{unsafe{RegDeleteValueW(key,name.as_ptr())}};
    unsafe{RegCloseKey(key);}
    if result!=0 && !(value.is_none() && result==2){return Err(format!("Не удалось восстановить автозапуск: {result}"));}
    Ok(())
}
fn legacy_runs(j:&Journal,restore:bool)->Result<(),String> {
    let legacy=j.legacy.as_ref().ok_or("Описание старой установки отсутствует")?;
    for (name,value) in [("MicNoize.TagHost",&legacy.host_login[0]),("MicNoiseReducer.TagHost",&legacy.host_login[1]),("MicNoize",&legacy.app_login)] {
        write_run(name,if restore{value.as_deref()}else{None})?;
    }
    Ok(())
}
fn locked_legacy(j:&Journal)->Result<File,String> {
    use std::os::windows::fs::OpenOptionsExt;
    let expected=j.legacy.as_ref().and_then(|old|old.host_sha256.as_ref()).ok_or("Предыдущий core-хост отсутствует")?;
    if !expected.eq_ignore_ascii_case(LEGACY_HOST_SHA256){return Err("Версия старого core-хоста не проверена для отката".into());}
    let path=j.runtime.join("bin/mic_tag_host.exe");
    let mut file=File::options().read(true).share_mode(1).open(&path).map_err(|e|e.to_string())?;
    if !hash(&mut file)?.eq_ignore_ascii_case(expected){return Err("Старый хост изменился; его процесс не остановлен".into());}
    Ok(file)
}
fn legacy_call(j:&Journal,stop:bool)->Result<(),String> {
    let _image=locked_legacy(j)?;
    native(|e,n|unsafe{mnr_tag_legacy_host(i32::from(stop),e,n)},1).map(|_|())
}
fn legacy_before_matches(j:&Journal)->Result<(),String> {
    legacy_ui_matches(j)?;
    let _image=locked_legacy(j)?;Ok(())
}
fn legacy_ui_matches(j:&Journal)->Result<(),String> {
    use velopack::locator::{auto_locate_app_manifest,LocationContext};
    check_hash(&j.install.join("current/MicNoize.exe"),&j.before.ui_sha256)?;
    let location=auto_locate_app_manifest(LocationContext::FromSpecifiedRootDir(j.install.clone(),None)).map_err(|e|e.to_string())?;
    if location.get_manifest_id()!="MicNoize" || location.get_manifest_version().to_string()!=j.before.version{return Err("Метаданные старой установки не восстановлены".into());}
    Ok(())
}
fn stop_upgrade_host(j:&Journal)->Result<(),String> {
    if let Err(error)=stop() {
        legacy_call(j,true).map_err(|old|format!("{error}; {old}"))?;
        stop()?;
    }
    task(0)?;Ok(())
}
fn remove_upgrade_task(j:&Journal)->Result<(),String> {
    if j.legacy.is_none(){return Err("Удаление задачи разрешено только при откате legacy-перехода".into());}
    native(|e,n|unsafe{mnr_tag_remove_task(e,n)},1).map(|_|())
}
fn retain_current_package(j:&Journal)->Result<(),String> {
    let source=folder(j).join("candidate.nupkg");
    check_hash(&source,&j.candidate_hash)?;
    let parts:Vec<_>=j.after.version.split('.').collect();
    if parts.len()!=3 || parts.iter().any(|part|part.is_empty() || !part.bytes().all(|b|b.is_ascii_digit())) {
        return Err("Неверная версия полного пакета".into());
    }
    let packages=j.install.join("packages");
    fs::create_dir_all(&packages).map_err(|e|e.to_string())?;
    let destination=packages.join(format!("MicNoize-{}-win-x64-stable-v2-full.nupkg",j.after.version));
    if destination.exists(){return check_hash(&destination,&j.candidate_hash);}
    let pending=destination.with_extension("pending");
    copy_synced(&source,&pending)?;
    check_hash(&pending,&j.candidate_hash)?;
    fs::rename(&pending,&destination).map_err(|e|e.to_string())?;
    Ok(())
}
fn complete_legacy(j:&mut Journal,rollback:bool)->Result<(),String> {
    j.phase=Phase::Complete;save(j)?;
    if !rollback{retain_current_package(j)?;}
    if rollback{legacy_runs(j,true)?;}else{write_run("MicNoize",j.legacy.as_ref().and_then(|old|old.app_login.as_deref()))?;}
    let hold=j.runtime.join(".update/hold");if hold.exists(){fs::remove_file(hold).map_err(|e|e.to_string())?;}
    j.legacy=None;save(j)?;run_recovery(None,&j.runtime)
}
fn restore_legacy(j:&mut Journal)->Result<(),String> {
    let _ui=legacy_guard()?;
    atomic(&j.runtime.join(".update/hold"),b"legacy rollback")?;
    stop_upgrade_host(j)?;remove_upgrade_task(j)?;
    let source=folder(j).join("legacy-host.exe");check_hash(&source,&j.before.host_sha256)?;
    copy_synced(&source,&j.runtime.join("bin/mic_tag_host.exe"))?;
    legacy_before_matches(j)?;
    // This transaction already has explicit line-repair consent. Windows may
    // have forgotten the own line during the reboot that interrupted the update.
    native(|e,n|unsafe{mnr_tag_repair_lines(e,n)},1)?;
    native(|e,n|unsafe{mnr_tag_legacy_host(3,e,n)},1)?;
    let _image=locked_legacy(j)?;
    let mut process=Command::new(j.runtime.join("bin/mic_tag_host.exe")).current_dir(&j.runtime).creation_flags(0x08000000).spawn().map_err(|e|e.to_string())?;
    let mut ready=false;let mut error=String::new();
    for _ in 0..50 {
        match legacy_call(j,false){Ok(())=>{ready=true;break},Err(e)=>error=e}
        if process.try_wait().map_err(|e|e.to_string())?.is_some(){break;}
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !ready{return Err(format!("Предыдущие файлы восстановлены, но старый хост не готов: {error}"));}
    // The old SDK automatically applies any newer cached full package at startup.
    // Remove only our rejected candidate, which remains retained in the transaction.
    let location=velopack::locator::auto_locate_app_manifest(velopack::locator::LocationContext::FromSpecifiedRootDir(j.install.clone(),None)).map_err(|e|e.to_string())?;
    let packages=location.get_packages_dir();
    check_hash(&folder(j).join("candidate.nupkg"),&j.candidate_hash)?;
    for (path,manifest) in velopack::locator::find_local_full_packages(&packages) {
        if manifest.id=="MicNoize" && manifest.version>location.get_manifest_version() {
            if hash(&mut File::open(&path).map_err(|e|e.to_string())?)?.eq_ignore_ascii_case(&j.candidate_hash){fs::remove_file(path).map_err(|e|e.to_string())?;}
            else{return Err("В кэше найден другой пакет: запуск старого UI отложен, чтобы исключить повторное обновление".into());}
        }
    }
    let name=&j.legacy.as_ref().unwrap().package_name;
    fs::create_dir_all(&packages).map_err(|e|e.to_string())?;
    check_hash(&folder(j).join("previous.nupkg"),&j.previous_hash)?;
    copy_synced(&folder(j).join("previous.nupkg"),&packages.join(name))?;
    // A completed legacy rollback still restores Run values on a repeated recovery.
    complete_legacy(j,true)
}
fn release(j: &mut Journal) -> Result<(), String> {
    let hold = j.runtime.join(".update/hold"); if hold.exists() { fs::remove_file(hold).map_err(|e| e.to_string())?; }
    task(i32::from(j.task_enabled))?;
    if j.legacy.is_some(){return complete_legacy(j,false);}
    j.phase = Phase::Complete; save(j)?; run_recovery(None,&j.runtime)
}
/// Setup.exe can replace an installation while its persistent update journal survives.
/// Accept that replacement only as a complete, signed release, never as a mixed pair.
fn verify_reinstallation(j:&Journal,version:&str,path:&Path,envelope:&str,key:[u8;32])->Result<(),String> {
    if j.legacy.is_some() || version==j.before.version || version==j.after.version {
        return Err("Это не отдельная переустановка приложения".into());
    }
    verify_update(path,version,envelope,key)?;
    package(path,version)?.installed(&j.install)
}
fn download_reinstallation_package(version:&str,name:&str,pending:&Path)->Result<(),String> {
    let url=format!("https://github.com/Arkanoidvfx/MicNoize/releases/download/v{version}/{name}");
    let agent:ureq::Agent=ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_global(Some(std::time::Duration::from_secs(120))).build().into();
    let response=agent.get(&url).call().map_err(|e|format!("Загрузка пакета переустановки {version}: {e}"))?;
    let mut reader=response.into_parts().1.into_reader().take(512*1024*1024+1);
    let mut file=File::create(pending).map_err(|e|format!("Создание {}: {e}",pending.display()))?;
    let size=std::io::copy(&mut reader,&mut file).map_err(|e|format!("Загрузка {}: {e}",pending.display()))?;
    if size>512*1024*1024 {return Err("Полный пакет переустановки превышает 512 МиБ".into());}
    file.sync_all().map_err(|e|format!("Сохранение {}: {e}",pending.display()))
}
fn restore_reinstallation_package(j:&Journal,packages:&Path,version:&str,envelope:&str,key:[u8;32],download:impl FnOnce(&Path)->Result<(),String>)->Result<PathBuf,String> {
    let name=format!("MicNoize-{version}-win-x64-stable-v2-full.nupkg");check_update_name(version,&name)?;
    let destination=packages.join(&name);
    if destination.is_file() {verify_reinstallation(j,version,&destination,envelope,key)?;return Ok(destination);}
    if envelope.len()>1<<20 {return Err("Подпись обновления слишком велика".into());}
    let payload=crate::components::signed_payload(envelope,key).map_err(|_|String::from("Подпись переустановки не прошла проверку"))?;
    fs::create_dir_all(packages).map_err(|e|format!("Папка пакетов {}: {e}",packages.display()))?;
    let pending=destination.with_extension("partial");
    let result=(|| {
        if let Ok(source)=current_package(packages,&j.runtime,version) {copy_synced(&source,&pending)?;}
        else {
            crate::logs::note(&j.runtime,&format!("Кэш переустановки {version} отсутствует; восстанавливаем официальный полный пакет"));
            download(&pending)?;
        }
        check_signed_files(&pending,version,&payload)?;
        package(&pending,version)?.installed(&j.install)?;
        fs::rename(&pending,&destination).map_err(|e|format!("Сохранение пакета {}: {e}",destination.display()))?;
        verify_reinstallation(j,version,&destination,envelope,key)?;
        Ok(destination)
    })();
    if result.is_err() {let _=fs::remove_file(&pending);}
    result
}
fn finish_reinstallation(j:&mut Journal)->Result<bool,String> {
    use velopack::locator::{auto_locate_app_manifest,LocationContext};
    let location=auto_locate_app_manifest(LocationContext::FromCurrentExe).map_err(|e|format!("Метаданные переустановки {}: {e}",j.install.display()))?;
    let version=location.get_manifest_version().to_string();
    if version==j.before.version || version==j.after.version {return Ok(false);}
    if location.get_root_dir()!=j.install {return Err("Переустановка относится к другой папке приложения".into());}
    let name=format!("MicNoize-{version}-win-x64-stable-v2-full.nupkg");
    check_update_name(&version,&name)?;
    let signature=match fs::read_to_string(signature_path(&j.runtime,&name)) {
        Ok(text)=>text,
        Err(_)=>crate::components::update_signature(&version,&name)?,
    };
    restore_reinstallation_package(j,&location.get_packages_dir(),&version,&signature,crate::components::release_key()?,|pending|download_reinstallation_package(&version,&name,pending))?;
    atomic(&signature_path(&j.runtime,&name),signature.as_bytes())?;
    // Keep the interrupted operation and rollback packages for support before retiring it.
    fs::create_dir_all(folder(j)).map_err(|e|e.to_string())?;
    copy_synced(&j.runtime.join(".update/journal.json"),&folder(j).join("superseded-journal.json"))?;
    let hold=j.runtime.join(".update/hold");if hold.exists(){fs::remove_file(hold).map_err(|e|e.to_string())?;}
    verify_ready(&j.runtime,true)?;
    release(j)?;
    crate::logs::note(&j.runtime,&format!("Подписанная переустановка {version} проверена; прежний журнал обновления сохранён и завершён"));
    Ok(true)
}
fn process_created(handle: isize) -> Option<u64> {
    let (mut created,mut exited,mut kernel,mut user)=(0,0,0,0);
    (unsafe{GetProcessTimes(handle,&mut created,&mut exited,&mut kernel,&mut user)}!=0).then_some(created)
}
fn process_alive(identity:Option<(u32,u64)>) -> bool {
    let Some((pid,created))=identity else{return false};
    let handle=unsafe{OpenProcess(0x100000|0x1000,0,pid)};if handle==0{return false;}
    let alive=unsafe{WaitForSingleObject(handle,0)}==258 && process_created(handle)==Some(created);
    unsafe{CloseHandle(handle);};alive
}
fn applier_alive(j:&Journal)->bool {process_alive(j.applier)}
pub fn upgrade_legacy(install:&Path,candidate:&Path)->Result<(),String> {
    use velopack::locator::{auto_locate_app_manifest,find_local_full_packages,LocationContext};
    let _lock=Lock::acquire()?;let _ui=legacy_guard()?;
    if !install.is_absolute() || !candidate.is_absolute(){return Err("Для установщика нужны абсолютные пути".into());}
    let location=auto_locate_app_manifest(LocationContext::FromSpecifiedRootDir(install.to_path_buf(),None)).map_err(|e|e.to_string())?;
    if location.get_manifest_id()!="MicNoize" || location.get_manifest_version().to_string()!="0.2.5" {return Err("Этот переход рассчитан на установленную Mic Noize 0.2.5".into());}
    let exe=std::env::current_exe().map_err(|e|e.to_string())?;
    if exe.starts_with(location.get_current_bin_dir()){return Err("Распакуйте установщик вне папки current".into());}
    let after=package(candidate,env!("CARGO_PKG_VERSION"))?;check_hash(&exe,&after.ui_sha256)?;
    let candidate_hash=hash(&mut File::open(candidate).map_err(|e|e.to_string())?)?;
    let paths=crate::paths::Paths::resolve()?;
    let runtime=std::env::var_os("MNR_RUNTIME_ROOT").map(PathBuf::from).unwrap_or_else(||paths.runtime_root().to_path_buf());
    let state=runtime.join(".update");fs::create_dir_all(&state).map_err(|e|e.to_string())?;
    if state.join("repair.json").exists() || (state.join("journal.json").exists() && read(&runtime)?.phase!=Phase::Complete){return Err("Сначала завершите предыдущее восстановление установки".into());}
    let previous=find_local_full_packages(&location.get_packages_dir()).into_iter().find(|(_,m)|m.id=="MicNoize" && m.version==location.get_manifest_version()).ok_or("Предыдущий полный пакет не найден; безопасный откат недоступен")?.0;
    for (path,manifest) in find_local_full_packages(&location.get_packages_dir()) {
        if manifest.id=="MicNoize" && manifest.version>location.get_manifest_version() && !hash(&mut File::open(path).map_err(|e|e.to_string())?)?.eq_ignore_ascii_case(&candidate_hash) {
            return Err("В установке уже подготовлено другое обновление; переход не начат".into());
        }
    }
    let mut old=retain_legacy(install,&runtime,&previous,"0.2.5",[legacy_login("MicNoize.TagHost")?,legacy_login("MicNoiseReducer.TagHost")?])?;
    old.host_login=[legacy_login("MicNoize.TagHost")?,legacy_login("MicNoiseReducer.TagHost")?];old.app_login=legacy_login("MicNoize")?;
    let old_host=old.host_sha256.clone().ok_or("Core старой версии не установлен; сначала завершите его установку")?;
    if !old_host.eq_ignore_ascii_case(LEGACY_HOST_SHA256){return Err("Версия старого core-хоста не проверена для автоматического перехода".into());}
    let before=Bundle{version:old.version.clone(),protocol:1,host_build:1,ui_sha256:old.ui_sha256.clone(),host_sha256:old_host};
    check_hash(&location.get_current_bin_dir().join("MicNoize.exe"),&before.ui_sha256)?;
    let mut j=Journal{schema:1,transaction:uuid::Uuid::new_v4().to_string(),install:location.get_root_dir(),runtime,phase:Phase::Prepared,before,after,
        previous_hash:old.package_sha256.clone(),candidate_hash,updater_hash:String::new(),task_enabled:true,applier:None,rollback_attempts:0,resume:None,legacy:Some(old)};
    unsafe{std::env::set_var("MNR_RUNTIME_ROOT",&j.runtime);std::env::set_var("MNR_TAG_HOST_PATH",j.install.join("current/mic_tag_host.exe"));}
    {let _image=locked_legacy(&j)?;native(|e,n|unsafe{mnr_tag_legacy_host(2,e,n)},1)?;}
    let files=folder(&j);fs::create_dir(&files).map_err(|e|e.to_string())?;
    for (source,name) in [(state.join("legacy/previous.nupkg"),"previous.nupkg"),(state.join("legacy/mic_tag_host.exe"),"legacy-host.exe"),(candidate.to_path_buf(),"candidate.nupkg"),(location.get_update_path(),"Update.exe"),(exe,"Recovery.exe")] {copy_synced(&source,&files.join(name))?;}
    check_hash(&files.join("candidate.nupkg"),&j.candidate_hash)?;
    j.updater_hash=hash(&mut File::open(files.join("Update.exe")).map_err(|e|e.to_string())?)?;
    save(&j)?;
    let action=(|| {
        run_recovery(Some(&files.join("Recovery.exe")),&j.runtime)?;
        atomic(&state.join("hold"),b"legacy update")?;
        legacy_runs(&j,false)?;
        j.phase=Phase::Applying;save(&j)?;
        legacy_call(&j,true)?;
        task(0)?;
        native(|e,n|unsafe{mnr_tag_repair_lines(e,n)},1)?;
        apply(&mut j,false,None)
    })();
    if let Err(error)=action {
        // Release the UI gate before the recovery routine acquires it itself.
        drop(_ui);
        if !applier_alive(&j){restore_legacy(&mut j).map_err(|old|format!("{error}; откат: {old}"))?;}
        return Err(error);
    }
    std::process::exit(0)
}
fn restore_update_metadata(j:&Journal)->Result<(),String> {
    use velopack::locator::VelopackLocatorConfig;
    let current=j.install.join("current");let metadata=current.join("sq.version");
    let config=|path|VelopackLocatorConfig{ManifestPath:path,..Default::default()};
    if let Ok(manifest)=config(metadata.clone()).load_manifest() {
        if manifest.id!="MicNoize" || manifest.main_exe!="MicNoize.exe" ||
            ![j.before.version.as_str(),j.after.version.as_str()].contains(&manifest.version.to_string().as_str()) {
            return Err("Идентичность установки изменилась; метаданные не заменены".into());
        }
    }else{
        // Velopack cannot apply even a retained package without current/sq.version.
        // Only seed its locator; the verified full package still restores all binaries.
        let mut zip=zip::ZipArchive::new(File::open(folder(j).join("previous.nupkg")).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        if zip.len()>4096{return Err("Слишком много файлов в сохранённом пакете".into());}
        let names=zip.file_names().map(str::to_owned).collect::<Vec<_>>();let name=archive_entry(&names,"MicNoize.nuspec")?;
        let mut bytes=Vec::new();zip.by_name(&name).map_err(|e|e.to_string())?.take(524289).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
        if bytes.len()>524288{return Err("Метаданные сохранённого пакета слишком велики".into());}
        let retained=folder(j).join("restore.sq.version");atomic(&retained,&bytes)?;
        let manifest=config(retained).load_manifest().map_err(|e|e.to_string())?;
        if manifest.id!="MicNoize" || manifest.main_exe!="MicNoize.exe" || manifest.version.to_string()!=j.before.version {
            return Err("Метаданные сохранённого пакета не соответствуют откату".into());
        }
        fs::create_dir_all(&current).map_err(|e|e.to_string())?;atomic(&metadata,&bytes)?;
    }
    let updater=j.install.join("Update.exe");
    if !updater.exists(){atomic(&updater,&fs::read(folder(j).join("Update.exe")).map_err(|e|e.to_string())?)?;}
    Ok(())
}
/// `window`: where the watcher shows the app's own update window ("x,y" centre in logical px, or
/// "centered"); Velopack then runs silently. Without it Velopack shows its standard dialog.
fn apply(j: &mut Journal, previous: bool, window: Option<&str>) -> Result<(), String> {
    let files = folder(j); let package = files.join(if previous {"previous.nupkg"} else {"candidate.nupkg"});
    check_hash(&package, if previous {&j.previous_hash} else {&j.candidate_hash})?;
    check_hash(&files.join("Update.exe"),&j.updater_hash)?;
    if previous {restore_update_metadata(j)?;}
    // Windows keeps a handle to a process's working directory. Helpers must not
    // inherit replaceable `current`, or their own lifetime prevents rollback.
    let mut child=Command::new(files.join("Update.exe"));
    child.current_dir(&files).creation_flags(0x08000000);
    if window.is_some(){child.arg("--silent");}
    let mut child=child
        .arg("apply").arg("--norestart").arg("--package").arg(package).arg("--root").arg(&j.install)
        .arg("--waitPid").arg(std::process::id().to_string()).spawn().map_err(|e| e.to_string())?;
    let created=process_created(child.as_raw_handle() as isize);
    if let Some(created)=created {j.applier=Some((child.id(),created));}
    if let Err(error)=created.ok_or("Updater identity unavailable".into()).and_then(|_|save(j)) {
        child.kill().map_err(|e|format!("{error}; cannot cancel owned updater: {e}"))?;
        let _=child.wait();return Err(error);
    }
    // Survives the old UI and catches an updater that exits without restarting it.
    let watcher=(|| {
        let created=process_created(unsafe{GetCurrentProcess()}).ok_or("UI process identity unavailable")?;
        let mut watcher=Command::new(files.join("Recovery.exe"));
        watcher.current_dir(&files).creation_flags(0x08000000)
            .arg("--recover-update").arg(&j.runtime).arg("--watch-update")
            .arg(std::process::id().to_string()).arg(created.to_string());
        if let Some(at)=window{watcher.arg("--update-window").arg(at);}
        watcher.spawn().map_err(|e|e.to_string())?;
        Ok::<(),String>(())
    })();
    if let Err(error)=watcher {child.kill().map_err(|e|format!("{error}; cannot cancel owned updater: {e}"))?;let _=child.wait();return Err(error);}
    Ok(())
}
pub fn prepare(candidate: &velopack::VelopackAsset) -> Result<(), String> {
    use velopack::locator::{auto_locate_app_manifest, LocationContext};
    let _lock = Lock::acquire()?;
    let location = auto_locate_app_manifest(LocationContext::FromCurrentExe).map_err(|e| e.to_string())?;
    let paths = crate::paths::Paths::resolve()?; let runtime = paths.runtime_root().to_path_buf();
    let state = runtime.join(".update"); fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    if state.join("repair.json").exists(){return Err("Сначала завершите восстановление устройства".into());}
    if state.join("journal.json").exists() {
        let old = read(&runtime)?; if old.phase != Phase::Complete { return Err("Сначала завершите предыдущее восстановление Mic Noize".into()); }
    }
    if Path::new(&candidate.FileName).file_name().and_then(|n|n.to_str()) != Some(candidate.FileName.as_str()) { return Err("Invalid package filename".into()); }
    let next = location.get_packages_dir().join(&candidate.FileName);
    // Fail closed: nothing applies without a valid release signature over its installed files.
    check_update_name(&candidate.Version,&candidate.FileName)?;
    let envelope = match fs::read_to_string(signature_path(&runtime,&candidate.FileName)) {
        Ok(text) => text,
        Err(_) => crate::components::update_signature(&candidate.Version,&candidate.FileName)?,
    };
    verify_update(&next,&candidate.Version,&envelope,crate::components::release_key()?)?;
    let after = package(&next,&candidate.Version)?;
    let previous = current_package(&location.get_packages_dir(),&runtime,&location.get_manifest_version().to_string())?;
    let before = package(&previous,&location.get_manifest_version().to_string())?;
    before.installed(&location.get_root_dir())?;
    let mut j = Journal { schema:1, transaction:uuid::Uuid::new_v4().to_string(), install:location.get_root_dir(), runtime, phase:Phase::Prepared,
        before, after, previous_hash:String::new(), candidate_hash:String::new(), updater_hash:String::new(), task_enabled:task(-1)?, applier:None, rollback_attempts:0, resume:None,legacy:None };
    let files = folder(&j); fs::create_dir(&files).map_err(|e| e.to_string())?;
    for (from,name) in [(&previous,"previous.nupkg"),(&next,"candidate.nupkg"),(&location.get_update_path(),"Update.exe"),(&std::env::current_exe().map_err(|e|e.to_string())?,"Recovery.exe")] { copy_synced(from,&files.join(name))?; }
    j.previous_hash = hash(&mut File::open(files.join("previous.nupkg")).map_err(|e|e.to_string())?)?;
    j.candidate_hash = hash(&mut File::open(files.join("candidate.nupkg")).map_err(|e|e.to_string())?)?;
    j.updater_hash = hash(&mut File::open(files.join("Update.exe")).map_err(|e|e.to_string())?)?;
    save(&j)
}
pub fn apply_prepared(intent:Option<crate::ResumeIntent>,window:Option<String>) -> Result<(), String> {
    use velopack::locator::{auto_locate_app_manifest,LocationContext};
    let _lock=Lock::acquire()?;
    let location=auto_locate_app_manifest(LocationContext::FromCurrentExe).map_err(|e|e.to_string())?;
    let runtime=crate::paths::Paths::resolve()?.runtime_root().to_path_buf();
    let state=runtime.join(".update");let mut j=read(&runtime)?;
    if j.phase!=Phase::Prepared || j.install!=location.get_root_dir() || state.join("repair.json").exists(){return Err("Подготовленное обновление больше не соответствует установке".into());}
    j.before.installed(&location.get_root_dir())?;
    j.resume=intent;save(&j)?;
    let files=folder(&j);
    let ready=crate::update_window::ready_file(&j.runtime);let _=fs::remove_file(&ready);
    if let Err(error) = (|| { run_recovery(Some(&files.join("Recovery.exe")),&j.runtime)?; atomic(&state.join("hold"),b"update")?; stop()?; task(0)?;
        j.phase=Phase::Applying; save(&j)?; apply(&mut j,false,window.as_deref()) })() { if !applier_alive(&j){release(&mut j)?;} return Err(error); }
    // The UI stays on screen until the watcher's identical update window covers it, so the
    // hand-over shows no gap. Velopack waits for this process to exit before touching files.
    if window.is_some() {
        for _ in 0..60 {if ready.exists(){break;}std::thread::sleep(std::time::Duration::from_millis(50));}
    }
    std::process::exit(0)
}
/// Versions of the update in progress, for the update window: (installed, incoming).
pub fn update_versions(runtime: &Path) -> Option<(String, String)> {
    read(runtime).ok().map(|j| (j.before.version, j.after.version))
}
fn read(runtime: &Path) -> Result<Journal, String> {
    let path=runtime.join(".update/journal.json");
    let mut bytes=Vec::new();File::open(&path).map_err(|e|format!("Открытие журнала {}: {e}",path.display()))?.take(16385).read_to_end(&mut bytes).map_err(|e|format!("Чтение журнала {}: {e}",path.display()))?;
    if bytes.len()>16384 {return Err("Update journal exceeds size limit".into());}
    let j: Journal = serde_json::from_slice(&bytes).map_err(|e|format!("Разбор журнала {}: {e}",path.display()))?;
    if j.schema!=1 || j.runtime!=runtime || !j.install.is_absolute() || !j.runtime.is_absolute() {return Err("Invalid update journal identity".into());}
    if uuid::Uuid::parse_str(&j.transaction).is_err() || j.transaction.len()!=36 {return Err("Invalid transaction directory".into());}
    if j.resume.is_some_and(|intent|!(0..=8).contains(&intent.monitor)){return Err("Invalid monitor resume mode".into());}
    if let Some(old)=&j.legacy
        && (old.install!=j.install || old.version!=j.before.version || old.ui_sha256!=j.before.ui_sha256 || old.host_sha256.as_ref()!=Some(&j.before.host_sha256) || old.package_sha256!=j.previous_hash || Path::new(&old.package_name).file_name().and_then(|n|n.to_str())!=Some(old.package_name.as_str())) {
            return Err("Invalid legacy transaction identity".into());
    }
    j.before.validate()?; j.after.validate()?; Ok(j)
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Repair {runtime:PathBuf, app:PathBuf, app_hash:String, task_enabled:bool}
fn finish_repair(record:&Repair)->Result<(),String> {
    let state=record.runtime.join(".update");
    if state.join("hold").exists(){fs::remove_file(state.join("hold")).map_err(|e|e.to_string())?;}
    task(i32::from(record.task_enabled))?;
    fs::remove_file(state.join("repair.json")).map_err(|e|e.to_string())?;
    run_recovery(None,&record.runtime)
}
pub fn repair(runtime:&Path,allow_reinstall:bool,repair_lines:bool)->Result<(),String> {
    let _lock=Lock::acquire()?;
    let state=runtime.join(".update");fs::create_dir_all(&state).map_err(|e|e.to_string())?;
    if state.join("repair.json").exists() || (state.join("journal.json").exists() && read(runtime)?.phase!=Phase::Complete) {
        return Err("Предыдущая операция не завершена: перезапустите Mic Noize для восстановления".into());
    }
    // Healthy/recoverable lines are reused. Repeating Repair must not recreate endpoints.
    if !repair_lines && native(|e,n|unsafe{mnr_refresh_host(e,n)},1).is_ok(){return Ok(());}
    let app=std::env::current_exe().map_err(|e|e.to_string())?;
    let record=Repair{runtime:runtime.to_path_buf(),app_hash:hash(&mut File::open(&app).map_err(|e|e.to_string())?)?,app,task_enabled:task(-1)?};
    copy_synced(&record.app,&state.join("RepairRecovery.exe"))?;
    atomic(&state.join("repair.json"),&serde_json::to_vec(&record).map_err(|e|e.to_string())?)?;
    let result=(|| {
        run_recovery(Some(&state.join("RepairRecovery.exe")),runtime)?;
        atomic(&state.join("hold"),b"repair")?;stop()?;task(0)?;
        let line_error=if repair_lines {native(|e,n|unsafe{mnr_tag_repair_lines(e,n)},1).err()}else{None};
        fs::remove_file(state.join("hold")).map_err(|e|e.to_string())?;
        if line_error.is_none() && native(|e,n|unsafe{mnr_refresh_host(e,n)},1).is_ok(){return Ok(());}
        if !allow_reinstall {return Err(line_error.unwrap_or_else(||"Безопасное восстановление не помогло. Проверьте доступ к микрофону и состояние устройства в Windows; переустановку можно разрешить в окне восстановления".into()));}
        atomic(&state.join("hold"),b"driver-install")?;stop()?;task(0)?;
        crate::components::install_driver(runtime)?;
        if repair_lines {native(|e,n|unsafe{mnr_tag_repair_lines(e,n)},1)?;}
        fs::remove_file(state.join("hold")).map_err(|e|e.to_string())?;
        native(|e,n|unsafe{mnr_refresh_host(e,n)},1).map(|_|())
    })();
    finish_repair(&record)?;result
}
/// False means a recovery process owns startup or launched the restored application.
pub fn startup() -> Result<bool,String> {
    let args:Vec<_>=std::env::args_os().collect();
    let recovery = args.iter().position(|v|v=="--recover-update");
    let runtime = if let Some(i)=recovery {PathBuf::from(args.get(i+1).ok_or("Recovery runtime missing")?)} else {crate::paths::Paths::resolve()?.runtime_root().to_path_buf()};
    if recovery.is_some(){std::env::set_current_dir(&runtime).map_err(|e|format!("Папка восстановления {}: {e}",runtime.display()))?;}
    if let Some(i)=args.iter().position(|v|v=="--watch-update") {
        if recovery.is_none(){return Err("Watcher requires recovery context".into());}
        let pid=args.get(i+1).and_then(|v|v.to_str()).and_then(|v|v.parse().ok()).ok_or("Watcher PID missing")?;
        let created=args.get(i+2).and_then(|v|v.to_str()).and_then(|v|v.parse().ok()).ok_or("Watcher identity missing")?;
        // Never retain the maintenance mutex while waiting for either process to leave.
        for _ in 0..300 {
            if !process_alive(Some((pid,created))) && !applier_alive(&read(&runtime)?){break;}
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        if process_alive(Some((pid,created))) || applier_alive(&read(&runtime)?){return Err("Установщик не завершился за 5 минут. Повторное восстановление доступно при следующем запуске Mic Noize".into());}
    }
    let repair_path=runtime.join(".update/repair.json");
    if repair_path.exists() {
        let _lock=Lock::wait(10000)?;let mut bytes=Vec::new();
        File::open(&repair_path).map_err(|e|format!("Открытие журнала восстановления {}: {e}",repair_path.display()))?.take(16385).read_to_end(&mut bytes).map_err(|e|format!("Чтение журнала восстановления {}: {e}",repair_path.display()))?;
        if bytes.len()>16384{return Err("Repair journal exceeds size limit".into());}
        let record:Repair=serde_json::from_slice(&bytes).map_err(|e|format!("Разбор журнала восстановления {}: {e}",repair_path.display()))?;
        if record.runtime!=runtime || !record.app.is_absolute(){return Err("Invalid repair journal identity".into());}
        check_hash(&record.app,&record.app_hash)?;
        unsafe{std::env::set_var("MNR_RUNTIME_ROOT",&runtime);std::env::set_var("MNR_TAG_HOST_PATH",record.app.parent().ok_or("Repair app directory missing")?.join("mic_tag_host.exe"));}
        // Interrupted repair never repeats driver installation/UAC on its own.
        finish_repair(&record)?;
        if recovery.is_some(){Command::new(&record.app).creation_flags(0x08000000).spawn().map_err(|e|format!("Запуск {}: {e}",record.app.display()))?;return Ok(false);}
    }
    if !runtime.join(".update/journal.json").exists() {return Ok(recovery.is_none());}
    let _lock = match Lock::wait(10000) {Ok(lock)=>lock,Err(_)=>return Ok(false)};
    let mut j=read(&runtime)?;
    crate::logs::note(&runtime,&format!("Восстановление обновления: {:?}, {} -> {}",j.phase,j.before.version,j.after.version));
    for _ in 0..100 {if !applier_alive(&j){break;}std::thread::sleep(std::time::Duration::from_millis(100));}
    if applier_alive(&j){return Ok(false);}
    if j.phase==Phase::Complete && j.legacy.is_none() { run_recovery(None,&runtime)?; return Ok(recovery.is_none()); }
    let current=j.install.join("current");
    unsafe {std::env::set_var("MNR_RUNTIME_ROOT",&runtime);std::env::set_var("MNR_TAG_HOST_PATH",current.join("mic_tag_host.exe"));}
    if j.legacy.is_some() {
        let previous_ui=legacy_ui_matches(&j).is_ok();
        if previous_ui {
            restore_legacy(&mut j)?;
            Command::new(current.join("MicNoize.exe")).current_dir(&runtime).creation_flags(0x08000000).spawn().map_err(|e|format!("Запуск {} из {}: {e}",current.join("MicNoize.exe").display(),runtime.display()))?;
            return Ok(false);
        }
        if j.phase!=Phase::RollingBack && j.after.installed(&j.install).is_ok() {
            if recovery.is_some() {
                Command::new(current.join("MicNoize.exe")).current_dir(&runtime).creation_flags(0x08000000).spawn().map_err(|e|format!("Запуск {} из {}: {e}",current.join("MicNoize.exe").display(),runtime.display()))?;
                return Ok(false);
            }
            let hold=runtime.join(".update/hold");if hold.exists(){fs::remove_file(&hold).map_err(|e|e.to_string())?;}
            let login=j.legacy.as_ref().unwrap().host_login.iter().any(Option::is_some);
            let ready=native(|e,n|unsafe{mnr_tag_autostart(i32::from(login),e,n)},0).and_then(|_|verify_ready(&runtime,false));
            if ready.is_ok(){release(&mut j)?;return Ok(true);}
            if let Err(error)=ready{crate::logs::note(&runtime,&format!("Переход со старой версии отклонён: {error}"));}
        }
        if j.rollback_attempts>=2{return Err("Откат старой установки не завершён; исходный комплект сохранён в .update".into());}
        atomic(&runtime.join(".update/hold"),b"legacy rollback")?;
        stop_upgrade_host(&j)?;
        j.phase=Phase::RollingBack;j.rollback_attempts+=1;save(&j)?;apply(&mut j,true,None)?;
        return Ok(false);
    }
    // The copied recovery executable must not validate a newer protocol with its old native ABI.
    if recovery.is_some() && j.after.installed(&j.install).is_ok() && j.phase!=Phase::RollingBack {
        Command::new(current.join("MicNoize.exe")).creation_flags(0x08000000).spawn().map_err(|e|format!("Запуск {}: {e}",current.join("MicNoize.exe").display()))?; return Ok(false);
    }
    if j.before.installed(&j.install).is_ok() {
        if recovery.is_some() {
            Command::new(current.join("MicNoize.exe")).creation_flags(0x08000000).spawn().map_err(|e|format!("Запуск {}: {e}",current.join("MicNoize.exe").display()))?;return Ok(false);
        }
        if j.phase!=Phase::Prepared {
            let hold=runtime.join(".update/hold");if hold.exists(){fs::remove_file(&hold).map_err(|e|e.to_string())?;}
            // Check a usable host when its separately installed prerequisites are present.
            verify_ready(&runtime,true)?;
        }
        *RESUME.lock().map_err(|e|e.to_string())?=j.resume;
        release(&mut j)?;
        return Ok(true);
    }
    if j.phase!=Phase::RollingBack && j.after.installed(&j.install).is_ok() {
        let hold=runtime.join(".update/hold");if hold.exists(){fs::remove_file(&hold).map_err(|e|e.to_string())?;}
        let ready=verify_ready(&runtime,true);
        if ready.is_ok(){retain_current_package(&j)?;*RESUME.lock().map_err(|e|e.to_string())?=j.resume;release(&mut j)?;return Ok(true);}
    }
    if recovery.is_none() && finish_reinstallation(&mut j)? {return Ok(true);}
    if j.rollback_attempts>=2{return Err("Автоматический откат не завершился. Предыдущий пакет сохранён в .update; требуется восстановление установки".into());}
    atomic(&runtime.join(".update/hold"),b"rollback")?; stop()?; task(0)?;
    j.phase=Phase::RollingBack;j.rollback_attempts+=1;save(&j)?;apply(&mut j,true,None)?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_file_errors_identify_paths() {
        let t=temp();let missing=t.0.join("missing.nupkg");let copy=t.0.join("copy.nupkg");
        let error=check_signed_files(&missing,"1.2.3","MicNoize 1.2.3\n").unwrap_err();
        assert!(error.contains(&missing.display().to_string()),"{error}");
        let error=copy_synced(&missing,&copy).unwrap_err();
        assert!(error.contains(&missing.display().to_string()) && error.contains(&copy.display().to_string()),"{error}");
    }
    #[test]
    fn update_download_retains_current_package_outside_velopack_cache() {
        let t=temp();let root=&t.0;let runtime=root.join("runtime");let current=root.join("current");
        let packages=root.join("packages");fs::create_dir_all(&current).unwrap();fs::create_dir(&packages).unwrap();
        fs::write(root.join("Update.exe"),b"updater").unwrap();fs::write(root.join(".portable"),b"").unwrap();
        fs::write(current.join("MicNoize.exe"),b"UI").unwrap();fs::write(current.join("mic_tag_host.exe"),b"host").unwrap();
        fs::write(current.join("sq.version"),b"<package><metadata><id>MicNoize</id><version>0.2.7</version><mainExe>MicNoize.exe</mainExe></metadata></package>").unwrap();
        let source=packages.join("MicNoize-0.2.7-win-x64-stable-v2-full.nupkg");
        archive(&source,&bundle("0.2.7",b"UI",b"host"),b"UI",b"host");
        preserve_current_package(root,&packages,&runtime,"0.2.7").unwrap();
        fs::remove_file(&source).unwrap(); // Velopack removes old packages after download.
        let retained=current_package(&packages,&runtime,"0.2.7").unwrap();
        package(&retained,"0.2.7").unwrap().installed(root).unwrap();
        preserve_current_package(root,&packages,&runtime,"0.2.7").unwrap();
        fs::write(&retained,b"corrupt").unwrap();
        assert!(preserve_current_package(root,&packages,&runtime,"0.2.7").is_err());
    }
    #[test]
    fn update_applies_only_when_every_installed_file_is_signed() {
        use base64::Engine as _;use ed25519_dalek::{Signer,SigningKey};
        let t=temp();let path=t.0.join("MicNoize-1.2.3-win-x64-stable-v2-full.nupkg");
        let pack=|entries:&[(&str,&str)]| {
            let mut zip=zip::ZipWriter::new(File::create(&path).unwrap());
            for (name,data) in entries {zip.start_file(*name,zip::write::SimpleFileOptions::default()).unwrap();zip.write_all(data.as_bytes()).unwrap();}
            zip.finish().unwrap();
        };
        let key=SigningKey::from_bytes(&[9;32]);let public=key.verifying_key().to_bytes();
        let sign=|payload:&str|serde_json::json!({"payload":payload,"signature":base64::engine::general_purpose::STANDARD.encode(key.sign(payload.as_bytes()).to_bytes())}).to_string();
        let sha=|data:&str|hash(&mut data.as_bytes()).unwrap();
        let payload=format!("MicNoize 1.2.3\n{}  lib/app/MicNoize.exe\n{}  lib/app/Squirrel.exe\n",sha("UI"),sha("updater"));
        let envelope=sign(&payload);
        pack(&[("MicNoize.nuspec","metadata is not installed"),("lib/app/MicNoize.exe","UI"),("lib/app/Squirrel.exe","updater")]);
        verify_update(&path,"1.2.3",&envelope,public).unwrap();
        assert!(verify_update(&path,"1.2.3",&sign(&payload.replace("1.2.3","1.2.4")),public).is_err(),"another version");
        assert!(verify_update(&path,"1.2.3",&envelope.replace("MicNoize 1.2.3","MicNoize 1.2.3 "),public).is_err(),"tampered payload");
        assert!(verify_update(&path,"1.2.3",&envelope,SigningKey::from_bytes(&[8;32]).verifying_key().to_bytes()).is_err(),"another key");
        assert!(verify_update(&t.0.join("other.nupkg"),"1.2.3",&envelope,public).is_err(),"unexpected package name");
        pack(&[("lib/app/MicNoize.exe","UI"),("lib/app/Squirrel.exe","evil")]);
        assert!(verify_update(&path,"1.2.3",&envelope,public).is_err(),"changed updater");
        pack(&[("lib/app/MicNoize.exe","UI"),("lib/app/Squirrel.exe","updater"),("lib/app/evil.dll","x")]);
        assert!(verify_update(&path,"1.2.3",&envelope,public).is_err(),"unsigned file");
        pack(&[("lib/app/MicNoize.exe","UI")]);
        assert!(verify_update(&path,"1.2.3",&envelope,public).is_err(),"missing signed file");
    }
    /// Release gate run by package-release.ps1: what is about to be published passes the client's
    /// own check. MNR_SIGNED_PUBLIC_KEY (base64) replaces the release key only to test the script.
    #[test]
    #[ignore = "package-release.ps1 runs it on the signed package"]
    fn signed_release_package() {
        use base64::Engine as _;
        let var=|name:&str|std::env::var(name).unwrap_or_else(|_|panic!("{name} is required"));
        let key=match std::env::var("MNR_SIGNED_PUBLIC_KEY") {
            Ok(key)=>base64::engine::general_purpose::STANDARD.decode(key).unwrap().try_into().unwrap(),
            Err(_)=>crate::components::release_key().unwrap(),
        };
        let envelope=fs::read_to_string(var("MNR_SIGNED_ENVELOPE")).unwrap();
        verify_update(Path::new(&var("MNR_SIGNED_PACKAGE")),&var("MNR_SIGNED_VERSION"),&envelope,key).unwrap();
    }
    #[test]
    fn migrated_package_is_cached_for_the_next_safe_update() {
        let t=temp();let runtime=t.0.join("runtime");
        let bytes=b"complete candidate package";
        let candidate_hash=hash(&mut &bytes[..]).unwrap();
        let mut j=Journal{schema:1,transaction:uuid::Uuid::new_v4().to_string(),install:t.0.clone(),runtime,
            phase:Phase::Complete,before:bundle("0.2.5",b"old",b"old host"),after:bundle("0.2.7",b"new",b"new host"),
            previous_hash:"a".repeat(64),candidate_hash,updater_hash:"b".repeat(64),task_enabled:true,
            applier:None,rollback_attempts:0,resume:None,legacy:None};
        fs::create_dir_all(folder(&j)).unwrap();
        fs::write(folder(&j).join("candidate.nupkg"),bytes).unwrap();
        let cached=j.install.join("packages/MicNoize-0.2.7-win-x64-stable-v2-full.nupkg");
        retain_current_package(&j).unwrap();
        assert_eq!(fs::read(&cached).unwrap(),bytes);
        retain_current_package(&j).unwrap();
        fs::write(&cached,b"corrupt").unwrap();
        assert!(retain_current_package(&j).is_err());
        j.after.version="../../escape".into();
        assert!(retain_current_package(&j).is_err());
    }
    #[test]
    fn legacy_backup_survives_package_cleanup_and_refuses_corruption() {
        let t=temp();let runtime=t.0.join("runtime");fs::create_dir_all(runtime.join("bin")).unwrap();
        fs::write(runtime.join("bin/mic_tag_host.exe"),b"legacy host").unwrap();
        let previous=t.0.join("old.nupkg");
        let mut zip=zip::ZipWriter::new(File::create(&previous).unwrap());
        zip.start_file("lib/net45/MicNoize.exe",zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"legacy UI").unwrap();zip.finish().unwrap();
        let login=[Some("old host --start".into()),None];
        retain_legacy(&t.0,&runtime,&previous,"0.2.5",login.clone()).unwrap();
        fs::remove_file(&previous).unwrap();
        fs::write(runtime.join("bin/mic_tag_host.exe"),b"replacement host").unwrap();
        retain_legacy(&t.0,&runtime,&previous,"0.2.5",[None,None]).unwrap();
        let files=runtime.join(".update/legacy");
        let saved:LegacyBackup=serde_json::from_slice(&fs::read(files.join("backup.json")).unwrap()).unwrap();
        assert_eq!(saved.host_login,login);
        assert_eq!(fs::read(files.join("mic_tag_host.exe")).unwrap(),b"legacy host");
        assert!(retain_legacy(&t.0.join("other"),&runtime,&previous,"0.2.5",[None,None]).is_err());
        fs::write(files.join("previous.nupkg"),b"corrupt").unwrap();
        assert!(retain_legacy(&t.0,&runtime,&previous,"0.2.5",[None,None]).is_err());
        assert_eq!(fs::read(files.join("previous.nupkg")).unwrap(),b"corrupt", "a bad backup must not be overwritten silently");
    }
    #[test]
    fn readiness_waits_for_late_audio_but_not_access_or_version_failures() {
        let mut attempts=0;let mut waits=Vec::new();
        ready_with_retry(true,||{attempts+=1;if attempts<4{Err("0x88890010".into())}else{Ok(())}},|seconds|waits.push(seconds)).unwrap();
        assert_eq!(attempts,4);assert_eq!(waits,vec![2,5,15]);
        for error in ["TAG open driver: HRESULT 0x80070005","TAG host protocol mismatch","TAG microphone endpoint disabled in Windows"] {
            let mut attempts=0;
            assert!(ready_with_retry(true,||{attempts+=1;Err(error.into())},|_|panic!("Permanent error retried")).is_err());
            assert_eq!(attempts,1);
        }
        waits.clear();
        assert!(ready_with_retry(true,||Err("Waiting for the TAG microphone endpoint in Windows".into()),|seconds|waits.push(seconds)).is_err());
        assert_eq!(waits,crate::RECOVERY_DELAYS);
    }
    #[test]
    fn missing_prerequisites_open_setup_without_waiting_for_the_host() {
        ready_with_retry(false,||panic!("Setup must install core/driver before checking the host"),|_|panic!("Setup must not wait")).unwrap();
        let t=temp();
        // An empty runtime cannot start TAG even on a machine whose driver is installed.
        assert!(!crate::components::core_installed(&t.0));
        verify_ready(&t.0,true).unwrap();
        assert!(fs::read_to_string(t.0.join("results/app.log")).unwrap().contains("Открываем интерфейс"));
    }
    struct Temp(PathBuf);
    impl Drop for Temp { fn drop(&mut self){let _=fs::remove_dir_all(&self.0);} }
    fn temp()->Temp {
        let p=PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join(".tmp").join(format!("maintenance-{}",uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();Temp(p)
    }
    fn bundle(version:&str,ui:&[u8],host:&[u8])->Bundle {
        Bundle{version:version.into(),protocol:2,host_build:1,ui_sha256:hash(&mut &ui[..]).unwrap(),host_sha256:hash(&mut &host[..]).unwrap()}
    }
    fn archive(path:&Path,b:&Bundle,ui:&[u8],host:&[u8]) {
        let mut zip=zip::ZipWriter::new(File::create(path).unwrap());
        for (name,data) in [("lib/net45/MicNoize.exe",ui.to_vec()),("lib/net45/mic_tag_host.exe",host.to_vec()),("lib/net45/micnoize-bundle.json",serde_json::to_vec(b).unwrap())] {
            zip.start_file(name,zip::write::SimpleFileOptions::default()).unwrap();zip.write_all(&data).unwrap();
        }
        zip.start_file("MicNoize.nuspec",zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(format!("<package><metadata><id>MicNoize</id><version>{}</version><mainExe>MicNoize.exe</mainExe></metadata></package>",b.version).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    #[test]
    fn reinstallation_requires_a_signed_matching_pair_outside_old_transaction() {
        use base64::Engine as _;use ed25519_dalek::{Signer,SigningKey};
        let t=temp();let current=t.0.join("current");fs::create_dir(&current).unwrap();
        fs::write(t.0.join("Update.exe"),b"updater").unwrap();fs::write(t.0.join(".portable"),b"").unwrap();
        fs::write(current.join("sq.version"),"<package><metadata><id>MicNoize</id><version>1.2.3</version><mainExe>MicNoize.exe</mainExe></metadata></package>").unwrap();
        fs::write(current.join("MicNoize.exe"),b"new UI").unwrap();fs::write(current.join("mic_tag_host.exe"),b"new host").unwrap();
        let path=t.0.join("MicNoize-1.2.3-win-x64-stable-v2-full.nupkg");
        archive(&path,&bundle("1.2.3",b"new UI",b"new host"),b"new UI",b"new host");
        let mut zip=zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut payload=String::from("MicNoize 1.2.3\n");
        for i in 0..zip.len() {
            let mut file=zip.by_index(i).unwrap();
            if file.name().starts_with("lib/") {let name=file.name().to_owned();payload.push_str(&format!("{}  {name}\n",hash(&mut file).unwrap()));}
        }
        drop(zip);
        let key=SigningKey::from_bytes(&[11;32]);let public=key.verifying_key().to_bytes();
        let envelope=serde_json::json!({"payload":payload,"signature":base64::engine::general_purpose::STANDARD.encode(key.sign(payload.as_bytes()).to_bytes())}).to_string();
        let mut j=Journal{schema:1,transaction:uuid::Uuid::new_v4().to_string(),install:t.0.clone(),runtime:t.0.join("runtime"),phase:Phase::RollingBack,
            before:bundle("1.0.0",b"old UI",b"old host"),after:bundle("1.1.0",b"failed UI",b"failed host"),previous_hash:"a".repeat(64),candidate_hash:"b".repeat(64),updater_hash:"c".repeat(64),task_enabled:true,applier:None,rollback_attempts:2,resume:None,legacy:None};
        assert!(j.before.installed(&t.0).is_err() && j.after.installed(&t.0).is_err());
        verify_reinstallation(&j,"1.2.3",&path,&envelope,public).unwrap();
        let packages=t.0.join("packages");fs::create_dir(&packages).unwrap();
        let renamed=packages.join("MicNoize-1.2.3-full.nupkg");copy_synced(&path,&renamed).unwrap();
        let restored=restore_reinstallation_package(&j,&packages,"1.2.3",&envelope,public,|_|panic!("cached package must avoid download")).unwrap();
        verify_reinstallation(&j,"1.2.3",&restored,&envelope,public).unwrap();
        fs::remove_file(&restored).unwrap();fs::remove_file(&renamed).unwrap();
        // Setup may leave no full package. Restore it without trusting download bytes.
        assert!(restore_reinstallation_package(&j,&packages,"1.2.3",&envelope,public,|pending|copy_synced(&path,pending)).is_ok());
        fs::remove_file(&restored).unwrap();
        assert!(restore_reinstallation_package(&j,&packages,"1.2.3",&envelope,[0;32],|_|panic!("reject invalid signature before download")).is_err());
        assert!(restore_reinstallation_package(&j,&packages,"1.2.3",&envelope,public,|pending|{fs::write(pending,b"tampered").unwrap();Ok(())}).is_err());
        assert!(!restored.exists() && !restored.with_extension("partial").exists());
        assert!(restore_reinstallation_package(&j,&packages,"1.2.3",&envelope,public,|_|Err("offline".into())).unwrap_err().contains("offline"));
        assert!(verify_reinstallation(&j,"1.2.3",&path,&envelope,[0;32]).is_err(),"wrong signature key");
        fs::write(current.join("mic_tag_host.exe"),b"old host").unwrap();
        assert!(verify_reinstallation(&j,"1.2.3",&path,&envelope,public).is_err(),"mixed UI/host pair");
        fs::write(current.join("mic_tag_host.exe"),b"new host").unwrap();
        j.after.version="1.2.3".into();
        assert!(verify_reinstallation(&j,"1.2.3",&path,&envelope,public).is_err(),"same-version corruption is not a reinstall");
    }
    #[test]
    fn installed_pair_rejects_stale_version_metadata() {
        let t=temp();let current=t.0.join("current");fs::create_dir(&current).unwrap();
        fs::write(t.0.join("Update.exe"),b"updater").unwrap();
        fs::write(t.0.join(".portable"),b"").unwrap();
        fs::write(current.join("MicNoize.exe"),b"UI").unwrap();
        fs::write(current.join("mic_tag_host.exe"),b"host").unwrap();
        let b=bundle("0.2.6",b"UI",b"host");
        for (id,version,accepted) in [("MicNoize","0.2.6",true),("MicNoize","0.2.5",false),("Other","0.2.6",false)] {
            fs::write(current.join("sq.version"),format!("<package><metadata><id>{id}</id><version>{version}</version><mainExe>MicNoize.exe</mainExe></metadata></package>")).unwrap();
            assert_eq!(b.installed(&t.0).is_ok(),accepted,"{id} {version}");
        }
    }
    #[test]
    fn package_and_interrupted_journal_never_accept_mixed_pair() {
        let t=temp();let runtime=t.0.join("runtime");fs::create_dir_all(runtime.join(".update")).unwrap();
        let before=bundle("1.0.0",b"old UI",b"old host");let after=bundle("2.0.0",b"new UI",b"new host");
        let pkg=t.0.join("new.nupkg");archive(&pkg,&after,b"new UI",b"new host");
        assert!(package(&pkg,"2.0.0").is_ok());assert!(package(&pkg,"1.0.0").is_err());
        archive(&pkg,&after,b"new UI",b"tampered");assert!(package(&pkg,"2.0.0").is_err());
        let current=t.0.join("current");fs::create_dir(&current).unwrap();
        fs::write(current.join("MicNoize.exe"),b"old UI").unwrap();fs::write(current.join("mic_tag_host.exe"),b"old host").unwrap();
        let mut j=Journal{schema:1,transaction:uuid::Uuid::new_v4().to_string(),install:t.0.clone(),runtime:runtime.clone(),phase:Phase::Prepared,before,after,
            previous_hash:"a".repeat(64),candidate_hash:"b".repeat(64),updater_hash:"c".repeat(64),task_enabled:true,applier:None,rollback_attempts:0,resume:Some(crate::ResumeIntent{microphone:false,headphones:true,monitor:0,full_monitor:false}),legacy:None};
        for phase in [Phase::Prepared,Phase::Applying,Phase::RollingBack,Phase::Complete] {
            j.phase=phase;save(&j).unwrap();
            // A torn temporary write must not replace the last durable journal.
            fs::write(runtime.join(".update/journal.tmp"),b"{\"schema\":").unwrap();
            let persisted=read(&runtime).unwrap();assert_eq!(persisted.phase,phase);assert!(persisted.before.matches(&current).is_ok());
            assert!(!persisted.resume.unwrap().microphone && persisted.resume.unwrap().headphones);
        }
        fs::create_dir(folder(&j)).unwrap();
        archive(&folder(&j).join("previous.nupkg"),&j.before,b"old UI",b"old host");
        fs::write(folder(&j).join("Update.exe"),b"retained updater").unwrap();
        fs::remove_dir_all(&current).unwrap();
        restore_update_metadata(&j).unwrap();
        assert!(current.join("sq.version").is_file() && t.0.join("Update.exe").is_file());
        assert!(!current.join("MicNoize.exe").exists(),"Locator repair must not pretend it restored the UI");
        restore_update_metadata(&j).unwrap();
        let valid=fs::read(current.join("sq.version")).unwrap();
        fs::write(current.join("sq.version"),String::from_utf8(valid.clone()).unwrap().replace("MicNoize</id>","Other</id>")).unwrap();
        assert!(restore_update_metadata(&j).is_err(),"Do not overwrite another installation's valid metadata");
        fs::write(current.join("sq.version"),valid).unwrap();
        fs::write(current.join("mic_tag_host.exe"),b"old host").unwrap();
        fs::write(current.join("MicNoize.exe"),b"new UI").unwrap();
        assert!(j.before.matches(&current).is_err());assert!(j.after.matches(&current).is_err());
        fs::write(current.join("mic_tag_host.exe"),b"new host").unwrap();assert!(j.after.matches(&current).is_ok());
        j.transaction="../elsewhere".into();save(&j).unwrap();assert!(read(&runtime).is_err());
        fs::write(runtime.join(".update/journal.json"),vec![0;16385]).unwrap();assert!(read(&runtime).is_err());
    }
}
