//! Soundpad library: a user folder of mp3 / wav / ogg clips, decoded here to 48 kHz mono
//! for the native engine, with per-clip hotkeys and gains persisted in settings.ini.
use crate::settings::Settings;
use std::path::{Path, PathBuf};

pub const RATE: u32 = 48_000;
/// Clips longer than this are truncated: they are memes, not podcasts, and every loaded
/// clip stays resident in the engine (192 KB per second).
pub const MAX_SECONDS: usize = 300;
pub const EXTENSIONS: [&str; 4] = ["mp3", "wav", "ogg", "m4a"];

#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    pub name: String,
    pub path: PathBuf,
    pub key: u32,
    /// 0..=200 percent.
    pub volume: u32,
    /// Unix seconds of the last start from the list or a hotkey; 0 = never.
    pub played: u64,
    /// File modification time, unix seconds: "newest" order.
    pub modified: u64,
    pub state: State,
}
/// List order. Persisted as `[soundpad] sort`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    Hotkey,
    Recent,
    Newest,
}
impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Name, Sort::Hotkey, Sort::Recent, Sort::Newest];
    pub fn code(self) -> i32 {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0) as i32
    }
    pub fn from_code(code: i32) -> Self {
        Self::ALL.get(code as usize).copied().unwrap_or(Sort::Name)
    }
}
impl std::fmt::Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Sort::Name => "По имени",
            Sort::Hotkey => "С хоткеем сверху",
            Sort::Recent => "Недавно игравшие",
            Sort::Newest => "Новые файлы",
        })
    }
}
/// Indices of `sounds` in `sort` order; ties fall back to the name.
pub fn order(sounds: &[Sound], sort: Sort) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..sounds.len()).collect();
    indices.sort_by_cached_key(|&i| {
        let s = &sounds[i];
        let name = s.name.to_lowercase();
        match sort {
            Sort::Name => (0u64, name),
            // Bound clips first, in key order so a row of F-keys reads left to right.
            Sort::Hotkey => (if s.key == 0 { u64::MAX } else { s.key as u64 }, name),
            Sort::Recent => (u64::MAX - s.played, name),
            Sort::Newest => (u64::MAX - s.modified, name),
        }
    });
    indices
}
/// "danya - hehe.mp3" -> ("danya", "danya"): the part before the first " - ", trimmed, with a
/// lowercase key for grouping. Names without the separator have no group.
pub fn prefix(name: &str) -> Option<(String, String)> {
    let (head, _) = name.split_once(" - ")?;
    let head = head.trim();
    (!head.is_empty()).then(|| (head.to_owned(), head.to_lowercase()))
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub label: String,
    pub key: String,
    pub count: usize,
}
/// Automatic sections: every prefix shared by at least two clips, by count then name.
pub fn groups(sounds: &[Sound]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for sound in sounds {
        let Some((label, key)) = prefix(&sound.name) else {
            continue;
        };
        match groups.iter_mut().find(|g| g.key == key) {
            Some(group) => group.count += 1,
            None => groups.push(Group {
                label,
                key,
                count: 1,
            }),
        }
    }
    groups.retain(|g| g.count >= 2);
    groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    groups
}
/// A user section: clips are referenced by file name so a rescan keeps them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub files: Vec<String>,
}
/// Section names are keys in settings.ini: no tabs, colons or pipes, at most 40 chars.
pub fn section_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '\t' | ':' | '|' | '\r' | '\n'))
        .take(40)
        .collect::<String>()
        .trim()
        .to_owned()
}
/// `[soundpad] sections`: `name:file|file` entries separated by tabs. `|` cannot appear in a
/// Windows file name, so no escaping is needed.
pub fn sections(settings: &Settings) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for entry in settings.get("soundpad", "sections").unwrap_or_default().split('\t') {
        let Some((name, files)) = entry.split_once(':') else {
            continue;
        };
        let name = section_name(name);
        if name.is_empty() || sections.iter().any(|s| s.name == name) {
            continue;
        }
        sections.push(Section {
            name,
            files: files
                .split('|')
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect(),
        });
    }
    sections
}
pub fn serialize_sections(sections: &[Section]) -> String {
    sections
        .iter()
        .map(|s| format!("{}:{}", s.name, s.files.join("|")))
        .collect::<Vec<_>>()
        .join("\t")
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Unloaded,
    Loading,
    Loaded(f32),
    Failed(String),
}
impl Sound {
    pub fn gain(&self) -> f32 {
        self.volume as f32 / 100.0
    }
}

/// Clips of the folder in name order; keys, gains and play times come from `saved` (see [`entries`]).
pub fn scan(folder: &Path, saved: &[Entry]) -> Result<Vec<Sound>, String> {
    let mut sounds: Vec<Sound> = std::fs::read_dir(folder)
        .map_err(|e| format!("{}: {e}", folder.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        })
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (key, volume, played) = saved
                .iter()
                .find(|e| e.name == name)
                .map(|e| (e.key, e.volume, e.played))
                .unwrap_or((0, 100, 0));
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Sound {
                name,
                path,
                key,
                volume,
                played,
                modified,
                state: State::Unloaded,
            }
        })
        .collect();
    sounds.sort_by_key(|s| s.name.to_lowercase());
    Ok(sounds)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub key: u32,
    pub volume: u32,
    pub played: u64,
}
/// One settings line for everything that is not the default: `key:volume:played:name`, tab
/// separated (a `key:volume:name` line from the first build still loads). Names cannot
/// contain tabs or colons on Windows, so no escaping is needed.
pub fn entries(settings: &Settings) -> Vec<Entry> {
    settings
        .get("soundpad", "sounds")
        .unwrap_or_default()
        .split('\t')
        .filter_map(|entry| {
            let (key, rest) = entry.split_once(':')?;
            let (volume, rest) = rest.split_once(':')?;
            let (played, name) = match rest.split_once(':') {
                Some((played, name)) => (played.parse::<u64>().ok()?, name),
                None => (0, rest),
            };
            let key = key.parse::<u32>().ok().filter(|k| *k <= 2046)?;
            let volume = volume.parse::<u32>().ok().filter(|v| *v <= 200)?;
            (!name.is_empty()).then(|| Entry {
                name: name.to_owned(),
                key,
                volume,
                played,
            })
        })
        .collect()
}
pub fn serialize(sounds: &[Sound]) -> String {
    sounds
        .iter()
        .filter(|s| s.key != 0 || s.volume != 100 || s.played != 0)
        .map(|s| format!("{}:{}:{}:{}", s.key, s.volume, s.played, s.name))
        .collect::<Vec<_>>()
        .join("\t")
}

/// Decode a clip to 48 kHz mono, clamped to [-1, 1] and at most [`MAX_SECONDS`].
pub fn decode(path: &Path) -> Result<Vec<f32>, String> {
    decode_limited(path, MAX_SECONDS)
}

/// Same decoder with a smaller bound for sequencer samples.
pub fn decode_limited(path: &Path, seconds: usize) -> Result<Vec<f32>, String> {
    use symphonia::core::{
        codecs::audio::AudioDecoderOptions, errors::Error, formats::TrackType,
        formats::probe::Hint, io::MediaSourceStream,
    };
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, Default::default(), Default::default())
        .map_err(|e| format!("Формат не распознан: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("В файле нет аудиодорожки")?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("Кодек не поддерживается")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| format!("Кодек не поддерживается: {e}"))?;
    let mut interleaved = Vec::new();
    let mut mono = Vec::new();
    let mut rate = 0;
    let limit = seconds.clamp(1, MAX_SECONDS) * RATE as usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(Error::DecodeError(_)) => continue, // one bad frame must not lose the clip
            Err(e) => return Err(e.to_string()),
        };
        let channels = audio.spec().channels().count().max(1);
        rate = audio.spec().rate();
        interleaved.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut interleaved);
        for frame in interleaved.chunks_exact(channels) {
            mono.push(frame.iter().sum::<f32>() / channels as f32);
        }
        // Stop reading once even the resampled result would exceed the cap.
        if rate > 0 && mono.len() as u64 * RATE as u64 / rate as u64 > limit as u64 {
            break;
        }
    }
    if mono.is_empty() || rate == 0 {
        return Err("Пустой файл".into());
    }
    let mut samples = resample(&mono, rate, RATE);
    samples.truncate(limit);
    for v in &mut samples {
        *v = if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
    }
    Ok(samples)
}

/// Default loudness window, LUFS of the mono clip as played (a stereo source reads ~3 dB below
/// its stereo BS.1770 figure; the window is tuned to this measure).
pub const NORM_MIN: i32 = -18;
pub const NORM_MAX: i32 = -12;
/// Peak ceiling a boost may reach, and the largest boost: a near-silent clip is mostly noise.
const CEILING_DB: f32 = -1.0;
const MAX_BOOST_DB: f32 = 12.0;

/// Integrated loudness (ITU-R BS.1770-4, one channel, 48 kHz) in LUFS; `None` for silence.
pub fn loudness(samples: &[f32]) -> Option<f32> {
    // K-weighting: high-shelf then high-pass, 48 kHz coefficients from the standard. f64: the
    // high-pass pole sits at 0.99 where f32 state noise would bias the result.
    const STAGES: [([f64; 3], [f64; 2]); 2] = [
        (
            [1.53512485958697, -2.69169618940638, 1.19839281085285],
            [-1.69065929318241, 0.73248077421585],
        ),
        ([1.0, -2.0, 1.0], [-1.99004745483398, 0.99007225036621]),
    ];
    const STEP: usize = RATE as usize / 10; // 100 ms; a gating block is 4 steps
    let mut state = [[0f64; 2]; 2];
    let mut steps = Vec::with_capacity(samples.len() / STEP + 1);
    let mut energy = 0f64;
    for (i, &x) in samples.iter().enumerate() {
        let mut v = x as f64;
        for ((b, a), s) in STAGES.iter().zip(&mut state) {
            // Transposed direct form II.
            let y = b[0] * v + s[0];
            s[0] = b[1] * v - a[0] * y + s[1];
            s[1] = b[2] * v - a[1] * y;
            v = y;
        }
        energy += v * v;
        if (i + 1) % STEP == 0 {
            steps.push(energy);
            energy = 0.0;
        }
    }
    let blocks: Vec<f64> = if steps.len() < 4 {
        // Shorter than one 400 ms block: the whole clip is the block.
        if samples.is_empty() {
            return None;
        }
        vec![(steps.iter().sum::<f64>() + energy) / samples.len() as f64]
    } else {
        steps
            .windows(4)
            .map(|w| w.iter().sum::<f64>() / (4 * STEP) as f64)
            .collect()
    };
    let lufs = |power: f64| -0.691 + 10.0 * power.log10();
    let gated_mean = |threshold: f64| {
        let kept: Vec<f64> = blocks.iter().copied().filter(|&p| lufs(p) > threshold).collect();
        (!kept.is_empty()).then(|| kept.iter().sum::<f64>() / kept.len() as f64)
    };
    let relative = lufs(gated_mean(-70.0)?) - 10.0;
    gated_mean(relative.max(-70.0)).map(|p| lufs(p) as f32)
}

/// Scale a clip into the `[min, max]` LUFS window; returns the applied gain in dB. Cuts are
/// unlimited; a boost stops at [`MAX_BOOST_DB`] and at a [`CEILING_DB`] sample peak.
/// ponytail: sample-peak ceiling, no limiter; a quiet clip with sharp peaks may stay below `min`.
pub fn normalize(samples: &mut [f32], min: f32, max: f32) -> f32 {
    let Some(level) = loudness(samples) else {
        return 0.0;
    };
    let mut db = level.clamp(min, max) - level;
    if db > 0.0 {
        let peak = samples.iter().fold(0f32, |m, v| m.max(v.abs()));
        let headroom = if peak > 0.0 { CEILING_DB - 20.0 * peak.log10() } else { 0.0 };
        db = db.min(MAX_BOOST_DB).min(headroom).max(0.0);
    }
    if db.abs() < 0.05 {
        return 0.0;
    }
    let gain = 10f32.powf(db / 20.0);
    for v in samples.iter_mut() {
        *v *= gain;
    }
    db
}

/// `[soundpad] normalize`: on unless set to 0.
pub fn normalize_enabled(settings: &Settings) -> bool {
    settings.number("soundpad", "normalize", 1, 0, 1) != 0
}
/// The loudness window from `[soundpad] norm_min / norm_max`; an inverted pair falls back to the
/// defaults.
pub fn window(settings: &Settings) -> (f32, f32) {
    let min = settings.number("soundpad", "norm_min", NORM_MIN, -40, -5);
    let max = settings.number("soundpad", "norm_max", NORM_MAX, -40, -5);
    let (min, max) = if min <= max { (min, max) } else { (NORM_MIN, NORM_MAX) };
    (min as f32, max as f32)
}

/// Linear interpolation. ponytail: fine for memes; use a windowed sinc if bright clips alias.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let count = (input.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..count)
        .map(|i| {
            let at = i as f64 * step;
            let index = at as usize;
            let next = input[(index + 1).min(input.len() - 1)];
            let frac = (at - index as f64) as f32;
            input[index.min(input.len() - 1)] * (1.0 - frac) + next * frac
        })
        .collect()
}

/// Write 48 kHz mono samples as 16-bit PCM: what every player and [`decode`] accept.
pub fn write_wav(path: &Path, samples: &[f32]) -> Result<(), String> {
    let data = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend(b"RIFF");
    bytes.extend((36 + data).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend([1u16.to_le_bytes(), 1u16.to_le_bytes()].concat()); // PCM, mono
    bytes.extend(RATE.to_le_bytes());
    bytes.extend((RATE * 2).to_le_bytes());
    bytes.extend([2u16.to_le_bytes(), 16u16.to_le_bytes()].concat()); // block align, bits
    bytes.extend(b"data");
    bytes.extend(data.to_le_bytes());
    for v in samples {
        let value = if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
        bytes.extend(((value * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Copy picked files into the library folder; a name that already exists is skipped.
pub fn import(folder: &Path, files: &[PathBuf]) -> Result<(usize, usize), String> {
    let (mut copied, mut skipped) = (0, 0);
    for file in files {
        let Some(name) = file.file_name() else {
            skipped += 1;
            continue;
        };
        let target = folder.join(name);
        if target.exists() || !file.is_file() {
            skipped += 1;
            continue;
        }
        std::fs::copy(file, &target).map_err(|e| format!("{}: {e}", name.to_string_lossy()))?;
        copied += 1;
    }
    Ok((copied, skipped))
}

#[cfg(test)]
pub(crate) fn test_wav(path: &Path, rate: u32, channels: u16, frames: usize) {
    let mut bytes = Vec::new();
    let data_len = (frames * channels as usize * 2) as u32;
    bytes.extend(b"RIFF");
    bytes.extend((36 + data_len).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(channels.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * channels as u32 * 2).to_le_bytes());
    bytes.extend((channels * 2).to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(data_len.to_le_bytes());
    for i in 0..frames {
        let value = ((i as f32 * 0.05).sin() * 16000.0) as i16;
        for _ in 0..channels {
            bytes.extend(value.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_decode_and_persist() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(".tmp/soundpad-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_wav(&dir.join("Beta.wav"), 44_100, 2, 44_100);
        test_wav(&dir.join("alpha.WAV"), 48_000, 1, 480);
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let settings = Settings::for_test(
            "[soundpad]\nsounds=375:150:Beta.wav\t0:100:1700000000:alpha.WAV\t9999:1:x\t5:300:y\t:bad",
        );
        let saved = entries(&settings);
        assert_eq!(
            saved,
            vec![
                Entry { name: "Beta.wav".into(), key: 375, volume: 150, played: 0 },
                Entry { name: "alpha.WAV".into(), key: 0, volume: 100, played: 1_700_000_000 },
            ]
        );
        let sounds = scan(&dir, &saved).unwrap();
        assert_eq!(
            sounds.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["alpha.WAV", "Beta.wav"]
        );
        assert_eq!((sounds[1].key, sounds[1].volume), (375, 150));
        assert_eq!((sounds[0].key, sounds[0].volume, sounds[0].played), (0, 100, 1_700_000_000));
        assert!(sounds[0].modified > 1_600_000_000);
        assert_eq!(serialize(&sounds), "0:100:1700000000:alpha.WAV\t375:150:0:Beta.wav");
        assert_eq!(order(&sounds, Sort::Name), [0, 1]);
        assert_eq!(order(&sounds, Sort::Hotkey), [1, 0]);
        assert_eq!(order(&sounds, Sort::Recent), [0, 1]);
        assert_eq!(Sort::from_code(Sort::Recent.code()), Sort::Recent);
        assert_eq!(Sort::from_code(99), Sort::Name);
        assert_eq!(prefix("Danya - hehe.mp3"), Some(("Danya".into(), "danya".into())));
        assert_eq!(prefix("plain.mp3"), None);
        assert_eq!(prefix(" - x.mp3"), None);
        let named = |names: &[&str]| -> Vec<Sound> {
            names
                .iter()
                .map(|n| Sound {
                    name: n.to_string(),
                    path: PathBuf::new(),
                    key: 0,
                    volume: 100,
                    played: 0,
                    modified: 0,
                    state: State::Unloaded,
                })
                .collect()
        };
        let library = named(&["danya - a.mp3", "Danya - b.wav", "des - c.mp3", "solo.mp3", "gerych - d.mp3", "gerych - e.mp3", "gerych - f.mp3"]);
        let found = groups(&library);
        assert_eq!(
            found.iter().map(|g| (g.label.as_str(), g.count)).collect::<Vec<_>>(),
            [("gerych", 3), ("danya", 2)]
        );
        let sections = sections(&Settings::for_test(
            "[soundpad]\nsections=Мемы:danya - a.mp3|solo.mp3\tМемы:x\t:y\tПустой:\tA|B:z.mp3",
        ));
        assert_eq!(
            sections,
            vec![
                Section { name: "Мемы".into(), files: vec!["danya - a.mp3".into(), "solo.mp3".into()] },
                Section { name: "Пустой".into(), files: vec![] },
                Section { name: "AB".into(), files: vec!["z.mp3".into()] },
            ]
        );
        assert_eq!(
            serialize_sections(&sections),
            "Мемы:danya - a.mp3|solo.mp3\tПустой:\tAB:z.mp3"
        );
        assert_eq!(section_name("  a:b|c\td  "), "abcd");
        let beta = decode(&sounds[1].path).unwrap();
        assert_eq!(beta.len(), 48_000); // 1 s at 44.1 kHz stereo -> 1 s at 48 kHz mono
        assert!(beta.iter().all(|v| v.abs() <= 1.0));
        assert!(beta.iter().any(|v| v.abs() > 0.3));
        let alpha = decode(&sounds[0].path).unwrap();
        assert_eq!(alpha.len(), 480);
        assert!(decode(&dir.join("notes.txt")).is_err());
        assert_eq!(resample(&[0.0, 1.0], 1, 2), vec![0.0, 0.5, 1.0, 1.0]);
        let source = dir.join("Beta.wav");
        let library = dir.join("library");
        std::fs::create_dir_all(&library).unwrap();
        assert_eq!(import(&library, std::slice::from_ref(&source)).unwrap(), (1, 0));
        assert_eq!(import(&library, &[source, dir.join("missing.mp3")]).unwrap(), (0, 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn normalize_window() {
        let sine = |amplitude: f32, seconds: f32| -> Vec<f32> {
            (0..(seconds * RATE as f32) as usize)
                .map(|i| (i as f32 * 997.0 * std::f32::consts::TAU / RATE as f32).sin() * amplitude)
                .collect()
        };
        let near = |value: f32, expected: f32, tolerance: f32| {
            assert!((value - expected).abs() <= tolerance, "{value} is not {expected} ± {tolerance}");
        };
        // BS.1770 reference: a full-scale 997 Hz sine in one channel reads -3.01 LUFS.
        near(loudness(&sine(1.0, 3.0)).unwrap(), -3.01, 0.1);
        let mut loud = sine(1.0, 3.0);
        near(normalize(&mut loud, -18.0, -12.0), -9.0, 0.1);
        near(loudness(&loud).unwrap(), -12.0, 0.1);
        let inside = sine(0.3, 3.0);
        let mut untouched = inside.clone();
        assert_eq!(normalize(&mut untouched, -18.0, -12.0), 0.0);
        assert_eq!(untouched, inside);
        let mut quiet = sine(0.01, 3.0);
        assert_eq!(normalize(&mut quiet, -18.0, -12.0), MAX_BOOST_DB);
        let mut peaky = sine(0.02, 3.0);
        peaky[RATE as usize] = 0.8;
        near(normalize(&mut peaky, -18.0, -12.0), CEILING_DB - 20.0 * 0.8f32.log10(), 0.01);
        assert!(peaky.iter().all(|v| v.abs() <= 10f32.powf(CEILING_DB / 20.0) + 1e-4));
        assert!(loudness(&peaky).unwrap() < -18.0);
        let mut silence = vec![0.0; RATE as usize];
        assert_eq!(loudness(&silence), None);
        assert_eq!(normalize(&mut silence, -18.0, -12.0), 0.0);
        assert!(silence.iter().all(|v| *v == 0.0));
        assert_eq!(loudness(&[]), None);
        assert_eq!(normalize(&mut [], -18.0, -12.0), 0.0);
        near(loudness(&sine(0.5, 0.1)).unwrap(), -9.0, 0.5);
        assert!(normalize_enabled(&Settings::for_test("")));
        assert!(!normalize_enabled(&Settings::for_test("[soundpad]\nnormalize=0")));
        assert!(normalize_enabled(&Settings::for_test("[soundpad]\nnormalize=7")));
        assert_eq!(window(&Settings::for_test("")), (-18.0, -12.0));
        assert_eq!(
            window(&Settings::for_test("[soundpad]\nnorm_min=-10\nnorm_max=-20")),
            (-18.0, -12.0)
        );
        assert_eq!(
            window(&Settings::for_test("[soundpad]\nnorm_min=-99\nnorm_max=-8")),
            (-18.0, -8.0)
        );
    }
}
