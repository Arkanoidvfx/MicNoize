use crate::settings::Settings;
use std::os::windows::process::CommandExt;
use std::path::Path;

pub const CHUNKS: [u32; 5] = [100, 150, 200, 300, 500];
pub const MODEL_CATALOGS: [(&str, &str, &str); 2] = [
    ("Voice Models", "Каталог голосов и персонажей", "https://voice-models.com/"),
    ("Hugging Face", "Файлы моделей · фильтр RVC", "https://huggingface.co/models?other=rvc"),
];
const DISPLAY_NAME_FILE: &str = "display-name.txt";
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    pub slot: u32,
    pub pitch: i32,
    pub index: u32,
    pub chunk: u32,
    pub gain: u32,
}
impl Options {
    pub fn load(settings: &Settings) -> Self {
        let chunk = settings.number("rvc", "chunk_ms", 200, 100, 500) as u32;
        Self {
            slot: settings.number("rvc", "slot", 0, 0, 65535) as u32,
            pitch: settings.number("rvc", "pitch", 0, -24, 24),
            index: settings.number("rvc", "index", 0, 0, 100) as u32,
            chunk: if CHUNKS.contains(&chunk) { chunk } else { 200 },
            gain: settings.number("rvc", "gain", 100, 50, 300) as u32,
        }
    }
    pub fn save(self, settings: &mut Settings) {
        for (key, value) in [
            ("slot", self.slot as i32),
            ("pitch", self.pitch),
            ("index", self.index as i32),
            ("chunk_ms", self.chunk as i32),
            ("gain", self.gain as i32),
        ] {
            settings.set("rvc", key, value);
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub slot: u32,
    pub name: String,
    pub has_index: bool,
}
impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} · слот {}", self.name, self.slot)
    }
}
pub fn models(root: &Path) -> Result<Vec<Model>, String> {
    let folder = root.join("vendor/vcclient-2.1.4-alpha/dist/main/model_dir");
    let mut models = Vec::new();
    for entry in std::fs::read_dir(folder).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let Some(slot) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| *n <= 65535)
        else {
            continue;
        };
        if !entry.path().join("params.json").is_file() {
            continue;
        }
        let files: Vec<_> = std::fs::read_dir(entry.path())
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let has_index = files
            .iter()
            .any(|f| f.path().extension().is_some_and(|x| x == "index"));
        if let Some(file) = files.iter().find(|f| {
            f.path()
                .extension()
                .is_some_and(|x| x == "pth" || x == "onnx")
        }) {
            let fallback = file
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            models.push(Model {
                slot,
                name: std::fs::read_to_string(entry.path().join(DISPLAY_NAME_FILE))
                    .ok()
                    .and_then(|name| valid_name(&name).ok())
                    .unwrap_or(fallback),
                has_index,
            });
        }
    }
    models.sort_by_key(|m| m.slot);
    Ok(models)
}

fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        Err("Название должно содержать от 1 до 80 обычных символов".into())
    } else {
        Ok(name.into())
    }
}

pub fn rename(root: &Path, slot: u32, name: &str) -> Result<String, String> {
    let name = valid_name(name)?;
    if !models(root)?.iter().any(|model| model.slot == slot) {
        return Err("Выбранная модель больше не существует".into());
    }
    std::fs::write(
        root.join("vendor/vcclient-2.1.4-alpha/dist/main/model_dir")
            .join(slot.to_string())
            .join(DISPLAY_NAME_FILE),
        &name,
    )
    .map_err(|e| e.to_string())?;
    Ok(name)
}

pub fn delete(root: &Path, slot: u32) -> Result<(), String> {
    if !models(root)?.iter().any(|model| model.slot == slot) {
        return Err("Выбранная модель больше не существует".into());
    }
    std::fs::remove_dir_all(
        root.join("vendor/vcclient-2.1.4-alpha/dist/main/model_dir")
            .join(slot.to_string()),
    )
    .map_err(|e| e.to_string())
}

pub fn import(root: &Path) -> Result<Option<(Vec<Model>, u32)>, String> {
    let temporary = root.join(".tmp");
    std::fs::create_dir_all(&temporary).map_err(|e| e.to_string())?;
    let result = temporary.join(format!("rvc-import-{}.txt", std::process::id()));
    if result.exists() {
        std::fs::remove_file(&result).map_err(|e| e.to_string())?;
    }
    let status = std::process::Command::new(
        root.join("vendor/vcclient-2.1.4-alpha/dist/main/mnr_vcclient_server.exe"),
    )
    .arg("--import-model")
    .arg(&result)
    .env("TEMP", &temporary)
    .env("TMP", &temporary)
    .creation_flags(0x08000000)
    .status()
    .map_err(|e| e.to_string())?;
    let response = std::fs::read_to_string(&result);
    let _ = std::fs::remove_file(&result);
    if !status.success() {
        return Err("Не удалось запустить импорт; см. results/rvc-server-error.log".into());
    }
    let response = response.map_err(|e| e.to_string())?;
    if response == "cancel" {
        return Ok(None);
    }
    let slot = response.parse::<u32>().map_err(|_| response)?;
    Ok(Some((models(root)?, slot)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_display_name_and_delete() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(format!(".tmp/rvc-model-test-{}", std::process::id()));
        let slot = root.join("vendor/vcclient-2.1.4-alpha/dist/main/model_dir/3");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("params.json"), "{}").unwrap();
        std::fs::write(slot.join("voice.pth"), "test").unwrap();

        assert_eq!(models(&root).unwrap()[0].name, "voice");
        assert_eq!(rename(&root, 3, "  New voice  ").unwrap(), "New voice");
        assert_eq!(models(&root).unwrap()[0].name, "New voice");
        assert!(rename(&root, 3, "   ").is_err());
        delete(&root, 3).unwrap();
        assert!(models(&root).unwrap().is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
