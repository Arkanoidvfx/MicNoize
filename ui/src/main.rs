#![windows_subsystem = "windows"]
mod components;
mod cpu_denoise;
mod engine;
mod logs;
mod paths;
mod rvc;
mod settings;
mod smooth;
mod tacho;
mod soundpad;
mod studio;
mod telemetry;
mod updater;
mod maintenance;
mod update_window;
mod view;
use engine::{Config, Controls, Device, Engine, Reply, Snapshot};
use iced::{Element, Font, Size, Subscription, Task, Theme, keyboard, window};
use settings::{Settings, key_name};
use soundpad::{Section, Sound, Sort as SoundSort, State as SoundState};
use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

const TAG_HOST_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const DEVICE_REPAIRED: &str = "Устройство восстановлено";
const DEVICE_REPAIRING: &str = "Проверяем и восстанавливаем виртуальное устройство…";
const EXIT_EVENT: u32 = 2;
const RESTART_EVENT: u32 = 4;
/// Mirrors `mic::rvcSlack` (src/audio.hpp): RVC output is a fixed delay line of chunk + slack.
const RVC_SLACK_MS: u32 = 200;
const DISCORD_VOLUME_AT_100: f32 = 0.08;
const DISCORD_VOLUME_MAX_PERCENT: f32 = 200.0;
/// `Msg::Bind` targets above the 13 effect keys: the soundpad stop key and one per clip.
const SOUND_STOP_BIND: usize = 99;
/// The old 20 % soundpad setting is the new 100 %: physical gain 0.04 (-28 dB).
const SOUND_VOLUME_AT_100: f32 = 0.04;
const SOUND_BIND_BASE: usize = 100;
/// Engine clip ids of the recordings list; above every soundpad clip id.
const CLIP_ID_BASE: u32 = 900_000;
/// How many recordings the microphone page keeps on disk.
const CLIPS_KEPT: usize = 6;

/// Local wall clock for recording names: std has no local time, `GetLocalTime` does.
#[repr(C)]
#[derive(Default)]
struct LocalTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLocalTime(time: *mut LocalTime);
}
/// What lies under the update morph's layer.
#[derive(Clone, Copy)]
enum MorphBase {
    Root,
    Card(tacho::BarStage),
    Key,
}
#[derive(Clone, Copy, Debug)]
enum MorphStep {
    HideBase,
    ShowCard,
    ShowRoot,
    Done,
}
struct MorphView {
    base: MorphBase,
    from_version: String,
    to_version: String,
    anim: Option<tacho::Morph<Msg>>,
    /// The keyed window's handle, to un-key it afterwards.
    hwnd: Option<u64>,
    /// Set for the shrink: where the watcher's update window must stand.
    center: Option<iced::Point>,
}
fn clip_name() -> String {
    let mut t = LocalTime::default();
    unsafe { GetLocalTime(&mut t) };
    format!(
        "Запись {:04}-{:02}-{:02} {:02}-{:02}-{:02} (mix).wav",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}
/// A recording name -> "14:05:12"; any other name keeps its stem.
fn clip_label(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let Some((date, time)) = stem
        .trim_start_matches("Запись ")
        .split_once(' ')
    else {
        return stem.to_owned();
    };
    let time = time.split_whitespace().next().unwrap_or(time);
    match (date.split('-').count(), time.split('-').count()) {
        (3, 3) => time.replace('-', ":"),
        _ => stem.to_owned(),
    }
}
/// `folder/name`, with " (n)" appended while that file exists: neither two recordings in the
/// same second nor a second save into the same folder may overwrite anything.
fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, "wav"));
    let mut path = folder.join(name);
    for n in 2..100 {
        if !path.exists() {
            break;
        }
        path = folder.join(format!("{stem} ({n}).{extension}"));
    }
    path
}

fn discord_volume_gain(percent: f32) -> f32 {
    percent.clamp(0.0, DISCORD_VOLUME_MAX_PERCENT) * DISCORD_VOLUME_AT_100 / 100.0
}

fn discord_volume_percent(gain: f32) -> f32 {
    (gain * 100.0 / DISCORD_VOLUME_AT_100).clamp(0.0, DISCORD_VOLUME_MAX_PERCENT)
}

fn load_discord_volume(settings: &Settings) -> f32 {
    let percent = if settings.number("effects", "discord_volume_scale", 1, 1, 2) == 2 {
        settings.number("effects", "discord_volume", 100, 0, 200) as f32
    } else {
        settings.number("effects", "discord_volume", 8, 0, 100) as f32 * 12.5
    };
    discord_volume_gain(percent)
}

fn load_sound_volume(settings: &Settings) -> f32 {
    let percent = |key| {
        settings
            .get("soundpad", key)
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|value| (0..=200).contains(value))
    };
    let legacy = percent("volume");
    let display = percent("volume_display");
    let value = match (legacy, display) {
        (Some(old), Some(new)) if old == (new + 2) / 5 => new,
        (Some(old), _) => (old * 5).min(200),
        (None, Some(new)) => new,
        (None, None) => 100,
    };
    value as f32 / 100.0
}

/// Decode a clip, scaled into the loudness `window` when levelling is on. Runs off the UI thread.
fn decode_for(path: &Path, window: Option<(f32, f32)>) -> Result<Vec<f32>, String> {
    let mut pcm = soundpad::decode(path)?;
    if let Some((min, max)) = window {
        soundpad::normalize(&mut pcm, min, max);
    }
    Ok(pcm)
}

/// Hand a decoded clip to the engine unless its list was rescanned meanwhile: the engine keeps
/// clips by id, so a stale decode would replace the current one. The error never reaches the
/// user, the handler drops results of an old generation.
/// ponytail: microsecond gap between check and load; a generation in `mnr_sound_load` closes it.
fn load_if_live(
    loader: &engine::SoundLoader,
    live: &AtomicU32,
    generation: u32,
    id: u32,
    pcm: &[f32],
    gain: f32,
) -> Result<(), String> {
    if live.load(Ordering::Acquire) != generation {
        return Err("Список звуков обновился".into());
    }
    loader.load(id, pcm, gain)
}

/// Keyboard focus targets. Values overlap between pages (each page has its own Tab order) and
/// are asserted literally by the controller tests, so they must never change.
mod focus {
    pub const NONE: usize = usize::MAX;
    /// Page tabs: `TAB_BASE + page` (0 microphone, 1 voice changer, 2 settings, 3 headphones).
    pub const TAB_BASE: usize = 40;
    /// Page 4 lives outside `TAB_BASE + page`: 44 already belongs to `rvc::NAME`.
    pub const TAB_SOUNDPAD: usize = 60;
    /// Update banner above every page; it only exists while an update is downloaded.
    pub const UPDATE_BANNER: usize = 74;
    /// Page 5 (logs), outside `TAB_BASE + page` like the soundpad.
    pub const TAB_LOGS: usize = 76;
    /// Page 6 (effects), split from the microphone page in 0.2.8.
    pub const TAB_EFFECTS: usize = 80;
    pub const TAB_STUDIO: usize = 30000;
    pub fn tab(page: u8) -> usize {
        match page {
            4 => TAB_SOUNDPAD,
            5 => TAB_LOGS,
            6 => TAB_EFFECTS,
            7 => TAB_STUDIO,
            _ => TAB_BASE + page as usize,
        }
    }
    pub mod logs {
        pub const COPY: usize = 77;
        pub const FOLDER: usize = 78;
        pub const SEND: usize = 73;
        pub const BACK: usize = 84;
    }
    pub mod soundpad {
        pub const FOLDER: usize = 61;
        pub const ADD: usize = 62;
        pub const REFRESH: usize = 63;
        pub const VOLUME: usize = 64;
        pub const HEAR: usize = 65;
        pub const STOP_BIND: usize = 66;
        pub const FILTER: usize = 67;
        pub const SORT: usize = 68;
        pub const SECTION_ADD: usize = 69;
        pub const SECTION_NAME: usize = 70;
        pub const SECTION_DELETE: usize = 71;
        pub const NORMALIZE: usize = 75;
        /// Row `i`: `ROW_BASE + 3 * i` play, `+ 1` volume, `+ 2` hotkey.
        pub const ROW_BASE: usize = 1000;
        /// Sidebar entry `i` of `App::section_items`.
        pub const SECTION_BASE: usize = 20000;
    }
    pub mod studio {
        pub const RECORD: usize = 30001;
        pub const IMPORT: usize = 30002;
        pub const BPM: usize = 30003;
        pub const PLAY: usize = 30004;
        pub const EXPORT: usize = 30005;
        pub const CLEAR: usize = 30006;
        pub const STOP: usize = 30007;
        pub const HEAR: usize = 30008;
        pub const CURSOR: usize = 30009;
        pub const LOOP: usize = 30014;
        pub const LOOP_START: usize = 30015;
        pub const LOOP_END: usize = 30016;
        pub const ZOOM: usize = 30017;
        pub const VOLUME: usize = 30018;
        pub const DELETE_CONFIRM: usize = 30019;
        pub const DELETE_CANCEL: usize = 30020;
        pub const LIBRARY: usize = 30021;
        pub const BAR_BASE: usize = 30010;
        pub const SAMPLE_BASE: usize = 30100;
        pub const DELETE_BASE: usize = 31000;
        pub const CELL_BASE: usize = 32000;
    }
    pub mod settings {
        pub const INPUT: usize = 0;
        pub const OUTPUT: usize = 1;
        pub const VERSION: usize = 2;
        pub const BUFFER: usize = 3;
        pub const REFRESH: usize = 5;
        pub const DONE: usize = 6;
        pub const QUIT: usize = 7;
        pub const AUTOSTART: usize = 8;
        pub const APP_AUTOSTART: usize = 79;
        pub const UPDATE: usize = 57;
        pub const APPLY_UPDATE: usize = 58;
        pub const DRIVER: usize = 72;
        pub const REPAIR: usize = 90;
        pub const REPAIR_REINSTALL: usize = 91;
        pub const REPAIR_CONFIRM: usize = 92;
        pub const REPAIR_CANCEL: usize = 93;
        pub const REPAIR_LINES: usize = 94;
        pub const LOGS: usize = 95;
        pub const REHEARSE: usize = 96;
        pub const PIXEL_SHIFT: usize = 97;
        pub const SLIDER_IDLE: usize = 98;
    }
    pub mod effects {
        pub const INPUT: usize = 35;
        pub const INTENSITY: usize = 34;
        pub const ALT_INTENSITY: usize = 37;
        pub const NOISE_BIND: usize = 38;
        pub const OVERLOAD: usize = 21;
        pub const BOOST: usize = 2;
        pub const BOOST_BIND: usize = 3;
        pub const PITCH: usize = 4;
        pub const PITCH_BIND: usize = 5;
        pub const SLOW: usize = 10;
        pub const SLOW_BIND: usize = 11;
        pub const FAST: usize = 12;
        pub const FAST_BIND: usize = 13;
        pub const CANCEL_PHRASE: usize = 14;
        pub const REVERSE_BIND: usize = 15;
        /// Discord bindings of the five effects: `DISCORD_BIND_BASE + effect`.
        pub const DISCORD_BIND_BASE: usize = 16;
        pub const MONITOR: usize = 9;
        pub const MONITOR_BIND: usize = 23;
        pub const REPLAY_BIND: usize = 32;
        /// Save menu of the recording opened in `App::clip_menu`.
        pub const CLIP_TO_SOUNDPAD: usize = 1990;
        pub const CLIP_TO_FOLDER: usize = 1991;
        /// Recording `i`: `CLIP_BASE + 2 * i` play, `+ 1` save.
        pub const CLIP_BASE: usize = 2000;
        pub const DISCORD_VOLUME: usize = 22;
        pub const EFFECTS_MONITOR: usize = 31;
        pub const BOOST_MONITOR: usize = 36;
        /// Шумодав page: headphone gear and the folded processing route.
        pub const HEADPHONE_GEAR: usize = 81;
        pub const ROUTE: usize = 82;
        pub const REVERSE_WORD: usize = 83;
        /// The first-run setup card: retry the download, install the virtual microphone.
        pub const SETUP_RETRY: usize = 84;
        pub const SETUP_DRIVER: usize = 85;
        pub const SETUP_SETTINGS: usize = 86;
    }
    pub mod rvc {
        pub const ENABLE: usize = 24;
        pub const MODEL: usize = 25;
        pub const PITCH: usize = 26;
        pub const INDEX: usize = 27;
        pub const GAIN: usize = 28;
        pub const CHUNK: usize = 29;
        pub const REFRESH: usize = 30;
        pub const ADVANCED: usize = 33;
        pub const IMPORT: usize = 39;
        pub const NAME: usize = 44;
        pub const RENAME: usize = 45;
        pub const DELETE: usize = 46;
        pub const INSTALL: usize = 59;
    }
    pub mod headphones {
        pub const OUTPUT: usize = 50;
        pub const TOGGLE: usize = 51;
        pub const NOISE: usize = 52;
        pub const INTENSITY: usize = 53;
        pub const VOLUME: usize = 54;
        pub const PITCH: usize = 55;
        pub const REVERSE: usize = 56;
        pub const LINES: usize = 87;
    }
}
static RESTART: AtomicBool = AtomicBool::new(false);

fn restart_requested(events: u32) -> bool {
    events & (EXIT_EVENT | RESTART_EVENT) == RESTART_EVENT
}

/// Device loss keeps waiting at the last delay; other failures retain the attempt limit.
const RECOVERY_DELAYS: [u64; 5] = [2, 5, 15, 30, 60];
fn transient_device_failure(error: &str) -> bool {
    [
        "Waiting for the TAG microphone endpoint in Windows",
        "TAG microphone endpoint is not active",
        "Bound Mic Noize TAG microphone endpoint is not active",
        "TAG endpoint controller unavailable",
        "TAG background host unavailable",
        "TAG host stopped responding",
        "TAG host response timeout",
        "TAG endpoint changed; reconnect processing",
        "TAG host restarted or protocol changed; reconnect processing",
        "TAG connection expired; reconnect processing",
        "TAG response expired; reconnect processing",
        "TAG host initializing or waiting for driver",
        "0x88890004", // AUDCLNT_E_DEVICE_INVALIDATED
        "0x88890010", // AUDCLNT_E_SERVICE_NOT_RUNNING
        "0x80070490", // Selected endpoint temporarily absent (ERROR_NOT_FOUND).
    ]
    .iter()
    .any(|reason| error.contains(reason))
}
/// Restarts processing after any failure: TAG host gone, microphone lost across sleep, a start
/// that hit a device not ready yet after login. There is no Start button, so without this a
/// failure lasted until the user pressed Refresh. A settings error just spends the attempts;
/// a minute of healthy running since the last failure forgives them.
#[derive(Default)]
struct Recovery {
    attempts: usize,
    since: Option<Instant>,
    due: Option<Instant>,
    wait_for_device: bool,
}
impl Recovery {
    /// Feed every engine state; returns the pause when a restart gets scheduled.
    fn observe(&mut self, state: i32, now: Instant) -> Option<Duration> {
        match state {
            2 | 3 => {
                // Running again (maybe started by hand): nothing left to recover.
                self.due = None;
                let since = *self.since.get_or_insert(now);
                if now.duration_since(since) >= Duration::from_secs(60) {
                    self.attempts = 0;
                }
                None
            }
            5 if self.due.is_none() => {
                // A flapping session must not count its old healthy stretch as forgiveness.
                self.since = None;
                let seconds = RECOVERY_DELAYS.get(self.attempts).copied().or_else(|| {
                    self.wait_for_device.then_some(60)
                })?;
                let delay = Duration::from_secs(seconds);
                self.attempts = self.attempts.saturating_add(1);
                self.due = Some(now + delay);
                Some(delay)
            }
            _ => None,
        }
    }
    fn take_due(&mut self, now: Instant) -> bool {
        let due = self.due.is_some_and(|at| now >= at);
        if due {
            self.due = None;
        }
        due
    }
    fn exhausted(&self) -> bool {
        !self.wait_for_device && self.due.is_none() && self.attempts >= RECOVERY_DELAYS.len()
    }
}

const APP_RUN_NAME: &str = "MicNoize";
/// Windows login starts this exe straight into the tray; the saved route then starts itself.
/// Rewritten on every launch so the value follows the exe (updates, dev vs installed copy).
fn set_app_autostart(enabled: bool) -> Result<(), String> {
    let mut command = Command::new("reg");
    command.creation_flags(0x08000000);
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        command.args(["add", TAG_HOST_RUN_KEY, "/v", APP_RUN_NAME, "/t", "REG_SZ", "/d"])
            .arg(format!(r#""{}" --start-tray"#, exe.display()))
            .arg("/f");
    } else {
        command.args(["delete", TAG_HOST_RUN_KEY, "/v", APP_RUN_NAME, "/f"]);
    }
    let output = command.output().map_err(|e| e.to_string())?;
    // Deleting a value that was never there is not an error worth showing.
    if output.status.success() || !enabled {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn tag_autostart_default(settings: &Settings, current: bool) -> bool {
    // A missing settings file is a new install. Existing installs keep their task setting.
    settings.number("ui", "tag_autostart", if settings.path.exists() { current as i32 } else { 1 }, 0, 1) != 0
}

#[derive(Debug, Clone)]
enum Msg {
    HeadphonePanel(bool),
    RouteToggle,
    ReverseWord(String),
    ReverseEdit(bool),
    /// A left click anywhere; ends the reverse word edit when it left the field.
    PointerDown,
    HeadphoneToggle,
    HeadphoneOutput(Device),
    HeadphoneNoise(bool),
    HeadphoneIntensity(f32),
    HeadphoneVolume(f32),
    HeadphonePitch(f32),
    HeadphoneReverse(bool),
    Tick,
    Opened(window::Id),
    WindowFocus(window::Id, bool),
    /// Windows minimized the window (0×0 resize), e.g. a taskbar click on the active window.
    Minimized(window::Id),
    Restored(window::Id),
    MinimizedState(window::Id, Option<bool>),
    Hide,
    Show,
    Minimize,
    Drag,
    Quit,
    Monitor,
    EffectsMonitor(bool),
    BoostMonitor(bool),
    Settings,
    Page(u8),
    Refresh,
    Autostart(bool),
    AutostartUpdated(Result<bool, String>),
    AppAutostart(bool),
    /// Визуальные эффекты: the page-switch pixelation and the sliders' idle wave + warm-up.
    PixelShift(bool),
    SliderIdle(bool),
    Input(Device),
    Output(Device),
    Version(i32),
    Buffer(u32),
    Intensity(f32),
    AlternateIntensity(f32),
    Boost(f32),
    Overload(bool),
    DiscordVolume(f32),
    Pitch(f32),
    Slow(f32),
    Fast(f32),
    Rvc(bool),
    RvcModel(rvc::Model),
    RvcPitch(f32),
    RvcIndex(f32),
    RvcGain(f32),
    RvcChunk(u32),
    RvcRefresh,
    RvcImport,
    RvcImported(Result<Option<(Vec<rvc::Model>, u32)>, String>),
    RvcName(String),
    RvcRename,
    RvcDelete,
    RvcAdvanced,
    RvcInstall,
    RvcInstalled(Result<String, String>),
    CoreInstalled(Result<String, String>),
    /// The setup card's «Повторить» after a failed component download.
    RetryCore,
    InstallDriver,
    /// The headphone panel's «Освободить место»: device repair with the line transfer ticked.
    HeadphoneLines,
    Repair,
    RepairReinstall(bool),
    RepairLines(bool),
    RepairConfirm,
    RepairCancel,
    Repaired(Result<(), String>),
    DriverInstalled(Result<(), String>),
    SendReport,
    ReportSent(Result<String, String>),
    /// A fresh logs report; `true` also puts it on the clipboard.
    Logs(String, bool),
    LogsCopy,
    LogsFolder,
    UpdateCheck,
    UpdateChecked(updater::Status),
    UpdatePrepared(Result<(),String>),
    UpdateApplied(Result<(),String>),
    ApplyUpdate,
    CancelPhrase,
    Bind(usize),
    SoundpadFolder,
    SoundpadPicked(bool, Result<Vec<PathBuf>, String>),
    SoundpadAdd,
    SoundpadRefresh,
    SoundpadVolume(f32),
    SoundpadHear(bool),
    /// Level clip loudness on decode; reloads the library.
    SoundpadNormalize(bool),
    SoundpadFilter(String),
    SoundpadSort(SoundSort),
    /// Absolute scroll offset and viewport height of the body: the clip list renders only
    /// the rows near the viewport.
    SoundpadScroll(f32, f32),
    SoundHover(usize, bool),
    /// Mouse wheel over a scrollable (by id), in pixels; eased by `smooth`.
    Wheel(&'static str, f32),
    ScrollProbe(&'static str, Option<(f32, f32, f32)>),
    ScrollFrame(Instant),
    SoundpadStop,
    StudioBpm(u32),
    StudioVolume(u32),
    StudioZoom(usize),
    StudioLoopEnabled(bool),
    StudioLoopStart(usize),
    StudioLoopEnd(usize),
    StudioLoopGrab(usize),
    StudioLoopMove(usize),
    StudioBar(usize),
    StudioSelect(String),
    StudioCell(usize, u8),
    StudioDrawStart(usize, u8),
    StudioCellEnter(usize, u8),
    StudioEraseStart(usize, u8),
    StudioRightEnd,
    StudioGrab(usize),
    StudioDrag(usize),
    StudioScroll(f32, f32),
    StudioRecord,
    StudioRecorded(Result<String, String>),
    StudioImport,
    StudioPicked(Result<Vec<PathBuf>, String>),
    StudioImported(Result<(), String>),
    StudioRender(bool),
    StudioRendered(bool, Result<Option<PathBuf>, String>),
    StudioClear,
    StudioDeleteAsk(String),
    StudioDeleteConfirm,
    StudioDeleteCancel,
    StudioDeleted(String, Result<(), String>),
    StudioLibraryToggle,
    SectionSelect(usize),
    SectionAdd,
    SectionName(String),
    SectionRename,
    SectionDelete,
    DragStart(usize),
    DragOver(Option<usize>),
    DragEnd,
    SoundUnassign(usize),
    SoundPlay(usize),
    SoundVolume(usize, f32),
    SoundLoaded(usize, u32, Result<f32, String>),
    /// A finished hold-effect recording was written to the recordings folder.
    ClipRecorded(Result<(), String>),
    ClipPlay(usize),
    ClipLoaded(usize, u32, Result<f32, String>),
    /// Open (or close) the "where to save" menu of recording `i`.
    ClipMenu(Option<usize>),
    /// Save recording `i` into the soundpad folder (`true`) or a folder picked now.
    ClipSave(usize, bool),
    ClipPicked(usize, bool, Result<Vec<PathBuf>, String>),
    CancelBind,
    ClearBind,
    AcceptBind,
    Key(keyboard::Key, keyboard::Modifiers, bool),
    Noop,
    /// The page-switch pixelation has played out.
    PageShiftDone,
    /// «Перезапустить»: the window's position and size, to shrink it into the update window.
    DecayGeometry(Option<iced::Point>, Size),
    /// The window's native handle: colour-key it for a morph.
    Keyed(u64),
    /// A point in the running morph's timeline.
    MorphStep(MorphStep),
    /// After an update: the hidden window is keyed and shown as the update window; then it grows.
    IntroStart,
    /// Plays the whole update hand-over for real, without installing anything.
    RehearseUpdate,
    /// The rehearsal's update window is on screen (or could not start).
    RehearsalReady(Result<(), String>),
    Screenshot(window::Screenshot),
}
/// Which sidebar entry filters the clip list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    All,
    /// Automatic group by name prefix (lowercase key).
    Group(String),
    /// Index into `App::sections`.
    Custom(usize),
}
struct SectionItem {
    label: String,
    count: usize,
    selection: Selection,
}
#[derive(Clone,Copy,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeIntent {microphone:bool,headphones:bool,monitor:i32,full_monitor:bool}
struct StudioStroke { anchor: usize, note: u8, sample: String, before: Vec<studio::Event> }
struct App {
    soundpad_page: bool,
    studio_page: bool,
    studio_folder: PathBuf,
    studio_samples: Vec<String>,
    studio_selected: Option<String>,
    studio_events: Vec<studio::Event>,
    studio_bpm: u32,
    studio_volume: u32,
    studio_zoom: usize,
    studio_loop: bool,
    studio_live: bool,
    studio_rebuild: bool,
    studio_loop_start: usize,
    studio_loop_end: usize,
    studio_loop_drag: Option<bool>, // true adjusts the start; false adjusts the end
    studio_stroke: Option<StudioStroke>,
    studio_erasing: bool,
    studio_delete: Option<String>,
    studio_library_open: bool,
    studio_bar: usize,
    studio_cursor: f32,
    studio_scrubbing: bool,
    studio_follow: bool,
    studio_play_bpm: u32,
    studio_scroll: (f32, f32),
    studio_generation: u32,
    studio_busy: bool,
    studio_note: String,
    sound_folder: Option<PathBuf>,
    sounds: Vec<Sound>,
    sound_volume: f32,
    sound_monitor: bool,
    /// Scale clips into the `[soundpad]` loudness window as they are decoded.
    sound_normalize: bool,
    sound_filter: String,
    sound_sort: SoundSort,
    sound_scroll: (f32, f32),
    sound_hover: Option<usize>,
    scroll_anims: std::collections::HashMap<&'static str, smooth::Anim>,
    scroll_pending: std::collections::HashMap<&'static str, f32>,
    sections: Vec<Section>,
    section: Selection,
    section_name: String,
    /// Clip index being dragged from the list, and the custom section under the cursor.
    dragging: Option<usize>,
    drag_over: Option<usize>,
    sound_stop_key: u32,
    /// Bumped on every rescan so a decode finishing for an old list is ignored.
    sound_generation: u32,
    /// Always equal to `sound_generation` (see `bump_sounds`): a decode task checks it right
    /// before loading, so a clip decoded for an old list never replaces a newer one in the engine.
    sound_live: Arc<AtomicU32>,
    sound_pending_play: Option<usize>,
    sound_playing: (u32, f32, f32),
    sound_note: String,
    sound_dialog: bool,
    /// The six newest hold-effect recordings, newest first, and their folder.
    clips: Vec<Sound>,
    clips_folder: PathBuf,
    /// Generation of the recording last taken from the engine.
    clip_generation: u32,
    /// Bumped on every recordings rescan so a decode for an old list is ignored.
    clip_loads: u32,
    /// Always equal to `clip_loads` (see `bump_clips`), like `sound_live`.
    clip_live: Arc<AtomicU32>,
    clip_menu: Option<usize>,
    clip_pending_play: Option<usize>,
    clip_note: String,
    /// Decayed peak of the monitor's own output: shows that "hear sounds" really renders.
    monitor_peak: f32,
    /// The headphone panel is open over the Шумодав page (old page 3).
    headphone_page: bool,
    effects_page: bool,
    route_open: bool,
    reverse_word: String,
    reverse_edit: bool,
    /// Shared animation clock and the moment the window was last shown.
    epoch: Instant,
    opened_at: Option<Instant>,
    in_peak: f32,
    /// Bound keys held right now (bitset by virtual-key code), for the pressed keycaps.
    keys_down: [u64; 4],
    /// The new page, painted small, while its mosaic resolves over it after a page switch.
    page_shift: Option<(std::sync::Arc<tacho::Mosaic>, Instant)>,
    /// Flipped on frames that swap most of the page: see [`App::backdrop`].
    repaint_all: bool,
    /// The update shrink («Перезапустить») or grow (first start after an update).
    morph: Option<MorphView>,
    /// The downloaded update's version, for the update window.
    update_version: Option<String>,
    /// First start after an update: the update window's centre to grow out of.
    intro: Option<iced::Point>,
    /// Quitting to rehearse the update animations instead of applying an update.
    rehearse_after_quit: bool,
    headphone_output: Option<Device>,
    headphone_denoise: bool,
    headphone_intensity: f32,
    headphone_volume: f32,
    headphone_pitch: i32,
    headphone_reverse: bool,
    headphone_state: i32,
    headphone_busy: bool,
    headphone_message: String,
    engine: Engine,
    settings: Settings,
    window: Option<window::Id>,
    window_focused: bool,
    /// Set by the title-bar "−": that minimize stays in the taskbar instead of hiding to tray.
    own_minimize: bool,
    hidden_window: Option<window::Id>,
    inputs: Vec<Device>,
    outputs: Vec<Device>,
    input: Option<Device>,
    output: Option<Device>,
    controls: Controls,
    rvc_models: Vec<rvc::Model>,
    runtime_root: PathBuf,
    component_root: PathBuf,
    core_installing: bool,
    driver_installing: bool,
    driver_ready: bool,
    /// The device lists arrived once: before that a missing microphone is no problem yet.
    devices_known: bool,
    core_present: bool,
    /// This GPU's denoiser models are on disk, or it has none to download.
    models_present: bool,
    setup_error: String,
    driver_error: String,
    /// Recent (time, bytes done) of the running install phase, for its speed.
    transfer: Vec<(Instant, u64)>,
    transfer_stage: Option<(components::Item, components::Phase)>,
    /// When the install's bytes last moved: a long pause shows as a stall.
    transfer_moved: Instant,
    device_state: engine::DeviceState,
    device_detail: String,
    repair_confirm: bool,
    repair_reinstall: bool,
    repair_lines: bool,
    repair_resume: Option<ResumeIntent>,
    repair_started: bool,
    quit_after_repair: bool,
    resume_monitor: Option<(i32,bool)>,
    report_sending: bool,
    logs_page: bool,
    /// Engine denoiser: 1 NVIDIA, 2 none, 3 DeepFilterNet on the CPU, 4 input already denoised
    /// (the text names it); otherwise the text says why not NVIDIA.
    denoiser: (i32, String),
    logs_text: String,
    logs_copied: bool,
    /// Last `message` written to app.log, digits removed so countdowns do not repeat it.
    logged_message: String,
    /// NVIDIA model architecture and GPU name, or why there is none.
    gpu: Result<(String, String), String>,
    rvc_runtime_installed: bool,
    rvc_runtime_installing: bool,
    keys: [u32; 13],
    discord_state: i32,
    discord_source: bool,
    discord_message: String,
    phrase_state: i32,
    phrase_seconds: f32,
    version: i32,
    buffer: u32,
    period: u32,
    graphs: i32,
    snapshot: Snapshot,
    monitor: i32,
    monitor_all: bool,
    effects_monitor: bool,
    boost_monitor: bool,
    monitor_message: String,
    message: String,
    details: bool,
    rvc_page: bool,
    rvc_advanced: bool,
    rvc_importing: bool,
    rvc_import_note: String,
    rvc_name: String,
    rvc_delete_confirm: bool,
    update_checking: bool,
    update_ready: bool,
    update_status: String,
    apply_after_quit: bool,
    update_resume: Option<ResumeIntent>,
    /// "Update" was pressed: apply as soon as the re-check before it finishes.
    apply_pending: bool,
    busy: bool,
    quitting: bool,
    /// `Msg::Bind` target whose button is capturing a key in place.
    binding: Option<usize>,
    candidate: u32,
    /// Captured key that another binding already uses; shown on the capturing button.
    bind_conflict: Option<u32>,
    auto_started: bool,
    recovery: Recovery,
    focus: usize,
    /// Like CSS `:focus-visible`: slider focus rings show after keyboard use, not after a drag.
    focus_visible: bool,
    dirty: Option<Instant>,
    hint_shown: bool,
    autostart: bool,
    autostart_busy: bool,
    task_warning: String,
    app_autostart: bool,
    pixel_shift: bool,
    slider_idle: bool,
    tray_ok: bool,
    peak: f32,
    ticks: u64,
    capture_path: Option<PathBuf>,
    capture_started: bool,
    qa_scale: f32,
    benchmark: bool,
    usage: (Instant, u64),
    measurements: String,
}
/// The [`CLIPS_KEPT`] newest recordings of `folder`, newest first; older files are deleted.
fn newest_clips(folder: &Path) -> Vec<Sound> {
    let mut clips = soundpad::scan(folder, &[]).unwrap_or_default();
    clips.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
    if clips.len() > CLIPS_KEPT {
        for extra in clips.drain(CLIPS_KEPT..) {
            let _ = std::fs::remove_file(&extra.path);
        }
    }
    clips
}
/// Which keys of `bindings` (virtual key plus the Ctrl/Alt/Shift bits) are held now. Only
/// bound keys are read, and only while the window is focused, to animate their keycaps.
fn held_keys(bindings: impl Iterator<Item = u32>) -> [u64; 4] {
    #[link(name = "user32")]
    unsafe extern "system" { fn GetAsyncKeyState(key: i32) -> i16; }
    let mut down = [0u64; 4];
    for binding in bindings.filter(|b| *b != 0) {
        let mods = binding >> 8;
        let vks = [binding & 255, if mods & 1 != 0 { 0x11 } else { 0 }, if mods & 2 != 0 { 0x12 } else { 0 }, if mods & 4 != 0 { 0x10 } else { 0 }];
        for vk in vks.into_iter().filter(|vk| *vk != 0) {
            if unsafe { GetAsyncKeyState(vk as i32) } < 0 {
                down[(vk / 64) as usize] |= 1 << (vk % 64);
            }
        }
    }
    down
}
fn timer(visible: bool) -> Task<Msg> {
    Task::perform(
        async move {
            std::thread::sleep(Duration::from_millis(if visible { 50 } else { 250 }));
        },
        |_| Msg::Tick,
    )
}
fn exit_ui() -> Task<Msg> {
    // Winit 0.30.13 can enter MsgWaitForMultipleObjectsEx after AboutToWait
    // already requested exit. A tray-only daemon has no window destruction to wake it.
    // Queue the native quit message as well; iced::exit still drops the app normally.
    #[link(name = "user32")]
    unsafe extern "system" { fn PostQuitMessage(code: i32); }
    if !cfg!(test) { unsafe { PostQuitMessage(0); } }
    iced::exit()
}
fn window_icon() -> window::Icon {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../assets/app-64.png")));
    let mut reader = decoder
        .read_info()
        .expect("decode embedded application icon");
    let mut rgba = vec![0; reader.output_buffer_size().expect("application icon size")];
    let info = reader
        .next_frame(&mut rgba)
        .expect("read embedded application icon");
    window::icon::from_rgba(rgba[..info.buffer_size()].to_vec(), info.width, info.height)
        .expect("valid embedded application icon")
}
impl App {
    fn window_size() -> Size {
        if std::env::args().any(|s| s == "--ui-small") { Size::new(620.0, 440.0) } else { Size::new(1040.0, 740.0) }
    }
    /// `intro`: open hidden, centred on the update window it will grow out of.
    fn open(_scale: f32, intro: Option<iced::Point>) -> (window::Id, Task<Msg>) {
        let small = std::env::args().any(|s| s == "--ui-small");
        let size = Self::window_size();
        let (id, task) = window::open(window::Settings {
            size,
            min_size: Some(if small {
                Size::new(620.0, 440.0) // Explicit QA mode only.
            } else {
                Size::new(960.0, 680.0)
            }),
            position: intro.map_or(window::Position::Centered, |c| {
                window::Position::Specific(iced::Point::new(c.x - size.width / 2.0, c.y - size.height / 2.0))
            }),
            visible: intro.is_none(),
            icon: Some(window_icon()),
            decorations: false,
            exit_on_close_request: false,
            ..Default::default()
        });
        (id, task.map(Msg::Opened))
    }
    fn new() -> Result<Option<(Self, Task<Msg>)>, String> {
        let paths = paths::Paths::resolve()?;
        let runtime_root = paths.runtime_root().to_path_buf();
        // Native engine and the RVC worker share this version-independent component root.
        unsafe { std::env::set_var("MNR_RUNTIME_ROOT", &runtime_root) };
        let settings = Settings::load(&paths.data)?;
        telemetry::record(&paths.data, "session-start", serde_json::json!({}));
        Self::from_settings_and_runtime(settings, runtime_root, paths.components)
    }
    #[cfg(test)]
    fn from_settings(settings: Settings) -> Result<Option<(Self, Task<Msg>)>, String> {
        let runtime_root = settings
            .path
            .parent()
            .ok_or("Invalid settings path")?
            .to_path_buf();
        Self::from_settings_and_runtime(settings, runtime_root.clone(), runtime_root)
    }
    fn from_settings_and_runtime(
        settings: Settings,
        runtime_root: PathBuf,
        component_root: PathBuf,
    ) -> Result<Option<(Self, Task<Msg>)>, String> {
        let headphone_denoise = settings.number("headphones", "denoise", 1, 0, 1) != 0;
        let headphone_intensity =
            settings.number("headphones", "intensity", 80, 0, 200) as f32 / 100.0;
        let headphone_volume = settings.number("headphones", "volume", 70, 0, 100) as f32 / 100.0;
        let headphone_pitch = settings.number("headphones", "pitch", 0, -12, 12);
        let headphone_reverse = settings.number("headphones", "reverse", 0, 0, 1) != 0;
        let effects_monitor = settings.number("effects", "monitor_effects", 0, 0, 1) != 0;
        let boost_monitor = settings.number("effects", "monitor_boost", 0, 0, 1) != 0;
        let controls = Controls {
            slow: settings.number("effects", "slow_speed", 70, 50, 95) as f32 / 100.0,
            fast: settings.number("effects", "fast_speed", 150, 105, 200) as f32 / 100.0,
            volume: 1.0,
            boost: settings.number("effects", "boost", 300, 100, 2000) as f32 / 100.0,
            overload: settings.number("effects", "overload", 0, 0, 1) != 0,
            discord_volume: load_discord_volume(&settings),
            pitch: settings.number("effects", "pitch", -5, -12, 12),
            intensity: settings.number("audio", "intensity", 100, 0, 200) as f32 / 100.0,
            alternate_intensity: settings.number("audio", "alternate_intensity", 15, 0, 200) as f32
                / 100.0,
            muted: false,
            rvc: settings.number("effects", "rvc_enabled", 0, 0, 1) != 0,
            rvc_options: rvc::Options::load(&settings),
        };
        let Some(engine) = Engine::new(controls)? else {
            return Ok(None);
        };
        if !cfg!(test){engine.request_device_state();}
        let mut keys = [
            settings.number("effects", "boost_key", 0, 0, 2046) as u32,
            settings.number("effects", "pitch_key", 0, 0, 2046) as u32,
            settings.number("effects", "slow_key", 0, 0, 2046) as u32,
            settings.number("effects", "fast_key", 0, 0, 2046) as u32,
            settings.number("effects", "reverse_key", 0, 0, 2046) as u32,
            settings.number("effects", "discord_boost_key", 0, 0, 2046) as u32,
            settings.number("effects", "discord_pitch_key", 0, 0, 2046) as u32,
            settings.number("effects", "discord_slow_key", 0, 0, 2046) as u32,
            settings.number("effects", "discord_fast_key", 0, 0, 2046) as u32,
            settings.number("effects", "discord_reverse_key", 0, 0, 2046) as u32,
            settings.number("effects", "monitor_key", 0, 0, 2046) as u32,
            settings.number("effects", "replay_key", 119 | 256, 0, 2046) as u32,
            settings.number("effects", "noise_key", 0, 0, 2046) as u32,
        ];
        for i in 0..keys.len() {
            if keys[i] != 0
                && (!(3..=254).contains(&(keys[i] & 255)) || keys[..i].contains(&keys[i]))
            {
                keys[i] = 0;
            }
        }
        engine.bindings(keys);
        let sound_folder = settings
            .get("soundpad", "folder")
            .filter(|f| !f.is_empty())
            .map(PathBuf::from);
        let sound_volume = load_sound_volume(&settings);
        let sound_monitor = settings.number("soundpad", "monitor", 0, 0, 1) != 0;
        let sound_normalize = soundpad::normalize_enabled(&settings);
        let sound_stop_key = settings.number("soundpad", "stop_key", 0, 0, 2046) as u32;
        let sound_sort = SoundSort::from_code(settings.number("soundpad", "sort", 0, 0, 3));
        let sections = soundpad::sections(&settings);
        let (sounds, sound_note) = match &sound_folder {
            Some(folder) => match soundpad::scan(folder, &soundpad::entries(&settings)) {
                Ok(sounds) => (sounds, String::new()),
                Err(e) => (vec![], format!("Папка недоступна: {e}")),
            },
            None => (vec![], String::new()),
        };
        engine.sound_volume(sound_volume * SOUND_VOLUME_AT_100);
        let clips_folder = settings
            .path
            .parent()
            .unwrap_or(Path::new("."))
            .join("Записи");
        let clips = newest_clips(&clips_folder);
        let studio_folder = settings.path.parent().unwrap_or(Path::new(".")).join("Студия").join("Звуки");
        let studio_samples = studio::scan(&studio_folder);
        let studio_selected = studio_samples.first().cloned();
        let (studio_bpm, studio_events) = studio::load(&settings);
        let studio_volume = settings.number("studio", "volume", 100, 0, 200) as u32;
        let studio_zoom = studio::ZOOMS.iter().position(|&z| z == settings.number("studio", "zoom", 100, 50, 150) as u8).unwrap_or(2);
        let studio_loop_start = settings.number("studio", "loop_start", 0, 0, 63) as usize;
        let studio_loop_end = (settings.number("studio", "loop_end", 64, 1, 64) as usize).max(studio_loop_start + 1);
        let studio_loop = settings.number("studio", "loop", 0, 0, 1) != 0;
        if !cfg!(test) {
            engine.refresh();
        }
        let version = settings.number("audio", "version", 2, 1, 2);
        let buffer = settings.number("audio", "buffer_ms", 40, 10, 80) as u32;
        let period = settings.number("audio", "period_ms", 5, 2, 20) as u32;
        let graphs = settings.number("audio", "cuda_graphs", -1, -1, 1);
        let hint_shown = settings.get("ui", "tray_hint") == Some("1");
        let pixel_shift = settings.number("ui", "page_pixelate", 1, 0, 1) != 0;
        let slider_idle = settings.number("ui", "slider_idle", 1, 0, 1) != 0;
        // On by default: a fresh install starts with Windows until the user unticks it.
        let app_autostart = settings.number("ui", "app_autostart", 1, 0, 1) != 0;
        if !cfg!(test) {
            let _ = set_app_autostart(app_autostart);
        }
        let args: Vec<_> = std::env::args().collect();
        let capture_path = args
            .iter()
            .position(|s| s == "--ui-snapshot")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from);
        let qa_scale = args
            .iter()
            .position(|s| s == "--ui-scale")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 1.0 && *v <= 2.0)
            .unwrap_or(1.0);
        let tray_start = cfg!(test) || args.iter().any(|s| s == "--start-tray");
        // After an update the watcher's window waits for this one to take over.
        let intro = if cfg!(test) { None } else { update_window::take_handoff(&runtime_root) };
        // Without a hand-over to play, a waiting update window may close right away.
        if !cfg!(test) && (intro.is_none() || tray_start) {
            update_window::signal_ui_shown(&runtime_root);
        }
        let intro = intro.filter(|_| !tray_start);
        let (window, open) = if tray_start {
            (None, Task::none())
        } else {
            let (id, task) = Self::open(qa_scale, intro);
            (Some(id), task)
        };
        let gpu = if cfg!(test) { Err(String::new()) } else { engine::gpu() };
        let arch = gpu.as_ref().ok().map(|(arch, _)| arch.clone());
        // Missing models for this GPU are downloaded like the core: the engine waits for both.
        let core_present = cfg!(test) || components::core_installed(&runtime_root);
        let models_present = cfg!(test) || components::models_present(&runtime_root, arch.as_deref());
        let core_installing = !core_present || !models_present;
        let tag_task_enabled = !cfg!(test) && engine::tag_autostart(-1).unwrap_or(false);
        let autostart = tag_autostart_default(&settings, tag_task_enabled);
        let autostart_busy = !cfg!(test) && !core_installing && autostart != tag_task_enabled;
        if !cfg!(test) {
            logs::note(
                &runtime_root,
                &format!(
                    "Запуск Mic Noize {}; GPU: {}",
                    env!("CARGO_PKG_VERSION"),
                    match &gpu {
                        Ok((arch, name)) => format!("{name} ({arch})"),
                        Err(error) => error.clone(),
                    }
                ),
            );
        }
        // The TAG driver is machine-wide and absent on a fresh install; the core component
        // carries its files, so this can only run once they are on disk.
        let driver_ready = cfg!(test) || components::driver_installed();
        let driver_installing = false;
        let rvc_runtime_installed = cfg!(test) || components::rvc_installed(&runtime_root);
        let rvc_models = if cfg!(test) {
            vec![]
        } else {
            rvc::models(&runtime_root).unwrap_or_default()
        };
        let rvc_name = rvc_models
            .iter()
            .find(|model| model.slot == controls.rvc_options.slot)
            .map(|model| model.name.clone())
            .unwrap_or_default();
        let mut app = Self {
                soundpad_page: args.iter().any(|s| s == "--ui-soundpad"),
                studio_page: args.iter().any(|s| s == "--ui-studio"),
                studio_folder,
                studio_samples,
                studio_selected,
                studio_events,
                studio_bpm,
                studio_volume,
                studio_zoom,
                studio_loop,
                studio_live: studio_loop,
                studio_rebuild: false,
                studio_loop_start,
                studio_loop_end,
                studio_loop_drag: None,
                studio_stroke: None,
                studio_erasing: false,
                studio_delete: None,
                studio_library_open: false,
                studio_bar: 0,
                studio_cursor: 0.0,
                studio_scrubbing: false,
                studio_follow: true,
                studio_play_bpm: studio_bpm,
                studio_scroll: {
                    let viewport = if Self::window_size().width < 846.0 { 240.0 } else { 300.0 };
                    (view::studio_offset(studio::ROOT_NOTE, studio::ZOOMS[studio_zoom], viewport), viewport)
                },
                studio_generation: 0,
                studio_busy: false,
                studio_note: String::new(),
                sound_folder,
                sounds,
                sound_volume,
                sound_monitor,
                sound_normalize,
                sound_filter: String::new(),
                sound_sort,
                sound_scroll: (0.0, 800.0),
                sound_hover: None,
                scroll_anims: std::collections::HashMap::new(),
                scroll_pending: std::collections::HashMap::new(),
                sections,
                section: Selection::All,
                section_name: String::new(),
                dragging: None,
                drag_over: None,
                sound_stop_key,
                sound_generation: 0,
                sound_live: Arc::new(AtomicU32::new(0)),
                sound_pending_play: None,
                sound_playing: (0, 0.0, 0.0),
                sound_note,
                sound_dialog: false,
                clips,
                clips_folder,
                clip_generation: 0,
                clip_loads: 0,
                clip_live: Arc::new(AtomicU32::new(0)),
                clip_menu: None,
                clip_pending_play: None,
                clip_note: String::new(),
                monitor_peak: 0.0,
                engine,
                settings,
                window,
                window_focused: false,
                own_minimize: false,
                hidden_window: None,
                inputs: vec![],
                outputs: vec![],
                input: None,
                output: None,
                headphone_page: args.iter().any(|s| s == "--ui-headphones"),
                effects_page: args.iter().any(|s| s == "--ui-effects"),
                route_open: false,
                reverse_word: "Привет".into(),
                reverse_edit: false,
                epoch: Instant::now(),
                opened_at: None,
                in_peak: 0.0,
                keys_down: [0; 4],
                page_shift: None,
                repaint_all: false,
                morph: None,
                update_version: None,
                intro,
                rehearse_after_quit: false,
                headphone_output: None,
                headphone_denoise,
                headphone_intensity,
                headphone_volume,
                headphone_pitch,
                headphone_reverse,
                headphone_state: 0,
                headphone_busy: false,
                headphone_message: String::new(),
                controls,
                rvc_models,
                runtime_root,
                component_root: component_root.clone(),
                core_installing,
                driver_installing,
                driver_ready,
                devices_known: false,
                core_present,
                models_present,
                setup_error: String::new(),
                driver_error: String::new(),
                transfer: Vec::new(),
                transfer_stage: None,
                transfer_moved: Instant::now(),
                device_state:if driver_ready{engine::DeviceState::Starting}else{engine::DeviceState::WaitingDriver},
                device_detail:String::new(),
                repair_confirm:args.iter().any(|s|s=="--ui-repair"),
                repair_reinstall:false,
                repair_lines:false,
                repair_resume:None,
                repair_started:false,
                quit_after_repair:false,
                resume_monitor:None,
                report_sending: false,
                logs_page: args.iter().any(|s| s == "--ui-logs"),
                denoiser: (0, String::new()),
                logs_text: String::new(),
                logs_copied: false,
                logged_message: String::new(),
                gpu,
                rvc_runtime_installed,
                rvc_runtime_installing: false,
                keys,
                phrase_state: 0,
                discord_state: 0,
                discord_source: false,
                discord_message: String::new(),
                phrase_seconds: 0.0,
                version,
                buffer,
                period,
                graphs,
                snapshot: Snapshot::default(),
                monitor: 0,
                monitor_all: false,
                effects_monitor,
                boost_monitor,
                monitor_message: String::new(),
                message: String::new(),
                details: args.iter().any(|s| s == "--ui-settings" || s=="--ui-repair"),
                rvc_page: args.iter().any(|s| s == "--ui-rvc"),
                rvc_advanced: false,
                rvc_importing: false,
                rvc_import_note: String::new(),
                rvc_name,
                rvc_delete_confirm: false,
                update_checking: !cfg!(test),
                update_ready: false,
                update_status: if cfg!(test) {
                    String::new()
                } else {
                    "Проверяем обновления…".into()
                },
                apply_after_quit: false,
                update_resume: maintenance::take_resume(),
                apply_pending: false,
                busy: false,
                quitting: false,
                binding: None,
                candidate: 0,
                bind_conflict: None,
                auto_started: false,
                recovery: Recovery::default(),
                focus: focus::NONE,
                focus_visible: false,
                dirty: None,
                hint_shown,
                autostart,
                autostart_busy,
                task_warning: String::new(),
                app_autostart,
                pixel_shift,
                slider_idle,
                tray_ok: true,
                peak: 0.0,
                ticks: 0,
                capture_path,
                capture_started: false,
                qa_scale,
                benchmark: cfg!(test) || args.iter().any(|s| s == "--ui-benchmark"),
                usage: (Instant::now(), 0),
                measurements: "state,cpu_one_core_percent,working_set_mb\n".into(),
        };
        app.sanitize_sound_keys();
        let preload = app.sync_sound_bindings();
        let logs = if app.logs_page { app.load_logs(false) } else { Task::none() };
        Ok(Some((
            app,
            Task::batch([
                open,
                timer(true),
                preload,
                logs,
                if cfg!(test) {
                    Task::none()
                } else {
                    Task::perform(async { updater::check_and_download() }, Msg::UpdateChecked)
                },
                if core_installing {
                    Task::perform(
                        async move { components::install_core(&component_root, arch.as_deref()) },
                        Msg::CoreInstalled,
                    )
                } else {
                    Task::none()
                },
                if autostart_busy {
                    Task::perform(async move { engine::tag_autostart(i32::from(autostart)) }, Msg::AutostartUpdated)
                } else {
                    Task::none()
                },
            ]),
        )))
    }
    fn config(&self) -> Option<Config> {
        Some(Config {
            input: self.input.as_ref()?.id.clone(),
            output: self.output.as_ref()?.id.clone(),
            version: self.version,
            buffer: self.buffer,
            period: self.period,
            graphs: self.graphs,
            intensity: self.controls.intensity,
        })
    }
    fn save(&mut self) {
        if let Some(d) = &self.headphone_output {
            self.settings.set("headphones", "output", &d.id);
        }
        self.settings
            .set("headphones", "denoise", self.headphone_denoise as i32);
        self.settings.set(
            "headphones",
            "intensity",
            (self.headphone_intensity * 100.0).round() as i32,
        );
        self.settings.set(
            "headphones",
            "volume",
            (self.headphone_volume * 100.0).round() as i32,
        );
        self.settings
            .set("headphones", "pitch", self.headphone_pitch);
        self.settings
            .set("headphones", "reverse", self.headphone_reverse as i32);
        if self.benchmark {
            self.dirty = None;
            return;
        }
        if let Some(d) = &self.input {
            self.settings.set("audio", "input", &d.id);
        }
        if let Some(d) = &self.output {
            self.settings.set("audio", "output", &d.id);
        }
        for (k, v) in [
            ("version", self.version),
            ("buffer_ms", self.buffer as i32),
            (
                "intensity",
                (self.controls.intensity * 100.0).round() as i32,
            ),
            (
                "alternate_intensity",
                (self.controls.alternate_intensity * 100.0).round() as i32,
            ),
        ] {
            self.settings.set("audio", k, v);
        }
        for (k, v) in [
            ("volume", 100),
            ("boost", (self.controls.boost * 100.0).round() as i32),
            ("overload", self.controls.overload as i32),
            (
                "discord_volume",
                discord_volume_percent(self.controls.discord_volume).round() as i32,
            ),
            ("discord_volume_scale", 2),
            ("pitch", self.controls.pitch),
            ("boost_key", self.keys[0] as i32),
            ("pitch_key", self.keys[1] as i32),
            ("slow_key", self.keys[2] as i32),
            ("fast_key", self.keys[3] as i32),
            ("reverse_key", self.keys[4] as i32),
            ("discord_boost_key", self.keys[5] as i32),
            ("discord_pitch_key", self.keys[6] as i32),
            ("discord_slow_key", self.keys[7] as i32),
            ("discord_fast_key", self.keys[8] as i32),
            ("discord_reverse_key", self.keys[9] as i32),
            ("monitor_key", self.keys[10] as i32),
            ("replay_key", self.keys[11] as i32),
            ("noise_key", self.keys[12] as i32),
            ("monitor_effects", self.effects_monitor as i32),
            ("monitor_boost", self.boost_monitor as i32),
            ("slow_speed", (self.controls.slow * 100.0).round() as i32),
            ("fast_speed", (self.controls.fast * 100.0).round() as i32),
            ("rvc_enabled", self.controls.rvc as i32),
        ] {
            self.settings.set("effects", k, v);
        }
        self.settings.set("ui", "tray_hint", self.hint_shown as i32);
        self.settings.set("ui", "app_autostart", self.app_autostart as i32);
        self.settings.set("ui", "page_pixelate", self.pixel_shift as i32);
        self.settings.set("ui", "slider_idle", self.slider_idle as i32);
        self.settings.set("ui", "tag_autostart", self.autostart as i32);
        if let Some(folder) = &self.sound_folder {
            self.settings
                .set("soundpad", "folder", folder.to_string_lossy());
        }
        let sound_percent = (self.sound_volume * 100.0).round() as i32;
        self.settings.set("soundpad", "volume_display", sound_percent);
        self.settings.set("soundpad", "volume", (sound_percent + 2) / 5);
        self.settings
            .set("soundpad", "monitor", self.sound_monitor as i32);
        self.settings
            .set("soundpad", "normalize", self.sound_normalize as i32);
        self.settings
            .set("soundpad", "stop_key", self.sound_stop_key as i32);
        self.settings
            .set("soundpad", "sort", self.sound_sort.code());
        self.settings.set(
            "soundpad",
            "sections",
            soundpad::serialize_sections(&self.sections),
        );
        self.settings
            .set("soundpad", "sounds", soundpad::serialize(&self.sounds));
        self.settings.set("studio", "bpm", self.studio_bpm);
        self.settings.set("studio", "volume", self.studio_volume);
        self.settings.set("studio", "zoom", studio::ZOOMS[self.studio_zoom]);
        self.settings.set("studio", "loop", self.studio_loop as i32);
        self.settings.set("studio", "loop_start", self.studio_loop_start);
        self.settings.set("studio", "loop_end", self.studio_loop_end);
        self.settings.set("studio", "events", serde_json::to_string(&self.studio_events).unwrap_or_default());
        self.controls.rvc_options.save(&mut self.settings);
        self.engine
            .save(self.settings.path.clone(), self.settings.text());
        self.dirty = None;
    }
    fn headphone_changed(&mut self) {
        self.engine.headphone_controls(
            self.headphone_intensity,
            self.headphone_volume,
            self.headphone_pitch,
            self.headphone_reverse,
        );
        self.dirty = Some(Instant::now());
    }
    fn headphone_outputs(&self) -> Vec<Device> {
        self.outputs
            .iter()
            .filter(|d| {
                let name = d.name.to_lowercase();
                d.id != "TAG"
                    && ![
                        "thin audio",
                        "mic noize",
                        "cable",
                        "voicemeeter",
                        "broadcast",
                    ]
                    .iter()
                    .any(|v| name.contains(v))
            })
            .cloned()
            .collect()
    }
    fn changed(&mut self) {
        self.engine.controls(self.controls);
        self.dirty = Some(Instant::now());
    }
    fn running(&self) -> bool {
        matches!(self.snapshot.state, 1..=4)
    }
    /// Shows a headphone failure in the panel and keeps it in app.log once: the host swallows the
    /// reason, so the log is all a report from another PC carries.
    fn headphone_failed(&mut self, error: String) {
        if error != self.headphone_message && !cfg!(test) {
            logs::note(&self.runtime_root, &format!("Наушники: {error}"));
        }
        self.headphone_message = error;
    }
    /// The host refused the headphone line. On a fresh install the driver's three lines are its
    /// own two defaults and the microphone, so there is no slot; the device repair frees one.
    fn headphone_needs_lines(&self) -> bool {
        self.headphone_message.contains("Headphone host rejected request")
    }
    /// Something the voice needs is still missing or failed to install.
    fn setup_pending(&self) -> bool {
        self.core_installing || !self.setup_error.is_empty() || !self.driver_ready || self.driver_installing
    }
    /// Everything is installed, yet the voice cannot start until the user picks a device:
    /// `auto_start` needs a microphone and TAG or Voicemeeter as the output.
    fn start_blocked(&self) -> bool {
        self.devices_known
            && !self.running()
            && !self.busy
            && (self.input.is_none() || self.output.as_ref().is_none_or(|o| o.id != "TAG" && !o.name.contains("Voicemeeter")))
    }
    /// The setup card shows only for a problem (or an install running): once everything is in
    /// place it leaves the page, and it comes back if something goes missing later.
    fn setup_visible(&self) -> bool {
        self.setup_pending() || self.start_blocked()
    }
    /// After the shrink: the real update, or the rehearsal's stand-in watcher.
    fn hand_over(&self, center: Option<iced::Point>) -> Task<Msg> {
        if !self.rehearse_after_quit {
            return self.apply_update(center);
        }
        let runtime = self.runtime_root.clone();
        Task::perform(async move { update_window::rehearse(&runtime, center) }, Msg::RehearsalReady)
    }
    /// Hands the update to Velopack and the watcher; `center` places the watcher's update
    /// window (centred on screen when the app window is not shown).
    fn apply_update(&self, center: Option<iced::Point>) -> Task<Msg> {
        let intent = self.update_resume;
        let at = center.map_or("centered".to_owned(), |c| format!("{:.1},{:.1}", c.x, c.y));
        Task::batch([Task::perform(async move { updater::apply_and_restart(intent, Some(at)) }, Msg::UpdateApplied), timer(false)])
    }
    /// The window background. tiny-skia repaints every damaged region separately with all
    /// that overlaps it; a page swap damages 7–30 regions and cost 10–120 ms a frame, against
    /// 2–10 ms for one full pass. A changed background is its only switch to a full pass, so
    /// `repaint_all` flips the lowest mantissa bit: a different colour, the same pixels.
    fn backdrop(&self) -> iced::Color {
        iced::Color { r: f32::from_bits(view::BG.r.to_bits() ^ self.repaint_all as u32), ..view::BG }
    }
    fn page_key(&self) -> [bool; 6] {
        [self.soundpad_page, self.logs_page, self.details, self.rvc_page, self.effects_page, self.studio_page]
    }
    fn ui_active(&self) -> bool {
        self.window.is_some() && self.window_focused
    }
    fn reveal_studio(&mut self, note: u8) -> Task<Msg> {
        let zoom = studio::ZOOMS[self.studio_zoom];
        let viewport = self.studio_scroll.1;
        self.studio_scroll.0 = view::studio_offset(note, zoom, viewport);
        view::studio_reveal(note, zoom, viewport)
    }
    fn studio_loop_range(&self) -> Option<(usize, usize)> {
        self.studio_loop.then(|| (studio::frame_at(self.studio_loop_start, self.studio_play_bpm),
            studio::frame_at(self.studio_loop_end, self.studio_play_bpm)))
    }
    fn sync_studio_loop(&self) {
        self.engine.sound_loop(studio::TRACK_ID, self.studio_loop_range());
    }
    fn seek_studio(&mut self, step: usize) {
        let step = step.min(studio::STEPS - 1);
        self.studio_cursor = step as f32;
        self.studio_bar = step / 16;
        self.studio_follow = true;
        if self.running() {
            let bpm = if self.sound_playing.0 == studio::TRACK_ID { self.studio_play_bpm } else { self.studio_bpm };
            self.engine.sound_seek(studio::TRACK_ID, step as f32 * 15.0 / bpm as f32);
        }
    }
    /// Native monitor mode: 1 is the full voice, otherwise 1 + mask (1 effects, 2 boost, 4 sounds).
    fn monitor_mode(&self) -> i32 {
        if self.monitor_all {
            return 1;
        }
        let mask = self.effects_monitor as i32 | (self.boost_monitor as i32) << 1
            | (self.sound_monitor as i32) << 2;
        if mask == 0 { 0 } else { 1 + mask }
    }
    fn effect_monitoring(&self) -> bool {
        self.effects_monitor || self.boost_monitor || self.sound_monitor
    }
    /// Every hotkey in use, as (`Msg::Bind` target, key).
    fn all_keys(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        self.keys
            .iter()
            .enumerate()
            .map(|(i, &k)| (i, k))
            .chain(std::iter::once((SOUND_STOP_BIND, self.sound_stop_key)))
            .chain(
                self.sounds
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (SOUND_BIND_BASE + i, s.key)),
            )
    }
    fn key_taken(&self, target: usize, key: u32) -> bool {
        key != 0 && self.all_keys().any(|(t, k)| t != target && k == key)
    }
    fn sanitize_sound_keys(&mut self) {
        let valid = |k: u32| (3..=254).contains(&(k & 255));
        if self.sound_stop_key != 0
            && (!valid(self.sound_stop_key) || self.keys.contains(&self.sound_stop_key))
        {
            self.sound_stop_key = 0;
        }
        for i in 0..self.sounds.len() {
            let key = self.sounds[i].key;
            if key != 0 && (!valid(key) || self.key_taken(SOUND_BIND_BASE + i, key)) {
                self.sounds[i].key = 0;
            }
        }
    }
    fn sound_id(index: usize) -> u32 {
        index as u32 + 1
    }
    /// Push the clip hotkeys to the engine and start decoding every bound clip not yet loaded.
    fn sync_sound_bindings(&mut self) -> Task<Msg> {
        let mut bindings: Vec<(u32, u32)> = self
            .sounds
            .iter()
            .enumerate()
            .filter(|(_, s)| s.key != 0)
            .map(|(i, s)| (Self::sound_id(i), s.key))
            .collect();
        if self.sound_stop_key != 0 {
            bindings.push((0, self.sound_stop_key));
        }
        if !self.engine.sound_bindings(&bindings) {
            self.message = "Хоткеи звуков отклонены движком".into();
        }
        let bound: Vec<usize> = (0..self.sounds.len())
            .filter(|&i| self.sounds[i].key != 0 && self.sounds[i].state == SoundState::Unloaded)
            .collect();
        Task::batch(bound.into_iter().map(|i| self.load_sound(i)))
    }
    fn clip_id(index: usize) -> u32 {
        CLIP_ID_BASE + index as u32
    }
    /// Re-read the recordings folder; older files beyond [`CLIPS_KEPT`] are deleted.
    fn bump_sounds(&mut self) {
        self.sound_generation += 1;
        self.sound_live.store(self.sound_generation, Ordering::Release);
    }
    fn bump_clips(&mut self) {
        self.clip_loads += 1;
        self.clip_live.store(self.clip_loads, Ordering::Release);
    }
    /// The loudness window new decodes use, if levelling is on.
    fn sound_window(&self) -> Option<(f32, f32)> {
        // The flag lives here, not in `settings`: those only catch up on the next save.
        self.sound_normalize.then(|| soundpad::window(&self.settings))
    }
    fn rescan_clips(&mut self) {
        self.bump_clips();
        self.clip_menu = None;
        self.clip_note.clear();
        self.clip_pending_play = None;
        self.clips = newest_clips(&self.clips_folder);
    }
    fn load_clip(&mut self, index: usize) -> Task<Msg> {
        let live = self.clip_live.clone();
        let Some(clip) = self.clips.get_mut(index) else {
            return Task::none();
        };
        if clip.state == SoundState::Loading {
            return Task::none();
        }
        clip.state = SoundState::Loading;
        let path = clip.path.clone();
        let rendered = clip.name.contains(" (mix)");
        let (loader, id, generation) = (
            self.engine.sound_loader(),
            Self::clip_id(index),
            self.clip_loads,
        );
        Task::perform(
            async move {
                let mut pcm = soundpad::decode(&path)?;
                // Old files have no source metadata; use the former Discord 100% gain.
                if !rendered {
                    for sample in &mut pcm {
                        *sample *= DISCORD_VOLUME_AT_100;
                    }
                }
                load_if_live(&loader, &live, generation, id, &pcm, 1.0)?;
                Ok(pcm.len() as f32 / soundpad::RATE as f32)
            },
            move |result| Msg::ClipLoaded(index, generation, result),
        )
    }
    /// Copy recording `index` into `folder` under its own name.
    fn copy_clip(&mut self, index: usize, folder: &Path) {
        let Some(clip) = self.clips.get(index) else {
            return;
        };
        let target = unique_path(folder, &clip.name);
        self.clip_note = match std::fs::copy(&clip.path, &target) {
            Ok(_) => format!(
                "Сохранено: {}",
                target.file_name().unwrap_or_default().to_string_lossy()
            ),
            Err(e) => format!(
                "Не удалось сохранить: {e}"
            ),
        };
    }
    fn load_sound(&mut self, index: usize) -> Task<Msg> {
        let (window, live) = (self.sound_window(), self.sound_live.clone());
        let Some(sound) = self.sounds.get_mut(index) else {
            return Task::none();
        };
        if sound.state == SoundState::Loading {
            return Task::none();
        }
        sound.state = SoundState::Loading;
        let (path, gain) = (sound.path.clone(), sound.gain());
        let (loader, id, generation) = (
            self.engine.sound_loader(),
            Self::sound_id(index),
            self.sound_generation,
        );
        Task::perform(
            async move {
                let pcm = decode_for(&path, window)?;
                load_if_live(&loader, &live, generation, id, &pcm, gain)?;
                Ok(pcm.len() as f32 / soundpad::RATE as f32)
            },
            move |result| Msg::SoundLoaded(index, generation, result),
        )
    }
    /// Re-read the folder, keeping keys and gains of clips that are still there.
    fn rescan_sounds(&mut self) -> Task<Msg> {
        self.bump_sounds();
        self.sound_pending_play = None;
        self.engine.sound_clear();
        // sound_clear drops the loaded recordings too; they decode again on the next play, and
        // a recording still decoding must not mark itself loaded.
        self.bump_clips();
        self.clip_pending_play = None;
        for clip in &mut self.clips {
            clip.state = SoundState::Unloaded;
        }
        let Some(folder) = self.sound_folder.clone() else {
            self.sounds.clear();
            return Task::none();
        };
        let saved: Vec<soundpad::Entry> = self
            .sounds
            .iter()
            .map(|s| soundpad::Entry {
                name: s.name.clone(),
                key: s.key,
                volume: s.volume,
                played: s.played,
            })
            .collect();
        match soundpad::scan(&folder, &saved) {
            Ok(sounds) => {
                self.sounds = sounds;
                self.sound_note = if self.sounds.is_empty() {
                    "В папке нет mp3, wav или ogg. Нажмите «Добавить звуки».".into()
                } else {
                    String::new()
                };
            }
            Err(e) => {
                self.sounds.clear();
                self.sound_note = format!("Папка недоступна: {e}");
            }
        }
        self.sanitize_sound_keys();
        self.dirty = Some(Instant::now());
        self.sync_sound_bindings()
    }
    /// Indices of the clips shown by the sidebar selection and the text filter, in the
    /// selected order.
    fn visible_sounds(&self) -> Vec<usize> {
        let filter = self.sound_filter.trim().to_lowercase();
        let custom = match &self.section {
            Selection::Custom(i) => self.sections.get(*i),
            _ => None,
        };
        soundpad::order(&self.sounds, self.sound_sort)
            .into_iter()
            .filter(|&i| {
                let sound = &self.sounds[i];
                (filter.is_empty() || sound.name.to_lowercase().contains(&filter))
                    && match &self.section {
                        Selection::All => true,
                        Selection::Group(key) => {
                            soundpad::prefix(&sound.name).is_some_and(|(_, k)| k == *key)
                        }
                        Selection::Custom(_) => {
                            custom.is_some_and(|s| s.files.contains(&sound.name))
                        }
                    }
            })
            .collect()
    }
    /// Sidebar entries: everything, automatic prefix groups, then the user's sections.
    fn section_items(&self) -> Vec<SectionItem> {
        let mut items = vec![SectionItem {
            label: "Все звуки".into(),
            count: self.sounds.len(),
            selection: Selection::All,
        }];
        items.extend(soundpad::groups(&self.sounds).into_iter().map(|g| SectionItem {
            label: g.label,
            count: g.count,
            selection: Selection::Group(g.key),
        }));
        items.extend(self.sections.iter().enumerate().map(|(i, s)| SectionItem {
            label: s.name.clone(),
            count: s
                .files
                .iter()
                .filter(|f| self.sounds.iter().any(|sound| sound.name == **f))
                .count(),
            selection: Selection::Custom(i),
        }));
        items
    }
    fn custom_section(&self) -> Option<usize> {
        match self.section {
            Selection::Custom(i) if i < self.sections.len() => Some(i),
            _ => None,
        }
    }
    fn select_section(&mut self, selection: Selection) {
        self.section_name.clear();
        self.section = selection;
    }
    /// Every new text in the status line goes to app.log once; a countdown is the same text.
    fn log_message(&mut self) {
        let key: String = self.message.chars().filter(|c| !c.is_ascii_digit()).collect();
        if key != self.logged_message {
            if !self.message.is_empty() && !cfg!(test) {
                logs::note(&self.runtime_root, &self.message);
            }
            self.logged_message = key;
        }
    }
    fn load_logs(&mut self, copy: bool) -> Task<Msg> {
        self.log_message();
        let state = match self.snapshot.state {
            0 => "остановлен",
            1 => "запуск",
            2 | 3 => "работает",
            4 => "остановка",
            _ => "ошибка",
        };
        let header = format!(
            "Mic Noize {}\nGPU: {}\nДвижок: {state}; шумодав {}; выход {}; модель v{}; буфер {} мс\n\
             Виртуальный микрофон: {}; runtime {}\nПрослушивание: {} {}\nСтатус: {}",
            env!("CARGO_PKG_VERSION"),
            match &self.gpu {
                Ok((arch, name)) => format!("{name} ({arch})"),
                Err(error) => error.clone(),
            },
            match self.denoiser.0 {
                1 => "NVIDIA".to_owned(),
                2 => format!("выключен ({})", self.denoiser.1),
                3 => format!("DeepFilterNet на CPU ({})", self.denoiser.1),
                4 => format!("на входе ({})", self.denoiser.1),
                _ => "-".to_owned(),
            },
            self.output.as_ref().map_or("не выбран", |d| d.name.as_str()),
            self.version,
            self.buffer,
            if self.driver_ready { "драйвер установлен" } else { "драйвер НЕ установлен" },
            if self.core_installing { "загружается" } else { "готов" },
            self.monitor,
            self.monitor_message,
            if self.message.is_empty() { "-" } else { &self.message },
        );
        let runtime = self.runtime_root.clone();
        Task::perform(
            async move { logs::report(&runtime, &header) },
            move |text| Msg::Logs(text, copy),
        )
    }
    fn processing_intent(&self)->ResumeIntent {
        ResumeIntent{microphone:self.running()||self.busy,headphones:matches!(self.headphone_state,1|2)||self.headphone_busy,
            monitor:if matches!(self.monitor,1|2){self.monitor_mode()}else{0},full_monitor:self.monitor_all}
    }
    fn resume_processing(&mut self,intent:ResumeIntent) {
        if self.controls.rvc {self.engine.rvc(true,self.controls.rvc_options);}
        self.auto_started = !intent.microphone;
        self.resume_monitor=(intent.monitor!=0).then_some((intent.monitor,intent.full_monitor));
        if intent.microphone {self.auto_start();}
        if intent.headphones && let Some(output)=&self.headphone_output {
            self.headphone_busy=true;self.engine.headphones(true,output.id.clone(),self.headphone_denoise);
        }
    }
    fn auto_start(&mut self) {
        if self.auto_started
            || self.quitting
            || self.benchmark
            || self.busy
            || self.running()
            || self.core_installing
            || self.driver_installing
            || !self.driver_ready
        {
            return;
        }
        let Some(c) = self.config() else { return };
        let output = self.output.as_ref().unwrap();
        if output.id != "TAG" && !output.name.contains("Voicemeeter") {
            self.message =
                "Автозапуск остановлен: выберите TAG или Voicemeeter в настройках.".into();
            return;
        }
        self.auto_started = true;
        self.busy = true;
        self.snapshot.state = 1;
        self.save();
        self.engine.start(c);
    }
    fn update(&mut self, msg: Msg) -> Task<Msg> {
        if matches!(&msg, Msg::Page(_) | Msg::Hide | Msg::Minimize
            | Msg::WindowFocus(_, false) | Msg::SoundpadFilter(_) | Msg::SoundpadSort(_)
            | Msg::SectionSelect(_) | Msg::SoundpadRefresh | Msg::SoundpadNormalize(_)
            | Msg::SoundpadPicked(_, _))
        {
            self.scroll_anims.clear();
            self.scroll_pending.clear();
            self.sound_hover = None;
            self.studio_scrubbing = false;
            self.studio_loop_drag = None;
            self.studio_stroke = None;
            self.studio_erasing = false;
            // A rebuilt clip list repaints in one pass, as a page swap does. Not on hiding or
            // focus loss (switching to a game): no extra frame there.
            self.repaint_all ^= !matches!(&msg, Msg::Page(_) | Msg::Hide | Msg::Minimize | Msg::WindowFocus(..));
        }
        match msg {
            Msg::Tick => {
                self.ticks += 1;
                self.keys_down = if self.ui_active() && !cfg!(test) {
                    let bound = self.keys.iter().copied().chain(self.sounds.iter().map(|s| s.key)).chain([self.sound_stop_key]);
                    held_keys(bound)
                } else {
                    [0; 4]
                };
                if !cfg!(test) && self.ticks.is_multiple_of(20) {
                    self.engine.request_device_state();
                    let warning = engine::tag_task_warning();
                    if warning != self.task_warning {
                        if self.message == self.task_warning { self.message.clear(); }
                        if !warning.is_empty() {
                            logs::note(&self.runtime_root, &warning);
                            if self.message.is_empty() { self.message = warning.clone(); }
                        }
                        self.task_warning = warning;
                    }
                }
                self.log_message();
                let status = components::status();
                if self.rvc_runtime_installing
                    && let Some(share) = status.share()
                {
                    self.rvc_import_note = format!("RVC runtime: {} {}%", status.phase.label(), (share * 100.0) as u8);
                }
                if self.core_installing {
                    let now = Instant::now();
                    let last = self.transfer.last().map(|&(_, done)| done);
                    // A new phase starts over; so does a jump no line could make in one tick: the
                    // parts already on disk after a restart count at once and are no speed.
                    if self.transfer_stage != Some((status.item, status.phase)) || last.is_some_and(|done| status.done > done + 32 * 1_048_576) {
                        self.transfer.clear();
                        self.transfer_stage = Some((status.item, status.phase));
                        self.transfer_moved = now;
                    }
                    if last.is_some_and(|done| status.done != done) {
                        self.transfer_moved = now;
                    }
                    self.transfer.retain(|&(at, _)| now.duration_since(at) < Duration::from_secs(4));
                    self.transfer.push((now, status.done));
                }
                if self.benchmark {
                    if self.ticks==4 && self.repair_confirm {
                        if std::env::args().any(|arg|arg=="--ui-repair-lines") {self.repair_lines=true;}
                        self.focus=focus::settings::REPAIR_CONFIRM;
                        return Task::batch([view::reveal_focus(),timer(false)]);
                    }
                    if self.ticks == 8 {
                        self.usage = (Instant::now(), engine::usage().0);
                        if self.repair_confirm && std::env::args().any(|arg|arg=="--ui-repair-run") {
                            return Task::batch([self.update(Msg::RepairConfirm),timer(false)]);
                        }
                    }
                    if [24, 44, 60].contains(&self.ticks) {
                        let (cpu, memory) = engine::usage();
                        let name = match self.ticks {
                            24 => "open_stopped",
                            44 => "tray_stopped",
                            _ => "reopened_stopped",
                        };
                        self.measurements.push_str(&format!(
                            "{name},{:.3},{:.2}\n",
                            (cpu - self.usage.1) as f64
                                / 100_000.0
                                / self.usage.0.elapsed().as_secs_f64(),
                            memory as f64 / 1048576.0
                        ));
                        self.usage = (Instant::now(), cpu);
                        if self.ticks == 60 {
                            let path = self
                                .settings
                                .path
                                .parent()
                                .unwrap()
                                .join("results/rust-ui-resources.csv");
                            if let Err(e) = std::fs::write(path, &self.measurements) {
                                self.message = e.to_string();
                            }
                            return Task::batch([self.update(Msg::Quit), timer(false)]);
                        }
                        let action = if self.ticks == 24 {
                            Msg::Hide
                        } else {
                            Msg::Show
                        };
                        return Task::batch([self.update(action), timer(false)]);
                    }
                }
                while let Some(reply) = self.engine.reply() {
                    match reply {
                        Reply::Host(result) => { if let Err(error) = result { self.message = error; } }
                        Reply::Started(_, result) => {
                            self.busy = false;
                            self.monitor_all = false;
                            self.engine.controls(self.controls);
                            if let Err(e) = result {
                                self.message = e;
                                self.snapshot.state = 5;
                            } else {
                                self.message.clear();
                                if let Some((mode,full))=self.resume_monitor.take() {
                                    self.monitor_all=full;self.engine.monitor(mode);
                                } else if self.effect_monitoring() { self.engine.monitor(self.monitor_mode()); }
                            }
                        }
                        Reply::Quit => {
                            if self.repair_resume.is_some() && !self.repair_started {
                                self.repair_started=true;
                                let root=self.runtime_root.clone();let reinstall=self.repair_reinstall;let lines=self.repair_lines;
                                return Task::batch([Task::perform(async move {maintenance::repair(&root,reinstall,lines)},Msg::Repaired),timer(false)]);
                            }
                            telemetry::record_blocking(
                                self.settings.path.parent().unwrap(),
                                "session-end",
                                serde_json::json!({
                                    "update": self.apply_after_quit,
                                    "underruns": self.snapshot.underruns,
                                    "drops": self.snapshot.drops,
                                    "process_ms": self.snapshot.process_ms,
                                    "queue_ms": self.snapshot.queue_ms,
                                    "rvc_latency_ms": self.snapshot.rvc_latency_ms,
                                }),
                            );
                            if self.apply_after_quit || self.rehearse_after_quit {
                                // Shrink into the update window first when the window is on screen.
                                if let Some(id)=self.window.filter(|_|!cfg!(test)) {
                                    return window::position(id).then(move |position| window::size(id).map(move |size| Msg::DecayGeometry(position,size)));
                                }
                                return self.hand_over(None);
                            }
                            if !cfg!(test) { logs::note(&self.runtime_root,"Выход: движок остановлен, завершение UI"); }
                            return exit_ui();
                        }
                        Reply::Saved(result) => {
                            if let Err(e) = result {
                                self.message = format!("Настройки не сохранены: {e}");
                            }
                        }
                        Reply::Monitor(result) => {
                            if let Err(e) = result {
                                self.message = format!("Прослушивание: {e}");
                            }
                        }
                        Reply::Headphones(result) => {
                            self.headphone_busy = false;
                            if let Err(e) = result {
                                self.headphone_failed(e);
                                self.headphone_state = 3;
                            }
                            self.engine.refresh();
                        }
                        Reply::Rvc(result) => {
                            if let Err(e) = result {
                                self.controls.rvc = false;
                                self.engine.controls(self.controls);
                                self.message = format!("Ошибка RVC: {e}");
                                self.dirty = Some(Instant::now());
                            }
                        }
                        Reply::DeviceState(_,state,detail) => {
                            self.device_state=if self.driver_ready{state}else{engine::DeviceState::WaitingDriver};
                            self.device_detail=detail;
                        }
                        Reply::Devices(result) => match result {
                            Ok((i, o)) => {
                                self.devices_known = true;
                                let input_id = self
                                    .input
                                    .as_ref()
                                    .map(|d| d.id.as_str())
                                    .or(self.settings.get("audio", "input"));
                                let output_id = self
                                    .output
                                    .as_ref()
                                    .map(|d| d.id.as_str())
                                    .or(self.settings.get("audio", "output"))
                                    .unwrap_or("TAG");
                                // A saved microphone must match exactly or stay unselected; without
                                // one, prefer HyperX, then the Windows default (first in the list).
                                self.input = match input_id {
                                    Some(id) => i.iter().find(|d| d.id == id),
                                    None => i
                                        .iter()
                                        .find(|d| d.name.contains("HyperX"))
                                        .or_else(|| i.first()),
                                }
                                .cloned();
                                self.output = o.iter().find(|d| d.id == output_id).cloned();
                                self.inputs = i;
                                if self.headphone_output.is_none() {
                                    self.headphone_output = o
                                        .iter()
                                        .find(|d| {
                                            Some(d.id.as_str())
                                                == self.settings.get("headphones", "output")
                                        })
                                        .cloned();
                                }
                                self.outputs = o;
                                if self.recovery.wait_for_device && self.input.is_some() {
                                    self.recovery.due = Some(Instant::now());
                                }
                                if !self.quitting && !self.driver_installing {
                                    if let Some(intent)=self.update_resume.take(){self.resume_processing(intent);}else{self.auto_start();}
                                }
                            }
                            Err(e) => self.message = e,
                        },
                    }
                }
                let (headphone_state, headphone_error) = self.engine.headphone_state();
                if !self.headphone_busy {
                    if headphone_state != 0 || self.headphone_state != 3 {
                        self.headphone_state = headphone_state;
                    }
                    if !headphone_error.is_empty() {
                        self.headphone_failed(headphone_error);
                    }
                }
                let (snapshot, error) = self.engine.snapshot(self.ui_active());
                let studio_just_started = self.snapshot.state != 3 && snapshot.state == 3;
                self.snapshot = snapshot;
                (self.phrase_state, self.phrase_seconds) = self.engine.phrase();
                (
                    self.discord_state,
                    self.discord_source,
                    self.discord_message,
                ) = self.engine.discord_state();
                let denoiser = self.engine.denoiser_state();
                if matches!(denoiser.0, 2..=4) && self.denoiser.0 != denoiser.0 && !cfg!(test) {
                    let text = match denoiser.0 {
                        3 => format!("NVIDIA не используется, DeepFilterNet на CPU: {}", denoiser.1),
                        4 => format!("Вход уже очищен ({}), свой шумодав выключен", denoiser.1),
                        _ => format!("NVIDIA не используется, без шумодава: {}", denoiser.1),
                    };
                    logs::note(&self.runtime_root, &text);
                }
                self.denoiser = denoiser;
                let playing = self.engine.sound_state();
                // Hotkey starts happen natively: the engine state is the one source of "played".
                if playing.0 != 0 && playing.0 != self.sound_playing.0
                    && let Some(sound) = self.sounds.get_mut(playing.0 as usize - 1)
                {
                    sound.played = soundpad::now();
                    self.dirty = Some(Instant::now());
                }
                let studio_finished = self.sound_playing.0 == studio::TRACK_ID
                    && playing.0 != studio::TRACK_ID && !self.studio_loop;
                self.sound_playing = playing;
                if studio_finished {
                    self.studio_live = false;
                    self.studio_cursor = 0.0;
                    if self.studio_follow { self.studio_bar = 0; }
                }
                if self.studio_page && self.ui_active() && playing.0 == studio::TRACK_ID && !self.studio_scrubbing {
                    self.studio_cursor = (playing.1 * self.studio_play_bpm as f32 / 15.0)
                        .clamp(0.0, studio::STEPS as f32);
                    if self.studio_follow {
                        self.studio_bar = (self.studio_cursor as usize / 16).min(studio::BARS - 1);
                    }
                }
                let (monitor, monitor_message) = self.engine.monitor_state();
                if monitor == 3 && self.monitor != 3 {
                    self.message = format!("Прослушивание: {monitor_message}");
                }
                self.monitor = monitor;
                self.monitor_message = monitor_message;
                // `busy` covers an in-flight start: the engine still reports the old failure
                // until `Reply::Started` arrives, which must not count as another one.
                // Only after the first start: until then auto_start owns starting.
                if self.auto_started
                    && !self.busy
                    && !self.quitting
                    && !self.benchmark
                    && !self.core_installing
                    && !self.driver_installing
                {
                    let now = Instant::now();
                    self.recovery.wait_for_device = transient_device_failure(&error);
                    self.recovery.observe(snapshot.state, now);
                    if snapshot.state == 5
                        && self.recovery.take_due(now)
                        && let Some(config) = self.config()
                    {
                        self.busy = true;
                        self.snapshot.state = 1;
                        self.message.clear();
                        self.engine.start(config);
                    }
                }
                // During an in-flight restart the engine still reports the old failure: showing
                // it again would leave a stale red error over a session that then starts fine.
                if snapshot.state == 5 && !self.busy && !error.is_empty() {
                    self.message = match self.recovery.due {
                        Some(at) => format!(
                            "{error} Перезапуск через {} с.",
                            at.saturating_duration_since(Instant::now()).as_secs() + 1
                        ),
                        None if self.recovery.exhausted() => {
                            format!("{error} Автоперезапуск не помог: нажмите «Обновить устройства» в настройках.")
                        }
                        None => error,
                    };
                }
                if self.ui_active() {
                    self.peak = snapshot.output_peak.max(self.peak * 0.80);
                    self.in_peak = snapshot.input_peak.max(self.in_peak * 0.80);
                    self.monitor_peak = self.engine.monitor_peak().max(self.monitor_peak * 0.80);
                }
                if snapshot.captured_key != 0 && self.binding.is_some() {
                    if snapshot.captured_key == u32::MAX {
                        return Task::batch([
                            self.update(Msg::CancelBind),
                            timer(self.ui_active()),
                        ]);
                    }
                    // Capture happens on the button itself: a valid key is taken at once.
                    self.candidate = snapshot.captured_key;
                    return Task::batch([self.update(Msg::AcceptBind), timer(self.ui_active())]);
                }
                let events = self.engine.events();
                if events & 32 != 0 && !self.quitting { self.engine.refresh(); }
                if events & 16 != 0 {
                    self.tray_ok = false;
                    self.message =
                        "Не удалось создать значок трея. Окно останется доступным.".into();
                }
                if events & 8 != 0 && !self.busy && matches!(self.snapshot.state, 2 | 3) {
                    let _ = self.update(Msg::Monitor);
                }
                if restart_requested(events) {
                    RESTART.store(true, Ordering::Relaxed);
                }
                if events & (EXIT_EVENT | RESTART_EVENT) != 0 && !self.quitting {
                    if self.repair_resume.is_some() {self.quit_after_repair=true;}
                    else {self.quitting = true;self.save();self.engine.quit();}
                }
                if self
                    .dirty
                    .is_some_and(|t| t.elapsed() > Duration::from_millis(400))
                {
                    self.save();
                }
                let next = timer(self.ui_active() && self.running());
                if studio_just_started && self.studio_page && self.ui_active() && self.studio_loop
                    && self.studio_live && !self.studio_events.is_empty() {
                    return Task::batch([next, self.update(Msg::StudioRender(false))]);
                }
                if events & 17 != 0 {
                    return Task::batch([next, self.update(Msg::Show)]);
                }
                if let Some((generation, samples)) = self.engine.last_clip(self.clip_generation) {
                    self.clip_generation = generation;
                    let (folder, name) = (self.clips_folder.clone(), clip_name());
                    return Task::batch([
                        next,
                        Task::perform(
                            async move {
                                std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
                                soundpad::write_wav(&unique_path(&folder, &name), &samples)
                            },
                            Msg::ClipRecorded,
                        ),
                    ]);
                }
                if let Some((generation, samples)) = self.engine.studio_clip(self.studio_generation) {
                    self.studio_generation = generation;
                    let folder = self.studio_folder.clone();
                    let name = clip_name().replace("Запись", "Сэмпл").replace(" (mix)", "");
                    return Task::batch([next, Task::perform(async move {
                        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
                        let path = unique_path(&folder, &name);
                        soundpad::write_wav(&path, &samples)?;
                        Ok(path.file_name().unwrap().to_string_lossy().into_owned())
                    }, Msg::StudioRecorded)]);
                }
                if self.capture_path.is_some() && !self.capture_started && self.ticks >= 12 {
                    self.capture_started = true;
                    if let Some(id) = self.window {
                        return Task::batch([next, window::screenshot(id).map(Msg::Screenshot)]);
                    }
                }
                if let Some(id) = self
                    .window
                    .filter(|_| !self.own_minimize && self.tray_ok)
                {
                    return Task::batch([
                        next,
                        window::is_minimized(id).map(move |state| Msg::MinimizedState(id, state)),
                    ]);
                }
                return next;
            }
            Msg::Opened(id) => {
                self.window = Some(id);
                self.opened_at = Some(Instant::now());
                if self.intro.take().is_some() {
                    // Stand in for the watcher's update window first: same card, same place.
                    self.morph = Some(MorphView {
                        base: MorphBase::Card(tacho::BarStage::Launching),
                        from_version: String::new(),
                        to_version: env!("CARGO_PKG_VERSION").into(),
                        anim: None,
                        hwnd: None,
                        center: None,
                    });
                    return window::raw_id::<Msg>(id).map(Msg::Keyed);
                }
                if self.studio_page { return self.reveal_studio(studio::ROOT_NOTE); }
            }
            Msg::WindowFocus(id, focused) => {
                if self.window == Some(id) {
                    self.window_focused = focused;
                    self.own_minimize &= !focused;
                    if !focused {
                        self.studio_scrubbing = false;
                        self.studio_loop_drag = None;
                        self.studio_stroke = None;
                        self.studio_erasing = false;
                    }
                }
            }
            Msg::Minimized(id) => {
                // Like OBS: clicking the taskbar icon of the active window hides it to tray.
                if self.window == Some(id) && !self.own_minimize {
                    return self.update(Msg::Hide);
                }
            }
            Msg::Restored(id) => {
                if self.window == Some(id) {
                    self.own_minimize = false;
                }
            }
            Msg::MinimizedState(id, Some(true)) => {
                if self.window == Some(id) && !self.own_minimize {
                    return self.update(Msg::Hide);
                }
            }
            Msg::MinimizedState(_, _) => {}
            Msg::Hide => {
                if !self.tray_ok {
                    return Task::none();
                }
                self.studio_scrubbing = false;
                self.studio_loop_drag = None;
                self.studio_stroke = None;
                self.studio_erasing = false;
                self.engine.capture(false);
                self.binding = None;
                if !self.hint_shown {
                    self.engine.hint();
                    self.hint_shown = true;
                    self.dirty = Some(Instant::now());
                }
                if let Some(id) = self.window.take() {
                    self.window_focused = false;
                    // Keep the native window alive while audio continues in the tray.
                    self.hidden_window = Some(id);
                    return window::set_mode(id, window::Mode::Hidden);
                }
            }
            Msg::Show => {
                if let Some(id) = self.window {
                    return Task::batch([window::minimize(id, false), window::gain_focus(id)]);
                }
                if let Some(id) = self.hidden_window.take() {
                    self.window = Some(id);
                    return window::set_mode(id, window::Mode::Windowed)
                        .chain(window::minimize(id, false))
                        .chain(window::gain_focus(id));
                }
                let (id, t) = Self::open(self.qa_scale, None);
                self.window = Some(id);
                return t;
            }
            Msg::Minimize => {
                self.window_focused = false;
                if let Some(id) = self.window {
                    self.own_minimize = true;
                    return window::minimize(id, true);
                }
            }
            Msg::Drag => {
                if let Some(id) = self.window {
                    return window::drag(id);
                }
            }
            Msg::Quit => {
                if self.repair_resume.is_some() {
                    self.quit_after_repair=true;
                    self.message="Выход после завершения восстановления. Запрос Windows можно отменить.".into();
                    return Task::none();
                }
                if !self.quitting {
                    if self.apply_after_quit {self.update_resume=Some(self.processing_intent());}
                    self.quitting = true;
                    self.busy = true;
                    self.save();
                    self.engine.quit();
                }
            }
            Msg::UpdateCheck => {
                if !self.update_checking {
                    self.update_checking = true;
                    self.update_status = "Проверяем обновления…".into();
                    return Task::perform(
                        async { updater::check_and_download() },
                        Msg::UpdateChecked,
                    );
                }
            }
            Msg::UpdateChecked(status) => {
                self.update_checking = false;
                // Apply after the fresh check: the newest download (Ready), or the one already on
                // disk when the check itself failed (offline). Current means the release is gone.
                let apply = std::mem::take(&mut self.apply_pending)
                    && matches!(status, updater::Status::Ready(_) | updater::Status::Unavailable(_));
                match status {
                    updater::Status::Current => {
                        self.update_ready = false;
                        self.update_status = "Установлена актуальная версия".into();
                    }
                    updater::Status::Ready(version) => {
                        self.update_ready = true;
                        self.update_version = Some(version.clone());
                        self.update_status = format!("Версия {version} скачана и готова");
                    }
                    updater::Status::Unavailable(error) => {
                        self.update_ready = false;
                        self.update_status =
                            if error.contains("locat") || error.contains("manifest") {
                                "Обновления доступны после установки через Setup.exe".into()
                            } else {
                                format!("Не удалось проверить обновления: {error}")
                            };
                    }
                }
                if apply && !self.quitting && !self.driver_installing && !self.core_installing {
                    self.apply_pending=true;self.update_checking=true;
                    self.update_status="Проверяем и сохраняем комплект для отката…".into();
                    return Task::perform(async{updater::prepare()},Msg::UpdatePrepared);
                }
            }
            Msg::UpdatePrepared(result) => {
                self.apply_pending=false;self.update_checking=false;
                match result {
                    Ok(()) if !self.quitting && !self.driver_installing && !self.core_installing => {
                        self.apply_after_quit=true;return self.update(Msg::Quit);
                    }
                    Err(error)=>{self.update_status=format!("Подготовка обновления: {error}");}
                    _=>{}
                }
            }
            Msg::UpdateApplied(result) => {
                if result.is_err() && let Some(m)=self.morph.take() && let Some(hwnd)=m.hwnd {
                    update_window::color_key(hwnd,false);
                }
                if let Err(error)=result {
                    self.quitting=false;self.busy=false;self.apply_after_quit=false;
                    self.engine.resume_after_failed_update();
                    self.snapshot.state=0;self.headphone_state=0;self.headphone_busy=false;
                    self.update_status=format!("Обновление не применено: {error}");self.message=self.update_status.clone();
                    if let Some(intent)=self.update_resume.take(){self.resume_processing(intent);}
                } else {return exit_ui();}
            }
            Msg::ApplyUpdate => {
                // The download may be days old in a tray app: fetch the newest release first,
                // so one click lands on the latest version instead of the next one.
                if self.update_ready && !self.quitting && !self.update_checking && !self.driver_installing && !self.core_installing {
                    self.update_checking = true;
                    self.apply_pending = true;
                    self.update_status = "Проверяем последнюю версию…".into();
                    return Task::perform(
                        async { updater::check_and_download() },
                        Msg::UpdateChecked,
                    );
                }
            }
            Msg::Monitor => {
                self.focus = focus::effects::MONITOR;
                if !self.busy && !self.quitting && matches!(self.snapshot.state, 2 | 3) {
                    self.message.clear();
                    self.monitor_all = !(self.monitor_all && matches!(self.monitor, 1 | 2));
                    self.engine.monitor(self.monitor_mode());
                }
            }
            Msg::EffectsMonitor(enabled) | Msg::BoostMonitor(enabled) => {
                if matches!(msg, Msg::BoostMonitor(_)) {
                    self.boost_monitor = enabled;
                    self.focus = focus::effects::BOOST_MONITOR;
                } else {
                    self.effects_monitor = enabled;
                    self.focus = focus::effects::EFFECTS_MONITOR;
                }
                self.dirty = Some(Instant::now());
                if !matches!(self.monitor, 1 | 2) {
                    self.monitor_all = false;
                }
                if !self.monitor_all && !self.busy && !self.quitting && self.running() {
                    self.message.clear();
                    self.engine.monitor(self.monitor_mode());
                }
            }
            Msg::Settings => {
                return self.update(Msg::Page(if self.details { 0 } else { 2 }));
            }
            Msg::HeadphonePanel(open) => {
                self.headphone_page = open;
                if !open && (focus::headphones::TOGGLE..=focus::headphones::REVERSE).contains(&self.focus) {
                    self.focus = focus::effects::HEADPHONE_GEAR;
                }
            }
            Msg::RouteToggle => self.route_open = !self.route_open,
            Msg::ReverseWord(word) => self.reverse_word = word.chars().take(12).collect(),
            Msg::PointerDown => {
                self.focus_visible = false;
                if self.reverse_edit {
                    // The field has already handled the click: it stays focused only if clicked.
                    return iced::widget::operation::is_focused("reverse-word")
                        .map(|inside| if inside { Msg::Noop } else { Msg::ReverseEdit(false) });
                }
            }
            Msg::ReverseEdit(edit) => {
                self.reverse_edit = edit;
                if !edit && self.reverse_word.trim().is_empty() {
                    self.reverse_word = "Привет".into();
                }
                if edit {
                    return iced::widget::operation::focus("reverse-word");
                }
            }
            Msg::Page(page) => {
                if self.binding.is_some() {
                    let _ = self.update(Msg::CancelBind);
                }
                let before = self.page_key();
                self.soundpad_page = page == 4;
                self.studio_page = page == 7;
                // Page 3 is no longer a page: the headphone panel opens over Шумодав.
                self.headphone_page = page == 3;
                self.logs_page = page == 5;
                self.logs_copied = false;
                self.details = page == 2;
                self.rvc_page = page == 1;
                self.effects_page = page == 6;
                self.reverse_edit = false;
                self.focus = focus::NONE;
                // Only a real page change pixelates; re-selecting the same page stays still.
                // Without the effect nothing is painted offscreen.
                self.page_shift = (!cfg!(test) && self.pixel_shift && self.ui_active() && self.page_key() != before)
                    .then(|| self.page_mosaic())
                    .flatten()
                    .map(|to| (to, Instant::now()));
                self.repaint_all ^= self.page_key() != before;
                let snap = iced::widget::operation::snap_to(
                    "body",
                    iced::widget::scrollable::RelativeOffset::START,
                );
                if self.logs_page {
                    return Task::batch([snap, self.load_logs(false)]);
                }
                if self.studio_page {
                    let reveal = self.reveal_studio(studio::ROOT_NOTE);
                    if self.ui_active() && self.studio_live && self.studio_loop && self.snapshot.state == 3
                        && self.sound_playing.0 != studio::TRACK_ID && !self.studio_events.is_empty() {
                        return Task::batch([snap, reveal, self.update(Msg::StudioRender(false))]);
                    }
                    return Task::batch([snap, reveal]);
                }
                return snap;
            }
            Msg::HeadphoneToggle => {
                if !self.headphone_busy {
                    let enabled = !matches!(self.headphone_state, 1 | 2);
                    if enabled && self.headphone_output.is_none() {
                        self.headphone_message = "Выберите физические наушники.".into();
                    } else {
                        self.engine.headphone_controls(
                            self.headphone_intensity,
                            self.headphone_volume,
                            self.headphone_pitch,
                            self.headphone_reverse,
                        );
                        self.headphone_busy = true;
                        self.headphone_message.clear();
                        self.engine.headphones(
                            enabled,
                            self.headphone_output
                                .as_ref()
                                .map(|d| d.id.clone())
                                .unwrap_or_default(),
                            self.headphone_denoise,
                        );
                    }
                }
                self.focus = focus::headphones::TOGGLE;
            }
            Msg::HeadphoneOutput(d) => {
                if !self.headphone_busy && self.headphone_outputs().contains(&d) {
                    let restart = matches!(self.headphone_state, 1 | 2);
                    self.headphone_output = Some(d.clone());
                    self.dirty = Some(Instant::now());
                    if restart {
                        self.headphone_busy = true;
                        self.headphone_message.clear();
                        self.engine.headphones(true, d.id, self.headphone_denoise);
                    }
                }
                self.focus = focus::headphones::OUTPUT;
            }
            Msg::HeadphoneNoise(value) => {
                if !self.headphone_busy {
                    self.headphone_denoise = value;
                    self.dirty = Some(Instant::now());
                    // The NVIDIA model is chosen when a session opens, so a running session
                    // is reopened in the new mode (engine commands run in order).
                    if matches!(self.headphone_state, 1 | 2) {
                        let output = self.headphone_output.as_ref().map(|d| d.id.clone()).unwrap_or_default();
                        self.headphone_busy = true;
                        self.headphone_message.clear();
                        self.engine.headphones(false, output.clone(), value);
                        self.engine.headphones(true, output, value);
                    }
                }
                self.focus = focus::headphones::NOISE;
            }
            Msg::HeadphoneLines => {
                // Settings' device repair with the line transfer already ticked; it still asks.
                self.headphone_page = false;
                let page = self.update(Msg::Page(2));
                let _ = self.update(Msg::Repair);
                if self.repair_confirm {
                    self.repair_lines = true;
                }
                return Task::batch([page, view::reveal_focus()]);
            }
            Msg::HeadphoneReverse(enabled) => {
                self.headphone_reverse = enabled;
                self.headphone_changed();
                self.focus = focus::headphones::REVERSE;
            }
            Msg::HeadphoneIntensity(v) => {
                self.headphone_intensity = (v / 100.0).clamp(0.0, 2.0);
                self.headphone_changed();
                self.focus = focus::headphones::INTENSITY;
            }
            Msg::HeadphoneVolume(v) => {
                self.headphone_volume = (v / 100.0).clamp(0.0, 1.0);
                self.headphone_changed();
                self.focus = focus::headphones::VOLUME;
            }
            Msg::HeadphonePitch(v) => {
                self.headphone_pitch = (v.round() as i32).clamp(-12, 12);
                self.headphone_changed();
                self.focus = focus::headphones::PITCH;
            }
            Msg::Refresh => {
                if !self.quitting && !self.driver_installing && !self.core_installing {
                    if self.busy || !matches!(self.snapshot.state, 2 | 3) {
                        self.engine.cancel_start();
                        self.busy = false;
                        self.snapshot.state = 0;
                        self.recovery = Recovery::default();
                        self.message.clear();
                        self.auto_started = false;
                    }
                    self.engine.refresh();
                }
                self.focus = focus::settings::REFRESH;
            }
            Msg::AppAutostart(enabled) => {
                self.focus = focus::settings::APP_AUTOSTART;
                match if cfg!(test) { Ok(()) } else { set_app_autostart(enabled) } {
                    Ok(()) => {
                        self.app_autostart = enabled;
                        self.save();
                    }
                    Err(e) => self.message = format!("Автозапуск не изменён: {e}"),
                }
            }
            Msg::PixelShift(enabled) => {
                self.focus = focus::settings::PIXEL_SHIFT;
                self.pixel_shift = enabled;
                self.save();
            }
            Msg::SliderIdle(enabled) => {
                self.focus = focus::settings::SLIDER_IDLE;
                self.slider_idle = enabled;
                self.save();
            }
            Msg::Autostart(enabled) => {
                self.focus = focus::settings::AUTOSTART;
                if !self.autostart_busy {
                    self.autostart_busy = true;
                    return Task::perform(async move { if cfg!(test) { Ok(enabled) } else { engine::tag_autostart(i32::from(enabled)) } }, Msg::AutostartUpdated);
                }
            }
            Msg::AutostartUpdated(result) => {
                self.autostart_busy = false;
                match result {
                    Ok(enabled) => { self.autostart = enabled; self.save(); }
                    Err(e) => self.message = format!("Автозапуск не изменён: {e}"),
                }
            }
            Msg::Input(d) => {
                if !self.busy && self.inputs.contains(&d) {
                    let restart = self.running();
                    self.input = Some(d);
                    self.focus = if self.details {
                        focus::settings::INPUT
                    } else {
                        focus::effects::INPUT
                    };
                    self.changed();
                    if restart {
                        if let Some(config) = self.config() {
                            self.busy = true;
                            self.snapshot.state = 1;
                            self.message.clear();
                            self.save();
                            self.engine.start(config);
                        }
                    } else {
                        self.auto_started = false;
                        self.auto_start();
                    }
                }
            }
            Msg::Output(d) => {
                if !self.running() && !self.busy {
                    self.output = Some(d);
                    self.focus = focus::settings::OUTPUT;
                    self.changed();
                    self.auto_started = false;
                    self.auto_start();
                }
            }
            Msg::Version(v) => {
                if !self.running() && !self.busy {
                    self.version = v;
                    self.focus = focus::settings::VERSION;
                    self.changed();
                }
            }
            Msg::Buffer(b) => {
                if !self.running() && !self.busy {
                    self.buffer = b;
                    self.focus = focus::settings::BUFFER;
                    self.changed();
                }
            }
            Msg::Intensity(v) => {
                self.controls.intensity = (v / 100.0).clamp(0.0, 2.0);
                self.focus = focus::effects::INTENSITY;
                self.changed();
            }
            Msg::AlternateIntensity(v) => {
                self.controls.alternate_intensity = (v / 100.0).clamp(0.0, 2.0);
                self.focus = focus::effects::ALT_INTENSITY;
                self.changed();
            }
            Msg::Boost(v) => {
                self.controls.boost = v / 100.0;
                self.focus = focus::effects::BOOST;
                self.changed();
            }
            Msg::Overload(v) => {
                self.controls.overload = v;
                self.focus = focus::effects::OVERLOAD;
                self.changed();
            }
            Msg::DiscordVolume(v) => {
                self.controls.discord_volume = discord_volume_gain(v);
                self.focus = focus::effects::DISCORD_VOLUME;
                self.changed();
            }
            Msg::Pitch(v) => {
                self.controls.pitch = v as i32;
                self.focus = focus::effects::PITCH;
                self.changed();
            }
            Msg::Slow(v) => {
                self.controls.slow = v / 100.0;
                self.focus = focus::effects::SLOW;
                self.changed();
            }
            Msg::Fast(v) => {
                self.controls.fast = v / 100.0;
                self.focus = focus::effects::FAST;
                self.changed();
            }
            Msg::Rvc(v) => {
                if v && !self.rvc_runtime_installed {
                    return self.update(Msg::RvcInstall);
                }
                if v && !self
                    .rvc_models
                    .iter()
                    .any(|m| m.slot == self.controls.rvc_options.slot)
                {
                    self.message = "Выберите установленную модель RVC".into();
                    return Task::none();
                }
                self.controls.rvc = v;
                self.focus = focus::rvc::ENABLE;
                // Stop sending audio before tearing down the server process.
                self.changed();
                self.engine.rvc(v, self.controls.rvc_options);
            }
            Msg::RvcInstall => {
                if self.core_installing {
                    self.rvc_import_note = "Дождитесь установки основного runtime".into();
                } else if !self.rvc_runtime_installed && !self.rvc_runtime_installing {
                    self.rvc_runtime_installing = true;
                    self.rvc_import_note = "Подготовка загрузки RVC runtime…".into();
                    let root = self.component_root.clone();
                    return Task::perform(
                        async move { components::install_rvc(&root) },
                        Msg::RvcInstalled,
                    );
                }
            }
            Msg::RvcInstalled(result) => {
                self.rvc_runtime_installing = false;
                match result {
                    Ok(version) => {
                        self.rvc_runtime_installed = true;
                        self.rvc_import_note =
                            format!("RVC runtime {version} установлен. Теперь импортируйте голос.");
                    }
                    Err(error) => self.rvc_import_note = format!("RVC runtime: {error}"),
                }
            }
            Msg::CoreInstalled(result) => {
                self.core_installing = false;
                self.transfer.clear();
                // The setup card shows the outcome; app.log keeps it.
                let note = match &result {
                    Ok(version) => format!("Основной runtime {version} установлен"),
                    Err(error) => format!("Основной runtime: {error}"),
                };
                if !cfg!(test) {
                    logs::note(&self.runtime_root, &note);
                }
                self.setup_error = result.err().unwrap_or_default();
                // A failed model download must not keep a fresh PC without its virtual
                // microphone: the driver only needs the core files.
                if components::core_installed(&self.component_root) {
                    self.runtime_root = self.component_root.clone();
                    unsafe { std::env::set_var("MNR_RUNTIME_ROOT", &self.runtime_root) };
                    self.core_present = true;
                    self.models_present = components::models_present(&self.runtime_root, self.gpu.as_ref().ok().map(|(arch, _)| arch.as_str()));
                    self.rvc_runtime_installed = components::rvc_installed(&self.runtime_root);
                    // Voice already running means these are retried models: it runs without them
                    // (on the CPU) until it restarts, which the device refresh below starts.
                    if self.models_present && self.setup_error.is_empty() && matches!(self.snapshot.state, 2 | 3) && !self.busy {
                        self.engine.cancel_start();
                        self.snapshot.state = 0;
                        self.recovery = Recovery::default();
                        self.auto_started = false;
                    }
                    self.engine.refresh();
                    if !components::driver_installed() {
                        self.driver_ready = false;
                    }
                    if !self.autostart_busy && engine::tag_autostart(-1).ok() != Some(self.autostart) {
                        self.autostart_busy = true;
                        let enabled = self.autostart;
                        return Task::perform(async move { engine::tag_autostart(i32::from(enabled)) }, Msg::AutostartUpdated);
                    }
                }
            }
            Msg::RetryCore => {
                self.focus = focus::effects::SETUP_RETRY;
                // RVC shares the download folder and the progress counters.
                if !self.core_installing && !self.rvc_runtime_installing && !self.quitting {
                    self.core_installing = true;
                    self.setup_error.clear();
                    self.transfer.clear();
                    self.transfer_stage = None;
                    self.transfer_moved = Instant::now();
                    let root = self.component_root.clone();
                    let arch = self.gpu.as_ref().ok().map(|(arch, _)| arch.clone());
                    return Task::perform(async move { components::install_core(&root, arch.as_deref()) }, Msg::CoreInstalled);
                }
            }
            Msg::InstallDriver => {
                self.focus = if self.details { focus::settings::DRIVER } else { focus::effects::SETUP_DRIVER };
                if !self.driver_installing && !self.driver_ready && !self.core_installing {
                    self.driver_installing = true;
                    self.driver_error.clear();
                    let root = self.runtime_root.clone();
                    return Task::perform(
                        async move { components::install_driver(&root) },
                        Msg::DriverInstalled,
                    );
                }
            }
            Msg::Repair => {
                if !self.driver_installing && !self.core_installing && !self.quitting && !self.apply_pending {
                    self.repair_confirm=true;self.repair_reinstall=false;self.repair_lines=false;self.focus=focus::settings::REPAIR_CONFIRM;
                }
            }
            Msg::RepairReinstall(value) => {self.repair_reinstall=value;self.focus=focus::settings::REPAIR_REINSTALL;}
            Msg::RepairLines(value) => {self.repair_lines=value;self.focus=focus::settings::REPAIR_LINES;}
            Msg::RepairCancel => {self.repair_confirm=false;self.focus=focus::settings::REPAIR;}
            Msg::RepairConfirm => {
                if self.repair_confirm && !self.driver_installing && !self.quitting && !self.apply_pending {
                    self.repair_resume=Some(self.processing_intent());
                    self.repair_confirm=false;self.repair_started=false;self.driver_installing=true;self.busy=true;
                    self.recovery=Recovery::default();self.message=DEVICE_REPAIRING.into();
                    self.engine.quit();
                }
            }
            Msg::Repaired(result) => {
                let intent=self.repair_resume.take();self.driver_installing=false;self.repair_started=false;self.busy=false;
                self.snapshot.state=0;self.headphone_state=0;self.headphone_busy=false;
                self.driver_ready=cfg!(test)||components::driver_installed();
                self.engine.resume_after_failed_update();
                self.message=match result {Ok(())=>DEVICE_REPAIRED.into(),Err(error)=>format!("Восстановление: {error}")};
                if self.quit_after_repair {self.quit_after_repair=false;return self.update(Msg::Quit);}
                if let Some(intent)=intent {self.resume_processing(intent);}
                self.engine.refresh();
            }
            Msg::LogsCopy => {
                self.focus = focus::logs::COPY;
                return self.load_logs(true);
            }
            Msg::Logs(text, copy) => {
                self.logs_text = text;
                if copy {
                    self.logs_copied = true;
                    return iced::clipboard::write(self.logs_text.clone());
                }
            }
            Msg::LogsFolder => {
                self.focus = focus::logs::FOLDER;
                let folder = self.runtime_root.join("results");
                let _ = std::fs::create_dir_all(&folder);
                if let Err(error) = std::process::Command::new("explorer.exe").arg(&folder).spawn() {
                    self.message = format!("Не удалось открыть папку логов: {error}");
                }
            }
            Msg::SendReport => {
                self.focus = focus::logs::SEND;
                if !self.report_sending {
                    // The note carries what the log files cannot: what the UI last showed.
                    let note = format!(
                        "state={} driver={} core={} message={}",
                        self.snapshot.state, self.driver_ready, !self.core_installing, self.message
                    );
                    self.report_sending = true;
                    self.message = "Отправляем логи…".into();
                    let runtime = self.runtime_root.clone();
                    return Task::perform(
                        async move {
                            telemetry::report(&paths::Paths::resolve()?.data, &runtime, &note)
                        },
                        Msg::ReportSent,
                    );
                }
            }
            Msg::ReportSent(result) => {
                self.report_sending = false;
                self.message = match result {
                    Ok(text) => text,
                    Err(error) => error,
                };
            }
            Msg::DriverInstalled(result) => {
                self.driver_installing = false;
                match result {
                    // The virtual microphone appears only after the driver exists; the host
                    // opens it on the next engine start.
                    Ok(()) => {
                        self.driver_ready = true;
                        if !cfg!(test) {
                            logs::note(&self.runtime_root, "Виртуальный микрофон Mic Noize установлен");
                        }
                        self.engine.refresh();
                    }
                    // Shown next to the button that repeats the request.
                    Err(error) => {
                        if !cfg!(test) {
                            logs::note(&self.runtime_root, &error);
                        }
                        self.driver_error = error;
                    }
                }
            }
            Msg::RvcModel(model) => {
                self.controls.rvc_options.slot = model.slot;
                self.rvc_name = model.name.clone();
                self.rvc_delete_confirm = false;
                if !model.has_index {
                    self.controls.rvc_options.index = 0;
                }
                self.focus = focus::rvc::MODEL;
                self.changed();
            }
            Msg::RvcPitch(v) => {
                self.controls.rvc_options.pitch = v.round().clamp(-24.0, 24.0) as i32;
                self.focus = focus::rvc::PITCH;
                self.changed();
            }
            Msg::RvcIndex(v) => {
                self.controls.rvc_options.index = v.round().clamp(0.0, 100.0) as u32;
                self.focus = focus::rvc::INDEX;
                self.changed();
            }
            Msg::RvcGain(v) => {
                self.controls.rvc_options.gain = v.round().clamp(50.0, 300.0) as u32;
                self.focus = focus::rvc::GAIN;
                self.changed();
            }
            Msg::RvcChunk(v) => {
                if rvc::CHUNKS.contains(&v) {
                    self.controls.rvc_options.chunk = v;
                    self.changed();
                }
                self.focus = focus::rvc::CHUNK;
            }
            Msg::RvcAdvanced => {
                self.rvc_advanced = !self.rvc_advanced;
                self.focus = focus::rvc::ADVANCED;
            }
            Msg::RvcRefresh => {
                match rvc::models(&self.runtime_root) {
                    Ok(models) => {
                        self.rvc_models = models;
                        self.rvc_name = self
                            .rvc_models
                            .iter()
                            .find(|model| model.slot == self.controls.rvc_options.slot)
                            .map(|model| model.name.clone())
                            .unwrap_or_default();
                        self.rvc_delete_confirm = false;
                    }
                    Err(e) => self.message = format!("Модели RVC: {e}"),
                }
                self.focus = focus::rvc::REFRESH;
            }
            Msg::RvcImport => {
                if self.rvc_importing {
                    return Task::none();
                }
                self.rvc_importing = true;
                self.focus = focus::rvc::IMPORT;
                self.message.clear();
                self.rvc_import_note =
                    "Выберите .pth и, при наличии, его .index через Ctrl + щелчок".into();
                let root = self.runtime_root.clone();
                return Task::perform(async move { rvc::import(&root) }, Msg::RvcImported);
            }
            Msg::RvcImported(result) => {
                self.rvc_importing = false;
                match result {
                    Ok(Some((models, slot))) => {
                        self.rvc_models = models;
                        if !self.controls.rvc {
                            self.controls.rvc_options.slot = slot;
                            self.controls.rvc_options.index = 0;
                            self.changed();
                        }
                        self.rvc_name = self
                            .rvc_models
                            .iter()
                            .find(|model| model.slot == self.controls.rvc_options.slot)
                            .map(|model| model.name.clone())
                            .unwrap_or_default();
                        self.rvc_import_note = if self.controls.rvc {
                            "Модель импортирована и доступна в списке".into()
                        } else {
                            "Модель импортирована и выбрана".into()
                        };
                    }
                    Ok(None) => self.rvc_import_note.clear(),
                    Err(e) => {
                        self.rvc_import_note.clear();
                        self.message = format!("Импорт модели: {e}");
                    }
                }
            }
            Msg::RvcName(name) => {
                self.rvc_name = name.chars().take(80).collect();
                self.rvc_delete_confirm = false;
                self.focus = focus::rvc::NAME;
            }
            Msg::RvcRename => {
                self.focus = focus::rvc::RENAME;
                if !self.rvc_can_manage() {
                    self.message = "Выключите RVC и дождитесь остановки модели".into();
                } else {
                    let slot = self.controls.rvc_options.slot;
                    match rvc::rename(&self.runtime_root, slot, &self.rvc_name) {
                        Ok(name) => {
                            if let Some(model) =
                                self.rvc_models.iter_mut().find(|model| model.slot == slot)
                            {
                                model.name = name.clone();
                            }
                            self.rvc_name = name;
                            self.rvc_import_note = "Модель переименована".into();
                            self.message.clear();
                        }
                        Err(e) => self.message = format!("Переименование модели: {e}"),
                    }
                }
            }
            Msg::RvcDelete => {
                self.focus = focus::rvc::DELETE;
                if !self.rvc_can_manage() {
                    self.message = "Выключите RVC и дождитесь остановки модели".into();
                } else if !self.rvc_delete_confirm {
                    self.rvc_delete_confirm = true;
                    self.rvc_import_note = "Нажмите «Удалить ещё раз» для подтверждения".into();
                } else {
                    let slot = self.controls.rvc_options.slot;
                    match rvc::delete(&self.runtime_root, slot) {
                        Ok(()) => {
                            self.rvc_models.retain(|model| model.slot != slot);
                            if let Some(model) = self.rvc_models.first() {
                                self.controls.rvc_options.slot = model.slot;
                                if !model.has_index {
                                    self.controls.rvc_options.index = 0;
                                }
                                self.rvc_name = model.name.clone();
                            } else {
                                self.controls.rvc_options.slot = 0;
                                self.controls.rvc_options.index = 0;
                                self.rvc_name.clear();
                            }
                            self.rvc_delete_confirm = false;
                            self.rvc_import_note = "Модель удалена".into();
                            self.message.clear();
                            self.changed();
                        }
                        Err(e) => self.message = format!("Удаление модели: {e}"),
                    }
                }
            }
            Msg::CancelPhrase => {
                self.engine.cancel_phrase();
                self.focus = focus::effects::CANCEL_PHRASE;
            }
            Msg::Bind(i) => {
                if i >= SOUND_BIND_BASE && i - SOUND_BIND_BASE >= self.sounds.len() {
                    return Task::none();
                }
                if self.binding == Some(i) {
                    return self.update(Msg::CancelBind); // second click on the same button
                }
                self.binding = Some(i);
                self.candidate = 0;
                self.bind_conflict = None;
                self.focus = Self::bind_focus(i);
                self.engine.capture(true);
            }
            Msg::SoundpadFolder | Msg::SoundpadAdd => {
                let folder = matches!(msg, Msg::SoundpadFolder);
                self.focus = if folder {
                    focus::soundpad::FOLDER
                } else {
                    focus::soundpad::ADD
                };
                if self.sound_dialog {
                    return Task::none();
                }
                if !folder && self.sound_folder.is_none() {
                    self.sound_note = "Сначала выберите папку со звуками.".into();
                    return Task::none();
                }
                self.sound_dialog = true;
                return Task::perform(async move { engine::pick_paths(folder) }, move |r| {
                    Msg::SoundpadPicked(folder, r)
                });
            }
            Msg::SoundpadPicked(folder, result) => {
                self.sound_dialog = false;
                match result {
                    Ok(paths) if paths.is_empty() => {}
                    Ok(paths) if folder => {
                        self.sound_folder = paths.into_iter().next();
                        self.sounds.clear();
                        return self.rescan_sounds();
                    }
                    Ok(paths) => {
                        let Some(target) = self.sound_folder.clone() else {
                            return Task::none();
                        };
                        match soundpad::import(&target, &paths) {
                            Ok((copied, skipped)) => {
                                let task = self.rescan_sounds();
                                if self.sound_note.is_empty() {
                                    self.sound_note = format!(
                                        "Добавлено: {copied}{}",
                                        if skipped > 0 {
                                            format!(", пропущено (уже есть): {skipped}")
                                        } else {
                                            String::new()
                                        }
                                    );
                                }
                                return task;
                            }
                            Err(e) => self.sound_note = format!("Не удалось добавить: {e}"),
                        }
                    }
                    Err(e) => self.sound_note = format!("Диалог не открылся: {e}"),
                }
            }
            Msg::SoundpadRefresh => {
                self.focus = focus::soundpad::REFRESH;
                return self.rescan_sounds();
            }
            Msg::SoundpadVolume(v) => {
                self.sound_volume = (v / 100.0).clamp(0.0, 2.0);
                self.engine.sound_volume(self.sound_volume * SOUND_VOLUME_AT_100);
                self.focus = focus::soundpad::VOLUME;
                self.dirty = Some(Instant::now());
            }
            Msg::SoundpadHear(enabled) => {
                self.sound_monitor = enabled;
                self.focus = if self.studio_page { focus::studio::HEAR } else { focus::soundpad::HEAR };
                self.dirty = Some(Instant::now());
                if !matches!(self.monitor, 1 | 2) {
                    self.monitor_all = false;
                }
                if !self.monitor_all && !self.busy && !self.quitting && self.running() {
                    self.message.clear();
                    self.engine.monitor(self.monitor_mode());
                }
            }
            Msg::SoundpadNormalize(enabled) => {
                self.sound_normalize = enabled;
                self.focus = focus::soundpad::NORMALIZE;
                self.dirty = Some(Instant::now());
                // Loaded clips carry the old level: drop them all, bound clips decode again.
                return self.rescan_sounds();
            }
            Msg::SoundpadFilter(text) => {
                self.sound_filter = text.chars().take(80).collect();
                self.focus = focus::soundpad::FILTER;
            }
            Msg::SoundpadSort(sort) => {
                self.sound_sort = sort;
                self.focus = focus::soundpad::SORT;
                self.dirty = Some(Instant::now());
            }
            Msg::SoundpadScroll(offset, height) => {
                self.sound_scroll = (offset, height);
            }
            Msg::StudioScroll(offset, height) => {
                self.studio_scroll = (offset, height);
            }
            Msg::SoundHover(i, entered) => {
                if entered && i < self.sounds.len() {
                    self.sound_hover = Some(i);
                } else if self.sound_hover == Some(i) {
                    self.sound_hover = None;
                }
            }
            Msg::Wheel(id, delta) => {
                if let Some(anim) = self.scroll_anims.get_mut(id) {
                    anim.push(delta);
                    return Task::none(); // the running frame loop picks the new target up
                }
                // First notch: measure the real offset before easing away from it.
                let pending = self.scroll_pending.entry(id).or_insert(0.0);
                let first = *pending == 0.0;
                *pending += delta;
                if first {
                    return smooth::probe(id, move |r| Msg::ScrollProbe(id, r));
                }
            }
            Msg::ScrollProbe(id, result) => {
                let Some((offset, viewport, content)) = result else {
                    self.scroll_anims.remove(id);
                    self.scroll_pending.remove(id);
                    return Task::none();
                };
                if let Some(delta) = self.scroll_pending.remove(id) {
                    if id == "body" && self.soundpad_page {
                        self.sound_scroll = (offset, viewport);
                    }
                    let mut anim = smooth::Anim {
                        current: offset,
                        target: offset,
                        viewport,
                        content,
                        last_frame: Instant::now(),
                    };
                    anim.push(delta);
                    self.scroll_anims.insert(id, anim);
                }
            }
            Msg::ScrollFrame(now) => {
                let mut tasks = Vec::with_capacity(self.scroll_anims.len());
                self.scroll_anims.retain(|&id, anim| {
                    let active = anim.step(now);
                    if id == "body" && self.soundpad_page {
                        self.sound_scroll = (anim.current, anim.viewport);
                    }
                    tasks.push(iced::widget::operation::scroll_to(id,
                        iced::widget::scrollable::AbsoluteOffset { x: None, y: Some(anim.current) }));
                    active
                });
                return Task::batch(tasks);
            }
            Msg::SectionSelect(i) => {
                let items = self.section_items();
                if let Some(item) = items.get(i) {
                    self.select_section(item.selection.clone());
                    self.focus = focus::soundpad::SECTION_BASE + i;
                    return iced::widget::operation::snap_to(
                        "body",
                        iced::widget::scrollable::RelativeOffset::START,
                    );
                }
            }
            Msg::SectionAdd => {
                self.focus = focus::soundpad::SECTION_ADD;
                let mut name = "Новый раздел".to_string();
                let mut n = 2;
                while self.sections.iter().any(|s| s.name == name) {
                    name = format!("Новый раздел {n}");
                    n += 1;
                }
                self.sections.push(Section {
                    name,
                    files: vec![],
                });
                self.select_section(Selection::Custom(self.sections.len() - 1));
                self.section_name.clear(); // the field starts empty; the placeholder shows the name
                self.dirty = Some(Instant::now());
                self.focus = focus::soundpad::SECTION_NAME;
                return Task::batch([
                    iced::widget::operation::focus("section-name"),
                    iced::widget::operation::snap_to(
                        "sections",
                        iced::widget::scrollable::RelativeOffset::END,
                    ),
                ]);
            }
            Msg::SectionName(name) => {
                self.section_name = name.chars().take(40).collect();
                self.focus = focus::soundpad::SECTION_NAME;
            }
            Msg::SectionRename => {
                self.focus = focus::soundpad::SECTION_NAME;
                let Some(i) = self.custom_section() else {
                    return Task::none();
                };
                let name = soundpad::section_name(&self.section_name);
                if name.is_empty() || name == self.sections[i].name {
                    self.section_name.clear();
                } else if self.sections.iter().enumerate().any(|(j, s)| j != i && s.name == name) {
                    self.sound_note = "Раздел с таким именем уже есть.".into();
                } else {
                    self.sections[i].name = name;
                    self.section_name.clear();
                    self.dirty = Some(Instant::now());
                }
            }
            Msg::SectionDelete => {
                self.focus = focus::soundpad::SECTION_DELETE;
                if let Some(i) = self.custom_section() {
                    self.sections.remove(i);
                    self.select_section(Selection::All);
                    self.dirty = Some(Instant::now());
                }
            }
            Msg::DragStart(i) => {
                if i < self.sounds.len() {
                    self.dragging = Some(i);
                    self.drag_over = None;
                }
            }
            Msg::DragOver(section) => {
                if self.dragging.is_some() {
                    self.drag_over = section.filter(|&s| s < self.sections.len());
                }
            }
            Msg::DragEnd => {
                self.studio_scrubbing = false;
                self.studio_loop_drag = None;
                let studio_edited = self.studio_stroke.take().is_some();
                if let (Some(i), Some(section)) = (self.dragging.take(), self.drag_over.take())
                    && let Some(sound) = self.sounds.get(i)
                    && let Some(target) = self.sections.get_mut(section)
                    && !target.files.contains(&sound.name)
                {
                    target.files.push(sound.name.clone());
                    self.sound_note = format!("«{}» добавлен в «{}»", sound.name, target.name);
                    self.dirty = Some(Instant::now());
                }
                self.dragging = None;
                self.drag_over = None;
                if studio_edited && self.studio_live { return self.update(Msg::StudioRender(false)); }
            }
            Msg::SoundUnassign(i) => {
                if let Some(section) = self.custom_section()
                    && let Some(sound) = self.sounds.get(i)
                {
                    self.sections[section].files.retain(|f| *f != sound.name);
                    self.dirty = Some(Instant::now());
                }
            }
            Msg::SoundpadStop => {
                self.engine.sound_play(0);
                self.sound_pending_play = None;
                if self.studio_live && self.studio_loop {
                    self.studio_loop = false;
                    self.dirty = Some(Instant::now());
                }
                self.studio_live = false;
                self.studio_rebuild = false;
            }
            Msg::StudioBpm(bpm) => {
                self.studio_bpm = bpm.clamp(60, 200);
                self.focus = focus::studio::BPM;
                self.dirty = Some(Instant::now());
                if self.studio_live { return self.update(Msg::StudioRender(false)); }
            }
            Msg::StudioVolume(value) => {
                self.studio_volume = value.min(200);
                self.engine.sound_gain(studio::TRACK_ID, self.studio_volume as f32 / 100.0);
                self.focus = focus::studio::VOLUME;
                self.dirty = Some(Instant::now());
            }
            Msg::StudioZoom(index) => {
                let old_pitch = 28.0 * studio::ZOOMS[self.studio_zoom] as f32 / 100.0;
                let row = ((self.studio_scroll.0 + self.studio_scroll.1 / 2.0) / old_pitch) as usize;
                let note = studio::FIRST_NOTE + (studio::NOTES - 1 - row.min(studio::NOTES - 1)) as u8;
                self.studio_zoom = index.min(studio::ZOOMS.len() - 1);
                self.focus = focus::studio::ZOOM;
                self.dirty = Some(Instant::now());
                return self.reveal_studio(note);
            }
            Msg::StudioLoopEnabled(enabled) => {
                self.studio_loop = enabled;
                self.focus = focus::studio::LOOP;
                self.sync_studio_loop();
                self.dirty = Some(Instant::now());
                if enabled {
                    self.studio_live = true;
                    if !self.studio_events.is_empty() { return self.update(Msg::StudioRender(false)); }
                }
            }
            Msg::StudioLoopStart(step) => {
                let activate = !self.studio_loop;
                self.studio_loop_start = step.min(self.studio_loop_end - 1);
                self.studio_loop = true;
                self.focus = focus::studio::LOOP_START;
                self.sync_studio_loop();
                self.dirty = Some(Instant::now());
                if activate && !self.studio_events.is_empty() {
                    self.studio_live = true;
                    return self.update(Msg::StudioRender(false));
                }
            }
            Msg::StudioLoopEnd(step) => {
                let activate = !self.studio_loop;
                self.studio_loop_end = step.clamp(self.studio_loop_start + 1, studio::STEPS);
                self.studio_loop = true;
                self.focus = focus::studio::LOOP_END;
                self.sync_studio_loop();
                self.dirty = Some(Instant::now());
                if activate && !self.studio_events.is_empty() {
                    self.studio_live = true;
                    return self.update(Msg::StudioRender(false));
                }
            }
            Msg::StudioLoopGrab(step) => {
                let start_distance = step.abs_diff(self.studio_loop_start);
                let end_distance = step.abs_diff(self.studio_loop_end - 1);
                let start = start_distance < end_distance;
                self.studio_loop_drag = Some(start);
                return self.update(if start { Msg::StudioLoopStart(step) } else { Msg::StudioLoopEnd(step + 1) });
            }
            Msg::StudioLoopMove(step) => {
                if let Some(start) = self.studio_loop_drag {
                    return self.update(if start { Msg::StudioLoopStart(step) } else { Msg::StudioLoopEnd(step + 1) });
                }
            }
            Msg::StudioBar(bar) => {
                self.studio_bar = bar.min(studio::BARS - 1);
                self.studio_follow = false;
                self.focus = focus::studio::BAR_BASE + self.studio_bar;
            }
            Msg::StudioGrab(step) => {
                self.studio_scrubbing = true;
                self.focus = focus::studio::CURSOR;
                self.seek_studio(step);
            }
            Msg::StudioDrag(step) => {
                if self.studio_scrubbing { self.seek_studio(step); }
            }
            Msg::StudioSelect(name) => {
                if self.studio_samples.contains(&name) {
                    self.focus = focus::studio::SAMPLE_BASE + self.studio_samples.iter().position(|s| s == &name).unwrap();
                    self.studio_selected = Some(name);
                    if Self::window_size().width < 846.0 { self.studio_library_open = false;self.focus = focus::studio::LIBRARY; }
                }
            }
            Msg::StudioLibraryToggle => {
                self.studio_library_open = !self.studio_library_open;
                self.focus = focus::studio::LIBRARY;
            }
            Msg::StudioCell(step, note) => {
                if step < studio::STEPS && (studio::FIRST_NOTE..studio::FIRST_NOTE + studio::NOTES as u8).contains(&note)
                    && let Some(sample) = &self.studio_selected {
                    studio::toggle(&mut self.studio_events, step, note, sample);
                    self.focus = focus::studio::CELL_BASE + (step % 16) * studio::NOTES + (note - studio::FIRST_NOTE) as usize;
                    self.dirty = Some(Instant::now());
                    if self.studio_live { return self.update(Msg::StudioRender(false)); }
                }
            }
            Msg::StudioDrawStart(step, note) => {
                if step >= studio::STEPS || !(studio::FIRST_NOTE..studio::FIRST_NOTE + studio::NOTES as u8).contains(&note) { return Task::none(); }
                if let Some(sample) = self.studio_selected.clone() {
                    let before = self.studio_events.clone();
                    let own = before.iter().find(|e| e.note == note && (e.step as usize..e.step as usize + e.length as usize).contains(&step)
                        && e.sample == sample);
                    let anchor = own.map_or(step, |e| e.step as usize);
                    if own.is_none() { studio::paint(&mut self.studio_events, step, step, note, &sample); }
                    self.studio_stroke = Some(StudioStroke { anchor, note, sample, before });
                    self.focus = focus::studio::CELL_BASE + (step % 16) * studio::NOTES + (note - studio::FIRST_NOTE) as usize;
                    self.dirty = Some(Instant::now());
                }
            }
            Msg::StudioCellEnter(step, note) => {
                if let Some(stroke) = &self.studio_stroke && stroke.note == note {
                    self.studio_events = stroke.before.clone();
                    studio::paint(&mut self.studio_events, stroke.anchor, step, note, &stroke.sample);
                    self.dirty = Some(Instant::now());
                } else if self.studio_erasing && studio::erase(&mut self.studio_events, step, note) {
                    self.dirty = Some(Instant::now());
                }
            }
            Msg::StudioEraseStart(step, note) => {
                self.studio_erasing = true;
                if studio::erase(&mut self.studio_events, step, note) { self.dirty = Some(Instant::now()); }
            }
            Msg::StudioRightEnd => {
                let edited = self.studio_erasing;
                self.studio_erasing = false;
                if edited && self.studio_live { return self.update(Msg::StudioRender(false)); }
            }
            Msg::StudioRecord => {
                self.focus = focus::studio::RECORD;
                let was_recording = self.engine.studio_recording();
                if self.engine.studio_record(!was_recording) {
                    self.studio_note = if was_recording { "Сохраняем запись…" } else { "Запись микрофона: до 20 секунд" }.into();
                } else {
                    self.studio_note = "Сначала запустите микрофон и выключите Mute".into();
                }
            }
            Msg::StudioRecorded(result) => match result {
                Ok(name) => {
                    self.studio_samples = studio::scan(&self.studio_folder);
                    self.studio_selected = Some(name);
                    self.studio_note = "Сэмпл сохранён".into();
                }
                Err(e) => self.studio_note = format!("Не удалось сохранить запись: {e}"),
            },
            Msg::StudioImport => {
                self.focus = focus::studio::IMPORT;
                if self.studio_busy { return Task::none(); }
                self.studio_busy = true;
                return Task::perform(async { engine::pick_paths(false) }, Msg::StudioPicked);
            }
            Msg::StudioPicked(result) => {
                let files = match result {
                    Ok(files) => files,
                    Err(e) => {self.studio_busy=false;self.studio_note=e;return Task::none();}
                };
                if files.is_empty() { self.studio_busy=false; return Task::none(); }
                let folder = self.studio_folder.clone();
                return Task::perform(async move {
                    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
                    soundpad::import(&folder, &files).map(|_| ())
                }, Msg::StudioImported);
            }
            Msg::StudioImported(result) => {
                self.studio_busy = false;
                self.studio_samples = studio::scan(&self.studio_folder);
                if self.studio_selected.is_none() { self.studio_selected = self.studio_samples.first().cloned(); }
                self.studio_note = result.map_or_else(|e| format!("Импорт: {e}"), |_| "Звуки добавлены".into());
            }
            Msg::StudioRender(export) => {
                self.focus = if export { focus::studio::EXPORT } else { focus::studio::PLAY };
                if self.studio_busy {
                    if !export && self.studio_live { self.studio_rebuild = true; }
                    return Task::none();
                }
                if self.studio_events.is_empty() { return Task::none(); }
                if !export && self.snapshot.state != 3 {
                    self.studio_note = "Сначала запустите виртуальный микрофон".into();
                    return Task::none();
                }
                if !export { self.studio_live = true;self.studio_rebuild = false; }
                self.studio_busy = true;
                self.studio_note = "Собираем трек…".into();
                let folder = self.studio_folder.clone();
                let events = self.studio_events.clone();
                let bpm = self.studio_bpm;
                let volume = self.studio_volume;
                let loop_range = self.studio_loop.then(|| (studio::frame_at(self.studio_loop_start, bpm),
                    studio::frame_at(self.studio_loop_end, bpm)));
                if !export { self.studio_play_bpm = bpm; }
                let loader = self.engine.sound_loader();
                return Task::perform(async move {
                    let audio = studio::render(&folder, bpm, &events)?;
                    if export {
                        let exports = folder.parent().unwrap().join("Треки");
                        std::fs::create_dir_all(&exports).map_err(|e| e.to_string())?;
                        let path = unique_path(&exports, "Трек.wav");
                        let selected = loop_range.map_or(&audio[..], |(start, end)| &audio[start..end]);
                        let gain = volume as f32 / 100.0;
                        let selected: Vec<f32> = selected.iter().map(|v| (v * gain).clamp(-1.0, 1.0)).collect();
                        soundpad::write_wav(&path, &selected)?;
                        Ok(Some(path))
                    } else {
                        loader.load_studio(studio::TRACK_ID, &audio, volume as f32 / 100.0, loop_range)?;
                        Ok(None)
                    }
                }, move |r| Msg::StudioRendered(export, r));
            }
            Msg::StudioRendered(export, result) => {
                self.studio_busy = false;
                let rebuild = self.studio_rebuild && self.studio_live && self.studio_loop && result.is_ok();
                self.studio_rebuild = false;
                match result {
                    Ok(Some(path)) => self.studio_note = format!("Сохранено: {}", path.display()),
                    Ok(None) if !export && self.snapshot.state == 3 && self.studio_live => {
                        if self.studio_cursor >= studio::STEPS as f32 { self.studio_cursor = 0.0; }
                        if self.studio_loop && (self.studio_cursor < self.studio_loop_start as f32
                            || self.studio_cursor >= self.studio_loop_end as f32) {
                            self.studio_cursor = self.studio_loop_start as f32;
                        }
                        self.studio_follow = true;
                        self.sync_studio_loop();
                        self.engine.sound_gain(studio::TRACK_ID, self.studio_volume as f32 / 100.0);
                        self.engine.sound_restart(studio::TRACK_ID);
                        self.engine.sound_seek(studio::TRACK_ID, self.studio_cursor * 15.0 / self.studio_play_bpm as f32);
                        self.studio_note = "Трек отправлен в виртуальный микрофон".into();
                    }
                    Ok(None) if !export && !self.studio_live => self.studio_note = "Остановлено".into(),
                    Ok(None) if !export => self.studio_note = "Микрофон остановлен — запустите его и повторите".into(),
                    Ok(None) => {},
                    Err(e) => self.studio_note = format!("Сборка трека: {e}"),
                }
                if rebuild { return self.update(Msg::StudioRender(false)); }
            }
            Msg::StudioClear => {
                self.focus = focus::studio::CLEAR;
                studio::clear_bar(&mut self.studio_events, self.studio_bar);
                self.dirty = Some(Instant::now());
                if self.studio_live {
                    if self.studio_events.is_empty() { self.engine.sound_play(0); }
                    else { return self.update(Msg::StudioRender(false)); }
                }
            }
            Msg::StudioDeleteAsk(name) => {
                if self.studio_samples.contains(&name) {
                    self.studio_delete = Some(name);
                    self.focus = focus::studio::DELETE_CANCEL;
                }
            }
            Msg::StudioDeleteCancel => {
                self.studio_delete = None;
                self.focus = focus::studio::IMPORT;
            }
            Msg::StudioDeleteConfirm => {
                let Some(name) = self.studio_delete.clone() else { return Task::none(); };
                if self.studio_busy || !studio::valid_name(&name) { return Task::none(); }
                self.studio_busy = true;
                let folder = self.studio_folder.clone();
                let target_name = name.clone();
                return Task::perform(async move {
                    let deleted = folder.parent().unwrap().join("Удалённые");
                    std::fs::create_dir_all(&deleted).map_err(|e| e.to_string())?;
                    std::fs::rename(folder.join(&target_name), unique_path(&deleted, &target_name)).map_err(|e| e.to_string())
                }, move |result| Msg::StudioDeleted(name, result));
            }
            Msg::StudioDeleted(name, result) => {
                self.studio_busy = false;
                match result {
                    Ok(()) => {
                        self.studio_delete = None;
                        self.studio_samples.retain(|s| s != &name);
                        self.studio_events.retain(|e| e.sample != name);
                        if self.studio_selected.as_ref() == Some(&name) {
                            self.studio_selected = self.studio_samples.first().cloned();
                        }
                        if self.sound_playing.0 == studio::TRACK_ID { self.engine.sound_play(0); }
                        self.studio_note = format!("Звук перемещён в «Удалённые»: {name}");
                        self.dirty = Some(Instant::now());
                        if self.studio_live && !self.studio_events.is_empty() {
                            return self.update(Msg::StudioRender(false));
                        }
                    }
                    Err(e) => self.studio_note = format!("Не удалось удалить звук: {e}"),
                }
            }
            Msg::SoundPlay(i) => {
                let Some(sound) = self.sounds.get(i) else {
                    return Task::none();
                };
                self.focus = focus::soundpad::ROW_BASE + 3 * i;
                match sound.state {
                    SoundState::Loaded(_) => self.engine.sound_play(Self::sound_id(i)),
                    SoundState::Loading => self.sound_pending_play = Some(i),
                    SoundState::Unloaded | SoundState::Failed(_) => {
                        self.sound_pending_play = Some(i);
                        return self.load_sound(i);
                    }
                }
            }
            Msg::SoundVolume(i, v) => {
                let Some(sound) = self.sounds.get_mut(i) else {
                    return Task::none();
                };
                sound.volume = v.round().clamp(0.0, 200.0) as u32;
                if matches!(sound.state, SoundState::Loaded(_)) {
                    self.engine.sound_gain(Self::sound_id(i), sound.gain());
                }
                self.focus = focus::soundpad::ROW_BASE + 3 * i + 1;
                self.dirty = Some(Instant::now());
            }
            Msg::SoundLoaded(i, generation, result) => {
                if generation != self.sound_generation {
                    return Task::none();
                }
                let Some(sound) = self.sounds.get_mut(i) else {
                    return Task::none();
                };
                match result {
                    Ok(seconds) => {
                        sound.state = SoundState::Loaded(seconds);
                        // The gain may have moved while the clip was decoding.
                        self.engine.sound_gain(Self::sound_id(i), sound.gain());
                        if self.sound_pending_play == Some(i) {
                            self.sound_pending_play = None;
                            self.engine.sound_play(Self::sound_id(i));
                        }
                    }
                    Err(e) => {
                        sound.state = SoundState::Failed(e);
                        if self.sound_pending_play == Some(i) {
                            self.sound_pending_play = None;
                        }
                    }
                }
            }
            Msg::ClipRecorded(result) => match result {
                Ok(()) => self.rescan_clips(),
                Err(e) => {
                    self.clip_note = format!(
                        "Запись не сохранена: {e}"
                    )
                }
            },
            Msg::ClipPlay(i) => {
                let Some(clip) = self.clips.get(i) else {
                    return Task::none();
                };
                self.focus = focus::effects::CLIP_BASE + 2 * i;
                match clip.state {
                    SoundState::Loaded(_) => self.engine.sound_play(Self::clip_id(i)),
                    SoundState::Loading => self.clip_pending_play = Some(i),
                    SoundState::Unloaded | SoundState::Failed(_) => {
                        self.clip_pending_play = Some(i);
                        return self.load_clip(i);
                    }
                }
            }
            Msg::ClipLoaded(i, generation, result) => {
                if generation != self.clip_loads {
                    return Task::none();
                }
                let Some(clip) = self.clips.get_mut(i) else {
                    return Task::none();
                };
                clip.state = match result {
                    Ok(seconds) => SoundState::Loaded(seconds),
                    Err(e) => {
                        self.clip_note = e.chars().take(70).collect();
                        SoundState::Failed(e)
                    }
                };
                if self.clip_pending_play == Some(i) {
                    self.clip_pending_play = None;
                    if matches!(self.clips[i].state, SoundState::Loaded(_)) {
                        self.engine.sound_play(Self::clip_id(i));
                    }
                }
            }
            Msg::ClipMenu(at) => {
                self.clip_note.clear();
                self.clip_menu = if self.clip_menu == at { None } else { at };
                if let Some(i) = at {
                    self.focus = focus::effects::CLIP_BASE + 2 * i + 1;
                }
            }
            Msg::ClipSave(i, to_soundpad) => {
                self.clip_menu = None;
                if i >= self.clips.len() {
                    return Task::none();
                }
                self.focus = focus::effects::CLIP_BASE + 2 * i + 1;
                match self.sound_folder.clone().filter(|_| to_soundpad) {
                    Some(folder) => {
                        self.copy_clip(i, &folder);
                        return self.rescan_sounds();
                    }
                    None => {
                        if self.sound_dialog {
                            return Task::none();
                        }
                        self.sound_dialog = true;
                        if to_soundpad {
                            self.clip_note = "Выберите папку саундпада…".into();
                        }
                        return Task::perform(async { engine::pick_paths(true) }, move |r| {
                            Msg::ClipPicked(i, to_soundpad, r)
                        });
                    }
                }
            }
            Msg::ClipPicked(i, to_soundpad, result) => {
                self.sound_dialog = false;
                let folder = match result {
                    Ok(paths) => match paths.into_iter().next() {
                        Some(folder) => folder,
                        None => {
                            self.clip_note.clear();
                            return Task::none();
                        }
                    },
                    Err(e) => {
                        self.clip_note = format!(
                            "Диалог не открылся: {e}"
                        );
                        return Task::none();
                    }
                };
                self.copy_clip(i, &folder);
                if to_soundpad {
                    // The picked folder becomes the library, with the copy already inside it.
                    self.sound_folder = Some(folder);
                    self.sounds.clear();
                    self.dirty = Some(Instant::now());
                    return self.rescan_sounds();
                }
            }
            Msg::CancelBind => {
                self.binding = None;
                self.candidate = 0;
                self.bind_conflict = None;
                self.engine.capture(false);
            }
            Msg::ClearBind => {
                if let Some(i) = self.binding {
                    let task = self.set_key(i, 0);
                    return Task::batch([task, self.update(Msg::CancelBind)]);
                }
                return self.update(Msg::CancelBind);
            }
            Msg::AcceptBind => {
                if let Some(i) = self.binding
                    && self.candidate != 0
                {
                    if self.key_taken(i, self.candidate) {
                        // Stay on the button, show the clash and wait for the next key.
                        self.bind_conflict = Some(self.candidate);
                        self.candidate = 0;
                        self.engine.capture(true);
                    } else {
                        let task = self.set_key(i, self.candidate);
                        return Task::batch([task, self.update(Msg::CancelBind)]);
                    }
                }
            }
            Msg::Key(key, mods, repeat) => {
                self.focus_visible = true;
                return self.key(key, mods, repeat);
            }
            Msg::Screenshot(shot) => {
                if let Some(path) = &self.capture_path {
                    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                        let file = std::fs::File::create(path)?;
                        let mut encoder =
                            png::Encoder::new(file, shot.size.width, shot.size.height);
                        encoder.set_color(png::ColorType::Rgba);
                        encoder.set_depth(png::BitDepth::Eight);
                        encoder.write_header()?.write_image_data(&shot.rgba)?;
                        Ok(())
                    })();
                    if let Err(e) = result {
                        self.message = e.to_string();
                    } else {
                        return self.update(Msg::Quit);
                    }
                }
            }
            Msg::Noop => {}
            Msg::PageShiftDone => self.page_shift = None,
            Msg::DecayGeometry(position, size) => {
                let Some(position) = position else { return self.hand_over(None) };
                let center = iced::Point::new(position.x + size.width / 2.0, position.y + size.height / 2.0);
                let to_version = self.update_version.clone().filter(|_| !self.rehearse_after_quit).unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
                let from = self.window_mosaic(size);
                let to = view::mosaic_of::<Msg>(view::update_card(tacho::BarStage::Waiting, env!("CARGO_PKG_VERSION"), &to_version), view::UPDATE_CARD);
                let (Some(from), Some(to), Some(id)) = (from, to, self.window) else { return self.hand_over(Some(center)) };
                let card = iced::Rectangle {
                    x: (size.width - view::UPDATE_CARD.width) / 2.0,
                    y: (size.height - view::UPDATE_CARD.height) / 2.0,
                    width: view::UPDATE_CARD.width,
                    height: view::UPDATE_CARD.height,
                };
                self.morph = Some(MorphView {
                    base: MorphBase::Root,
                    from_version: env!("CARGO_PKG_VERSION").into(),
                    to_version,
                    anim: Some(tacho::Morph {
                        from,
                        to,
                        from_rect: iced::Rectangle::with_size(size),
                        to_rect: card,
                        start: Instant::now(),
                        timeline: tacho::MorphTimeline::SHRINK,
                        events: vec![(120.0, Msg::MorphStep(MorphStep::HideBase)), (800.0, Msg::MorphStep(MorphStep::ShowCard)), (920.0, Msg::MorphStep(MorphStep::Done))],
                    }),
                    hwnd: None,
                    center: Some(center),
                });
                return window::raw_id::<Msg>(id).map(Msg::Keyed);
            }
            Msg::Keyed(hwnd) => {
                if let Some(m) = &mut self.morph {
                    update_window::color_key(hwnd, true);
                    m.hwnd = Some(hwnd);
                    // The intro's window is still hidden: show it as the update window, then grow.
                    if m.anim.is_none() && let Some(id) = self.window {
                        return Task::batch([
                            window::set_mode(id, window::Mode::Windowed),
                            Task::perform(async { std::thread::sleep(Duration::from_millis(60)) }, |_| Msg::IntroStart),
                        ]);
                    }
                }
            }
            Msg::IntroStart => {
                update_window::signal_ui_shown(&self.runtime_root);
                let size = Self::window_size();
                let card = iced::Rectangle {
                    x: (size.width - view::UPDATE_CARD.width) / 2.0,
                    y: (size.height - view::UPDATE_CARD.height) / 2.0,
                    width: view::UPDATE_CARD.width,
                    height: view::UPDATE_CARD.height,
                };
                let from = view::mosaic_of::<Msg>(view::update_card(tacho::BarStage::Launching, "", env!("CARGO_PKG_VERSION")), view::UPDATE_CARD);
                let to = self.window_mosaic(size);
                match (&mut self.morph, from, to) {
                    (Some(m), Some(from), Some(to)) => {
                        m.anim = Some(tacho::Morph {
                            from,
                            to,
                            from_rect: card,
                            to_rect: iced::Rectangle::with_size(size),
                            start: Instant::now(),
                            timeline: tacho::MorphTimeline::GROW,
                            events: vec![(100.0, Msg::MorphStep(MorphStep::HideBase)), (820.0, Msg::MorphStep(MorphStep::ShowRoot)), (950.0, Msg::MorphStep(MorphStep::Done))],
                        });
                    }
                    _ => return self.update(Msg::MorphStep(MorphStep::Done)),
                }
            }
            Msg::RehearseUpdate => {
                if !self.quitting && !self.busy && !self.driver_installing && !self.core_installing {
                    self.rehearse_after_quit = true;
                    return self.update(Msg::Quit);
                }
            }
            Msg::RehearsalReady(result) => {
                self.rehearse_after_quit = false;
                return match result {
                    Ok(()) => exit_ui(),
                    Err(error) => self.update(Msg::UpdateApplied(Err(format!("Проверка анимации: {error}")))),
                };
            }
            Msg::MorphStep(step) => {
                let Some(m) = &mut self.morph else { return Task::none() };
                match step {
                    MorphStep::HideBase => m.base = MorphBase::Key,
                    MorphStep::ShowCard => m.base = MorphBase::Card(tacho::BarStage::Waiting),
                    MorphStep::ShowRoot => m.base = MorphBase::Root,
                    MorphStep::Done => {
                        m.anim = None;
                        // Shrunk into the update window: now hand over to the watcher.
                        if let Some(center) = m.center {
                            return self.hand_over(Some(center));
                        }
                        if let Some(hwnd) = m.hwnd {
                            update_window::color_key(hwnd, false);
                        }
                        self.morph = None;
                        // The sliders' warm-up sweep plays once the app is fully there.
                        self.opened_at = Some(Instant::now());
                    }
                }
            }
        }
        Task::none()
    }
    /// Keyboard focus target of a `Msg::Bind` button.
    fn bind_focus(target: usize) -> usize {
        use focus::effects::*;
        match target {
            0 => BOOST_BIND,
            1 => PITCH_BIND,
            2 => SLOW_BIND,
            3 => FAST_BIND,
            4 => REVERSE_BIND,
            5..=9 => DISCORD_BIND_BASE + target - 5,
            10 => MONITOR_BIND,
            11 => REPLAY_BIND,
            12 => NOISE_BIND,
            SOUND_STOP_BIND => focus::soundpad::STOP_BIND,
            t => focus::soundpad::ROW_BASE + 3 * (t - SOUND_BIND_BASE) + 2,
        }
    }
    /// Store a validated key for an effect, the soundpad stop key or a clip, then push it native.
    fn set_key(&mut self, target: usize, key: u32) -> Task<Msg> {
        if target == SOUND_STOP_BIND {
            self.sound_stop_key = key;
        } else if target >= SOUND_BIND_BASE {
            if let Some(sound) = self.sounds.get_mut(target - SOUND_BIND_BASE) {
                sound.key = key;
            }
        } else {
            self.keys[target] = key;
            self.engine.bindings(self.keys);
            self.changed();
            return Task::none();
        }
        self.dirty = Some(Instant::now());
        self.sync_sound_bindings()
    }
    fn rvc_has_index(&self) -> bool {
        self.rvc_models
            .iter()
            .any(|m| m.slot == self.controls.rvc_options.slot && m.has_index)
    }
    fn rvc_can_manage(&self) -> bool {
        !self.controls.rvc
            && self.snapshot.rvc_state == 0
            && self
                .rvc_models
                .iter()
                .any(|model| model.slot == self.controls.rvc_options.slot)
    }
    fn key(&mut self, key: keyboard::Key, mods: keyboard::Modifiers, repeat: bool) -> Task<Msg> {
        use keyboard::{Key, key::Named};
        if self.window.is_none() {
            return Task::none();
        }
        if self.binding.is_some() {
            // The native capture owns the keyboard while a button waits for a key.
            if key == Key::Named(Named::Escape) {
                return self.update(Msg::CancelBind);
            }
            return Task::none();
        }
        if key == Key::Named(Named::Tab) {
            // Visual order of the rail.
            let tabs = [
                focus::TAB_BASE,
                focus::TAB_EFFECTS,
                focus::TAB_SOUNDPAD,
                focus::TAB_STUDIO,
                focus::TAB_BASE + 1,
                focus::TAB_BASE + 2,
            ];
            let order = if self.logs_page {
                use focus::logs::*;
                let mut items = vec![BACK, COPY, FOLDER, SEND];
                items.extend(tabs);
                items
            } else if self.studio_page {
                use focus::studio::*;
                let compact = Self::window_size().width < 846.0;
                let mut items = vec![RECORD, BPM, LOOP, LOOP_START, LOOP_END, ZOOM, VOLUME, HEAR, STOP, PLAY];
                if compact { items.push(LIBRARY); }
                if !compact || self.studio_library_open {
                    items.push(IMPORT);
                    for i in 0..self.studio_samples.len() { items.extend([SAMPLE_BASE + i, DELETE_BASE + i]); }
                    if self.studio_delete.is_some() { items.extend([DELETE_CONFIRM, DELETE_CANCEL]); }
                }
                items.extend((0..studio::BARS).map(|i| BAR_BASE + i));
                items.push(CURSOR);
                items.push(CLEAR);
                // One Tab stop for the roll; arrows move between its cells.
                items.push(if (CELL_BASE..CELL_BASE + 16 * studio::NOTES).contains(&self.focus) {
                    self.focus
                } else {
                    CELL_BASE + (studio::ROOT_NOTE - studio::FIRST_NOTE) as usize
                });
                items.push(EXPORT);
                items.extend(tabs);
                items
            } else if self.soundpad_page {
                use focus::soundpad::*;
                let mut items = if self.sound_folder.is_some() {
                    vec![FILTER, SORT, ADD, FOLDER, REFRESH]
                } else {
                    vec![ADD, FOLDER, REFRESH]
                };
                if self.sound_folder.is_some() {
                    items.extend((0..self.section_items().len()).map(|i| SECTION_BASE + i));
                    items.push(SECTION_ADD);
                    if self.custom_section().is_some() {
                        items.extend([SECTION_NAME, SECTION_DELETE]);
                    }
                    for i in self.visible_sounds() {
                        items.extend([ROW_BASE + 3 * i, ROW_BASE + 3 * i + 1, ROW_BASE + 3 * i + 2]);
                    }
                }
                items.extend([STOP_BIND, VOLUME, NORMALIZE, HEAR]);
                items.extend(tabs);
                items
            } else if self.details {
                use focus::settings::*;
                let mut items = if self.running() || self.busy {
                    vec![APP_AUTOSTART, AUTOSTART]
                } else {
                    vec![INPUT, OUTPUT, VERSION, BUFFER, APP_AUTOSTART, AUTOSTART]
                };
                if !self.driver_ready {
                    items.push(DRIVER);
                }
                if self.repair_confirm {
                    items.extend([REPAIR_LINES, REPAIR_REINSTALL, REPAIR_CONFIRM, REPAIR_CANCEL]);
                } else {
                    items.extend([REPAIR, REFRESH]);
                }
                items.push(UPDATE);
                if self.update_ready {
                    items.push(APPLY_UPDATE);
                }
                items.extend([PIXEL_SHIFT, SLIDER_IDLE]);
                items.extend([LOGS, QUIT, REHEARSE]);
                items.extend(tabs);
                items
            } else if self.rvc_page {
                use focus::rvc::*;
                let mut items = if self.rvc_runtime_installed {
                    vec![ENABLE, MODEL]
                } else {
                    vec![INSTALL]
                };
                if !self.rvc_importing {
                    items.push(IMPORT);
                }
                if self.rvc_can_manage() {
                    items.extend([NAME, RENAME, DELETE]);
                }
                items.push(PITCH);
                items.push(ADVANCED);
                if self.rvc_advanced {
                    if self.rvc_has_index() {
                        items.push(INDEX);
                    }
                    items.extend([GAIN, CHUNK, REFRESH]);
                }
                items.extend(tabs);
                items
            } else if self.effects_page {
                use focus::effects::*;
                let discord = |effect| DISCORD_BIND_BASE + effect;
                let mut items = vec![
                    OVERLOAD,
                    BOOST,
                    BOOST_BIND,
                    discord(0),
                    PITCH,
                    PITCH_BIND,
                    discord(1),
                    SLOW,
                    SLOW_BIND,
                    discord(2),
                    FAST,
                    FAST_BIND,
                    discord(3),
                    REVERSE_WORD,
                    REVERSE_BIND,
                    discord(4),
                ];
                if self.phrase_state != 0 {
                    items.push(CANCEL_PHRASE);
                }
                items.extend([MONITOR, MONITOR_BIND, EFFECTS_MONITOR, BOOST_MONITOR, DISCORD_VOLUME, REPLAY_BIND]);
                for i in 0..self.clips.len() {
                    items.extend([CLIP_BASE + 2 * i, CLIP_BASE + 2 * i + 1]);
                    if self.clip_menu == Some(i) {
                        items.extend([CLIP_TO_SOUNDPAD, CLIP_TO_FOLDER]);
                    }
                }
                items.extend(tabs);
                items
            } else {
                use focus::effects::*;
                let mut items = vec![];
                if !self.setup_error.is_empty() && !self.core_installing {
                    items.push(SETUP_RETRY);
                }
                if self.core_present && !self.core_installing && !self.driver_ready && !self.driver_installing {
                    items.push(SETUP_DRIVER);
                }
                if !self.setup_pending() && self.start_blocked() && (self.inputs.is_empty() || self.input.is_some()) {
                    items.push(SETUP_SETTINGS);
                }
                items.extend([INPUT, focus::headphones::OUTPUT, HEADPHONE_GEAR]);
                if self.headphone_page {
                    use focus::headphones::*;
                    items.extend([TOGGLE, NOISE, INTENSITY, VOLUME, PITCH, REVERSE]);
                    if self.headphone_needs_lines() {
                        items.push(LINES);
                    }
                }
                items.extend([ROUTE, INTENSITY, NOISE_BIND, ALT_INTENSITY]);
                items.extend(tabs);
                items
            };
            let mut order = order;
            if self.update_ready {
                order.insert(0, focus::UPDATE_BANNER);
            }
            self.focus = match order.iter().position(|&v| v == self.focus) {
                Some(i) => {
                    order[(i + if mods.shift() { order.len() - 1 } else { 1 }) % order.len()]
                }
                None => {
                    if mods.shift() {
                        *order.last().unwrap()
                    } else {
                        order[0]
                    }
                }
            };
            if self.studio_page && self.focus == focus::studio::CURSOR {
                self.studio_bar = (self.studio_cursor as usize / 16).min(studio::BARS - 1);
            }
            return Task::batch([
                view::reveal_focus(),
                if self.studio_page && (focus::studio::CELL_BASE..focus::studio::CELL_BASE + 16 * studio::NOTES).contains(&self.focus) {
                    let row = (self.focus - focus::studio::CELL_BASE) % studio::NOTES;
                    self.reveal_studio(studio::FIRST_NOTE + row as u8)
                } else { Task::none() },
                iced::widget::operation::focus(match self.focus {
                    focus::rvc::NAME => "rvc-name",
                    focus::soundpad::FILTER => "sound-filter",
                    focus::soundpad::SECTION_NAME => "section-name",
                    focus::effects::REVERSE_WORD if self.reverse_edit => "reverse-word",
                    _ => "no-text-input",
                }),
            ]);
        }
        if key == Key::Named(Named::Escape) && self.repair_confirm {return self.update(Msg::RepairCancel);}
        if key == Key::Named(Named::Escape) && self.reverse_edit {
            return self.update(Msg::ReverseEdit(false));
        }
        if key == Key::Named(Named::Escape) && self.headphone_page {
            return self.update(Msg::HeadphonePanel(false));
        }
        if key == Key::Named(Named::Escape) && self.logs_page {
            return self.update(Msg::Page(2));
        }
        if key == Key::Named(Named::Escape)
            && (self.details || self.rvc_page || self.soundpad_page || self.effects_page || self.studio_page)
        {
            return self.update(Msg::Page(0));
        }
        let activate = matches!(key, Key::Named(Named::Enter | Named::Space)) && !repeat;
        if activate && self.focus == focus::UPDATE_BANNER && self.update_ready {
            return self.update(Msg::ApplyUpdate);
        }
        if activate {
            if (focus::TAB_BASE..focus::TAB_BASE + 4).contains(&self.focus) {
                return self.update(Msg::Page((self.focus - focus::TAB_BASE) as u8));
            }
            if self.focus == focus::TAB_SOUNDPAD {
                return self.update(Msg::Page(4));
            }
            if self.focus == focus::TAB_LOGS {
                return self.update(Msg::Page(5));
            }
            if self.focus == focus::TAB_EFFECTS {
                return self.update(Msg::Page(6));
            }
            if self.focus == focus::TAB_STUDIO {
                return self.update(Msg::Page(7));
            }
        }
        let delta = match key {
            Key::Named(Named::ArrowLeft | Named::ArrowDown) => -1,
            Key::Named(Named::ArrowRight | Named::ArrowUp) => 1,
            _ => 0,
        };
        if self.studio_page && self.focus == focus::studio::CURSOR && delta != 0 {
            let step = (self.studio_cursor.floor() as i32 + delta).clamp(0, studio::STEPS as i32 - 1);
            self.seek_studio(step as usize);
            return Task::none();
        }
        if self.studio_page && (focus::studio::CELL_BASE..focus::studio::CELL_BASE + 16 * studio::NOTES).contains(&self.focus) {
            let i = self.focus - focus::studio::CELL_BASE;
            let (mut step, mut note) = (i / studio::NOTES, i % studio::NOTES);
            match key {
                Key::Named(Named::ArrowLeft) => step = step.saturating_sub(1),
                Key::Named(Named::ArrowRight) => step = (step + 1).min(15),
                Key::Named(Named::ArrowDown) => note = note.saturating_sub(1),
                Key::Named(Named::ArrowUp) => note = (note + 1).min(studio::NOTES - 1),
                _ => {},
            }
            let next = focus::studio::CELL_BASE + step * studio::NOTES + note;
            if next != self.focus {
                self.focus = next;
                return self.reveal_studio(studio::FIRST_NOTE + note as u8);
            }
        }
        let message = if self.logs_page {
            match self.focus {
                focus::logs::COPY if activate => Msg::LogsCopy,
                focus::logs::FOLDER if activate => Msg::LogsFolder,
                focus::logs::SEND if activate => Msg::SendReport,
                focus::logs::BACK if activate => Msg::Page(2),
                _ => Msg::Noop,
            }
        } else if self.studio_page {
            use focus::studio::*;
            match self.focus {
                RECORD if activate => Msg::StudioRecord,
                IMPORT if activate => Msg::StudioImport,
                BPM if delta != 0 => Msg::StudioBpm((self.studio_bpm as i32 + delta).clamp(60, 200) as u32),
                BPM if activate => Msg::StudioBpm((self.studio_bpm + 1).min(200)),
                LOOP if activate => Msg::StudioLoopEnabled(!self.studio_loop),
                LOOP_START if delta != 0 => Msg::StudioLoopStart((self.studio_loop_start as i32 + delta).clamp(0, self.studio_loop_end as i32 - 1) as usize),
                LOOP_START if activate => Msg::StudioLoopStart(self.studio_cursor as usize),
                LOOP_END if delta != 0 => Msg::StudioLoopEnd((self.studio_loop_end as i32 + delta).clamp(self.studio_loop_start as i32 + 1, studio::STEPS as i32) as usize),
                LOOP_END if activate => Msg::StudioLoopEnd(self.studio_cursor as usize + 1),
                ZOOM if delta != 0 => Msg::StudioZoom((self.studio_zoom as i32 + delta).clamp(0, studio::ZOOMS.len() as i32 - 1) as usize),
                ZOOM if activate => Msg::StudioZoom((self.studio_zoom + 1).min(studio::ZOOMS.len() - 1)),
                VOLUME if delta != 0 => Msg::StudioVolume((self.studio_volume as i32 + delta * 10).clamp(0, 200) as u32),
                VOLUME if activate => Msg::StudioVolume((self.studio_volume + 10).min(200)),
                HEAR if activate => Msg::SoundpadHear(!self.sound_monitor),
                STOP if activate => Msg::SoundpadStop,
                PLAY if activate => Msg::StudioRender(false),
                EXPORT if activate => Msg::StudioRender(true),
                CLEAR if activate => Msg::StudioClear,
                LIBRARY if activate => Msg::StudioLibraryToggle,
                DELETE_CONFIRM if activate => Msg::StudioDeleteConfirm,
                DELETE_CANCEL if activate => Msg::StudioDeleteCancel,
                f if activate && (BAR_BASE..BAR_BASE + studio::BARS).contains(&f) => Msg::StudioBar(f - BAR_BASE),
                f if activate && (SAMPLE_BASE..SAMPLE_BASE + self.studio_samples.len()).contains(&f) =>
                    Msg::StudioSelect(self.studio_samples[f - SAMPLE_BASE].clone()),
                f if activate && (DELETE_BASE..DELETE_BASE + self.studio_samples.len()).contains(&f) =>
                    Msg::StudioDeleteAsk(self.studio_samples[f - DELETE_BASE].clone()),
                f if activate && (CELL_BASE..CELL_BASE + 16 * studio::NOTES).contains(&f) => {
                    let i = f - CELL_BASE;
                    Msg::StudioCell(self.studio_bar * 16 + i / studio::NOTES, studio::FIRST_NOTE + (i % studio::NOTES) as u8)
                }
                _ => Msg::Noop,
            }
        } else if self.soundpad_page {
            use focus::soundpad::*;
            match self.focus {
                FOLDER if activate => Msg::SoundpadFolder,
                ADD if activate => Msg::SoundpadAdd,
                REFRESH if activate => Msg::SoundpadRefresh,
                VOLUME if delta != 0 => {
                    Msg::SoundpadVolume(self.sound_volume * 100.0 + delta as f32)
                }
                NORMALIZE if activate => Msg::SoundpadNormalize(!self.sound_normalize),
                HEAR if activate => Msg::SoundpadHear(!self.sound_monitor),
                STOP_BIND if activate => Msg::Bind(SOUND_STOP_BIND),
                SORT if delta != 0 || activate => {
                    let i = SoundSort::ALL
                        .iter()
                        .position(|&v| v == self.sound_sort)
                        .unwrap_or(0);
                    Msg::SoundpadSort(
                        SoundSort::ALL[(i as i32 + if delta == 0 { 1 } else { delta }).rem_euclid(4)
                            as usize],
                    )
                }
                SECTION_ADD if activate => Msg::SectionAdd,
                SECTION_DELETE if activate => Msg::SectionDelete,
                f if activate && f >= SECTION_BASE && f - SECTION_BASE < self.section_items().len() => {
                    Msg::SectionSelect(f - SECTION_BASE)
                }
                f if f >= ROW_BASE && (f - ROW_BASE) / 3 < self.sounds.len() => {
                    let (i, column) = ((f - ROW_BASE) / 3, (f - ROW_BASE) % 3);
                    match column {
                        0 if activate => Msg::SoundPlay(i),
                        1 if delta != 0 => {
                            Msg::SoundVolume(i, self.sounds[i].volume as f32 + delta as f32 * 5.0)
                        }
                        2 if activate => Msg::Bind(SOUND_BIND_BASE + i),
                        _ => Msg::Noop,
                    }
                }
                _ => Msg::Noop,
            }
        } else if (focus::headphones::OUTPUT..=focus::headphones::REVERSE).contains(&self.focus) {
            use focus::headphones::*;
            match self.focus {
                OUTPUT if delta != 0 || activate => {
                    let devices = self.headphone_outputs();
                    if devices.is_empty() {
                        Msg::Noop
                    } else {
                        let i = self
                            .headphone_output
                            .as_ref()
                            .and_then(|d| devices.iter().position(|v| v == d))
                            .unwrap_or(0);
                        Msg::HeadphoneOutput(
                            devices[(i as i32 + if delta == 0 { 1 } else { delta })
                                .rem_euclid(devices.len() as i32)
                                as usize]
                                .clone(),
                        )
                    }
                }
                TOGGLE if activate => Msg::HeadphoneToggle,
                NOISE if activate => Msg::HeadphoneNoise(!self.headphone_denoise),
                INTENSITY if delta != 0 => {
                    Msg::HeadphoneIntensity(self.headphone_intensity * 100.0 + delta as f32)
                }
                VOLUME if delta != 0 => {
                    Msg::HeadphoneVolume(self.headphone_volume * 100.0 + delta as f32)
                }
                PITCH if delta != 0 => {
                    Msg::HeadphonePitch(self.headphone_pitch as f32 + delta as f32)
                }
                REVERSE if activate => Msg::HeadphoneReverse(!self.headphone_reverse),
                LINES if activate => Msg::HeadphoneLines,
                _ => Msg::Noop,
            }
        } else if self.details {
            use focus::settings::*;
            match self.focus {
                INPUT | OUTPUT if delta != 0 || activate => {
                    let items = if self.focus == INPUT {
                        &self.inputs
                    } else {
                        &self.outputs
                    };
                    let selected = if self.focus == INPUT {
                        &self.input
                    } else {
                        &self.output
                    };
                    if items.is_empty() {
                        Msg::Noop
                    } else {
                        let i = selected
                            .as_ref()
                            .and_then(|d| items.iter().position(|x| x == d))
                            .unwrap_or(0);
                        let d = items[(i as i32 + if delta == 0 { 1 } else { delta })
                            .rem_euclid(items.len() as i32)
                            as usize]
                            .clone();
                        if self.focus == INPUT {
                            Msg::Input(d)
                        } else {
                            Msg::Output(d)
                        }
                    }
                }
                VERSION if delta != 0 || activate => {
                    Msg::Version(if self.version == 1 { 2 } else { 1 })
                }
                BUFFER if delta != 0 || activate => {
                    let values = [10, 20, 30, 40, 60, 80];
                    let i = values.iter().position(|&v| v == self.buffer).unwrap_or(3);
                    Msg::Buffer(
                        values[(i as i32 + if delta == 0 { 1 } else { delta }).rem_euclid(6)
                            as usize],
                    )
                }
                REFRESH if activate => Msg::Refresh,
                UPDATE if activate => Msg::UpdateCheck,
                APPLY_UPDATE if activate => Msg::ApplyUpdate,
                DRIVER if activate => Msg::InstallDriver,
                REPAIR if activate => Msg::Repair,
                REPAIR_REINSTALL if activate => Msg::RepairReinstall(!self.repair_reinstall),
                REPAIR_LINES if activate => Msg::RepairLines(!self.repair_lines),
                REPAIR_CONFIRM if activate => Msg::RepairConfirm,
                REPAIR_CANCEL if activate => Msg::RepairCancel,
                DONE if activate => Msg::Settings,
                QUIT if activate => Msg::Quit,
                AUTOSTART if activate => Msg::Autostart(!self.autostart),
                APP_AUTOSTART if activate => Msg::AppAutostart(!self.app_autostart),
                PIXEL_SHIFT if activate => Msg::PixelShift(!self.pixel_shift),
                SLIDER_IDLE if activate => Msg::SliderIdle(!self.slider_idle),
                LOGS if activate => Msg::Page(5),
                REHEARSE if activate => Msg::RehearseUpdate,
                _ => Msg::Noop,
            }
        } else {
            use focus::effects::*;
            match self.focus {
                INPUT if (delta != 0 || activate) && !self.inputs.is_empty() => {
                    let i = self
                        .input
                        .as_ref()
                        .and_then(|d| self.inputs.iter().position(|v| v == d))
                        .unwrap_or(0);
                    Msg::Input(
                        self.inputs[(i as i32 + if delta == 0 { 1 } else { delta })
                            .rem_euclid(self.inputs.len() as i32)
                            as usize]
                            .clone(),
                    )
                }
                MONITOR if activate => Msg::Monitor,
                HEADPHONE_GEAR if activate => Msg::HeadphonePanel(!self.headphone_page),
                ROUTE if activate => Msg::RouteToggle,
                SETUP_RETRY if activate => Msg::RetryCore,
                SETUP_DRIVER if activate => Msg::InstallDriver,
                SETUP_SETTINGS if activate => Msg::Page(2),
                REVERSE_WORD if activate && !self.reverse_edit => Msg::ReverseEdit(true),
                EFFECTS_MONITOR if activate => Msg::EffectsMonitor(!self.effects_monitor),
                BOOST_MONITOR if activate => Msg::BoostMonitor(!self.boost_monitor),
                MONITOR_BIND if activate => Msg::Bind(10),
                REPLAY_BIND if activate => Msg::Bind(11),
                focus::rvc::INSTALL if activate => Msg::RvcInstall,
                focus::rvc::ENABLE if activate => Msg::Rvc(!self.controls.rvc),
                focus::rvc::MODEL if (delta != 0 || activate) && !self.rvc_models.is_empty() => {
                    let i = self
                        .rvc_models
                        .iter()
                        .position(|m| m.slot == self.controls.rvc_options.slot)
                        .unwrap_or(0);
                    Msg::RvcModel(
                        self.rvc_models[(i as i32 + if delta == 0 { 1 } else { delta })
                            .rem_euclid(self.rvc_models.len() as i32)
                            as usize]
                            .clone(),
                    )
                }
                focus::rvc::PITCH if delta != 0 => {
                    Msg::RvcPitch((self.controls.rvc_options.pitch + delta) as f32)
                }
                focus::rvc::INDEX if delta != 0 && self.rvc_has_index() => {
                    Msg::RvcIndex(self.controls.rvc_options.index as f32 + delta as f32)
                }
                focus::rvc::GAIN if delta != 0 => {
                    Msg::RvcGain(self.controls.rvc_options.gain as f32 + delta as f32)
                }
                focus::rvc::CHUNK if delta != 0 || activate => {
                    let i = rvc::CHUNKS
                        .iter()
                        .position(|&v| v == self.controls.rvc_options.chunk)
                        .unwrap_or(2);
                    Msg::RvcChunk(
                        rvc::CHUNKS[(i as i32 + if delta == 0 { 1 } else { delta }).rem_euclid(5)
                            as usize],
                    )
                }
                focus::rvc::REFRESH if activate => Msg::RvcRefresh,
                focus::rvc::IMPORT if activate => Msg::RvcImport,
                focus::rvc::RENAME if activate => Msg::RvcRename,
                focus::rvc::DELETE if activate => Msg::RvcDelete,
                focus::rvc::ADVANCED if activate => Msg::RvcAdvanced,
                SLOW if delta != 0 => {
                    // The slider is mirrored (slower = right), so Right slows down.
                    Msg::Slow((self.controls.slow * 100.0 - delta as f32).round().clamp(50.0, 95.0))
                }
                SLOW_BIND if activate => Msg::Bind(2),
                FAST if delta != 0 => {
                    Msg::Fast((self.controls.fast * 100.0 + delta as f32).round().clamp(105.0, 200.0))
                }
                FAST_BIND if activate => Msg::Bind(3),
                CANCEL_PHRASE if activate => Msg::CancelPhrase,
                REVERSE_BIND if activate => Msg::Bind(4),
                f if activate && (DISCORD_BIND_BASE..DISCORD_BIND_BASE + 5).contains(&f) => {
                    Msg::Bind(5 + f - DISCORD_BIND_BASE)
                }
                OVERLOAD if activate => Msg::Overload(!self.controls.overload),
                DISCORD_VOLUME if delta != 0 => Msg::DiscordVolume(
                    discord_volume_percent(self.controls.discord_volume) + delta as f32,
                ),
                INTENSITY if delta != 0 => Msg::Intensity(
                    (self.controls.intensity * 100.0 + delta as f32).clamp(0.0, 200.0),
                ),
                ALT_INTENSITY if delta != 0 => Msg::AlternateIntensity(
                    (self.controls.alternate_intensity * 100.0 + delta as f32).clamp(0.0, 200.0),
                ),
                NOISE_BIND if activate => Msg::Bind(12),
                BOOST if delta != 0 => Msg::Boost(
                    (self.controls.boost * 100.0 + delta as f32).round().clamp(100.0, 2000.0),
                ),
                BOOST_BIND if activate => Msg::Bind(0),
                PITCH if delta != 0 => {
                    Msg::Pitch((self.controls.pitch + delta).clamp(-12, 12) as f32)
                }
                PITCH_BIND if activate => Msg::Bind(1),
                CLIP_TO_SOUNDPAD if activate => match self.clip_menu {
                    Some(i) => Msg::ClipSave(i, true),
                    None => Msg::Noop,
                },
                CLIP_TO_FOLDER if activate => match self.clip_menu {
                    Some(i) => Msg::ClipSave(i, false),
                    None => Msg::Noop,
                },
                f if activate && f >= CLIP_BASE && (f - CLIP_BASE) / 2 < self.clips.len() => {
                    if (f - CLIP_BASE).is_multiple_of(2) {
                        Msg::ClipPlay((f - CLIP_BASE) / 2)
                    } else {
                        Msg::ClipMenu(Some((f - CLIP_BASE) / 2))
                    }
                }
                _ => Msg::Noop,
            }
        };
        self.update(message)
    }
    fn subscription(&self) -> Subscription<Msg> {
        Subscription::batch([
            if self.scroll_anims.is_empty() {
                Subscription::none()
            } else {
                window::frames().map(Msg::ScrollFrame)
            },
            window::close_requests().map(|_| Msg::Hide),
            iced::event::listen_with(|event, _, id| {
                match event {
                    iced::Event::Window(window::Event::Focused) => {
                        return Some(Msg::WindowFocus(id, true));
                    }
                    iced::Event::Window(window::Event::Unfocused) => {
                        return Some(Msg::WindowFocus(id, false));
                    }
                    iced::Event::Window(window::Event::Resized(size))
                        if size.width == 0.0 && size.height == 0.0 =>
                    {
                        return Some(Msg::Minimized(id));
                    }
                    iced::Event::Window(window::Event::Resized(_)) => {
                        return Some(Msg::Restored(id));
                    }
                    _ => {}
                }
                match event {
                    iced::Event::Keyboard(keyboard::Event::KeyPressed {
                        key,
                        modifiers,
                        repeat,
                        ..
                    }) => Some(Msg::Key(key, modifiers, repeat)),
                    iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Left,
                    )) => Some(Msg::DragEnd),
                    iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Right,
                    )) => Some(Msg::StudioRightEnd),
                    iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    )) => Some(Msg::PointerDown),
                    _ => None,
                }
            }),
        ])
    }
}
fn main() {
    // A GUI-subsystem app has no console: without this a panic vanishes without a trace.
    if let Some(root)=paths::Paths::resolve().ok().map(|p|p.data) {
        std::panic::set_hook(Box::new(move |info| {
            let logs=root.join("Logs");
            let _=std::fs::create_dir_all(&logs);
            let text=format!("{} {}\n{}\n\n",env!("CARGO_PKG_VERSION"),info,std::backtrace::Backtrace::force_capture());
            use std::io::Write;
            if let Ok(mut file)=std::fs::OpenOptions::new().create(true).append(true).open(logs.join("rust-ui-panic.log")) {let _=file.write_all(text.as_bytes());}
        }));
    }
    // Host maintenance must run before every update; implicit startup apply bypasses it.
    let args:Vec<_>=std::env::args_os().collect();
    let upgrade=args.iter().position(|arg|arg=="--upgrade-legacy");
    let upgrade_launcher=std::env::current_exe().ok().and_then(|p|p.file_stem().map(|s|s.to_string_lossy().eq_ignore_ascii_case("MicNoizeUpgrade"))).unwrap_or(false);
    let legacy=if upgrade.is_some() || upgrade_launcher{Ok(())}else{maintenance::preserve_legacy()};
    if legacy.is_ok() && upgrade.is_none() && !upgrade_launcher{velopack::VelopackApp::build().set_auto_apply_on_startup(false).run();}
    cpu_denoise::register();
    let root = paths::Paths::resolve().ok().map(|p| p.data);
    let mut result = (|| -> Result<(), String> {
        legacy?;
        if let Some(at)=upgrade {
            if !args.iter().any(|arg|arg=="--repair-lines"){return Err("Переход требует явного разрешения на перенос старых виртуальных линий (--repair-lines)".into());}
            let install=args.get(at+1).ok_or("Путь установленной программы не указан")?;
            let package=args.get(at+2).ok_or("Полный пакет обновления не указан")?;
            return maintenance::upgrade_legacy(Path::new(install),Path::new(package));
        }
        if upgrade_launcher {
            let exe=std::env::current_exe().map_err(|e|e.to_string())?;
            let directory=exe.parent().ok_or("Папка установщика не найдена")?;
            let packages:Vec<_>=std::fs::read_dir(directory).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?
                .into_iter().map(|entry|entry.path()).filter(|path|path.extension().is_some_and(|ext|ext=="nupkg")).collect();
            if packages.len()!=1{return Err("Рядом с установщиком должен находиться один полный пакет обновления .nupkg".into());}
            maintenance::package(&packages[0],env!("CARGO_PKG_VERSION"))?;
            let install=std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or("LOCALAPPDATA не задан")?.join("MicNoize");
            #[link(name="user32")]
            unsafe extern "system" {fn MessageBoxW(window:isize,text:*const u16,title:*const u16,flags:u32)->i32;}
            let text:Vec<u16>=format!("Закройте Mic Noize перед продолжением.\n\nУстановить обновление {} и восстановить виртуальное устройство? Перед заменой будет сохранён предыдущий комплект.\n\nСтарые виртуальные линии могут быть перенесены. После этого может потребоваться заново выбрать Mic Noize в Discord.\0",env!("CARGO_PKG_VERSION")).encode_utf16().collect();
            let title:Vec<u16>="Восстановить устройство и обновить Mic Noize\0".encode_utf16().collect();
            if unsafe{MessageBoxW(0,text.as_ptr(),title.as_ptr(),0x24)}!=6{return Ok(());}
            return maintenance::upgrade_legacy(&install,&packages[0]);
        }
        // The watcher of a silent update shows the app's own update window while it works.
        if args.iter().any(|arg|arg=="--watch-update") && let Some(at)=args.iter().position(|arg|arg=="--update-window") {
            let runtime=args.iter().position(|arg|arg=="--recover-update").and_then(|i|args.get(i+1)).map(PathBuf::from).ok_or("Recovery runtime missing")?;
            let center=args.get(at+1).and_then(|v|v.to_str()).and_then(update_window::parse_point);
            return update_window::watch(runtime,center,None,maintenance::startup);
        }
        // The settings' rehearsal: a stand-in watcher that pretends to install, then restarts
        // the app so it grows out of the update window exactly as after a real update.
        if let Some(at)=args.iter().position(|arg|arg=="--ui-update-rehearsal") {
            let runtime=paths::Paths::resolve()?.runtime_root().to_path_buf();
            std::fs::create_dir_all(runtime.join(".update")).map_err(|e|e.to_string())?;
            let center=args.get(at+1).and_then(|v|v.to_str()).and_then(update_window::parse_point);
            let old=args.get(at+2).and_then(|v|v.to_str()).and_then(|v|v.parse::<u32>().ok());
            let exe=std::env::current_exe().map_err(|e|e.to_string())?;
            let version=env!("CARGO_PKG_VERSION").to_owned();
            return update_window::watch(runtime,center,Some((version.clone(),version)),move||{
                // The old UI exits as soon as this window stands; the pause is the "install".
                std::thread::sleep(Duration::from_secs(3));
                if let Some(pid)=old{update_window::wait_exit(pid,Duration::from_secs(10));}
                Command::new(&exe).spawn().map_err(|e|e.to_string())?;
                Ok(false)
            });
        }
        // QA: the update window alone, on a throwaway state folder, with a pretend install.
        if args.iter().any(|arg|arg=="--ui-update-preview") {
            let runtime=std::env::temp_dir().join("micnoize-update-preview");
            std::fs::create_dir_all(runtime.join(".update")).map_err(|e|e.to_string())?;
            let center=args.iter().position(|arg|arg=="--ui-update-preview").and_then(|i|args.get(i+1)).and_then(|v|v.to_str()).and_then(update_window::parse_point);
            return update_window::watch(runtime,center,Some(("0.2.14".into(),"0.2.15".into())),||{std::thread::sleep(Duration::from_secs(4));Ok(false)});
        }
        if !maintenance::startup()? { return Ok(()); }
        if args.iter().any(|arg|arg=="--ui-benchmark") && let Some(at)=args.iter().position(|arg|arg=="--check-update-package") {
            let package=args.get(at+1).ok_or("Check package path missing")?;
            return updater::check_local_package(std::path::Path::new(package));
        }
        let Some((app, task)) = App::new()? else {
            return Ok(());
        };
        let state = std::cell::RefCell::new(Some((app, task)));
        iced::daemon(
            move || state.borrow_mut().take().expect("boot once"),
            App::update,
            App::view,
        )
        .title("Mic Noize")
        .theme(|_: &App, _: window::Id| {
            Theme::custom(
                "Graphite",
                iced::theme::Palette {
                    background: view::BG,
                    text: view::INK,
                    primary: view::ORANGE,
                    success: view::GREEN,
                    danger: view::RED,
                    warning: view::ORANGE,
                },
            )
        })
        .style(|s: &App, _| iced::theme::Style { background_color: s.backdrop(), text_color: view::INK })
        .default_font(Font::with_name("Segoe UI"))
        .subscription(App::subscription)
        .scale_factor(|s: &App, _| s.qa_scale)
        .run()
        .map_err(|e| e.to_string())
    })();
    if result.is_ok() && RESTART.swap(false, Ordering::Relaxed) {
        result = std::env::current_exe()
            .and_then(|exe| Command::new(exe).spawn())
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
    if let Err(e) = result {
        if let Some(root) = root {
            let _ = std::fs::create_dir_all(root.join("Logs"));
            let _ = std::fs::write(root.join("Logs/rust-ui-error.log"), &e);
        }
        eprintln!("{e}");
        // Startup recovery failures must remain visible even in the GUI subsystem build.
        unsafe extern "system" {fn MessageBoxW(window:isize,text:*const u16,title:*const u16,flags:u32)->i32;}
        let text:Vec<u16>=format!("Mic Noize не удалось запустить:\n\n{e}\0").encode_utf16().collect();
        let title:Vec<u16>="Mic Noize\0".encode_utf16().collect();
        unsafe{MessageBoxW(0,text.as_ptr(),title.as_ptr(),0x10);}
    }
}

#[cfg(test)]
mod controller_tests {
    use super::*;
    #[test]
    fn tag_autostart_defaults_on_only_for_new_installs() {
        let mut settings = Settings::for_test("");
        assert!(tag_autostart_default(&settings, false));
        settings.path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(!tag_autostart_default(&settings, false));
        assert!(tag_autostart_default(&settings, true));
        settings.set("ui", "tag_autostart", 0);
        assert!(!tag_autostart_default(&settings, true));
    }
    #[test]
    fn update_shrink_and_grow_walk_their_steps() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        let id = App::open(1.0, None).0;
        app.window = Some(id);
        app.update_version = Some("9.9.9".into());
        // «Перезапустить»: the window shrinks into the update window, then hands over.
        let _ = app.update(Msg::DecayGeometry(Some(iced::Point::new(100.0, 50.0)), Size::new(1040.0, 740.0)));
        let m = app.morph.as_ref().expect("shrink starts");
        assert!(matches!(m.base, MorphBase::Root) && m.anim.is_some());
        assert_eq!(m.center, Some(iced::Point::new(620.0, 420.0)));
        assert_eq!(m.to_version, "9.9.9");
        let _ = app.update(Msg::MorphStep(MorphStep::HideBase));
        assert!(matches!(app.morph.as_ref().unwrap().base, MorphBase::Key));
        let _ = app.update(Msg::MorphStep(MorphStep::ShowCard));
        assert!(matches!(app.morph.as_ref().unwrap().base, MorphBase::Card(tacho::BarStage::Waiting)));
        let _ = app.update(Msg::MorphStep(MorphStep::Done));
        assert!(app.morph.as_ref().is_some_and(|m| m.anim.is_none()), "the card stays until the process exits");
        // A failed hand-over gives the app back.
        let _ = app.update(Msg::UpdateApplied(Err("test".into())));
        assert!(app.morph.is_none());
        // First start after an update: stand in as the update window, then grow into the app.
        app.intro = Some(iced::Point::new(620.0, 420.0));
        let _ = app.update(Msg::Opened(id));
        assert!(matches!(app.morph.as_ref().unwrap().base, MorphBase::Card(tacho::BarStage::Launching)));
        let _ = app.update(Msg::IntroStart);
        assert!(app.morph.as_ref().unwrap().anim.is_some());
        let _ = app.update(Msg::MorphStep(MorphStep::HideBase));
        let _ = app.update(Msg::MorphStep(MorphStep::ShowRoot));
        let _ = app.update(Msg::MorphStep(MorphStep::Done));
        assert!(app.morph.is_none() && app.intro.is_none());
    }
    #[test]
    fn studio_page_edits_and_keyboard() {
        use keyboard::{Key, Modifiers, key::Named};
        let saved = "[studio]\nbpm=137\nevents=[{\"step\":0,\"note\":60,\"sample\":\"kick.wav\"}]";
        let (mut app, _) = App::from_settings(Settings::for_test(saved)).unwrap().unwrap();
        assert_eq!(app.studio_bpm, 137);
        assert_eq!(app.studio_events.len(), 1);
        app.studio_samples.push("kick.wav".into());
        app.window = Some(App::open(1.0, None).0);
        let _ = app.update(Msg::Page(7));
        assert!(app.studio_page && !app.soundpad_page);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::studio::RECORD);
        app.focus = focus::studio::BPM;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::studio::LOOP);
        for _ in 0..5 { let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false); }
        assert_eq!(app.focus, focus::studio::HEAR);
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.sound_monitor && app.monitor_mode() == 5);
        assert_eq!(app.focus, focus::studio::HEAR);
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(!app.sound_monitor);
        let _ = app.update(Msg::StudioGrab(17));
        assert_eq!((app.studio_cursor, app.studio_bar, app.focus), (17.0, 1, focus::studio::CURSOR));
        assert!(app.studio_follow);
        let _ = app.update(Msg::StudioDrag(18));
        assert_eq!(app.studio_cursor, 18.0);
        let _ = app.update(Msg::DragEnd);
        let _ = app.update(Msg::StudioDrag(19));
        assert_eq!(app.studio_cursor, 18.0, "releasing the ruler ends the drag");
        let _ = app.key(Key::Named(Named::ArrowLeft), Modifiers::empty(), false);
        assert_eq!(app.studio_cursor, 17.0);
        let _ = app.update(Msg::StudioSelect("kick.wav".into()));
        let _ = app.update(Msg::StudioCell(1, 60));
        assert_eq!(app.studio_events.len(), 2);
        let _ = app.update(Msg::StudioBpm(145));
        assert_eq!(app.studio_bpm, 145);
        let _ = app.update(Msg::StudioBar(1));
        assert!(!app.studio_follow, "a chosen bar must stay visible while another bar plays");
        let _ = app.update(Msg::StudioCell(16, 61));
        let _ = app.update(Msg::StudioCell(17, 36));
        let _ = app.update(Msg::StudioCell(18, 83));
        app.focus = focus::studio::CELL_BASE + (studio::ROOT_NOTE - studio::FIRST_NOTE) as usize;
        let _ = app.key(Key::Named(Named::ArrowUp), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::studio::CELL_BASE + 25);
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::studio::CELL_BASE + studio::NOTES + 25);
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.studio_events.iter().any(|e| e.step == 17 && e.note == 61));
        let _ = app.update(Msg::StudioClear);
        assert_eq!(app.studio_events.len(), 2);
        let _ = app.key(Key::Named(Named::Escape), Modifiers::empty(), false);
        assert!(!app.studio_page);
    }
    #[test]
    fn studio_drag_loop_zoom_and_delete_confirmation() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.studio_samples.push("tone.wav".into());
        app.studio_selected = Some("tone.wav".into());
        let _ = app.update(Msg::StudioDrawStart(8, 60));
        let _ = app.update(Msg::StudioCellEnter(11, 60));
        assert_eq!((app.studio_events[0].step, app.studio_events[0].length), (8, 4));
        let _ = app.update(Msg::StudioCellEnter(9, 60));
        assert_eq!(app.studio_events[0].length, 2, "dragging back shortens the note");
        let _ = app.update(Msg::DragEnd);
        let _ = app.update(Msg::StudioEraseStart(9, 60));
        assert!(app.studio_events.is_empty());
        let _ = app.update(Msg::StudioRightEnd);
        assert!(!app.studio_erasing);
        app.studio_events.push(studio::Event { step: 8, note: 60, sample: "tone.wav".into(), length: 1 });
        app.snapshot.state = 3;
        let _ = app.update(Msg::StudioLoopEnabled(true));
        assert!(app.studio_live && app.studio_loop && app.studio_busy, "enabling a loop must schedule playback");
        app.studio_busy = false;
        let _ = app.update(Msg::SoundpadStop);
        assert!(!app.studio_live && !app.studio_loop, "Stop must release loop playback");
        let _ = app.update(Msg::StudioLoopStart(16));
        let _ = app.update(Msg::StudioLoopEnd(32));
        assert!(app.studio_loop && (app.studio_loop_start, app.studio_loop_end) == (16, 32));
        app.studio_busy = true;
        let _ = app.update(Msg::StudioRender(false));
        assert!(app.studio_rebuild, "edits during rendering must schedule a fresh loop");
        app.studio_busy = false;
        let _ = app.update(Msg::StudioVolume(170));
        assert_eq!(app.studio_volume, 170);
        let _ = app.update(Msg::StudioZoom(0));
        assert_eq!(studio::ZOOMS[app.studio_zoom], 50);
        let _ = app.update(Msg::StudioDeleteAsk("tone.wav".into()));
        assert_eq!(app.studio_delete.as_deref(), Some("tone.wav"));
        let _ = app.update(Msg::StudioDeleteCancel);
        assert!(app.studio_delete.is_none());
    }
    #[test]
    fn repair_requires_confirmation_and_exit_prevents_resume() {
        use keyboard::{Key,Modifiers,key::Named};
        let (mut app,_) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window=Some(App::open(1.0, None).0);app.details=true;app.snapshot.state=2;app.focus=focus::NONE;
        let (mut refresh,mut repair)=(false,false);
        for _ in 0..40 {let _=app.key(Key::Named(Named::Tab),Modifiers::empty(),false);refresh|=app.focus==focus::settings::REFRESH;repair|=app.focus==focus::settings::REPAIR;}
        assert!(refresh && repair,"Running routes need keyboard access to both device actions");
        let _=app.update(Msg::Repair);assert!(app.repair_confirm && !app.repair_reinstall && !app.repair_lines && !app.driver_installing);
        app.focus=focus::settings::REPAIR_LINES;
        let _=app.key(Key::Named(Named::Space),Modifiers::empty(),false);assert!(app.repair_lines);
        let _=app.key(Key::Named(Named::Escape),Modifiers::empty(),false);assert!(!app.repair_confirm);
        let _=app.update(Msg::Repair);assert!(!app.repair_lines);let _=app.update(Msg::RepairConfirm);
        assert!(app.driver_installing && app.repair_resume.is_some());
        let _=app.update(Msg::Quit);assert!(app.quit_after_repair && !app.quitting);
        let _=app.update(Msg::Repaired(Err("UAC cancelled".into())));
        assert!(app.quitting && app.repair_resume.is_none() && !app.driver_installing);
    }
    #[test]
    fn failed_update_can_resume_the_control_worker() {
        let (app,_) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.engine.quit();let deadline=Instant::now()+Duration::from_secs(3);
        while !matches!(app.engine.reply(),Some(Reply::Quit)) {assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        app.engine.resume_after_failed_update();
        app.engine.start(Config{input:String::new(),output:"TAG".into(),version:2,buffer:40,period:5,graphs:-1,intensity:1.0});
        loop {if let Some(Reply::Started(_,result))=app.engine.reply(){assert!(result.is_err());break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
    }
    #[test]
    fn refresh_keeps_running_route_and_cancels_pending_start() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.snapshot.state = 2;
        app.auto_started = true;
        let _ = app.update(Msg::Refresh);
        assert!(app.auto_started && app.running());
        app.snapshot.state = 1;
        app.busy = true;
        let _ = app.update(Msg::Refresh);
        assert!(!app.busy && !app.auto_started);
        app.engine.quit();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut devices = 0;
        loop {
            match app.engine.reply() {
                Some(Reply::Devices(_)) => devices += 1,
                Some(Reply::Quit) => break,
                _ => assert!(Instant::now() < deadline),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(devices, 1, "Repeated Refresh requests must coalesce");
    }
    #[test]
    fn quit_cancels_queued_starts_and_discards_late_replies() {
        let (app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        let invalid = Config { input: String::new(), output: "TAG".into(), version: 2, buffer: 40, period: 5, graphs: -1, intensity: 1.0 };
        for _ in 0..32 { app.engine.start(invalid.clone()); }
        app.engine.quit();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match app.engine.reply() {
                Some(Reply::Started(..)) => panic!("A cancelled start reached the controller"),
                Some(Reply::Quit) => break,
                _ => assert!(Instant::now() < deadline, "Quit did not finish"),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.engine.snapshot(false).0.state, 0);
    }
    #[test]
    fn scrolling_advances_on_frames_and_cancels_on_navigation() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.soundpad_page = true;
        let _ = app.update(Msg::Wheel("body", 96.0));
        let _ = app.update(Msg::ScrollProbe("body", Some((0.0, 500.0, 5000.0))));
        let start = app.scroll_anims["body"].last_frame;
        let _ = app.update(Msg::ScrollFrame(start + Duration::from_millis(16)));
        assert!(app.sound_scroll.0 > 30.0 && app.sound_scroll.0 < 33.0);
        assert!(app.scroll_pending.is_empty(), "frames must not need another probe");
        let _ = app.update(Msg::Wheel("body", 96.0));
        assert_eq!(app.scroll_anims["body"].target, 192.0);
        for frame in 2..60 {
            let _ = app.update(Msg::ScrollFrame(start + Duration::from_millis(frame * 16)));
        }
        assert!(app.scroll_anims.is_empty());
        assert_eq!(app.sound_scroll.0, 192.0);
        let _ = app.update(Msg::Wheel("body", 96.0));
        let _ = app.update(Msg::Page(0));
        let before = app.sound_scroll;
        let _ = app.update(Msg::ScrollProbe("body", Some((0.0, 500.0, 5000.0))));
        assert!(app.scroll_anims.is_empty());
        assert_eq!(app.sound_scroll, before, "ignore cancelled measurement");
    }
    #[test]
    fn discord_volume_uses_the_new_scale_and_migrates_legacy_eight_percent() {
        let (defaults, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        assert_eq!(defaults.controls.boost, 3.0);
        assert!(!defaults.controls.overload);
        assert!((defaults.controls.discord_volume - 0.08).abs() < 0.0001);
        assert_eq!(
            discord_volume_percent(defaults.controls.discord_volume),
            100.0
        );

        let (legacy, _) = App::from_settings(Settings::for_test("[effects]\ndiscord_volume=8"))
            .unwrap()
            .unwrap();
        assert!((legacy.controls.discord_volume - 0.08).abs() < 0.0001);

        let (new_scale, _) = App::from_settings(Settings::for_test(
            "[effects]\ndiscord_volume=200\ndiscord_volume_scale=2",
        ))
        .unwrap()
        .unwrap();
        assert!((new_scale.controls.discord_volume - 0.16).abs() < 0.0001);

        let (mut clamped, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        let _ = clamped.update(Msg::DiscordVolume(250.0));
        assert!((clamped.controls.discord_volume - 0.16).abs() < 0.0001);
    }
    #[test]
    fn soundpad_volume_migrates_and_handles_an_old_version_edit() {
        assert_eq!(load_sound_volume(&Settings::for_test("")), 1.0);
        assert_eq!(load_sound_volume(&Settings::for_test("[soundpad]\nvolume=20")), 1.0);
        assert_eq!(load_sound_volume(&Settings::for_test("[soundpad]\nvolume=40")), 2.0);
        assert_eq!(load_sound_volume(&Settings::for_test("[soundpad]\nvolume=100")), 2.0);
        assert_eq!(load_sound_volume(&Settings::for_test("[soundpad]\nvolume=20\nvolume_display=100")), 1.0);
        assert_eq!(load_sound_volume(&Settings::for_test("[soundpad]\nvolume=30\nvolume_display=100")), 1.5);
        assert!((SOUND_VOLUME_AT_100 - 0.2 * 0.2).abs() < 0.0001);
    }
    #[test]
    fn noise_presets_and_hotkey_roundtrip() {
        use keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test("[audio]\nintensity=105"))
            .unwrap()
            .unwrap();
        assert_eq!(app.controls.intensity, 1.05);
        assert_eq!(app.controls.alternate_intensity, 0.15);
        assert_eq!(app.keys[12], 0);
        app.window = Some(App::open(1.0, None).0);
        app.focus = 37;
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.controls.alternate_intensity, 0.16);
        assert_eq!(app.controls.intensity, 1.05);
        let _ = app.update(Msg::AlternateIntensity(250.0));
        assert_eq!(app.controls.alternate_intensity, 2.0);
        let _ = app.update(Msg::AlternateIntensity(-1.0));
        assert_eq!(app.controls.alternate_intensity, 0.0);
        let _ = app.update(Msg::AlternateIntensity(15.0));
        app.focus = 38;
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.binding, Some(12));
        app.candidate = app.keys[11];
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(12));
        assert_eq!(app.bind_conflict, Some(app.keys[11]));
        app.candidate = 120 | 256;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[12], 120 | 256);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(".tmp/noise-preset-test");
        std::fs::create_dir_all(&dir).unwrap();
        app.settings.path = dir.join("settings.ini");
        app.benchmark = false;
        app.save();
        let mut saved = false;
        for _ in 0..100 {
            if let Some(Reply::Saved(result)) = app.engine.reply() {
                result.unwrap();
                saved = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saved, "Settings write did not finish");
        let (restored, _) = App::from_settings(Settings::load(&dir).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(restored.controls.intensity, 1.05);
        assert_eq!(restored.controls.alternate_intensity, 0.15);
        assert_eq!(restored.keys[12], 120 | 256);
        std::fs::remove_file(dir.join("settings.ini")).unwrap();
    }
    #[test]
    fn visual_effect_toggles_load_save_and_take_keyboard() {
        use keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test("[ui]
page_pixelate=0")).unwrap().unwrap();
        assert!(!app.pixel_shift && app.slider_idle, "pixelation off from INI, wave on by default");
        assert!(app.clock().idle && app.clock().opened == app.opened_at);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join(".tmp/visual-toggles-test");
        std::fs::create_dir_all(&dir).unwrap();
        app.settings.path = dir.join("settings.ini");
        app.benchmark = false;
        app.window = Some(App::open(1.0, None).0);
        let _ = app.update(Msg::Page(2));
        app.focus = focus::settings::PIXEL_SHIFT;
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert!(app.pixel_shift);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::settings::SLIDER_IDLE);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert!(!app.slider_idle && !app.clock().idle && app.clock().opened.is_none());
        assert_eq!(app.settings.get("ui", "page_pixelate"), Some("1"));
        assert_eq!(app.settings.get("ui", "slider_idle"), Some("0"));
        let mut saved = 0;
        for _ in 0..200 {
            if let Some(Reply::Saved(result)) = app.engine.reply() {
                result.unwrap();
                saved += 1;
                if saved == 2 { break; }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(saved, 2, "Settings writes did not finish");
        let (restored, _) = App::from_settings(Settings::load(&dir).unwrap()).unwrap().unwrap();
        assert!(restored.pixel_shift && !restored.slider_idle);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn background_window_stops_meter_updates_and_restores_on_focus() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        let id = App::open(1.0, None).0;
        app.window = Some(id);
        assert!(!app.ui_active());
        let _ = app.update(Msg::WindowFocus(id, true));
        assert!(app.ui_active());
        let _ = app.update(Msg::WindowFocus(id, false));
        assert!(!app.ui_active());
        app.peak = 0.75;
        let _ = app.update(Msg::Tick);
        assert_eq!(app.peak, 0.75); // No meter animation behind another app.
        let _ = app.update(Msg::WindowFocus(id, true));
        let _ = app.update(Msg::Tick);
        assert!((app.peak - 0.6).abs() < 0.0001);
        let _ = app.update(Msg::Minimize);
        assert!(!app.ui_active());
        app.tray_ok = true;
        app.hint_shown = true;
        let _ = app.update(Msg::Minimized(id));
        assert_eq!(app.window, Some(id), "the title-bar minimize stays in the taskbar");
        let _ = app.update(Msg::Minimized(id));
        assert_eq!(app.window, Some(id), "repeat resize cannot hide the title-bar minimize");
        let _ = app.update(Msg::Restored(id));
        assert!(!app.own_minimize, "restoring through the taskbar clears the title-bar minimize latch");
        let _ = app.update(Msg::Minimized(id));
        assert_eq!(app.hidden_window, Some(id), "the next taskbar click hides even without a focus event");
        let _ = app.update(Msg::Show);
        let _ = app.update(Msg::WindowFocus(id, true));
        assert!(app.ui_active());
        let _ = app.update(Msg::MinimizedState(id, Some(true)));
        assert_eq!(app.hidden_window, Some(id), "a missed focus and resize event still hides to tray");
        let _ = app.update(Msg::Show);
        let _ = app.update(Msg::WindowFocus(id, true));
        let _ = app.update(Msg::WindowFocus(id, false));
        let _ = app.update(Msg::MinimizedState(id, Some(false)));
        assert_eq!(app.window, Some(id), "losing focus alone does not hide the window");
        let _ = app.update(Msg::MinimizedState(id, Some(true)));
        assert_eq!(app.hidden_window, Some(id), "a missed resize still hides to tray");
        let _ = app.update(Msg::Show);
        let _ = app.update(Msg::WindowFocus(id, true));
        let _ = app.update(Msg::Minimized(id));
        assert_eq!(app.hidden_window, Some(id), "a taskbar-click minimize hides to tray");
        let _ = app.update(Msg::Show);
        let _ = app.update(Msg::WindowFocus(id, true));
        assert!(app.ui_active());
        let _ = app.update(Msg::Hide);
        let _ = app.update(Msg::WindowFocus(id, true));
        assert!(!app.ui_active()); // Late focus event cannot wake the hidden UI.
        let _ = app.update(Msg::Show);
        assert!(!app.ui_active());
        let _ = app.update(Msg::WindowFocus(id, true));
        assert!(app.ui_active());
    }
    #[test]
    fn ready_update_is_reachable_from_any_page() {
        use keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window = Some(App::open(1.0, None).0);
        let tab = |app: &mut App| {
            let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        };
        // Without an update the banner is not in anyone's Tab order.
        app.focus = focus::NONE;
        for _ in 0..80 {
            tab(&mut app);
            assert_ne!(app.focus, focus::UPDATE_BANNER);
        }
        app.update_ready = true;
        app.update_status = "Версия 0.2.1 скачана и готова".into();
        for page in [false, true] {
            app.details = page;
            app.focus = focus::NONE;
            let mut seen = false;
            for _ in 0..80 {
                tab(&mut app);
                seen |= app.focus == focus::UPDATE_BANNER;
            }
            assert!(seen, "the update banner is missing from the Tab order (details={page})");
        }
        app.details = false;
        app.focus = focus::UPDATE_BANNER;
        // A withdrawn release (Current after the re-check) is not applied.
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.apply_pending && app.update_checking && !app.apply_after_quit);
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.update_checking, "a second press while checking is ignored");
        let _ = app.update(Msg::UpdateChecked(updater::Status::Current));
        assert!(!app.apply_pending && !app.apply_after_quit && !app.update_ready);
        // Enter re-checks first, then applies what that check downloaded: the newest release.
        app.update_ready = true;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(!app.apply_after_quit, "the stale download must not be applied before the re-check");
        let _ = app.update(Msg::UpdateChecked(updater::Status::Ready("0.2.4".into())));
        assert!(!app.quitting && app.apply_pending,"preparation must finish before stopping audio");
        let _ = app.update(Msg::UpdatePrepared(Ok(())));
        assert!(app.apply_after_quit, "Enter on the banner must apply the newest update");
    }
    #[test]
    fn offline_recheck_still_applies_the_downloaded_update() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.update_ready = true;
        let _ = app.update(Msg::ApplyUpdate);
        let _ = app.update(Msg::UpdateChecked(updater::Status::Unavailable("offline".into())));
        assert!(!app.quitting && app.apply_pending);
        let _ = app.update(Msg::UpdatePrepared(Ok(())));
        assert!(app.apply_after_quit);
    }
    #[test]
    fn failed_update_preparation_keeps_processing_intent() {
        let (mut app,_) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.snapshot.state=2;app.update_ready=true;
        let _=app.update(Msg::ApplyUpdate);
        let _=app.update(Msg::UpdateChecked(updater::Status::Ready("0.2.4".into())));
        let _=app.update(Msg::UpdatePrepared(Err("invalid bundle".into())));
        assert_eq!(app.snapshot.state,2);assert!(!app.quitting && !app.apply_pending && !app.apply_after_quit);
        assert!(app.update_status.contains("invalid bundle"));
    }
    #[test]
    fn boost_monitor_is_independent_and_defaults_off() {
        use keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test("[effects]\nmonitor_effects=1"))
            .unwrap()
            .unwrap();
        assert!(app.effects_monitor && !app.boost_monitor);
        assert_eq!(app.monitor_mode(), 2);
        app.window = Some(App::open(1.0, None).0);
        let _ = app.update(Msg::Page(6));
        app.focus = 31;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 36);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.monitor_mode(), 4);
        let _ = app.update(Msg::EffectsMonitor(false));
        assert!(app.boost_monitor);
        assert_eq!(app.monitor_mode(), 3);
        app.monitor_all = true;
        assert_eq!(app.monitor_mode(), 1);
        app.monitor_all = false;
        let _ = app.update(Msg::BoostMonitor(false));
        assert_eq!(app.monitor_mode(), 0);
        let _ = app.update(Msg::EffectsMonitor(true));
        assert!(!app.boost_monitor);
        let (saved, _) = App::from_settings(Settings::for_test(
            "[effects]\nmonitor_effects=0\nmonitor_boost=1",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(saved.monitor_mode(), 3);
    }
    #[test]
    fn route_device_selection_is_validated_and_keyboard_accessible() {
        use keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        let mic = Device {
            id: "test-mic".into(),
            name: "Test microphone".into(),
        };
        let headphones = Device {
            id: "test-out".into(),
            name: "Headphones".into(),
        };
        let virtual_out = Device {
            id: "TAG".into(),
            name: "Thin Audio Gateway".into(),
        };
        app.inputs = vec![mic.clone()];
        app.outputs = vec![headphones.clone(), virtual_out.clone()];
        app.window = Some(App::open(1.0, None).0);
        app.focus = 35;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert_eq!(app.input, Some(mic.clone()));
        assert_eq!(app.focus, 35);
        let _ = app.update(Msg::Input(headphones.clone()));
        assert_eq!(app.input, Some(mic));
        let _ = app.update(Msg::Page(3));
        app.focus = 50;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert_eq!(app.headphone_output, Some(headphones.clone()));
        let _ = app.update(Msg::HeadphoneOutput(virtual_out));
        assert_eq!(app.headphone_output, Some(headphones));
        app.headphone_busy = true;
        app.headphone_output = None;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.headphone_output.is_none());
        app.busy = true;
        app.input = None;
        let _ = app.update(Msg::Input(app.inputs[0].clone()));
        assert!(app.input.is_none());
    }
    #[test]
    fn logs_page_is_reachable_by_keyboard() {
        use iced::keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window = Some(App::open(1.0, None).0);
        let _ = app.update(Msg::Page(5));
        assert!(app.logs_page && !app.details && !app.soundpad_page && !app.headphone_page);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::logs::BACK);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::logs::COPY);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::logs::FOLDER);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::logs::SEND);
        let _ = app.update(Msg::Logs("report".into(), true));
        assert!(app.logs_copied && app.logs_text == "report");
        app.focus = focus::TAB_BASE + 2;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.details && !app.logs_page && !app.logs_copied);
        app.focus = focus::TAB_LOGS;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.logs_page);
        let _ = app.key(Key::Named(Named::Escape), Modifiers::empty(), false);
        assert!(!app.logs_page);
    }
    #[test]
    fn headphones_are_off_and_independent() {
        use iced::keyboard::{Key, Modifiers, key::Named};
        let (mut app, _) = App::from_settings(Settings::for_test(
            "[headphones]\nvolume=65\npitch=2\nintensity=175",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(app.headphone_state, 0);
        assert_eq!(app.headphone_intensity, 1.75);
        assert!(!app.headphone_busy);
        assert_eq!(app.engine.headphone_state().0, 0);
        let microphone = app.controls.intensity;
        let _ = app.update(Msg::Page(3));
        assert!(app.headphone_page && !app.details && !app.rvc_page);
        let _ = app.update(Msg::HeadphoneIntensity(30.0));
        assert_eq!(app.headphone_intensity, 0.3);
        let _ = app.update(Msg::HeadphoneIntensity(250.0));
        assert_eq!(app.headphone_intensity, 2.0);
        assert_eq!(app.controls.intensity, microphone);
        app.window = Some(App::open(1.0, None).0);
        app.focus = 55;
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.headphone_pitch, 3);
        // NVIDIA switches while running: the session reopens in the new mode.
        app.headphone_state = 2;
        let _ = app.update(Msg::HeadphoneNoise(false));
        assert!(!app.headphone_denoise && app.headphone_busy);
    }
    #[test]
    fn recordings_keep_six_play_and_save() {
        use keyboard::{Key, Modifiers, key::Named};
        assert_eq!(
            clip_label("Запись 2026-09-22 14-05-12.wav"),
            "14:05:12"
        );
        assert!(clip_name().ends_with(" (mix).wav"));
        assert_eq!(clip_label("Запись 2026-09-22 14-05-12 (mix).wav"), "14:05:12");
        assert_eq!(clip_label("Запись 2026-09-22 14-05-12 (mix) (2).wav"), "14:05:12");
        assert_eq!(clip_label("airhorn.mp3"), "airhorn");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(".tmp/clips-controller-test");
        let _ = std::fs::remove_dir_all(&dir);
        let (folder, library) = (dir.join("clips"), dir.join("library"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        // Eight recordings of the same second: only the six newest names survive a rescan.
        for i in 0..8 {
            let name = format!("Запись 2026-09-2{i} 14-05-1{i}.wav");
            soundpad::write_wav(&folder.join(name), &[0.25; 480]).unwrap();
        }
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.clips_folder = folder.clone();
        app.rescan_clips();
        assert_eq!(app.clips.len(), CLIPS_KEPT);
        assert!(app.clips[0].name.contains("2026-09-27"));
        assert!(app.clips[5].name.contains("2026-09-22"));
        assert!(!folder.join("Запись 2026-09-20 14-05-10.wav").exists());
        assert_eq!(soundpad::decode(&app.clips[0].path).unwrap().len(), 480);
        app.window = Some(App::open(1.0, None).0);
        let _ = app.update(Msg::ClipPlay(0));
        assert_eq!(app.clips[0].state, SoundState::Loading);
        assert_eq!(app.clip_pending_play, Some(0));
        let _ = app.update(Msg::ClipLoaded(0, app.clip_loads, Ok(0.01)));
        assert_eq!(app.clips[0].state, SoundState::Loaded(0.01));
        assert_eq!(app.clip_pending_play, None);
        let _ = app.update(Msg::ClipLoaded(1, app.clip_loads + 1, Ok(1.0)));
        assert_eq!(
            app.clips[1].state,
            SoundState::Unloaded,
            "a decode finishing for an older list must be ignored"
        );
        // Save through the row menu: the two targets are reachable only while it is open.
        app.sound_folder = Some(library.clone());
        let _ = app.update(Msg::Page(6));
        app.focus = focus::effects::CLIP_BASE + 1;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert_eq!(app.clip_menu, Some(0));
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, focus::effects::CLIP_TO_SOUNDPAD);
        let saved = app.clips[0].name.clone();
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert_eq!(app.clip_menu, None);
        assert!(library.join(&saved).exists());
        assert!(app.clip_note.starts_with("Сохранено"));
        assert_eq!(app.sounds.len(), 1, "the copy joins the soundpad library at once");
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_ne!(app.focus, focus::effects::CLIP_TO_FOLDER, "the closed menu leaves the Tab order");
        let _ = app.update(Msg::ClipSave(0, true));
        assert!(app.clip_note.contains(" (2).wav"), "a second save must not overwrite the first");
        assert!(matches!(app.clips[0].state, SoundState::Unloaded), "a library rescan unloads recordings");
        let _ = app.update(Msg::ClipSave(9, true));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn soundpad_keys_volumes_and_keyboard() {
        use keyboard::{Key, Modifiers, key::Named};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(".tmp/soundpad-controller-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        soundpad::test_wav(&dir.join("boom.wav"), 48_000, 1, 480);
        soundpad::test_wav(&dir.join("airhorn.mp3.wav"), 48_000, 1, 480);
        let (mut app, _) = App::from_settings(Settings::for_test(&format!(
            "[effects]
boost_key=119
[soundpad]
folder={}
volume=30
monitor=1
stop_key=120
sounds=119:80:boom.wav	121:30:airhorn.mp3.wav",
            dir.display()
        )))
        .unwrap()
        .unwrap();
        assert_eq!(app.sounds.len(), 2);
        assert_eq!(app.sounds[0].name, "airhorn.mp3.wav");
        assert_eq!((app.sounds[0].key, app.sounds[0].volume), (121, 30));
        assert_eq!(app.sounds[1].key, 0, "clip key colliding with an effect key must be dropped");
        assert_eq!(app.sounds[1].volume, 80);
        assert_eq!(app.sound_stop_key, 120);
        assert_eq!(app.sound_volume, 1.5);
        assert!(app.sound_monitor);
        assert_eq!(app.monitor_mode(), 5);
        app.effects_monitor = true;
        assert_eq!(app.monitor_mode(), 6);
        app.monitor_all = true;
        assert_eq!(app.monitor_mode(), 1);
        app.monitor_all = false;
        app.effects_monitor = false;
        app.window = Some(App::open(1.0, None).0);
        let _ = app.update(Msg::Page(4));
        assert!(app.soundpad_page && !app.details);
        let _ = app.update(Msg::SoundHover(0, true));
        let _ = app.update(Msg::SoundHover(1, false));
        assert_eq!(app.sound_hover, Some(0), "another row's exit keeps the active hover");
        let _ = app.update(Msg::SoundHover(0, false));
        assert_eq!(app.sound_hover, None);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 67);
        for expected in [68, 62, 61, 63, 20000, 69, 1000, 1001, 1002, 1003, 1004, 1005, 66, 64, 75, 65, 40] {
            let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
            assert_eq!(app.focus, expected);
        }
        app.focus = 68;
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.sound_sort, SoundSort::Hotkey);
        assert_eq!(app.visible_sounds(), [0, 1]);
        app.sounds[1].played = 5;
        let _ = app.update(Msg::SoundpadSort(SoundSort::Recent));
        assert_eq!(app.visible_sounds(), [1, 0]);
        let _ = app.update(Msg::SoundpadSort(SoundSort::Name));
        let _ = app.update(Msg::SoundpadFilter("BOOM".into()));
        app.focus = 69;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 1003, "filter hides the first clip from the Tab order");
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.sounds[1].volume, 85);
        let _ = app.update(Msg::SoundVolume(1, 500.0));
        assert_eq!(app.sounds[1].volume, 200);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.binding, Some(SOUND_BIND_BASE + 1));
        app.candidate = 119;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(SOUND_BIND_BASE + 1), "effect key must be rejected");
        app.candidate = 121;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(SOUND_BIND_BASE + 1), "other clip key must be rejected");
        app.candidate = 120;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(SOUND_BIND_BASE + 1), "stop key must be rejected");
        app.candidate = 122 | 256;
        let _ = app.update(Msg::AcceptBind);
        assert!(app.binding.is_none());
        assert_eq!(app.sounds[1].key, 122 | 256);
        let _ = app.update(Msg::Bind(1));
        app.candidate = 121;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[1], 0, "effect binding must not take a clip key");
        let _ = app.update(Msg::Bind(SOUND_STOP_BIND));
        let _ = app.update(Msg::ClearBind);
        assert_eq!(app.sound_stop_key, 0);
        let _ = app.update(Msg::SoundpadVolume(-5.0));
        assert_eq!(app.sound_volume, 0.0);
        let _ = app.update(Msg::SoundpadHear(false));
        assert_eq!(app.monitor_mode(), 0);
        // Loudness levelling: on by default; toggling reloads the library with fresh generations
        // and the decode-task mirrors follow them.
        assert!(app.sound_normalize);
        assert_eq!(app.sound_window(), Some((-18.0, -12.0)));
        let (sounds_before, clips_before) = (app.sound_generation, app.clip_loads);
        app.focus = focus::soundpad::NORMALIZE;
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert!(!app.sound_normalize);
        assert_eq!(app.sound_window(), None);
        assert_eq!(app.focus, focus::soundpad::NORMALIZE);
        assert!(app.sound_generation > sounds_before && app.clip_loads > clips_before);
        assert_eq!(app.sound_live.load(Ordering::Acquire), app.sound_generation);
        assert_eq!(app.clip_live.load(Ordering::Acquire), app.clip_loads);
        assert_eq!(app.sounds.len(), 2, "the reload keeps the library");
        assert_eq!(app.sounds[1].key, 122 | 256, "the reload keeps keys");
        app.benchmark = false;
        app.settings.path = dir.join("settings.ini");
        app.save();
        assert_eq!(app.settings.get("soundpad", "normalize"), Some("0"));
        assert_eq!(
            app.settings.get("soundpad", "sounds"),
            Some("121:30:0:airhorn.mp3.wav\t378:200:5:boom.wav")
        );
        assert_eq!(app.settings.get("soundpad", "sort"), Some("0"));
        assert_eq!(app.settings.get("soundpad", "stop_key"), Some("0"));
        let _ = app.update(Msg::SoundpadVolume(100.0));
        app.save();
        assert_eq!(app.settings.get("soundpad", "volume_display"), Some("100"));
        assert_eq!(app.settings.get("soundpad", "volume"), Some("20"));
        let _ = app.key(Key::Named(Named::Escape), Modifiers::empty(), false);
        assert!(!app.soundpad_page);
        let _ = app.update(Msg::SoundpadPicked(true, Ok(vec![])));
        assert_eq!(app.sounds.len(), 2, "cancelled picker keeps the library");
        // Sidebar: automatic groups need two clips with the same prefix; custom sections
        // take drops, rename, unassign and delete.
        let _ = app.update(Msg::SoundpadFilter(String::new()));
        assert_eq!(app.section_items().len(), 1);
        let _ = app.update(Msg::SectionAdd);
        let _ = app.update(Msg::SectionAdd);
        assert_eq!(app.sections.len(), 2);
        assert_eq!(app.sections[1].name, "Новый раздел 2");
        assert_eq!(app.section, Selection::Custom(1));
        assert!(app.visible_sounds().is_empty());
        let _ = app.update(Msg::SectionName("Мемы:|".into()));
        let _ = app.update(Msg::SectionRename);
        assert_eq!(app.sections[1].name, "Мемы");
        let _ = app.update(Msg::SectionName("Новый раздел".into()));
        let _ = app.update(Msg::SectionRename);
        assert_eq!(app.sections[1].name, "Мемы", "duplicate name rejected");
        let _ = app.update(Msg::DragStart(0));
        let _ = app.update(Msg::DragOver(Some(1)));
        let _ = app.update(Msg::DragEnd);
        assert_eq!(app.sections[1].files, ["airhorn.mp3.wav"]);
        assert_eq!(app.visible_sounds(), [0]);
        let _ = app.update(Msg::DragStart(1));
        let _ = app.update(Msg::DragOver(None));
        let _ = app.update(Msg::DragEnd);
        assert_eq!(app.sections[1].files.len(), 1, "drop outside a section does nothing");
        assert!(app.dragging.is_none());
        let _ = app.update(Msg::DragEnd);
        let _ = app.update(Msg::SectionSelect(2));
        assert_eq!(app.section, Selection::Custom(1));
        let _ = app.update(Msg::SoundUnassign(0));
        assert!(app.sections[1].files.is_empty());
        let _ = app.update(Msg::SectionDelete);
        assert_eq!(app.sections.len(), 1);
        assert_eq!(app.section, Selection::All);
        app.sections[0].files.push("boom.wav".into());
        app.save();
        assert_eq!(
            app.settings.get("soundpad", "sections"),
            Some("Новый раздел:boom.wav")
        );
        let _ = app.update(Msg::SoundLoaded(0, app.sound_generation + 1, Ok(1.0)));
        assert_eq!(app.sounds[0].state, soundpad::State::Loading, "stale decode ignored");
        for _ in 0..100 {
            if let Some(Reply::Saved(result)) = app.engine.reply() {
                result.unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn setup_card_shows_only_for_a_problem() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        assert!(!app.setup_visible(), "everything installed: no card");
        app.driver_ready = false;
        assert!(app.setup_visible(), "a missing virtual microphone shows it");
        app.driver_ready = true;
        app.setup_error = "io: Connection reset by peer (os error 10054)".into();
        assert!(app.setup_visible(), "a failed download shows it");
        app.setup_error.clear();
        assert!(!app.setup_visible(), "no microphone before the device list came is no problem yet");
        app.devices_known = true;
        assert!(app.setup_visible(), "no microphone to start with shows it");
        app.input = Some(Device { id: "mic".into(), name: "Mic".into() });
        app.output = Some(Device { id: "dac".into(), name: "Speakers".into() });
        assert!(app.setup_visible(), "an output auto-start refuses shows it");
        app.output = Some(Device { id: "TAG".into(), name: "Mic Noize Microphone".into() });
        assert!(!app.setup_visible(), "all set: the card leaves");
        app.output = None;
        app.snapshot.state = 2;
        assert!(!app.setup_visible(), "running voice: nothing to fix");
    }
    #[test]
    fn restart_event_does_not_override_session_exit() {
        assert!(restart_requested(RESTART_EVENT));
        assert!(!restart_requested(EXIT_EVENT));
        assert!(!restart_requested(EXIT_EVENT | RESTART_EVENT));
    }
    #[test]
    fn failed_session_restarts_with_backoff_and_gives_up() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Recovery::default();
        assert_eq!(r.observe(1, at(0)), None);
        assert!(!r.take_due(at(100)) && !r.exhausted());
        // A failure (mid-run or at start) is restarted after 2 s, once.
        assert_eq!(r.observe(3, at(10)), None);
        assert_eq!(r.observe(5, at(20)), Some(Duration::from_secs(2)));
        assert_eq!(r.observe(5, at(21)), None, "a waiting restart is not rescheduled");
        assert!(!r.take_due(at(21)));
        assert!(r.take_due(at(22)));
        assert!(!r.take_due(at(23)), "one restart per failure");
        // Failed restarts back off, then stop.
        for (i, delay) in RECOVERY_DELAYS.iter().enumerate().skip(1) {
            let now = at(100 * i as u64);
            assert_eq!(r.observe(5, now), Some(Duration::from_secs(*delay)));
            assert!(r.take_due(now + Duration::from_secs(*delay)));
        }
        assert_eq!(r.observe(5, at(1000)), None);
        assert!(r.exhausted());
        // A healthy minute forgives the attempts; a manual restart clears a pending one.
        let mut r = Recovery { attempts: 3, ..Recovery::default() };
        assert_eq!(r.observe(5, at(0)), Some(Duration::from_secs(30)));
        assert_eq!(r.observe(2, at(1)), None);
        assert!(!r.take_due(at(1000)), "running again drops the pending restart");
        assert_eq!(r.observe(3, at(61)), None);
        assert_eq!(r.observe(5, at(62)), Some(Duration::from_secs(2)));
        // Flapping: an old healthy stretch does not forgive a session that dies in seconds.
        let mut r = Recovery::default();
        assert_eq!(r.observe(3, at(0)), None);
        assert_eq!(r.observe(5, at(3600)), Some(Duration::from_secs(2)));
        assert!(r.take_due(at(3602)));
        assert_eq!(r.observe(3, at(3603)), None);
        assert_eq!(r.observe(5, at(3604)), Some(Duration::from_secs(5)));
    }
    #[test]
    fn device_wait_outlives_initial_retries_but_access_and_settings_errors_do_not() {
        assert!(transient_device_failure("Capture buffer: HRESULT 0x88890004"));
        assert!(transient_device_failure("Waiting for the TAG microphone endpoint in Windows"));
        assert!(!transient_device_failure("TAG open driver: HRESULT 0x80070005"));
        assert!(!transient_device_failure("Invalid audio settings"));
        assert!(!transient_device_failure("TAG host protocol mismatch"));
        let start = Instant::now();
        let mut recovery = Recovery { wait_for_device: true, ..Recovery::default() };
        for attempt in 0..20 {
            let now = start + Duration::from_secs(attempt * 100);
            let expected = RECOVERY_DELAYS.get(attempt as usize).copied().unwrap_or(60);
            assert_eq!(recovery.observe(5, now), Some(Duration::from_secs(expected)));
            assert!(recovery.take_due(now + Duration::from_secs(expected)));
            assert!(!recovery.exhausted());
        }
        recovery.wait_for_device = false;
        assert_eq!(recovery.observe(5, start + Duration::from_secs(3000)), None);
        assert!(recovery.exhausted());
    }

    #[test]
    fn keyboard_monitor_and_independent_bindings() {
        assert_eq!(std::mem::size_of::<Snapshot>(), 64);
        let (mut app, _) = App::from_settings(Settings::for_test(
            "[effects]\nboost_key=256\npitch_key=119\nslow_key=119\nfast_key=2047",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(&app.keys[..4], &[0, 119, 0, 0]);
        assert_eq!(app.keys[11], 119 | 256);
        assert!(
            app.engine.snapshot(false).0.epoch > 0,
            "Native bindings rejected the sanitized set"
        );
        assert!(app.window.is_none());
        assert!(app.benchmark);
        // A second controller must not contend for the live application's mutex.
        let (other, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        drop(other);
        // Keyboard handlers require a window ID; do not execute the window-open task.
        app.window = Some(App::open(1.0, None).0);
        app.keys = [0; 13];
        use keyboard::{Key, Modifiers, key::Named};
        // Шумодав: devices, the headphone gear, the folded route, then the two strengths.
        for expected in [35, 50, 81, 82, 34, 38, 37, 40] {
            let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
            assert_eq!(app.focus, expected);
        }
        // The gear opens the headphone panel in place and its controls join the order.
        app.focus = 81;
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(app.headphone_page);
        for expected in [51, 52, 53, 54, 55, 56, 82] {
            let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
            assert_eq!(app.focus, expected);
        }
        let _ = app.key(Key::Named(Named::Escape), Modifiers::empty(), false);
        assert!(!app.headphone_page && !app.effects_page);
        let _ = app.update(Msg::Page(6));
        for expected in [21, 2] {
            let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
            assert_eq!(app.focus, expected);
        }
        app.focus = 20;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 9);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.engine.monitor_state().0, 0); // Disabled while stopped.
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 23);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.binding, Some(10));
        app.candidate = 200;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[10], 200);
        app.focus = 23;
        for expected in [31, 36, 22, 32] {
            let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
            assert_eq!(app.focus, expected, "Tab follows the visual order of the bottom block");
        }
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.binding, Some(11));
        app.candidate = 119 | 256;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[11], 119 | 256);
        app.focus = 22;
        app.controls.discord_volume = discord_volume_gain(100.0);
        let _ = app.key(Key::Named(Named::ArrowLeft), Modifiers::empty(), false);
        assert!((discord_volume_percent(app.controls.discord_volume) - 99.0).abs() < 0.0001);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 32);
        let _ = app.update(Msg::Page(1));
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 24);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 25);
        app.rvc_models = vec![
            rvc::Model {
                slot: 0,
                name: "First".into(),
                has_index: false,
            },
            rvc::Model {
                slot: 7,
                name: "Second".into(),
                has_index: true,
            },
        ];
        app.controls.rvc_options.slot = 0;
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.controls.rvc_options.slot, 7);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 39);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 44);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 45);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 46);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 26);
        app.rvc_importing = true;
        let _ = app.update(Msg::RvcImported(Ok(None)));
        assert!(!app.rvc_importing);
        let models = app.rvc_models.clone();
        app.controls.rvc = false;
        let _ = app.update(Msg::RvcImported(Ok(Some((models.clone(), 0)))));
        assert_eq!(app.controls.rvc_options.slot, 0);
        app.controls.rvc = true;
        let _ = app.update(Msg::RvcImported(Ok(Some((models, 7)))));
        assert_eq!(app.controls.rvc_options.slot, 0); // Preserve the active voice.
        app.controls.rvc = false;
        let _ = app.update(Msg::RvcImported(Err("test error".into())));
        assert!(app.message.contains("test error"));
        let _ = app.update(Msg::RvcPitch(100.0));
        assert_eq!(app.controls.rvc_options.pitch, 24);
        let _ = app.update(Msg::RvcIndex(-10.0));
        assert_eq!(app.controls.rvc_options.index, 0);
        let _ = app.update(Msg::RvcGain(999.0));
        assert_eq!(app.controls.rvc_options.gain, 300);
        let _ = app.update(Msg::RvcChunk(150));
        let _ = app.update(Msg::RvcChunk(123));
        assert_eq!(app.controls.rvc_options.chunk, 150);
        let options = app.controls.rvc_options;
        options.save(&mut app.settings);
        assert_eq!(rvc::Options::load(&app.settings), options);
        let _ = app.update(Msg::RvcAdvanced);
        app.focus = 30;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 40);
        let _ = app.key(Key::Named(Named::Enter), Modifiers::empty(), false);
        assert!(!app.rvc_page && !app.details);
        let _ = app.update(Msg::Page(1));
        app.controls.rvc_options.slot = 0;
        app.focus = 26;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 33);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 28); // No index control for this model.
        let _ = app.key(Key::Named(Named::Escape), Modifiers::empty(), false);
        assert!(!app.rvc_page && !app.details);
        let _ = app.update(Msg::Bind(0));
        app.candidate = 119;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[0], 119);
        assert!(app.binding.is_none());
        let _ = app.update(Msg::Bind(1));
        app.candidate = 119;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(1));
        assert_eq!(app.keys[1], 0);
        app.candidate = 120;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys, [119, 120, 0, 0, 0, 0, 0, 0, 0, 0, 200, 375, 0]);
        let _ = app.update(Msg::Bind(0));
        let _ = app.update(Msg::ClearBind);
        assert_eq!(app.keys, [0, 120, 0, 0, 0, 0, 0, 0, 0, 0, 200, 375, 0]);
        let _ = app.update(Msg::Bind(2));
        app.candidate = 120;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(2));
        app.candidate = 121;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[2], 121);
        let _ = app.update(Msg::Bind(3));
        app.candidate = 121;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(3));
        app.candidate = 122;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[3], 122);
        let _ = app.update(Msg::Page(6));
        app.focus = 13;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 19);
        // The reverse row's practice word sits before its hotkeys, as on screen.
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 83);
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 15);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert_eq!(app.binding, Some(4));
        app.candidate = 122;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.binding, Some(4));
        app.candidate = 123;
        let _ = app.update(Msg::AcceptBind);
        assert_eq!(app.keys[4], 123);
        for i in 5..10 {
            app.focus = 11 + i;
            let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
            assert_eq!(app.binding, Some(i));
            app.candidate = 120;
            let _ = app.update(Msg::AcceptBind);
            assert_eq!(app.binding, Some(i));
            app.candidate = 124 + i as u32;
            let _ = app.update(Msg::AcceptBind);
            assert_eq!(app.keys[i], 124 + i as u32);
        }
        app.details = false;
        let _ = app.update(Msg::Intensity(200.0));
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert_eq!(app.controls.intensity, 2.0);
        let _ = app.key(Key::Named(Named::ArrowLeft), Modifiers::empty(), false);
        assert!((app.controls.intensity - 1.99).abs() < 0.0001);
        // The slow slider is mirrored on screen (slower = right), so Right slows down.
        let _ = app.update(Msg::Slow(70.0));
        let _ = app.key(Key::Named(Named::ArrowRight), Modifiers::empty(), false);
        assert!((app.controls.slow - 0.69).abs() < 0.0001, "one step, as the slider");
        let _ = app.key(Key::Named(Named::ArrowLeft), Modifiers::empty(), false);
        assert!((app.controls.slow - 0.70).abs() < 0.0001);
        app.details = false;
        app.controls.overload = false;
        app.focus = 37;
        let _ = app.key(Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(app.focus, 21);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert!(app.controls.overload);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), true);
        assert!(app.controls.overload);
        let _ = app.key(Key::Named(Named::Space), Modifiers::empty(), false);
        assert!(!app.controls.overload);
    }
}
