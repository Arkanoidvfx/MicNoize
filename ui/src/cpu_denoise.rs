//! DeepFilterNet 3 on the CPU: the denoiser when NVIDIA cannot run (GTX, AMD, Intel, missing
//! model). The C++ DSP thread calls it through `mnr_set_cpu_denoiser`, so the native checks
//! still link without Rust.
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
};

const HOP: usize = 480;
/// libDF returns zeros for a frame below this mean square *without* advancing its buffers, so
/// the first 30 ms after digital silence (a hardware mute button) replayed audio from before it.
const SILENT: f32 = 1e-7;
/// Its 30 ms delay in frames: after this many silent inputs the output is silent input too.
const DELAY_FRAMES: u32 = 3;

#[repr(C)]
struct Api {
    create: extern "C" fn() -> *mut c_void,
    process: extern "C" fn(*mut c_void, *const f32, *mut f32, f32) -> i32,
    destroy: extern "C" fn(*mut c_void),
}
unsafe extern "C" {
    fn mnr_set_cpu_denoiser(api: *const Api);
}

struct State {
    model: DfTract,
    strength: f32,
    silent: u32,
    padded: [f32; HOP],
}

/// Slider strength (0-2, as for NVIDIA) to DeepFilterNet's attenuation limit. 100 % and above is
/// full suppression. Exactly 0 dB makes the model pass the input through *without* its 30 ms
/// delay, so the floor stays above it: holding a hotkey to 0 % must not shift the timing.
fn attenuation_db(strength: f32) -> f32 {
    if strength >= 1.0 { 100.0 } else { (strength.max(0.0) * 40.0).max(0.5) }
}

/// libDF's defaults gate every frame on its SNR guess: below -10 dB the frame is zeroed, above
/// 20/30 dB decoder stages are skipped (their recurrent state then goes stale). On a noisy USB
/// microphone syllables straddle those lines and the voice chops in and out, so every frame runs
/// the full model and the model's own smooth gains do the suppression.
fn runtime_params() -> RuntimeParams {
    RuntimeParams::default_with_ch(1).with_thresholds(-100.0, 100.0, 100.0)
}

extern "C" fn create() -> *mut c_void {
    let model = catch_unwind(|| DfTract::new(DfParams::default(), &runtime_params()));
    match model {
        Ok(Ok(model)) if model.hop_size == HOP && model.sr == 48_000 => {
            Box::into_raw(Box::new(State { model, strength: f32::NAN, silent: 0, padded: [0.0; HOP] })).cast()
        }
        _ => std::ptr::null_mut(),
    }
}

extern "C" fn process(state: *mut c_void, input: *const f32, output: *mut f32, strength: f32) -> i32 {
    if state.is_null() || input.is_null() || output.is_null() {
        return 0;
    }
    // SAFETY: the engine passes the pointer `create` returned and two 480-sample frames.
    let (state, input, output) = unsafe {
        (
            &mut *state.cast::<State>(),
            std::slice::from_raw_parts(input, HOP),
            std::slice::from_raw_parts_mut(output, HOP),
        )
    };
    if state.strength != strength {
        state.model.set_atten_lim(attenuation_db(strength));
        state.strength = strength;
    }
    let power = input.iter().map(|v| v * v).sum::<f32>() / HOP as f32;
    let input = if power < SILENT * 2.0 {
        // Keep the model moving through silence with a -64 dBFS tone at 24 kHz (mean square
        // 4e-7): inaudible, and dropped below once the delayed output is silence too.
        state.silent = state.silent.saturating_add(1);
        for (i, (padded, v)) in state.padded.iter_mut().zip(input).enumerate() {
            *padded = v + if i % 2 == 0 { 6.3e-4 } else { -6.3e-4 };
        }
        &state.padded[..]
    } else {
        state.silent = 0;
        input
    };
    let (Ok(noisy), Ok(mut enhanced)) = (
        ArrayView2::from_shape((1, HOP), input),
        ArrayViewMut2::from_shape((1, HOP), output),
    ) else {
        return 0;
    };
    // A panic must not unwind into C++.
    let ok = catch_unwind(AssertUnwindSafe(|| state.model.process(noisy, enhanced.view_mut())))
        .is_ok_and(|r| r.is_ok());
    if state.silent > DELAY_FRAMES {
        enhanced.fill(0.0);
    }
    ok as i32
}

extern "C" fn destroy(state: *mut c_void) {
    if !state.is_null() {
        // SAFETY: `state` came from `Box::into_raw` in `create` and is destroyed once.
        drop(unsafe { Box::from_raw(state.cast::<State>()) });
    }
}

static API: Api = Api { create, process, destroy };

pub fn register() {
    unsafe { mnr_set_cpu_denoiser(&API) };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strength_maps_to_a_delay_stable_attenuation() {
        assert_eq!(attenuation_db(1.0), 100.0);
        assert_eq!(attenuation_db(2.0), 100.0);
        assert_eq!(attenuation_db(0.5), 20.0);
        assert_eq!(attenuation_db(0.0), 0.5);
        assert_eq!(attenuation_db(-1.0), 0.5);
    }
    #[test]
    fn every_frame_runs_the_full_model() {
        let model = DfTract::new(DfParams::default(), &runtime_params()).unwrap();
        // libDF clamps its SNR estimate to -15..35 dB.
        for lsnr in -15..=35 {
            assert_eq!(model.apply_stages(lsnr as f32), (true, false, true), "{lsnr} dB");
        }
    }
    #[test]
    fn deepfilternet_runs_480_sample_frames() {
        let state = create();
        assert!(!state.is_null());
        let mut seed = 7u32;
        let input: Vec<f32> = (0..HOP)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 0.1
            })
            .collect();
        let mut output = vec![0.0; HOP];
        for strength in [1.0, 0.0, 0.5] {
            for _ in 0..20 {
                assert_eq!(process(state, input.as_ptr(), output.as_mut_ptr(), strength), 1);
                assert!(output.iter().all(|v| v.is_finite()));
            }
        }
        assert_eq!(process(std::ptr::null_mut(), input.as_ptr(), output.as_mut_ptr(), 1.0), 0);
        destroy(state);
    }
    /// Loud audio, a hardware-mute stretch of zeros, then quiet audio: the quiet part must not
    /// start with 30 ms of the loud audio from before the mute, and the mute stays exact zeros.
    #[test]
    fn digital_silence_does_not_replay_old_audio() {
        let state = create();
        let mut seed = 5u32;
        let mut input = [0.0f32; HOP];
        let mut output = [0.0f32; HOP];
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        for frame in 0..250 {
            let amp = if frame < 100 { 0.5 } else if frame < 150 { 0.0 } else { 0.02 };
            for v in &mut input {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *v = ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * amp;
            }
            assert_eq!(process(state, input.as_ptr(), output.as_mut_ptr(), 0.0), 1);
            if (104..150).contains(&frame) {
                assert!(output.iter().all(|&v| v == 0.0), "frame {frame} not silent");
            }
            if (150..156).contains(&frame) {
                // Before the fix: 0.13-0.14 (the loud part), then 0.0055.
                assert!(rms(&output) < 0.01, "frame {frame}: {}", rms(&output));
            }
        }
        destroy(state);
    }
    /// Real engine session through the C++ DSP thread with `MNR_DENOISER=cpu`, muted so nothing
    /// is heard. `MNR_LIVE_INPUT` / `MNR_LIVE_OUTPUT` pick devices by name fragment.
    #[test]
    #[ignore = "live audio devices"]
    fn engine_runs_deepfilternet_live() {
        use crate::engine::{self, Config, Controls, Engine, Reply};
        unsafe { std::env::set_var("MNR_DENOISER", "cpu") };
        register();
        let pick = |capture, var: &str| {
            let name = std::env::var(var).unwrap_or_else(|_| panic!("{var} is not set"));
            let devices = engine::devices(capture).unwrap();
            devices.into_iter().find(|d| d.name.contains(&name)).unwrap_or_else(|| panic!("no {name}")).id
        };
        let settings = crate::settings::Settings::for_test("");
        let controls = Controls {
            slow: 0.7, fast: 1.5, volume: 1.0, boost: 3.0, overload: false, discord_volume: 1.0,
            pitch: 0, effects: engine::EffectOptions::default(), intensity: 1.0, alternate_intensity: 0.15, muted: true, rvc: false,
            rvc_options: crate::rvc::Options::load(&settings),
            noise_gate_db: -72.0,
        };
        let engine = Engine::new(controls).unwrap().unwrap();
        engine.controls(controls);
        engine.start(Config {
            input: pick(true, "MNR_LIVE_INPUT"),
            output: pick(false, "MNR_LIVE_OUTPUT"),
            version: 2, buffer: 40, period: 5, graphs: -1, intensity: 1.0,
        });
        let started = (0..200).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            match engine.reply() { Some(Reply::Started(_, r)) => Some(r), _ => None }
        });
        assert!(matches!(started, Some(Ok(()))), "{started:?}");
        std::thread::sleep(std::time::Duration::from_secs(8));
        let (snapshot, error) = engine.snapshot(true);
        let (denoiser, reason) = engine.denoiser_state();
        eprintln!("state={} denoiser={denoiser} ({reason}) process_ms={:.3} underruns={} drops={} error={error}",
            snapshot.state, snapshot.process_ms, snapshot.underruns, snapshot.drops);
        assert!(matches!(snapshot.state, 2 | 3), "{error}");
        assert_eq!(denoiser, 3);
        assert!(snapshot.process_ms < 10.0);
        engine.quit();
    }
}
