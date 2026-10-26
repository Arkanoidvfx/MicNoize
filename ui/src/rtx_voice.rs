use std::{
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Output},
};

const PROCESS: &str = "NVIDIA RTX Voice.exe";
const WARNING: &str = "Обнаружен запущенный NVIDIA RTX Voice.\n\n\
    Он может конфликтовать с Mic Noize и вызывать искажения или обрывы звука.\n\
    Закройте RTX Voice через его значок в трее.\n\n\
    Если входом Mic Noize выбран RTX Voice, переключите вход на физический микрофон.";

fn detected(output: &Output) -> Result<bool, String> {
    if !output.status.success() {
        return Err(format!("tasklist: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.strip_prefix('"')
            .and_then(|line| line.split_once("\","))
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case(PROCESS))
    }))
}

pub fn warn_if_running(runtime: &Path) {
    let running = Command::new("tasklist.exe")
        .args([
            "/FI",
            &format!("IMAGENAME eq {PROCESS}"),
            "/FO",
            "CSV",
            "/NH",
        ])
        .creation_flags(0x08000000) // No console flash from the GUI process.
        .output()
        .map_err(|e| e.to_string())
        .and_then(|output| detected(&output));
    match running {
        Ok(true) => {
            crate::logs::note(
                runtime,
                "Предупреждение: запущен NVIDIA RTX Voice; возможен конфликт звука.",
            );
            unsafe extern "system" {
                fn MessageBoxW(
                    window: isize,
                    text: *const u16,
                    title: *const u16,
                    flags: u32,
                ) -> i32;
            }
            let text: Vec<u16> = WARNING.encode_utf16().chain(Some(0)).collect();
            let title: Vec<u16> = "Mic Noize".encode_utf16().chain(Some(0)).collect();
            // Remain visible on tray-only login starts, before audio tasks begin.
            unsafe {
                MessageBoxW(0, text.as_ptr(), title.as_ptr(), 0x30 | 0x10000);
            }
        }
        Ok(false) => {}
        Err(error) => {
            crate::logs::note(runtime, &format!("Проверка RTX Voice недоступна: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::ExitStatusExt;

    #[test]
    fn warns_only_for_an_exact_running_process() {
        for (csv, expected) in [
            (
                "\"NVIDIA RTX Voice.exe\",\"123\",\"Console\",\"1\",\"12 000 K\"\r\n",
                true,
            ),
            ("\"nvidia rtx voice.EXE\",\"123\"\r\n", true),
            (
                "\"other.exe\",\"1\"\r\n\"NVIDIA RTX Voice.exe\",\"123\"\r\n",
                true,
            ),
            ("", false),
            (
                "INFO: No tasks are running which match the specified criteria.\r\n",
                false,
            ),
            (
                "ИНФОРМАЦИЯ: Задачи, отвечающие заданным критериям, отсутствуют.\r\n",
                false,
            ),
            ("\"NVIDIA RTX Voice.exe.bak\",\"123\"\r\n", false),
            ("\"NVIDIA Broadcast.exe\",\"123\"\r\n", false),
            (
                "C:\\Program Files\\NVIDIA Corporation\\NVIDIA RTX Voice\\NVIDIA RTX Voice.exe",
                false,
            ),
        ] {
            let output = Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: csv.as_bytes().to_vec(),
                stderr: vec![],
            };
            assert_eq!(detected(&output), Ok(expected), "{csv}");
        }
    }

    #[test]
    fn failed_process_query_does_not_confirm_rtx_voice() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: b"\"NVIDIA RTX Voice.exe\",\"123\"".to_vec(),
            stderr: vec![],
        };
        assert!(detected(&output).is_err());
    }
}
