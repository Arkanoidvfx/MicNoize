use std::path::{Path, PathBuf};

const APP_DIR: &str = "Mic Noize";
const LEGACY_APP_DIR: &str = "MicNoiseReducer"; // Legacy name, migration only.

pub struct Paths {
    pub app: PathBuf,
    pub data: PathBuf,
    pub components: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| format!("Путь исполняемого файла: {e}"))?;
        let app = exe
            .parent()
            .ok_or("Executable has no parent directory")?
            .to_path_buf();
        let roaming = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or("APPDATA is not set")?;
        let data = roaming.join(APP_DIR);
        let components = data.join("Components");
        std::fs::create_dir_all(&data).map_err(|e| format!("Создание папки данных {}: {e}", data.display()))?;

        // One-time migration from the old install, then the repository-local layout.
        let legacy_repo = app
            .parent()
            .filter(|_| app.file_name().is_some_and(|n| n == "bin"));
        let settings = data.join("settings.ini");
        if !settings.exists() {
            let legacy_install = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|p| p.join(LEGACY_APP_DIR).join("settings.ini"));
            let old = legacy_install.filter(|p| p.is_file()).or_else(|| {
                legacy_repo
                    .map(|p| p.join("settings.ini"))
                    .filter(|p| p.is_file())
            });
            if let Some(old) = old {
                std::fs::copy(&old, &settings).map_err(|e| format!("Перенос настроек {} -> {}: {e}", old.display(), settings.display()))?;
            }
        }
        Ok(Self {
            app,
            data,
            components,
        })
    }

    pub fn runtime_root(&self) -> &Path {
        if self.app.file_name().is_some_and(|n| n == "bin")
            && let Some(project) = self.app.parent()
            && project.join("vendor/nvidia-afx-3.0.0").is_dir()
        {
            return project;
        }
        &self.components
    }
}
