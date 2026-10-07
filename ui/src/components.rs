use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

const CORE_MANIFEST_URL: &str =
    "https://github.com/Arkanoidvfx/MicNoize/releases/download/runtime-core-v2/components.json";
const RVC_MANIFEST_URL: &str =
    "https://github.com/Arkanoidvfx/MicNoize/releases/download/runtime-rvc-v2.1.4/components.json";
/// Per-architecture NVIDIA denoiser models (`models-<arch>.json`); the core ships Ampere only.
const MODELS_RELEASE: &str =
    "https://github.com/Arkanoidvfx/MicNoize/releases/download/runtime-models-v3.0.0";
const MODELS_DIR: &str = "vendor/nvidia-afx-3.0.0/features/nvafxdenoiser/models";
/// Bump together with `MODELS_RELEASE` to make every install download newer models. A folder
/// without `version.txt` came with core v2, which carries 3.0.0 models.
const MODELS_VERSION: &str = "3.0.0";
const PUBLIC_KEY: &str = "plpoEiomh7k+cZtpxNJX9Zq2RNv0ugQruiH4lZGazHg=";
/// Device node, instance and hardware id of the signed TAG driver that ships inside the core
/// component; identical to `install-tag.ps1`, which stays the developer path with SDK checks.
const DRIVER_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Enum\Root\ThinAudioGateway_4d699d4a\0000";
const DRIVER_INSTANCE: &str = r"Root\ThinAudioGateway_4d699d4a\0000";
const DRIVER_HARDWARE_ID: &str = "ThinAudioGateway_4d699d4a-65a5-40ec-9875-8e6d5fc01e0c";
const DRIVER_DIR: &str = "vendor/tag-2.0.0.1903-demo";
const NO_WINDOW: u32 = 0x08000000; // A GUI parent would otherwise flash a console.
/// Bytes of the current phase done and in total; `STAGE` is `item << 8 | phase`.
static DONE: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static STAGE: AtomicU32 = AtomicU32::new(0);

/// Which component `install` works on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    None,
    Core,
    Models,
    Rvc,
}
/// What `install` does now. Every phase but `Connect` and `Place` counts bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Connect,
    Download,
    /// The parts are joined into the archive and its SHA-256 checked in the same pass.
    Verify,
    /// The archive is fed to tar through its standard input: the bytes fed are the progress.
    Unpack,
    Place,
}
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Status {
    pub item: Item,
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}
impl Status {
    pub fn share(&self) -> Option<f32> {
        (self.total > 0).then(|| (self.done as f64 / self.total as f64).min(1.0) as f32)
    }
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Connect => "подключаемся",
            Phase::Download => "скачиваем",
            Phase::Verify => "проверяем",
            Phase::Unpack => "распаковываем",
            Phase::Place => "устанавливаем",
        }
    }
}
pub fn status() -> Status {
    let stage = STAGE.load(Ordering::Relaxed);
    let item = [Item::None, Item::Core, Item::Models, Item::Rvc][(stage >> 8).min(3) as usize];
    let phase = [Phase::Connect, Phase::Download, Phase::Verify, Phase::Unpack, Phase::Place][(stage & 0xFF).min(4) as usize];
    Status { item, phase, done: DONE.load(Ordering::Relaxed), total: TOTAL.load(Ordering::Relaxed) }
}
/// For the design snapshots.
#[cfg(test)]
pub fn fake_status(item: Item, phase: Phase, done: u64, total: u64) {
    set_stage(item, phase, total);
    DONE.store(done, Ordering::Relaxed);
}
fn set_stage(item: Item, phase: Phase, total: u64) {
    DONE.store(0, Ordering::Relaxed);
    TOTAL.store(total, Ordering::Relaxed);
    STAGE.store((item as u32) << 8 | phase as u32, Ordering::Relaxed);
}

#[derive(Deserialize)]
struct Envelope {
    payload: String,
    signature: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    archive_sha256: String,
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    url: String,
    size: u64,
    sha256: String,
}

pub fn rvc_installed(root: &Path) -> bool {
    root.join("vendor/vcclient-2.1.4-alpha/dist/main/mnr_vcclient_server.exe")
        .is_file()
}

pub fn core_installed(root: &Path) -> bool {
    root.join("vendor/nvidia-afx-3.0.0/bin/NVAudioEffects.dll")
        .is_file()
        && root
            .join("vendor/tag-2.0.0.1903-demo/apidll/x64/tagapi.dll")
            .is_file()
}

/// Both model versions the engine can load for `arch` (the engine's own folder names), current.
pub fn models_installed(root: &Path, arch: &str) -> bool {
    let dir = root.join(MODELS_DIR).join(arch);
    let version = std::fs::read_to_string(dir.join("version.txt"));
    dir.join("denoiser_48k.trtpkg").is_file()
        && dir.join("denoiser_v2_48k.trtpkg").is_file()
        && version.as_deref().map_or("3.0.0", str::trim) == MODELS_VERSION
}

/// The GPU architectures with NVIDIA denoiser models.
pub const MODEL_ARCHS: [&str; 4] = ["turing", "ampere", "ada", "blackwell"];
/// Nothing to download for `arch`: its models are current, or there are none for it.
pub fn models_present(root: &Path, arch: Option<&str>) -> bool {
    arch.is_none_or(|a| !MODEL_ARCHS.contains(&a) || models_installed(root, a))
}

/// A dead server fails the install instead of hanging it. The body has no limit: a gigabyte on a
/// slow line takes long, and ureq has no idle timeout (the setup card shows a stall instead).
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

/// Why an install failed, in a few words for the setup card; app.log keeps the full text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Network,
    Disk,
    Corrupt,
    Access,
    Other,
}
pub fn reason(error: &str) -> Reason {
    let e = error.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| e.contains(n));
    if any(&["os error 112", "os error 39"]) {
        Reason::Disk
    } else if any(&["sha-256", "размер загруженной", "подпись"]) {
        Reason::Corrupt
    } else if any(&["os error 5)", "access is denied", "отказано в доступе"]) {
        Reason::Access
    } else if any(&["os error 100", "os error 110", "dns", "timeout", "timed out", "connection", "tls", "http status", "io:"]) {
        Reason::Network
    } else {
        Reason::Other
    }
}

/// Bytes per second over (time, bytes done) samples at least a second apart.
pub fn rate(samples: &[(std::time::Instant, u64)]) -> Option<f64> {
    let (&(t0, d0), &(t1, d1)) = (samples.first()?, samples.last()?);
    let seconds = t1.duration_since(t0).as_secs_f64();
    (seconds >= 1.0 && d1 > d0).then(|| (d1 - d0) as f64 / seconds)
}

pub fn driver_installed() -> bool {
    Command::new("reg")
        .args(["query", DRIVER_KEY, "/v", "Service"])
        .creation_flags(NO_WINDOW)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Installs the signed TAG driver that came with the core component. The user confirms one UAC
/// prompt; no Windows security setting is changed. `install-tag.ps1` stays the developer path.
pub fn install_driver(root: &Path) -> Result<(), String> {
    let manager = root.join(DRIVER_DIR).join("wdmdrvmgr/x64/wdmdrvmgr.exe");
    let inf = root
        .join(DRIVER_DIR)
        .join("driver/ThinAudioGateway_4d699d4a.inf");
    for file in [&manager, &inf] {
        if !file.is_file() {
            return Err(format!("Файл драйвера не найден: {}", file.display()));
        }
    }
    let status = Command::new(powershell())
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(install_script(&manager, &inf))
        .creation_flags(NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("Установка отменена: нужны права администратора. \
                    Нажмите кнопку ещё раз и подтвердите запрос Windows."
            .into());
    }
    if !driver_installed() {
        return Err("Установщик драйвера завершился, но устройство не появилось".into());
    }
    Ok(())
}

fn powershell() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
}

/// `Start-Process -Verb RunAs` is the only elevation path without a service. The INF argument
/// carries its own quotes: Windows PowerShell does not quote list items that contain spaces,
/// and the component path (`%APPDATA%\Mic Noize\Components`) has one.
fn install_script(manager: &Path, inf: &Path) -> String {
    format!(
        "$ErrorActionPreference='Stop';\
         $p=Start-Process -FilePath '{}' -ArgumentList @('-q','-h','{}','-i','{}','\"{}\"') \
         -Verb RunAs -WindowStyle Hidden -PassThru -Wait;exit $p.ExitCode",
        quoted(manager),
        DRIVER_INSTANCE,
        DRIVER_HARDWARE_ID,
        quoted(inf)
    )
}

fn quoted(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}

pub fn install_rvc(components: &Path) -> Result<String, String> {
    install(
        Item::Rvc,
        RVC_MANIFEST_URL,
        components,
        "vendor/vcclient-2.1.4-alpha/dist/main/mnr_vcclient_server.exe",
        &["vendor/vcclient-2.1.4-alpha"],
    )
}

/// The core runtime when it is missing, then the models for this GPU when the core lacks them.
/// Models go in their own folder: a running `mic_tag_host.exe` never has to be replaced.
pub fn install_core(components: &Path, arch: Option<&str>) -> Result<String, String> {
    let mut version = String::new();
    if !core_installed(components) {
        version = install(
            Item::Core,
            CORE_MANIFEST_URL,
            components,
            "vendor/nvidia-afx-3.0.0/bin/NVAudioEffects.dll",
            &[
                "vendor/nvidia-afx-3.0.0",
                "vendor/tag-2.0.0.1903-demo",
            ],
        )?;
    }
    if let Some(arch) = arch.filter(|a| MODEL_ARCHS.contains(a))
        && !models_installed(components, arch)
    {
        let entry = format!("{MODELS_DIR}/{arch}");
        let models = install(
            Item::Models,
            &format!("{MODELS_RELEASE}/models-{arch}.json"),
            components,
            &format!("{entry}/denoiser_48k.trtpkg"),
            &[&entry],
        )
        .map_err(|e| format!("модели NVIDIA для {arch}: {e}"))?;
        if version.is_empty() {
            version = format!("(модели NVIDIA {arch} {models})");
        }
    }
    Ok(version)
}

fn install(
    item: Item,
    manifest_url: &str,
    components: &Path,
    expected: &str,
    entries: &[&str],
) -> Result<String, String> {
    set_stage(item, Phase::Connect, 0);
    let mut response = agent().get(manifest_url).call().map_err(|e| e.to_string())?;
    let envelope: Envelope = serde_json::from_str(
        &response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    verify(&envelope)?;
    let manifest: Manifest = serde_json::from_str(&envelope.payload).map_err(|e| e.to_string())?;
    if manifest.parts.is_empty() {
        return Err("Манифест не содержит частей архива".into());
    }
    let total = manifest.parts.iter().map(|p| p.size).sum();
    set_stage(item, Phase::Download, total);
    let work = components.join(".download-rvc");
    let stage = components.join(".stage-rvc");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let archive = work.join("rvc-runtime.tar.zst");
    let mut paths = Vec::with_capacity(manifest.parts.len());
    for (index, part) in manifest.parts.iter().enumerate() {
        let path = work.join(format!("part-{index:03}"));
        download(part, &path)?;
        paths.push(path);
    }
    set_stage(item, Phase::Verify, total);
    assemble(&paths, &archive, &manifest.archive_sha256)?;
    set_stage(item, Phase::Unpack, total);
    unpack(&archive, &stage)?;
    set_stage(item, Phase::Place, 0);
    if !stage.join(expected).is_file() {
        return Err("Архив компонента не содержит ожидаемый файл".into());
    }
    for entry in entries {
        let source = stage.join(entry);
        let destination = components.join(entry);
        if destination.is_dir() {
            std::fs::remove_dir_all(&destination).map_err(|e| e.to_string())?;
        } else if destination.exists() {
            std::fs::remove_file(&destination).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(destination.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::rename(source, destination).map_err(|e| e.to_string())?;
    }
    if entries.iter().any(|entry| entry.contains("vcclient")) {
        std::fs::create_dir_all(components.join("vendor/vcclient-2.1.4-alpha/dist/main/model_dir"))
            .map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&stage);
    Ok(manifest.version)
}

/// The Ed25519 key that signs component manifests and application updates.
pub fn release_key() -> Result<[u8; 32], String> {
    STANDARD
        .decode(PUBLIC_KEY)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| String::from("Неверный публичный ключ"))
}

fn verify(envelope: &Envelope) -> Result<(), String> {
    verify_with_key(envelope, release_key()?)
}

/// The payload of a signed envelope (`{payload, signature}` JSON), checked against `key`.
pub fn signed_payload(envelope: &str, key: [u8; 32]) -> Result<String, String> {
    let envelope: Envelope = serde_json::from_str(envelope).map_err(|e| e.to_string())?;
    verify_with_key(&envelope, key)?;
    Ok(envelope.payload)
}

/// The signature `package-release.ps1` publishes next to an update's full package.
pub fn update_signature(version: &str, package: &str) -> Result<String, String> {
    let url = format!("https://github.com/Arkanoidvfx/MicNoize/releases/download/v{version}/{package}.sig.json");
    let mut response = agent().get(&url).call().map_err(|e| e.to_string())?;
    response.body_mut().read_to_string().map_err(|e| e.to_string())
}

fn verify_with_key(envelope: &Envelope, key: [u8; 32]) -> Result<(), String> {
    let signature: [u8; 64] = STANDARD
        .decode(&envelope.signature)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "Неверная подпись манифеста")?;
    VerifyingKey::from_bytes(&key)
        .map_err(|e| e.to_string())?
        .verify(
            envelope.payload.as_bytes(),
            &Signature::from_bytes(&signature),
        )
        .map_err(|_| "Подпись RVC-манифеста не прошла проверку".into())
}

fn download(part: &Part, path: &Path) -> Result<(), String> {
    if cached_part_is_valid(part, path) {
        DONE.fetch_add(part.size, Ordering::Relaxed);
        return Ok(());
    }
    let response = agent().get(&part.url).call().map_err(|e| e.to_string())?;
    let mut reader = response.into_parts().1.into_reader();
    let mut output = File::create(path).map_err(|e| e.to_string())?;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
        DONE.fetch_add(count as u64, Ordering::Relaxed);
    }
    drop(output);
    if path.metadata().map_err(|e| e.to_string())?.len() != part.size {
        return Err("Размер загруженной части RVC не совпадает с манифестом".into());
    }
    check_hash(path, &part.sha256)
}

fn cached_part_is_valid(part: &Part, path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.len() == part.size)
        && check_hash(path, &part.sha256).is_ok()
}

/// Joins the parts into the archive and checks the archive's SHA-256 in the same pass.
fn assemble(parts: &[PathBuf], archive: &Path, expected: &str) -> Result<(), String> {
    let mut output = File::create(archive).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    for part in parts {
        let mut input = File::open(part).map_err(|e| e.to_string())?;
        loop {
            let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count]).map_err(|e| e.to_string())?;
            hash.update(&buffer[..count]);
            DONE.fetch_add(count as u64, Ordering::Relaxed);
        }
    }
    if hex::encode(hash.finalize()).eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("SHA-256 не совпадает: {}", archive.display()))
    }
}

/// Unpacks through tar's standard input; tar reads as it writes, so the bytes fed track it.
/// zstd is decoded here: tar.exe before Windows 11 23H2 reads plain tar but not zstd.
/// tar's errors go to a file, not a pipe nobody drains while the archive is fed.
fn unpack(archive: &Path, stage: &Path) -> Result<(), String> {
    let log = archive.with_extension("log");
    let mut tar = Command::new("tar.exe")
        .args(["-xf", "-", "-C"])
        .arg(stage)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(File::create(&log).map_err(|e| e.to_string())?)
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    let fed = feed(archive, tar.stdin.take().ok_or("tar без stdin")?);
    let status = tar.wait().map_err(|e| e.to_string())?;
    let errors = std::fs::read_to_string(&log).unwrap_or_default();
    match fed {
        Ok(()) if status.success() => Ok(()),
        fed => Err(unpack_error(fed.err(), status.code(), &errors)),
    }
}

/// tar's own last error is the most useful; a broken pipe only means tar stopped first.
fn unpack_error(fed: Option<std::io::Error>, code: Option<i32>, errors: &str) -> String {
    let detail = match errors.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(line) => line.chars().take(300).collect(),
        None => match fed {
            Some(e) => e.to_string(),
            None => format!("tar завершился с кодом {code:?}"),
        },
    };
    format!("Не удалось распаковать компонент: {detail}")
}

fn feed(archive: &Path, mut input: ChildStdin) -> std::io::Result<()> {
    let compressed = std::io::BufReader::with_capacity(1024 * 1024, Counted(File::open(archive)?));
    let mut tar =
        ruzstd::decoding::StreamingDecoder::new(compressed).map_err(std::io::Error::other)?;
    std::io::copy(&mut tar, &mut input)?;
    Ok(())
}

/// Counts the compressed bytes read, the same units the download and SHA-256 stages count.
struct Counted(File);

impl Read for Counted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.0.read(buffer)?;
        DONE.fetch_add(count as u64, Ordering::Relaxed);
        Ok(count)
    }
}

fn check_hash(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|e| e.to_string())?;
    let actual = hex::encode(hash.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("SHA-256 не совпадает: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn component_manifest_signature_rejects_tampering() {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let payload = r#"{"version":"test"}"#;
        let envelope = Envelope {
            payload: payload.into(),
            signature: STANDARD.encode(signing.sign(payload.as_bytes()).to_bytes()),
        };
        assert!(verify_with_key(&envelope, signing.verifying_key().to_bytes()).is_ok());
        let tampered = Envelope {
            payload: "{}".into(),
            ..envelope
        };
        assert!(verify_with_key(&tampered, signing.verifying_key().to_bytes()).is_err());
    }

    #[test]
    fn cached_component_part_requires_matching_hash() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.tmp/component-cache-test")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"runtime").unwrap();
        let part = Part {
            url: String::new(),
            size: 7,
            sha256: "d92c6a81b2ff50096bcda80885427d1f59a25b5f483f7055523504925d16ab23".into(),
        };
        assert!(cached_part_is_valid(&part, &path));
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(!cached_part_is_valid(&part, &path));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn models_need_both_files_and_the_current_version() {
        let root = std::env::temp_dir().join(format!("mnr-models-{}", std::process::id()));
        let dir = root.join(MODELS_DIR).join("ada");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("denoiser_48k.trtpkg"), b"x").unwrap();
        assert!(!models_installed(&root, "ada"));
        std::fs::write(dir.join("denoiser_v2_48k.trtpkg"), b"x").unwrap();
        assert!(models_installed(&root, "ada"));
        std::fs::write(dir.join("version.txt"), "2.1.0\n").unwrap();
        assert!(!models_installed(&root, "ada"));
        std::fs::write(dir.join("version.txt"), format!("{MODELS_VERSION}\r\n")).unwrap();
        assert!(models_installed(&root, "ada"));
        assert!(!models_installed(&root, "turing"));
        std::fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn rate_needs_a_second_and_progress() {
        let t = std::time::Instant::now();
        let at = |ms| t + std::time::Duration::from_millis(ms);
        assert_eq!(rate(&[]), None);
        assert_eq!(rate(&[(t, 0), (at(500), 10)]), None, "too short to tell");
        assert_eq!(rate(&[(t, 5), (at(2000), 5)]), None, "stalled");
        assert_eq!(rate(&[(t, 0), (at(700), 1), (at(2000), 4_000_000)]), Some(2_000_000.0));
        assert!(models_present(Path::new("nowhere"), None) && models_present(Path::new("nowhere"), Some("pascal")));
        assert!(!models_present(Path::new("nowhere"), Some("ada")));
    }
    #[test]
    fn install_errors_get_a_reason() {
        assert_eq!(reason("io: Connection reset by peer (os error 10054)"), Reason::Network);
        assert_eq!(reason("dns failed: No such host is known. (os error 11001)"), Reason::Network);
        assert_eq!(reason("timeout: connect"), Reason::Network);
        assert_eq!(reason("http status: 404"), Reason::Network);
        assert_eq!(reason("There is not enough space on the disk. (os error 112)"), Reason::Disk);
        assert_eq!(reason(r"SHA-256 не совпадает: C:\x\part-000"), Reason::Corrupt);
        assert_eq!(reason("Access is denied. (os error 5)"), Reason::Access);
        assert_eq!(reason("Не удалось распаковать компонент"), Reason::Other);
    }
    #[test]
    fn archive_is_joined_checked_and_unpacked_with_progress() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.tmp/component-unpack-test").join(std::process::id().to_string());
        let (source, out) = (root.join("src"), root.join("out"));
        std::fs::create_dir_all(source.join("vendor")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(source.join("vendor/x.dll"), vec![7_u8; 300_000]).unwrap();
        let packed = root.join("whole.tar.zst");
        let packing = Command::new("tar.exe").arg("--zstd").arg("-cf").arg(&packed).arg("-C").arg(&source).arg("vendor").status().unwrap();
        assert!(packing.success());
        let bytes = std::fs::read(&packed).unwrap();
        let (a, b) = bytes.split_at(bytes.len() / 2);
        std::fs::write(root.join("part-000"), a).unwrap();
        std::fs::write(root.join("part-001"), b).unwrap();
        let parts = [root.join("part-000"), root.join("part-001")];
        let sha = hex::encode(Sha256::digest(&bytes));
        let archive = root.join("archive.tar.zst");
        assert!(assemble(&parts, &archive, &"0".repeat(64)).is_err(), "a wrong hash is refused");
        set_stage(Item::Core, Phase::Verify, bytes.len() as u64);
        assemble(&parts, &archive, &sha).unwrap();
        assert_eq!(status().share(), Some(1.0));
        set_stage(Item::Core, Phase::Unpack, bytes.len() as u64);
        unpack(&archive, &out).unwrap();
        assert_eq!(status(), Status { item: Item::Core, phase: Phase::Unpack, done: bytes.len() as u64, total: bytes.len() as u64 });
        assert_eq!(std::fs::read(out.join("vendor/x.dll")).unwrap().len(), 300_000);
        std::fs::write(&archive, b"not an archive").unwrap();
        std::fs::create_dir_all(root.join("bad")).unwrap();
        let error = unpack(&archive, &root.join("bad")).unwrap_err();
        assert!(error.starts_with("Не удалось распаковать компонент: ") && error.len() > 70, "{error}");
        assert_eq!(unpack_error(None, Some(1), "\r\ntar.exe: Error opening archive\r\n\r\n"), "Не удалось распаковать компонент: tar.exe: Error opening archive");
        std::fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn driver_install_script_quotes_paths_with_spaces() {
        let script = install_script(
            Path::new(r"C:\Program Files\wdmdrvmgr.exe"),
            Path::new(r"C:\Users\a\AppData\Roaming\Mic Noize\Components\tag.inf"),
        );
        assert!(script.contains(r"-FilePath 'C:\Program Files\wdmdrvmgr.exe'"));
        assert!(script.contains(r#"'"C:\Users\a\AppData\Roaming\Mic Noize\Components\tag.inf"'"#));
        assert!(script.contains(DRIVER_HARDWARE_ID));
        assert_eq!(quoted(Path::new(r"C:\it's\x.inf")), r"C:\it''s\x.inf");
    }
}
