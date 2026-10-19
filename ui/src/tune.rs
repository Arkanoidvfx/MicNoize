//! Picks the noise suppression strength for the current microphone from what it hears.
//!
//! Silence: strengths are swept from weak to strong while the room is quiet. The residual noise
//! after the denoiser is measured from the engine's own audio (the sound trace) at each step, and
//! the room's noise is described from the raw input: level, mains hum, hiss. Talking through a step
//! repeats that step instead of failing the whole tune.
//!
//! Voice: the user says a short microphone check while the weakest silencing strength and the step below it run in
//! turn. Before/after audio, aligned by the denoiser's delay, shows whether the voice loses level,
//! loses its sibilants, or gains broadband hash or 10 ms block pulsing. A damaging strength is backed
//! off. The noise gate is never changed here: the result only recommends a threshold.
//!
//! Meter peaks (one per UI tick) drive the phases and the live hints; audio is requested at the
//! end of each measured window.
use realfft::RealFftPlanner;
use std::time::{Duration, Instant};

/// Swept strengths, percent. Above 100 % is the experimental zone: never chosen automatically.
pub const STEPS: [u8; 10] = [0, 10, 20, 30, 40, 50, 60, 70, 85, 100];
/// Each strength runs this long; the first part lets the denoiser settle and is not measured.
const STEP: Duration = Duration::from_millis(380);
const SETTLE: Duration = Duration::from_millis(130);
/// Each voice candidate runs this long, with the same settle time.
const CANDIDATE: Duration = Duration::from_millis(2600);
/// Residual noise peaks at or under this are inaudible (the «После» meter starts at −72 dB).
pub const SILENT_DB: f32 = -70.0;
/// The weakest strength chosen: even a clean microphone keeps a little suppression.
const FLOOR: u8 = 10;
/// Speech frames are this far above the room's noise.
const SPEECH_OVER_NOISE_DB: f32 = 15.0;
/// What counts as damage to the voice at a strength.
const VOICE_DROP_DB: f32 = 6.0;
const SIBILANT_LOSS_DB: f32 = 6.0;
const HASH_ADDED_DB: f32 = 3.0;
const PULSE_ADDED: f32 = 0.5;
/// A step is repeated at most this many times before the tune gives up on silence.
const MAX_REPEATS: u32 = 10;
/// What to say: the usual microphone check; «связи» brings a few sibilants for the 4–8 kHz check.
pub const PHRASE: &str = "Раз, два, три, проверка связи";

const RATE: usize = 48_000;
const BLOCK: usize = 480;
fn samples(d: Duration) -> usize {
    (d.as_millis() as usize * RATE / 1000) / BLOCK * BLOCK
}
pub fn db(peak: f32) -> f32 {
    20.0 * peak.max(1e-6).log10()
}
fn power_db(power: f64) -> f32 {
    (10.0 * power.max(1e-14).log10()) as f32
}
fn median(values: &mut [f32]) -> f32 {
    percentile(values, 0.5)
}
fn percentile(values: &mut [f32], at: f32) -> f32 {
    if values.is_empty() {
        return -120.0;
    }
    values.sort_by(f32::total_cmp);
    values[((values.len() - 1) as f32 * at).round() as usize]
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Quiet,
    Voice,
    Done,
    Failed(String),
}

/// Audio wanted from the engine: the newest `samples` before/after the denoiser, measured while
/// `strength` ran.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Need {
    pub samples: usize,
    pub strength: u8,
}

/// What one voice candidate did to the voice.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Voice {
    pub strength: u8,
    /// Speech frames heard; under a quarter second the other numbers mean nothing.
    pub frames: usize,
    /// How much lower the voice came out, dB.
    pub drop: f32,
    /// Extra loss of the 4–8 kHz band beyond `drop`, dB.
    pub sibilants: f32,
    /// Broadband hash added by the denoiser, dB.
    pub hash: f32,
    /// 10 ms block pulsing added by the denoiser.
    pub pulse: f32,
    /// Speech level at the microphone, dBFS RMS.
    pub level: f32,
    /// Loudest input sample, dBFS.
    pub peak: f32,
    /// Quiet syllables after the denoiser: 10th percentile of speech frame peaks, dB.
    pub quiet_syllables: f32,
}
impl Voice {
    pub fn heard(&self) -> bool {
        self.frames >= 25
    }
    pub fn damaged(&self) -> bool {
        self.heard()
            && (self.drop > VOICE_DROP_DB || self.sibilants > SIBILANT_LOSS_DB || self.hash > HASH_ADDED_DB || self.pulse > PULSE_ADDED)
    }
    fn harm(&self) -> f32 {
        self.drop.max(0.0) + self.sibilants.max(0.0) + self.hash.max(0.0) + self.pulse.max(0.0) * 6.0
    }
}

pub struct Tune {
    pub phase: Phase,
    pub phase_started: Instant,
    /// The strength before, for «Вернуть».
    pub previous: f32,
    step: usize,
    step_started: Instant,
    /// Talking was heard in the running step: it will be repeated.
    dirty: bool,
    quiet_peaks: Vec<f32>,
    step_peaks: Vec<f32>,
    /// How many steps were repeated because of sound in the silence.
    pub repeats: u32,
    /// When sound was last heard during the silence, for the «тише» hint.
    pub heard_at: Option<Instant>,
    /// Input peak of the last tick, dB, for the live level.
    pub level: f32,
    /// (strength, median residual noise peak in dB) per measured step.
    pub curve: Vec<(u8, f32)>,
    noise_power: Vec<f64>,
    noise_frames: usize,
    noise_rms: Vec<f32>,
    /// 10 ms frame peaks of the raw room, dB.
    noise_peaks: Vec<f32>,
    /// The room's noise at the microphone, dBFS RMS (median 10 ms frame).
    pub noise_db: f32,
    /// What the room's noise sounds like, for the result card.
    pub kinds: Vec<&'static str>,
    candidates: Vec<u8>,
    candidate: usize,
    /// One entry per voice candidate, in the order they ran.
    pub voices: Vec<Voice>,
    pub chosen: u8,
    /// The voice check lowered the strength one step.
    pub backed_off: bool,
    /// Recommended noise gate threshold, dB; None when the gate would not help.
    pub gate: Option<f32>,
}

impl Tune {
    pub fn new(previous: f32, now: Instant) -> Self {
        Tune {
            phase: Phase::Quiet,
            phase_started: now,
            previous,
            step: 0,
            step_started: now,
            dirty: false,
            quiet_peaks: Vec::new(),
            step_peaks: Vec::new(),
            repeats: 0,
            heard_at: None,
            level: -120.0,
            curve: Vec::new(),
            noise_power: Vec::new(),
            noise_frames: 0,
            noise_rms: Vec::new(),
            noise_peaks: Vec::new(),
            noise_db: -120.0,
            kinds: Vec::new(),
            candidates: Vec::new(),
            candidate: 0,
            voices: Vec::new(),
            chosen: FLOOR,
            backed_off: false,
            gate: None,
        }
    }
    /// The strength the engine should run now, percent.
    pub fn strength(&self) -> u8 {
        match self.phase {
            Phase::Quiet => STEPS[self.step],
            Phase::Voice => self.candidates[self.candidate],
            _ => self.chosen,
        }
    }
    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Quiet | Phase::Voice)
    }
    /// Seconds left in the running phase.
    pub fn left(&self, now: Instant) -> u64 {
        let total = match self.phase {
            Phase::Quiet => STEP * (STEPS.len() - self.step) as u32,
            Phase::Voice => CANDIDATE * (self.candidates.len() - self.candidate) as u32,
            _ => return 0,
        };
        let started = if self.phase == Phase::Quiet { self.step_started } else { self.phase_started + CANDIDATE * self.candidate as u32 };
        total.saturating_sub(now.saturating_duration_since(started)).as_secs_f32().ceil() as u64
    }
    /// Sound was heard in the silence within the last second.
    pub fn hearing_sound(&self, now: Instant) -> bool {
        self.phase == Phase::Quiet && self.heard_at.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
    }
    /// The voice is loud enough to measure right now.
    pub fn hearing_voice(&self) -> bool {
        self.phase == Phase::Voice && self.level > self.noise_peak() + SPEECH_OVER_NOISE_DB
    }
    fn noise_peak(&self) -> f32 {
        median(&mut self.quiet_peaks.clone()).max(-90.0)
    }

    /// Takes one pair of meter peaks (before and after the denoiser). Returns the audio a
    /// finished measuring window needs; pass it to `analyze`.
    pub fn feed(&mut self, now: Instant, input: f32, _output: f32) -> Option<Need> {
        self.level = db(input);
        match self.phase {
            Phase::Quiet => {
                let into = now.saturating_duration_since(self.step_started);
                // Sound in the silence: a word, a cough, a keyboard. Loud on its own, or well above
                // the room's own peaks once they are known.
                let mut known = self.quiet_peaks.clone();
                known.extend(&self.step_peaks);
                let baseline = (known.len() >= 4).then(|| percentile(&mut known, 0.3));
                if self.level > -35.0 || baseline.is_some_and(|b| self.level > b + 15.0 && self.level > -55.0) {
                    self.dirty = true;
                    self.heard_at = Some(now);
                } else if into >= SETTLE {
                    self.step_peaks.push(self.level);
                }
                if into < STEP {
                    return None;
                }
                let strength = STEPS[self.step];
                self.step_started = now;
                if std::mem::take(&mut self.dirty) {
                    self.step_peaks.clear();
                    self.repeats += 1;
                    if self.repeats > MAX_REPEATS {
                        self.phase = Phase::Failed("Не получилось поймать тишину: вокруг слышны голос или звуки. Помолчите и попробуйте ещё раз.".into());
                    }
                    return None;
                }
                self.quiet_peaks.append(&mut self.step_peaks);
                Some(Need { samples: samples(STEP - SETTLE), strength })
            }
            Phase::Voice => {
                let into = now.saturating_duration_since(self.phase_started + CANDIDATE * self.candidate as u32);
                (into >= CANDIDATE).then(|| Need { samples: samples(CANDIDATE - SETTLE), strength: self.candidates[self.candidate] })
            }
            _ => None,
        }
    }

    /// Audio for the window `feed` asked for: before and after the denoiser on one timeline,
    /// `processed` lagging `input` by `delay` samples.
    pub fn analyze(&mut self, now: Instant, need: Need, input: &[f32], processed: &[f32], delay: usize) {
        match self.phase {
            Phase::Quiet => {
                // The residual: what is left of the room after the denoiser.
                let mut peaks: Vec<f32> = processed.chunks_exact(BLOCK).map(|f| db(peak(f))).collect();
                self.curve.push((need.strength, median(&mut peaks)));
                self.noise_rms.extend(input.chunks_exact(BLOCK).map(|f| power_db(mean_square(f))));
                self.noise_peaks.extend(input.chunks_exact(BLOCK).map(|f| db(peak(f))));
                accumulate(&mut self.noise_power, &mut self.noise_frames, input);
                self.step += 1;
                if self.step == STEPS.len() {
                    self.finish_quiet(now);
                }
            }
            Phase::Voice => {
                let mut voice = measure_voice(input, processed, delay, self.noise_db);
                voice.strength = need.strength;
                self.voices.push(voice);
                self.candidate += 1;
                if self.candidate == self.candidates.len() {
                    self.finish_voice(now);
                }
            }
            _ => {}
        }
    }

    fn finish_quiet(&mut self, now: Instant) {
        self.noise_db = median(&mut self.noise_rms.clone());
        self.kinds = describe_noise(&self.noise_power, median(&mut self.noise_peaks.clone()));
        self.chosen = self
            .curve
            .iter()
            .find(|&&(strength, residual)| strength >= FLOOR && residual <= SILENT_DB)
            .map_or(100, |&(strength, _)| strength);
        // The weakest silencing strength first, then the step below it as the fallback.
        self.candidates = vec![self.chosen];
        let at = STEPS.iter().position(|&s| s == self.chosen).unwrap_or(1);
        if at > 0 && STEPS[at - 1] >= FLOOR {
            self.candidates.push(STEPS[at - 1]);
        }
        self.candidate = 0;
        self.phase = Phase::Voice;
        self.phase_started = now;
    }
    fn finish_voice(&mut self, now: Instant) {
        let chosen = self.voices.iter().find(|v| v.strength == self.chosen).copied().unwrap_or_default();
        if chosen.damaged()
            && let Some(weaker) = self.voices.iter().find(|v| v.strength < self.chosen && v.heard() && v.harm() < chosen.harm())
        {
            self.chosen = weaker.strength;
            self.backed_off = true;
        }
        // A gate helps only when the residual noise sits well below the quietest syllables:
        // the threshold goes just above the noise, still under the speech.
        let final_voice = self.voice();
        self.gate = match (self.chosen_residual(), final_voice.filter(Voice::heard)) {
            (Some(residual), Some(v)) if v.quiet_syllables - residual >= 12.0 => {
                Some((residual + 6.0).min(v.quiet_syllables - 6.0).clamp(-71.0, -20.0).round())
            }
            _ => None,
        };
        self.phase = Phase::Done;
        self.phase_started = now;
    }
    /// Residual noise measured at the chosen strength.
    pub fn chosen_residual(&self) -> Option<f32> {
        self.curve.iter().find(|&&(s, _)| s == self.chosen).map(|&(_, r)| r)
    }
    /// The voice measured at the chosen strength.
    pub fn voice(&self) -> Option<Voice> {
        self.voices.iter().find(|v| v.strength == self.chosen).copied()
    }
    /// Warnings about the microphone itself, from the voice check.
    pub fn warnings(&self) -> Vec<String> {
        let Some(v) = self.voices.iter().find(|v| v.heard()) else { return Vec::new() };
        let mut out = Vec::new();
        if v.peak > -1.0 {
            out.push("Микрофон перегружается на громких словах: убавьте его уровень в Windows.".into());
        } else if v.level < -42.0 {
            out.push("Голос тихий: поднимите уровень микрофона в Windows или говорите ближе.".into());
        }
        if v.level - self.noise_db < SPEECH_OVER_NOISE_DB + 5.0 {
            out.push(format!("Фон всего на {:.0} dB тише голоса: шумодаву трудно их разделить.", v.level - self.noise_db));
        }
        out
    }
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}
fn mean_square(x: &[f32]) -> f64 {
    x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len().max(1) as f64
}
/// First-difference energy against signal energy: grows with broadband hash on the same voice.
fn tilt(x: &[f32]) -> (f64, f64) {
    let (mut xx, mut dd) = (0.0, 0.0);
    for w in x.windows(2) {
        xx += f64::from(w[1]) * f64::from(w[1]);
        dd += f64::from(w[1] - w[0]).powi(2);
    }
    (dd, xx)
}
/// The strongest 10 ms block phase of first-difference energy against the median phase. `x` starts
/// on a block boundary of the engine.
fn pulse(x: &[f32]) -> f32 {
    let mut phase = [0.0f64; BLOCK];
    for (i, w) in x.windows(2).enumerate() {
        phase[(i + 1) % BLOCK] += f64::from(w[1] - w[0]).powi(2);
    }
    let strongest = phase.iter().copied().fold(0.0, f64::max);
    let mut sorted = phase.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[BLOCK / 2];
    if median > 0.0 { (strongest / median) as f32 } else { 0.0 }
}

const NOISE_FFT: usize = 4800; // 10 Hz bins
fn accumulate(power: &mut Vec<f64>, frames: &mut usize, x: &[f32]) {
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(NOISE_FFT);
    if power.is_empty() {
        *power = vec![0.0; NOISE_FFT / 2 + 1];
    }
    let mut spectrum = fft.make_output_vec();
    for start in (0..x.len().saturating_sub(NOISE_FFT - 1)).step_by(NOISE_FFT / 2) {
        let mut frame: Vec<f32> = x[start..start + NOISE_FFT].iter().enumerate().map(|(i, &v)| v * hann(i, NOISE_FFT)).collect();
        if fft.process(&mut frame, &mut spectrum).is_ok() {
            for (p, c) in power.iter_mut().zip(&spectrum) {
                *p += f64::from(c.norm_sqr());
            }
            *frames += 1;
        }
    }
}
fn hann(i: usize, n: usize) -> f32 {
    0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()
}
/// Mains hum (50/60 Hz and harmonics standing out), hiss (most energy above 4 kHz) or a broad
/// room/fan noise; nothing when the room is already under the meter.
fn describe_noise(power: &[f64], noise_peak: f32) -> Vec<&'static str> {
    if power.is_empty() || noise_peak <= SILENT_DB {
        return vec!["тихо"];
    }
    let bin = |hz: f64| (hz * NOISE_FFT as f64 / RATE as f64).round() as usize;
    let band = |lo: f64, hi: f64| power[bin(lo)..bin(hi).min(power.len())].iter().sum::<f64>();
    let hum = |f0: f64| {
        (1..=3)
            .map(|k| {
                let at = bin(f0 * k as f64);
                let line = power[at.saturating_sub(1)..=at + 1].iter().copied().fold(0.0, f64::max);
                let mut around: Vec<f64> = (bin(f0 * k as f64 - 40.0)..bin(f0 * k as f64 - 15.0))
                    .chain(bin(f0 * k as f64 + 15.0)..bin(f0 * k as f64 + 40.0))
                    .map(|i| power[i])
                    .collect();
                around.sort_by(f64::total_cmp);
                10.0 * (line / around[around.len() / 2].max(1e-20)).log10()
            })
            .sum::<f64>()
            / 3.0
    };
    let mut kinds = Vec::new();
    if hum(50.0) > 10.0 {
        kinds.push("гул сети 50 Гц");
    } else if hum(60.0) > 10.0 {
        kinds.push("гул сети 60 Гц");
    }
    let total = band(80.0, 20_000.0).max(1e-20);
    if band(4_000.0, 20_000.0) / total > 0.5 {
        kinds.push("шипение");
    } else if kinds.is_empty() || band(200.0, 4_000.0) / total > 0.5 {
        kinds.push("ровный шум комнаты или вентилятора");
    }
    kinds
}

/// Before/after the denoiser over speech frames. `processed[i + delay]` is `input[i]` processed.
fn measure_voice(input: &[f32], processed: &[f32], delay: usize, noise_db: f32) -> Voice {
    const FRAME: usize = 960;
    let n = input.len().min(processed.len().saturating_sub(delay));
    let (input_aligned, output) = (&input[..n], &processed[delay..delay + n]);
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FRAME);
    let (mut a, mut b) = (fft.make_output_vec(), fft.make_output_vec());
    let bin = |hz: usize| hz * FRAME / RATE;
    let (mut drops, mut quiet) = (Vec::new(), Vec::new());
    let (mut sib_in, mut sib_out, mut all_in, mut all_out) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut hash_in, mut hash_out) = ((0.0, 0.0), (0.0, 0.0));
    let mut levels = Vec::new();
    for start in (0..n.saturating_sub(FRAME - 1)).step_by(FRAME / 2) {
        let (x, y) = (&input_aligned[start..start + FRAME], &output[start..start + FRAME]);
        let level = power_db(mean_square(x));
        if level < noise_db + SPEECH_OVER_NOISE_DB || level < -60.0 {
            continue;
        }
        levels.push(level);
        drops.push(level - power_db(mean_square(y)));
        quiet.push(db(peak(y)));
        let (dx, xx) = tilt(x);
        let (dy, yy) = tilt(y);
        hash_in = (hash_in.0 + dx, hash_in.1 + xx);
        hash_out = (hash_out.0 + dy, hash_out.1 + yy);
        let mut fx: Vec<f32> = x.iter().enumerate().map(|(i, &v)| v * hann(i, FRAME)).collect();
        let mut fy: Vec<f32> = y.iter().enumerate().map(|(i, &v)| v * hann(i, FRAME)).collect();
        if fft.process(&mut fx, &mut a).is_err() || fft.process(&mut fy, &mut b).is_err() {
            continue;
        }
        let energy = |s: &[realfft::num_complex::Complex<f32>], lo: usize, hi: usize| s[bin(lo)..bin(hi)].iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>();
        sib_in += energy(&a, 4_000, 8_000);
        sib_out += energy(&b, 4_000, 8_000);
        all_in += energy(&a, 80, 8_000);
        all_out += energy(&b, 80, 8_000);
    }
    let frames = levels.len();
    let ratio_db = |num: f64, den: f64| power_db(num.max(1e-20) / den.max(1e-20));
    Voice {
        strength: 0,
        frames,
        drop: median(&mut drops),
        sibilants: if frames > 0 { ratio_db(sib_in, sib_out) - ratio_db(all_in, all_out) } else { 0.0 },
        hash: if frames > 0 { ratio_db(hash_out.0, hash_out.1) - ratio_db(hash_in.0, hash_in.1) } else { 0.0 },
        // The processed copy starts on an engine block boundary.
        pulse: pulse(processed) - pulse(input),
        level: median(&mut levels),
        peak: db(peak(input)),
        quiet_syllables: percentile(&mut quiet, 0.1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const DELAY: usize = 1440;
    struct Room {
        seed: u32,
        last: f32,
    }
    impl Room {
        fn white(&mut self) -> f32 {
            self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.seed >> 8) as f32 / 16_777_216.0 - 0.5
        }
        /// White noise differentiated: most of its energy sits high, like hiss.
        fn hiss(&mut self) -> f32 {
            let now = self.white();
            std::mem::replace(&mut self.last, now) - now
        }
    }
    /// A pretend voice: 150 Hz harmonics with a syllable envelope plus a hiss of sibilants.
    fn voice(t: usize, room: &mut Room) -> f32 {
        let s = t as f32 / RATE as f32;
        let envelope = 0.55 + 0.45 * (TAU * 3.0 * s).sin();
        let tone: f32 = (1..=40).map(|k| (TAU * 150.0 * k as f32 * s).sin() / k as f32).sum();
        envelope * (0.08 * tone + 0.06 * room.white())
    }
    type Denoise = dyn Fn(u8, &[f32], &[f32]) -> Vec<f32>;
    /// Runs a tune against a pretend engine: `noise(t)` is the room, `denoise(strength, noise,
    /// voice)` what the denoiser makes of a window, `talk(step, repeats)` puts a word into a silence step.
    fn run(noise: impl Fn(usize, &mut Room) -> f32, denoise: &Denoise, speak: bool, talk: impl Fn(usize, u32) -> bool) -> Tune {
        let start = Instant::now();
        let mut tune = Tune::new(0.4, start);
        let (mut now, mut clock) = (start, 0usize);
        let mut room = Room { seed: 9, last: 0.0 };
        for _ in 0..4000 {
            if !tune.running() {
                break;
            }
            now += Duration::from_millis(50);
            clock += 2400;
            let quiet = tune.phase == Phase::Quiet;
            let level = if quiet && talk(tune.step, tune.repeats) {
                0.3
            } else if !quiet && speak {
                0.2
            } else {
                noise(clock, &mut room).abs().max(0.001)
            };
            let Some(need) = tune.feed(now, level, level) else { continue };
            let voiced = tune.phase == Phase::Voice && speak;
            // The window starts DELAY earlier: processed[i] is what became of raw[i].
            let first = clock - need.samples - DELAY;
            let room_part: Vec<f32> = (first..clock).map(|t| noise(t, &mut room)).collect();
            let voice_part: Vec<f32> = (first..clock).map(|t| if voiced { voice(t, &mut room) } else { 0.0 }).collect();
            let raw: Vec<f32> = room_part.iter().zip(&voice_part).map(|(a, b)| a + b).collect();
            let processed = denoise(need.strength, &room_part, &voice_part);
            tune.analyze(now, need, &raw[DELAY..], &processed[..need.samples], DELAY);
        }
        tune
    }
    fn white_noise(amp: f32) -> impl Fn(usize, &mut Room) -> f32 {
        move |_, room| room.white() * amp
    }
    /// Keeps `keep(strength)` of the room and the whole voice.
    fn keeps(keep: impl Fn(u8) -> f32 + 'static) -> Box<Denoise> {
        Box::new(move |s, noise, voice| noise.iter().zip(voice).map(|(n, v)| n * keep(s) + v).collect())
    }
    fn never(_: usize, _: u32) -> bool {
        false
    }

    #[test]
    fn the_weakest_strength_that_silences_the_room_wins() {
        // Noise peaks around −46 dB; from 30 % the denoiser takes them under −70 dB.
        let tune = run(white_noise(0.01), &*keeps(|s| if s >= 30 { 0.01 } else { 0.5 }), false, never);
        assert_eq!(tune.phase, Phase::Done);
        assert_eq!(tune.chosen, 30);
        assert_eq!(tune.curve.len(), STEPS.len());
        assert!((-60.0..-45.0).contains(&tune.noise_db), "{}", tune.noise_db);
        assert!(tune.voices.iter().all(|v| !v.heard()));
        assert_eq!(tune.gate, None, "no voice, no gate advice");
    }
    #[test]
    fn a_clean_voice_keeps_the_strength_and_gets_gate_advice() {
        let tune = run(white_noise(0.004), &*keeps(|s| if s >= 20 { 0.02 } else { 0.5 }), true, never);
        assert_eq!((tune.chosen, tune.backed_off), (20, false));
        let v = tune.voice().unwrap();
        assert!(v.heard() && !v.damaged(), "{v:?}");
        let gate = tune.gate.expect("residual far under the syllables");
        assert!(gate > tune.chosen_residual().unwrap() && gate < v.quiet_syllables, "{gate} {v:?}");
        assert!(tune.warnings().is_empty(), "{:?}", tune.warnings());
    }
    #[test]
    fn a_strength_that_eats_the_sibilants_is_backed_off() {
        // From 40 % the room is silent, but that strength also smears away the voice above 4 kHz.
        let denoise = |s: u8, noise: &[f32], voice: &[f32]| -> Vec<f32> {
            (0..noise.len())
                .map(|i| {
                    let v = if s >= 40 { voice[i.saturating_sub(5)..=i].iter().sum::<f32>() / 6.0 } else { voice[i] };
                    noise[i] * if s >= 40 { 0.02 } else { 0.7 } + v
                })
                .collect()
        };
        let tune = run(white_noise(0.004), &denoise, true, never);
        let at_40 = tune.voices.iter().find(|v| v.strength == 40).unwrap();
        assert!(at_40.sibilants > SIBILANT_LOSS_DB && at_40.damaged(), "{at_40:?}");
        assert_eq!((tune.chosen, tune.backed_off), (30, true));
    }
    #[test]
    fn block_pulsing_is_damage() {
        // From 30 % the voice is louder for the first 8 samples of every 10 ms block.
        let denoise = |s: u8, noise: &[f32], voice: &[f32]| -> Vec<f32> {
            (0..noise.len())
                .map(|i| noise[i] * if s >= 30 { 0.02 } else { 0.5 } + voice[i] * if s >= 30 && i % BLOCK < 8 { 3.0 } else { 1.0 })
                .collect()
        };
        let tune = run(white_noise(0.004), &denoise, true, never);
        let at_30 = tune.voices.iter().find(|v| v.strength == 30).unwrap();
        assert!(at_30.pulse > PULSE_ADDED && at_30.damaged(), "{at_30:?}");
        let at_20 = tune.voices.iter().find(|v| v.strength == 20).unwrap();
        assert!(!at_20.damaged(), "{at_20:?}");
        assert_eq!((tune.chosen, tune.backed_off), (20, true));
    }
    #[test]
    fn talking_through_the_silence_repeats_the_step() {
        let tune = run(white_noise(0.01), &*keeps(|s| if s >= 30 { 0.01 } else { 0.5 }), false, |step, repeats| step == 3 && repeats < 2);
        assert_eq!((tune.phase.clone(), tune.repeats, tune.chosen), (Phase::Done, 2, 30));
        assert_eq!(tune.curve.len(), STEPS.len(), "a repeated step is measured once");
        let failed = run(white_noise(0.01), &*keeps(|_| 0.5), false, |_, _| true);
        assert!(matches!(failed.phase, Phase::Failed(_)));
    }
    #[test]
    fn the_room_noise_is_named() {
        let hum = |t: usize, room: &mut Room| {
            let s = t as f32 / RATE as f32;
            0.005 * (TAU * 50.0 * s).sin() + 0.0025 * (TAU * 100.0 * s).sin() + 0.002 * (TAU * 150.0 * s).sin() + 0.0002 * room.white()
        };
        let tune = run(hum, &*keeps(|_| 0.001), false, never);
        assert!(tune.kinds.contains(&"гул сети 50 Гц"), "{:?}", tune.kinds);
        let tune = run(|_, room: &mut Room| 0.004 * room.hiss(), &*keeps(|_| 0.001), false, never);
        assert!(tune.kinds.contains(&"шипение"), "{:?}", tune.kinds);
        let clean = run(white_noise(0.0001), &*keeps(|_| 1.0), false, never);
        assert_eq!(clean.kinds, vec!["тихо"]);
    }
}
