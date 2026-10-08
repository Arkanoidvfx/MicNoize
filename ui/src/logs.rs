//! One copyable report: what the UI showed (`app.log`) plus the tails of every runtime log.
use std::{
    io::{Read, Seek, SeekFrom, Write},
    os::windows::process::CommandExt,
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const FILES: [(&str, usize); 6] = [
    ("app.log", 40),
    ("sessions.log", 15),
    ("tag-host.log", 15),
    ("tag-endpoint.log", 15),
    ("tag-headphones.log", 10),
    ("nvafx.log", 20),
];
const LIMIT: u64 = 512 * 1024;
const NO_WINDOW: u32 = 0x08000000;

fn utc(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{}Z", crate::telemetry::chrono_free_utc(seconds))
}

/// Appends one timestamped line to `results/app.log`; a full log keeps its newest half.
pub fn note(runtime: &Path, text: &str) {
    let folder = runtime.join("results");
    let path = folder.join("app.log");
    let _ = std::fs::create_dir_all(&folder);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LIMIT)
        && let Ok(old) = std::fs::read_to_string(&path)
    {
        let keep = old.len() / 2;
        // Byte search: `keep` may fall inside a Cyrillic character, a line start never does.
        let start = old.as_bytes()[keep..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(old.len(), |i| keep + i + 1);
        let _ = std::fs::write(&path, &old[start..]);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} {}", utc(SystemTime::now()), text.replace('\n', " "));
    }
}

/// Newest `count` lines; repeats in a row collapse into one line with a counter.
fn tail(text: &str, count: usize) -> Vec<String> {
    let mut lines: Vec<(String, usize)> = Vec::new();
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        match lines.last_mut() {
            Some((last, n)) if last == line => *n += 1,
            _ => lines.push((line.to_owned(), 1)),
        }
    }
    let skip = lines.len().saturating_sub(count);
    lines
        .into_iter()
        .skip(skip)
        .map(|(line, n)| if n > 1 { format!("{line}  (x{n})") } else { line })
        .collect()
}

pub(crate) fn command(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .creation_flags(NO_WINDOW)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Read only the newest bytes, including logs containing non-UTF-8 tool output.
pub(crate) fn read_tail(path: &Path, limit: usize) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(limit as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `header` carries the UI state; the rest is read here, off the UI thread.
pub fn report(runtime: &Path, header: &str) -> String {
    let mut out = header.trim_end().to_owned();
    let windows = command("cmd", &["/c", "ver"]);
    let nvidia = command(
        "nvidia-smi",
        &["--query-gpu=driver_version", "--format=csv,noheader"],
    );
    out.push_str(&format!(
        "\n{windows}\nДрайвер NVIDIA: {}\n",
        if nvidia.is_empty() { "не найден" } else { &nvidia }
    ));
    for (name, count) in FILES {
        let path = runtime.join("results").join(name);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let lines = tail(&text, count);
        if lines.is_empty() {
            continue;
        }
        let changed = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(utc)
            .unwrap_or_default();
        out.push_str(&format!("\n--- {name}, изменён {changed} ---\n"));
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    // Paths carry the Windows user name; it is not needed to read the report.
    match std::env::var("USERPROFILE") {
        Ok(profile) if !profile.is_empty() => out.replace(&profile, "%USERPROFILE%"),
        _ => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tail_keeps_newest_lines_and_collapses_repeats() {
        let text = "a\nb\nb\nb\n\nc\n";
        assert_eq!(tail(text, 2), ["b  (x3)", "c"]);
        assert_eq!(tail(text, 9), ["a", "b  (x3)", "c"]);
        assert!(tail("", 5).is_empty());
    }
}
