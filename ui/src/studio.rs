//! Four-bar sample piano roll. Rendering and file work run outside the audio thread.
use crate::{settings::Settings, soundpad};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

pub const TRACK_ID: u32 = 800_000;
pub const BARS: usize = 4;
pub const STEPS: usize = BARS * 16;
pub const FIRST_NOTE: u8 = 36; // C2
pub const NOTES: usize = 48; // C2..B5
pub const ROOT_NOTE: u8 = 60; // C4 keeps the sample's original pitch
const MAX_EVENTS: usize = 512;
const MAX_SOURCES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub step: u8,
    pub note: u8,
    pub sample: String,
}

pub fn load(settings: &Settings) -> (u32, Vec<Event>) {
    let bpm = settings.number("studio", "bpm", 120, 60, 200) as u32;
    let events: Vec<Event> = settings.get("studio", "events")
        .and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    let events = events.into_iter().filter(|e| (e.step as usize) < STEPS
        && (FIRST_NOTE..FIRST_NOTE + NOTES as u8).contains(&e.note)
        && valid_name(&e.sample)).take(MAX_EVENTS).collect();
    (bpm, events)
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 240 && name != "." && name != ".."
        && !name.contains(['/', '\\', ':', '\n', '\r'])
        && Path::new(name).file_name().is_some_and(|n| n == name)
}

pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[note as usize % 12], note as i16 / 12 - 1)
}

pub fn scan(folder: &Path) -> Vec<String> {
    let mut files: Vec<_> = std::fs::read_dir(folder).ok().into_iter().flatten()
        .filter_map(Result::ok).map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str())
            .is_some_and(|s| soundpad::EXTENSIONS.contains(&s.to_ascii_lowercase().as_str())))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|name| valid_name(name)).collect();
    files.sort_unstable_by_key(|s| s.to_lowercase());
    files
}

pub fn toggle(events: &mut Vec<Event>, step: usize, note: u8, sample: &str) {
    if step >= STEPS || !(FIRST_NOTE..FIRST_NOTE + NOTES as u8).contains(&note) || !valid_name(sample) { return; }
    if let Some(at) = events.iter().position(|e| e.step as usize == step && e.note == note) {
        if events[at].sample == sample { events.remove(at); }
        else { events[at].sample = sample.to_owned(); }
    } else if events.len() < MAX_EVENTS {
        events.push(Event { step: step as u8, note, sample: sample.to_owned() });
    }
}

pub fn render(folder: &Path, bpm: u32, events: &[Event]) -> Result<Vec<f32>, String> {
    if !(60..=200).contains(&bpm) || events.len() > MAX_EVENTS { return Err("Неверные параметры проекта".into()); }
    let step_frames = 60.0 * soundpad::RATE as f64 / bpm as f64 / 4.0;
    let length = (step_frames * STEPS as f64).round() as usize;
    let mut mix = vec![0.0f32; length];
    let mut decoded: HashMap<&str, Vec<f32>> = HashMap::new();
    for event in events {
        if (event.step as usize) >= STEPS || !(FIRST_NOTE..FIRST_NOTE + NOTES as u8).contains(&event.note)
            || !valid_name(&event.sample) { return Err("Повреждена нота проекта".into()); }
        if !decoded.contains_key(event.sample.as_str()) {
            if decoded.len() >= MAX_SOURCES { return Err("В треке может быть не больше 32 разных звуков".into()); }
            decoded.insert(&event.sample, soundpad::decode_limited(&folder.join(&event.sample), 20)?);
        }
        let pcm = &decoded[event.sample.as_str()];
        let start = (event.step as f64 * step_frames).round() as usize;
        let speed = 2f64.powf((event.note as f64 - ROOT_NOTE as f64) / 12.0);
        for (i, out) in mix[start..].iter_mut().enumerate() {
            let at = i as f64 * speed;
            let n = at as usize;
            if n >= pcm.len() { break; }
            let next = pcm.get(n + 1).copied().unwrap_or(pcm[n]);
            *out += pcm[n] + (next - pcm[n]) * (at - n as f64) as f32;
        }
    }
    let peak = mix.iter().fold(0.0f32, |peak, v| peak.max(v.abs()));
    if peak > 0.9 { for v in &mut mix { *v *= 0.9 / peak; } }
    Ok(mix)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn piano_roll_round_trip_and_pitch() {
        let mut events = vec![];
        toggle(&mut events, 0, 60, "tone.wav");
        toggle(&mut events, 4, 67, "tone.wav");
        toggle(&mut events, 8, 48, "tone.wav");
        toggle(&mut events, 12, 72, "tone.wav");
        toggle(&mut events, 12, 96, "tone.wav"); // outside C2..B5
        assert_eq!(events.len(), 4);
        assert_eq!(note_name(36), "C2");
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(83), "B5");
        let folder = std::env::current_dir().unwrap().join(".tmp/studio-test");
        std::fs::create_dir_all(&folder).unwrap();
        soundpad::write_wav(&folder.join("tone.wav"), &[0.5; 480]).unwrap();
        let audio = render(&folder, 120, &events).unwrap();
        assert_eq!(audio.len(), 4 * 16 * 6000);
        assert!((audio[0] - 0.5).abs() < 0.001);
        assert_eq!(audio[480], 0.0);
        assert!(audio[24_000 + 300] > 0.49);
        assert_eq!(audio[24_000 + 400], 0.0);
        assert!(audio[48_000 + 900] > 0.49); // C3: twice the source duration
        assert_eq!(audio[48_000 + 1000], 0.0);
        assert!(audio[72_000 + 200] > 0.49); // C5: half the source duration
        assert_eq!(audio[72_000 + 300], 0.0);
        toggle(&mut events, 0, 60, "tone.wav");
        toggle(&mut events, 4, 67, "tone.wav");
        toggle(&mut events, 8, 48, "tone.wav");
        toggle(&mut events, 12, 72, "tone.wav");
        assert!(events.is_empty());
        assert!(!valid_name("..\\escape.wav"));
        std::fs::remove_file(folder.join("tone.wav")).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
}
