//! Picks the noise suppression strength for the current microphone from what it hears. While
//! the room is quiet, strengths are swept from weak to strong and the residual noise after the
//! denoiser is measured at each: the weakest that silences it wins, since weaker means fewer
//! artefacts in the voice. Then a spoken check: a strength that audibly lowers the voice is
//! backed off one step. Readings are the engine's meter peaks, one per UI tick.
use std::time::{Duration, Instant};

/// Swept strengths, percent. Above 100 % is the experimental zone: never chosen automatically.
pub const STEPS: [u8; 10] = [0, 10, 20, 30, 40, 50, 60, 70, 85, 100];
/// Each strength runs this long; the first part lets the denoiser settle and is not measured.
const STEP: Duration = Duration::from_millis(380);
const SETTLE: Duration = Duration::from_millis(130);
pub const QUIET: Duration = Duration::from_millis(380 * 10);
pub const VOICE: Duration = Duration::from_millis(3500);
/// Residual noise at or under this peak is inaudible (the «После» meter starts at −72 dB).
pub const SILENT_DB: f32 = -70.0;
/// The weakest strength chosen: even a clean microphone keeps a little suppression.
const FLOOR: u8 = 10;
/// A voice this much lower after the denoiser than before it is being eaten.
const VOICE_DROP_DB: f32 = 6.0;
/// Speech frames are this far above the room's noise.
const SPEECH_OVER_NOISE_DB: f32 = 15.0;

pub fn db(peak: f32) -> f32 {
    20.0 * peak.max(1e-6).log10()
}
fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return -120.0;
    }
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Quiet,
    Voice,
    Done,
    Failed(String),
}

pub struct Tune {
    pub phase: Phase,
    pub phase_started: Instant,
    /// The strength before, for «Вернуть».
    pub previous: f32,
    step: usize,
    step_started: Instant,
    residual: Vec<f32>,
    raw: Vec<f32>,
    voice: Vec<(f32, f32)>,
    /// (strength, median residual noise in dB) per measured step.
    pub curve: Vec<(u8, f32)>,
    /// The room's noise at the microphone, dB.
    pub noise_db: f32,
    pub chosen: u8,
    /// How much lower the voice came out, dB; None when no speech was heard.
    pub voice_drop: Option<f32>,
    /// The spoken check lowered the strength one step.
    pub backed_off: bool,
}

impl Tune {
    pub fn new(previous: f32, now: Instant) -> Self {
        Tune {
            phase: Phase::Quiet,
            phase_started: now,
            previous,
            step: 0,
            step_started: now,
            residual: Vec::new(),
            raw: Vec::new(),
            voice: Vec::new(),
            curve: Vec::new(),
            noise_db: -120.0,
            chosen: FLOOR,
            voice_drop: None,
            backed_off: false,
        }
    }
    /// The strength the engine should run now, percent.
    pub fn strength(&self) -> u8 {
        match self.phase {
            Phase::Quiet => STEPS[self.step],
            _ => self.chosen,
        }
    }
    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Quiet | Phase::Voice)
    }
    /// Seconds left in the running phase.
    pub fn left(&self, now: Instant) -> u64 {
        let total = if self.phase == Phase::Quiet { QUIET } else { VOICE };
        total.saturating_sub(now.saturating_duration_since(self.phase_started)).as_secs_f32().ceil() as u64
    }
    /// Takes one pair of meter peaks (before and after the denoiser).
    pub fn feed(&mut self, now: Instant, input: f32, output: f32) {
        match self.phase {
            Phase::Quiet => {
                let into = now.saturating_duration_since(self.step_started);
                if into >= SETTLE {
                    self.residual.push(db(output));
                    self.raw.push(db(input));
                }
                if into >= STEP {
                    let residual = median(&mut self.residual);
                    self.curve.push((STEPS[self.step], residual));
                    self.residual.clear();
                    self.step += 1;
                    self.step_started = now;
                    if self.step == STEPS.len() {
                        self.finish_quiet(now);
                    }
                }
            }
            Phase::Voice => {
                self.voice.push((db(input), db(output)));
                if now.saturating_duration_since(self.phase_started) >= VOICE {
                    self.finish_voice(now);
                }
            }
            _ => {}
        }
    }
    fn finish_quiet(&mut self, now: Instant) {
        // Talking through the silence would measure speech as noise: the result would be wrong.
        let loud = self.raw.iter().filter(|&&v| v > -35.0).count();
        if loud * 3 > self.raw.len() {
            self.phase = Phase::Failed("Во время тишины было громко. Помолчите 4 секунды и попробуйте ещё раз.".into());
            return;
        }
        self.noise_db = median(&mut self.raw.clone());
        self.chosen = self
            .curve
            .iter()
            .find(|&&(strength, residual)| strength >= FLOOR && residual <= SILENT_DB)
            .map_or(100, |&(strength, _)| strength);
        self.phase = Phase::Voice;
        self.phase_started = now;
    }
    fn finish_voice(&mut self, now: Instant) {
        let speech: Vec<f32> = self
            .voice
            .iter()
            .filter(|(input, _)| *input > self.noise_db + SPEECH_OVER_NOISE_DB)
            .map(|(input, output)| input - output)
            .collect();
        if speech.len() >= 6 {
            let drop = median(&mut speech.clone());
            self.voice_drop = Some(drop);
            if drop > VOICE_DROP_DB && self.chosen > FLOOR {
                let at = STEPS.iter().position(|&s| s == self.chosen).unwrap_or(1);
                self.chosen = STEPS[at.saturating_sub(1)].max(FLOOR);
                self.backed_off = true;
            }
        }
        self.phase = Phase::Done;
        self.phase_started = now;
    }
    /// Residual noise measured at the chosen strength.
    pub fn chosen_residual(&self) -> Option<f32> {
        self.curve.iter().find(|&&(s, _)| s == self.chosen).map(|&(_, r)| r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a whole tune against a pretend microphone: `residual(strength)` is the noise left
    /// after the denoiser; `voice` the speech level in and out.
    fn run(noise: f32, residual: impl Fn(u8) -> f32, voice: Option<(f32, f32)>) -> Tune {
        let start = Instant::now();
        let mut tune = Tune::new(0.4, start);
        let mut t = start;
        while tune.running() {
            t += Duration::from_millis(50);
            let (input, output) = match tune.phase {
                Phase::Quiet => (noise, residual(tune.strength())),
                _ => voice.unwrap_or((noise, residual(tune.chosen))),
            };
            tune.feed(t, input, output);
        }
        tune
    }
    fn amp(db: f32) -> f32 {
        // 10^(db/20) for the few levels the tests use.
        match db as i32 {
            -80 => 0.0001,
            -74 => 0.0002,
            -66 => 0.0005,
            -60 => 0.001,
            -50 => 0.00316,
            -20 => 0.1,
            -30 => 0.0316,
            _ => 0.01,
        }
    }

    #[test]
    fn the_weakest_strength_that_silences_the_room_wins() {
        // Noise −50 dB; the denoiser reaches −74 dB from 30 %.
        let tune = run(amp(-50.0), |s| if s >= 30 { amp(-74.0) } else { amp(-60.0) }, Some((amp(-20.0), amp(-20.0))));
        assert_eq!(tune.phase, Phase::Done);
        assert_eq!(tune.chosen, 30);
        assert_eq!(tune.curve.len(), STEPS.len());
        assert!((tune.noise_db + 50.0).abs() < 0.5);
        assert!(tune.voice_drop.is_some_and(|d| d.abs() < 0.5) && !tune.backed_off);
    }
    #[test]
    fn a_clean_microphone_keeps_the_floor_and_a_loud_room_stops_at_100() {
        let clean = run(amp(-80.0), |_| amp(-80.0), None);
        assert_eq!((clean.chosen, clean.voice_drop), (FLOOR, None));
        let loud = run(amp(-40.0), |_| amp(-60.0), None);
        assert_eq!(loud.chosen, 100, "never above 100 % by itself");
    }
    #[test]
    fn a_strength_that_eats_the_voice_is_backed_off() {
        let tune = run(amp(-50.0), |s| if s >= 40 { amp(-74.0) } else { amp(-60.0) }, Some((amp(-20.0), amp(-30.0))));
        assert_eq!((tune.chosen, tune.backed_off), (30, true));
    }
    #[test]
    fn talking_through_the_silence_fails() {
        let tune = run(amp(-20.0), |_| amp(-20.0), None);
        assert!(matches!(tune.phase, Phase::Failed(_)));
    }
}
