use std::path::{Path, PathBuf};
#[derive(Debug)]
pub struct Settings {
    pub path: PathBuf,
    lines: Vec<String>,
}
impl Settings {
    #[cfg(test)]
    pub fn for_test(text: &str) -> Self {
        Self {
            path: PathBuf::from("unused-test-settings/settings.ini"),
            lines: text.lines().map(str::to_owned).collect(),
        }
    }
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("settings.ini");
        let bytes = match std::fs::read(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(format!("Чтение настроек {}: {e}", path.display())),
        };
        let text = decode_ini(&bytes)?;
        if !text.contains("[effects]") && path.exists() {
            let backup = root.join("settings.pre-rust.ini");
            if !backup.exists() {
                std::fs::copy(&path, &backup).map_err(|e| format!("Резервная копия настроек {} -> {}: {e}", path.display(), backup.display()))?;
            }
        }
        Ok(Self {
            path,
            lines: text
                .trim_start_matches('\u{feff}')
                .lines()
                .map(str::to_owned)
                .collect(),
        })
    }
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let mut current = "";
        let mut result = None;
        for line in &self.lines {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                current = &line[1..line.len() - 1];
            } else if current == section
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == key
            {
                result = Some(v.trim());
            }
        }
        result
    }
    pub fn number(&self, section: &str, key: &str, default: i32, min: i32, max: i32) -> i32 {
        self.get(section, key)
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|v| (*v >= min) && (*v <= max))
            .unwrap_or(default)
    }
    pub fn set(&mut self, section: &str, key: &str, value: impl ToString) {
        let value = value.to_string();
        let mut inside = false;
        let mut found = false;
        let mut updated = false;
        let mut insert = self.lines.len();
        for i in 0..self.lines.len() {
            let l = self.lines[i].trim();
            if l.starts_with('[') && l.ends_with(']') {
                if inside {
                    insert = i;
                }
                inside = l == format!("[{section}]");
                if inside {
                    found = true;
                    insert = i + 1;
                }
            } else if inside {
                insert = i + 1;
                if l.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
                    self.lines[i] = format!("{key}={value}");
                    updated = true;
                }
            }
        }
        if updated {
            return;
        }
        if !found {
            self.lines.push(format!("[{section}]"));
            insert = self.lines.len();
        }
        self.lines.insert(insert, format!("{key}={value}"));
    }
    pub fn text(&self) -> String {
        self.lines.join("\r\n") + "\r\n"
    }
}
fn decode_ini(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        if !bytes.len().is_multiple_of(2) {
            return Err("Повреждён UTF-16 settings.ini".into());
        }
        let words: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .collect();
        String::from_utf16(&words).map_err(|e| e.to_string())
    } else {
        String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
    }
}
pub fn key_name(binding: u32) -> String {
    if binding == 0 {
        return "Назначить".into();
    }
    let mut s = String::new();
    for (bit, name) in [(1, "Ctrl + "), (2, "Alt + "), (4, "Shift + ")] {
        if (binding >> 8) & bit != 0 {
            s.push_str(name);
        }
    }
    let key = binding & 255;
    s.push_str(&match key {
        5 => "Mouse 4".into(),
        6 => "Mouse 5".into(),
        4 => "Mouse 3".into(),
        32 => "Space".into(),
        9 => "Tab".into(),
        13 => "Enter".into(),
        112..=135 => format!("F{}", key - 111),
        8 => "Backspace".into(),
        27 => "Esc".into(),
        33 => "Page Up".into(),
        34 => "Page Down".into(),
        35 => "End".into(),
        36 => "Home".into(),
        37 => "Left".into(),
        38 => "Up".into(),
        39 => "Right".into(),
        40 => "Down".into(),
        45 => "Insert".into(),
        46 => "Delete".into(),
        96..=105 => format!("Num {}", key - 96),
        48..=57 | 65..=90 => (key as u8 as char).to_string(),
        _ => format!("VK 0x{key:02X}"),
    });
    s
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_duplicate_keys_and_sections() {
        let mut s = Settings {
            path: PathBuf::new(),
            lines: "[audio]\ninput=first\ninput=second\n[other]\ninput=keep\n[audio]\ninput=last"
                .lines()
                .map(str::to_owned)
                .collect(),
        };
        assert_eq!(s.get("audio", "input"), Some("last"));
        s.set("audio", "input", "new");
        assert_eq!(s.get("audio", "input"), Some("new"));
        assert_eq!(s.get("other", "input"), Some("keep"));
        s.set("audio", "buffer_ms", 40);
        assert_eq!(s.get("audio", "buffer_ms"), Some("40"));
        s.set("effects", "pitch", -5);
        assert_eq!(s.get("effects", "pitch"), Some("-5"));
        let reloaded = Settings::for_test(&s.text());
        assert_eq!(reloaded.get("audio", "input"), Some("new"));
        assert_eq!(reloaded.get("other", "input"), Some("keep"));
    }
    #[test]
    fn preserves_unknown_settings() {
        let mut s = Settings {
            path: PathBuf::new(),
            lines: vec![
                "[audio]".into(),
                "input=abc".into(),
                "unknown=42".into(),
                "[other]".into(),
                "keep=yes".into(),
            ],
        };
        s.set("audio", "input", "new");
        s.set("audio", "buffer_ms", 40);
        s.set("effects", "pitch", -5);
        assert_eq!(s.get("audio", "input"), Some("new"));
        assert_eq!(s.get("audio", "unknown"), Some("42"));
        assert_eq!(s.get("other", "keep"), Some("yes"));
        assert_eq!(s.get("effects", "pitch"), Some("-5"));
    }
    #[test]
    fn key_labels() {
        assert_eq!(key_name(119 | 256), "Ctrl + F8");
        assert_eq!(key_name(5), "Mouse 4");
        assert_eq!(key_name(36), "Home");
        assert_eq!(key_name(198), "VK 0xC6");
        assert_eq!(key_name(96 | 256), "Ctrl + Num 0");
    }
    #[test]
    fn old_win32_ini() {
        let source = "[audio]\r\ninput=Микрофон\r\n";
        let mut bytes = vec![0xff, 0xfe];
        for w in source.encode_utf16() {
            bytes.extend(w.to_le_bytes());
        }
        assert_eq!(decode_ini(&bytes).unwrap(), source);
        assert!(decode_ini(&bytes[..bytes.len() - 1]).is_err());
    }
}
