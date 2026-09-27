//! Four-bar sample piano roll. Rendering and file work run outside the audio thread.
use crate::{engine, settings::Settings, soundpad};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

pub const TRACK_ID: u32 = 800_000;
pub const BARS: usize = 4;
pub const STEPS: usize = BARS * 16;
pub const FIRST_NOTE: u8 = 36; // C2
pub const NOTES: usize = 48; // C2..B5
pub const ROOT_NOTE: u8 = 60; // C4 keeps the sample's original pitch
pub const ZOOMS: [u8; 5] = [50, 75, 100, 125, 150];
const MAX_EVENTS: usize = 512;
const MAX_SOURCES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub step: u8,
    pub note: u8,
    pub sample: String,
    #[serde(default = "one_step")]
    pub length: u8,
}
fn one_step() -> u8 { 1 }

pub fn frame_at(step: usize, bpm: u32) -> usize {
    (step as f64 * 60.0 * soundpad::RATE as f64 / bpm as f64 / 4.0).round() as usize
}

pub fn load(settings: &Settings) -> (u32, Vec<Event>) {
    let bpm = settings.number("studio", "bpm", 120, 60, 200) as u32;
    let events: Vec<Event> = settings.get("studio", "events")
        .and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    let events = events.into_iter().filter(|e| (e.step as usize) < STEPS
        && (FIRST_NOTE..FIRST_NOTE + NOTES as u8).contains(&e.note)
        && e.length > 0 && e.step as usize + e.length as usize <= STEPS
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
    if let Some(at) = events.iter().position(|e| e.note == note && (e.step as usize..e.step as usize + e.length as usize).contains(&step)) {
        if events[at].sample == sample { events.remove(at); }
        else { events[at].sample = sample.to_owned(); }
    } else if events.len() < MAX_EVENTS {
        events.push(Event { step: step as u8, note, sample: sample.to_owned(), length: 1 });
    }
}

pub fn paint(events: &mut Vec<Event>, from: usize, to: usize, note: u8, sample: &str) {
    if from >= STEPS || to >= STEPS || !(FIRST_NOTE..FIRST_NOTE + NOTES as u8).contains(&note) || !valid_name(sample) { return; }
    let start = from.min(to);
    let end = from.max(to) + 1;
    events.retain(|e| e.note != note || e.step as usize >= end || e.step as usize + e.length as usize <= start);
    if events.len() < MAX_EVENTS {
        events.push(Event { step: start as u8, note, sample: sample.to_owned(), length: (end - start) as u8 });
    }
}

pub fn erase(events: &mut Vec<Event>, step: usize, note: u8) -> bool {
    let before = events.len();
    events.retain(|e| e.note != note || !(e.step as usize..e.step as usize + e.length as usize).contains(&step));
    events.len() != before
}

pub fn clear_bar(events: &mut Vec<Event>, bar: usize) {
    if bar >= BARS { return; }
    let (begin, end) = (bar * 16, (bar + 1) * 16);
    let mut kept = Vec::with_capacity(events.len());
    for event in events.drain(..) {
        let start = event.step as usize;
        let finish = start + event.length as usize;
        if finish <= begin || start >= end { kept.push(event); continue; }
        if start < begin { kept.push(Event { length: (begin - start) as u8, ..event.clone() }); }
        if finish > end { kept.push(Event { step: end as u8, length: (finish - end) as u8, ..event }); }
    }
    *events = kept;
}

pub fn render(folder: &Path, bpm: u32, events: &[Event]) -> Result<Vec<f32>, String> {
    if !(60..=200).contains(&bpm) || events.len() > MAX_EVENTS { return Err("Неверные параметры проекта".into()); }
    let length = frame_at(STEPS, bpm);
    let mut mix = vec![0.0f32; length];
    let mut decoded: HashMap<&str, Vec<f32>> = HashMap::new();
    let mut pitched: HashMap<(&str, u8), Vec<f32>> = HashMap::new();
    let mut pitched_frames = 0;
    for event in events {
        if (event.step as usize) >= STEPS || event.length == 0 || event.step as usize + event.length as usize > STEPS
            || !(FIRST_NOTE..FIRST_NOTE + NOTES as u8).contains(&event.note)
            || !valid_name(&event.sample) { return Err("Повреждена нота проекта".into()); }
        if !decoded.contains_key(event.sample.as_str()) {
            if decoded.len() >= MAX_SOURCES { return Err("В треке может быть не больше 32 разных звуков".into()); }
            decoded.insert(&event.sample, soundpad::decode_limited(&folder.join(&event.sample), 20)?);
        }
        let source = &decoded[event.sample.as_str()];
        if source.is_empty() { return Err(format!("Пустой звук: {}", event.sample)); }
        let start = frame_at(event.step as usize, bpm);
        if event.note != ROOT_NOTE && !pitched.contains_key(&(event.sample.as_str(), event.note)) {
            let scale = 2f32.powf((event.note as f32 - ROOT_NOTE as f32) / 12.0);
            let variant = engine::studio_pitch(source, scale)?;
            // ponytail: keep at most about 64 MiB of variants; recompute uncommon notes if exceeded.
            if pitched_frames + variant.len() > 16_000_000 { pitched.clear(); pitched_frames = 0; }
            pitched_frames += variant.len();
            pitched.insert((event.sample.as_str(), event.note), variant);
        }
        let pcm = if event.note == ROOT_NOTE { source } else { &pitched[&(event.sample.as_str(), event.note)] };
        let sustain = event.length > 1;
        let note_frames = frame_at(event.step as usize + event.length as usize, bpm) - start;
        let period = pcm.len().max(frame_at(1, bpm));
        for (i, out) in mix[start..].iter_mut().take(if sustain { note_frames } else { length - start }).enumerate() {
            if !sustain && i >= pcm.len() { break; }
            let n = if sustain { i % period } else { i };
            if n >= pcm.len() { continue; }
            let envelope = if sustain { ((note_frames - i).min(240) as f32 / 240.0).min(1.0) } else { 1.0 };
            let seam = if sustain {
                let fade = pcm.len().min(480) / 2;
                if fade == 0 { 1.0 } else { ((n.min(pcm.len() - n) as f32) / fade as f32).min(1.0) }
            } else { 1.0 };
            *out += pcm[n] * envelope * seam;
        }
    }
    let peak = mix.iter().fold(0.0f32, |peak, v| peak.max(v.abs()));
    if peak > 0.0 {
        // ponytail: peak normalization is capped at 18 dB; a nearly silent recording stays quiet.
        let gain = (0.85 / peak).min(8.0);
        for v in &mut mix { *v *= gain; }
    }
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
        assert!(audio[0] > 0.1);
        assert_eq!(audio[480], 0.0);
        for step in [4, 8, 12] {
            let start = frame_at(step, 120);
            assert!(audio[start + 100..start + 400].iter().map(|v| v.abs()).sum::<f32>() > 5.0);
            assert_eq!(audio[start + 480], 0.0, "pitch must preserve the one-shot duration");
        }
        toggle(&mut events, 0, 60, "tone.wav");
        toggle(&mut events, 4, 67, "tone.wav");
        toggle(&mut events, 8, 48, "tone.wav");
        toggle(&mut events, 12, 72, "tone.wav");
        assert!(events.is_empty());
        assert!(!valid_name("..\\escape.wav"));
        std::fs::remove_file(folder.join("tone.wav")).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
    #[test]
    fn stretched_notes_and_legacy_settings() {
        let settings = Settings::for_test("[studio]\nevents=[{\"step\":8,\"note\":60,\"sample\":\"tone.wav\"}]");
        let (_, mut events) = load(&settings);
        assert_eq!(events[0].length, 1);
        paint(&mut events, 8, 11, 60, "tone.wav");
        assert_eq!((events[0].step, events[0].length), (8, 4));
        assert!(erase(&mut events, 10, 60) && events.is_empty());
        paint(&mut events, 11, 8, 60, "tone.wav");
        assert_eq!((events[0].step, events[0].length), (8, 4));
        paint(&mut events, 14, 18, 61, "tone.wav");
        clear_bar(&mut events, 1);
        assert!(events.iter().any(|e| e.step == 14 && e.length == 2 && e.note == 61));
        assert!(!events.iter().any(|e| e.step == 16));
        let folder = std::env::current_dir().unwrap().join(".tmp/studio-stretch-test");
        std::fs::create_dir_all(&folder).unwrap();
        soundpad::write_wav(&folder.join("tone.wav"), &[0.25; 480]).unwrap();
        let audio = render(&folder, 120, &events).unwrap();
        assert!(audio[frame_at(10, 120) + 240] > 0.1, "short sample must retrigger on a later grid step");
        assert_eq!(audio[frame_at(10, 120) + 480], 0.0, "short samples must not buzz between steps");
        assert_eq!(audio[frame_at(12, 120)], 0.0);
        std::fs::remove_file(folder.join("tone.wav")).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
}
