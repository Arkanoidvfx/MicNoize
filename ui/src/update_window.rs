//! The app's own update window. The recovery watcher shows it while Velopack applies the package
//! silently. The old UI shrinks into it and the new UI grows out of it; both hand-overs sit on the
//! exact same screen rectangle, signalled through small files in `.update`.
use crate::tacho::{self, BarStage};
use crate::view;
use iced::window::{self, Id};
use iced::{Element, Point, Task, Theme};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// The watcher's window is on screen: the old UI may exit.
pub fn ready_file(runtime: &Path) -> PathBuf {
    runtime.join(".update").join("window-ready")
}
/// The new UI shows its first frame: the watcher may close.
fn shown_file(runtime: &Path) -> PathBuf {
    runtime.join(".update").join("ui-shown")
}
/// Where the update window stands, for the new UI to grow out of it.
fn handoff_file(runtime: &Path) -> PathBuf {
    runtime.join(".update").join("handoff")
}

/// The new UI's hand-over, if an update window is waiting for it: the window's centre in
/// logical px. A stale file (older than two minutes) is ignored. Either way it is consumed.
pub fn take_handoff(runtime: &Path) -> Option<Point> {
    let path = handoff_file(runtime);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(120));
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    parse_point(text.as_deref()?).filter(|_| fresh)
}
/// Waits up to `timeout` for process `pid` to exit. A new UI started while the old one still
/// runs meets the single-instance guard and quits silently.
pub fn wait_exit(pid: u32, timeout: Duration) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const SYNCHRONIZE: u32 = 0x0010_0000;
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle != 0 {
            WaitForSingleObject(handle, timeout.as_millis() as u32);
            CloseHandle(handle);
        }
    }
}
/// Tells the watcher the new UI is on screen (or will not show a window at all).
pub fn signal_ui_shown(runtime: &Path) {
    let _ = std::fs::write(shown_file(runtime), b"shown");
}
/// "x,y" in logical px.
pub fn parse_point(text: &str) -> Option<Point> {
    let (x, y) = text.trim().split_once(',')?;
    let (x, y) = (x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?);
    (x.is_finite() && y.is_finite()).then_some(Point::new(x, y))
}

/// Turns Windows colour keying on or off for a window: pixels of exactly [`tacho::KEY`] become
/// transparent. tiny-skia windows cannot be alpha-transparent, but a keyed layered window can.
/// `alpha` 0 hides the whole window: the intro opens so until it has drawn its first frame.
/// While keyed, Windows' open and close animations are off: they draw the window without the
/// key, so its key-coloured surroundings (near black) flashed around the update card.
pub fn color_key(hwnd: u64, on: bool, alpha: u8) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowLongPtrW(window: isize, index: i32) -> isize;
        fn SetWindowLongPtrW(window: isize, index: i32, value: isize) -> isize;
        fn SetLayeredWindowAttributes(window: isize, key: u32, alpha: u8, flags: u32) -> i32;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(window: isize, attribute: u32, value: *const i32, size: u32) -> i32;
    }
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_LAYERED: isize = 0x0008_0000;
    const LWA_COLORKEY: u32 = 1;
    const LWA_ALPHA: u32 = 2;
    const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;
    let key = tacho::KEY;
    let colorref = (key.r * 255.0).round() as u32 | ((key.g * 255.0).round() as u32) << 8 | ((key.b * 255.0).round() as u32) << 16;
    let window = hwnd as isize;
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        let transitions_off = i32::from(on);
        DwmSetWindowAttribute(window, DWMWA_TRANSITIONS_FORCEDISABLED, &transitions_off, 4);
        if on {
            SetWindowLongPtrW(window, GWL_EXSTYLE, style | WS_EX_LAYERED);
            SetLayeredWindowAttributes(window, colorref, alpha, LWA_COLORKEY | LWA_ALPHA);
        } else {
            SetWindowLongPtrW(window, GWL_EXSTYLE, style & !WS_EX_LAYERED);
        }
    }
}

struct Watcher {
    runtime: PathBuf,
    center: Option<Point>,
    window: Option<Id>,
    stage: BarStage,
    versions: (String, String),
    work: Receiver<Result<bool, String>>,
    finished: Option<Instant>,
    error: std::rc::Rc<std::cell::RefCell<Option<String>>>,
}
#[derive(Clone, Debug)]
enum WatchMsg {
    Opened(Id),
    Handle(u64),
    Shown,
    Poll,
}
fn after(ms: u64, message: WatchMsg) -> Task<WatchMsg> {
    Task::perform(async move { std::thread::sleep(Duration::from_millis(ms)) }, move |_| message.clone())
}
fn quit() -> Task<WatchMsg> {
    // As in the app: wake Winit's message wait so a finished daemon really exits.
    #[link(name = "user32")]
    unsafe extern "system" { fn PostQuitMessage(code: i32); }
    unsafe { PostQuitMessage(0) };
    iced::exit()
}
impl Watcher {
    fn update(&mut self, message: WatchMsg) -> Task<WatchMsg> {
        match message {
            WatchMsg::Opened(id) => {
                self.window = Some(id);
                window::raw_id::<WatchMsg>(id).map(WatchMsg::Handle)
            }
            WatchMsg::Handle(hwnd) => {
                color_key(hwnd, true, 255);
                let show = self.window.map_or(Task::none(), |id| window::set_mode(id, window::Mode::Windowed));
                Task::batch([show, after(80, WatchMsg::Shown)])
            }
            WatchMsg::Shown => {
                let _ = std::fs::write(ready_file(&self.runtime), b"ready");
                self.stage = BarStage::Running(Instant::now());
                after(100, WatchMsg::Poll)
            }
            WatchMsg::Poll => {
                match self.work.try_recv() {
                    Ok(Ok(_)) => {
                        self.stage = BarStage::Done;
                        self.finished = Some(Instant::now());
                    }
                    Ok(Err(error)) => {
                        *self.error.borrow_mut() = Some(error);
                        return quit();
                    }
                    Err(TryRecvError::Disconnected) if self.finished.is_none() => {
                        *self.error.borrow_mut() = Some("Процесс восстановления обновления прервался".into());
                        return quit();
                    }
                    Err(_) => {}
                }
                // «Готово» for a moment, then «Запускаем…» with a live bar while the new version
                // starts (engine and GPU take a few seconds): a still green bar read as a hang.
                if self.stage == BarStage::Done && self.finished.is_some_and(|done| done.elapsed() > Duration::from_millis(400)) {
                    self.stage = BarStage::Launching;
                }
                // Wait for the new UI to cover this window, or give up after a while.
                if let Some(done) = self.finished {
                    let shown = shown_file(&self.runtime);
                    if shown.exists() || done.elapsed() > Duration::from_secs(12) {
                        let _ = std::fs::remove_file(shown);
                        let _ = std::fs::remove_file(handoff_file(&self.runtime));
                        return quit();
                    }
                }
                after(100, WatchMsg::Poll)
            }
        }
    }
    fn view(&self, _: Id) -> Element<'_, WatchMsg> {
        view::update_card_on_key(self.stage, &self.versions.0, &self.versions.1)
    }
}

/// Starts the rehearsal watcher (`--ui-update-rehearsal`) and waits, like a real update, until
/// its update window stands on screen, so the caller can exit without a visible gap.
pub fn rehearse(runtime: &Path, center: Option<Point>) -> Result<(), String> {
    let ready = ready_file(runtime);
    std::fs::create_dir_all(runtime.join(".update")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&ready);
    let at = center.map_or("centered".to_owned(), |c| format!("{:.1},{:.1}", c.x, c.y));
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe).arg("--ui-update-rehearsal").arg(at).arg(std::process::id().to_string()).spawn().map_err(|e| e.to_string())?;
    for _ in 0..60 {
        if ready.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("окно обновления не появилось за 3 секунды".into())
}

/// Runs the watcher's work on a thread and shows the update window meanwhile, placed so its
/// centre is `center` (the old window's), or centred on screen. Returns the work's error.
pub fn watch(runtime: PathBuf, center: Option<Point>, versions: Option<(String, String)>, work: impl FnOnce() -> Result<bool, String> + Send + 'static) -> Result<(), String> {
    let _ = std::fs::remove_file(shown_file(&runtime));
    match center {
        Some(c) => {
            let _ = std::fs::write(handoff_file(&runtime), format!("{},{}", c.x, c.y));
        }
        None => {
            let _ = std::fs::remove_file(handoff_file(&runtime));
        }
    }
    let versions = versions.or_else(|| crate::maintenance::update_versions(&runtime)).unwrap_or_default();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let error = std::rc::Rc::new(std::cell::RefCell::new(None));
    let boot = std::cell::RefCell::new(Some(Watcher { runtime, center, window: None, stage: BarStage::Waiting, versions, work: rx, finished: None, error: error.clone() }));
    iced::daemon(
        move || {
            let watcher = boot.borrow_mut().take().expect("boot once");
            let position = watcher.center.map_or(window::Position::Centered, |c| {
                window::Position::Specific(Point::new(c.x - view::UPDATE_CARD.width / 2.0, c.y - view::UPDATE_CARD.height / 2.0))
            });
            let (_, open) = window::open(window::Settings {
                size: view::UPDATE_CARD,
                position,
                visible: false,
                resizable: false,
                decorations: false,
                icon: Some(crate::window_icon()),
                ..Default::default()
            });
            (watcher, open.map(WatchMsg::Opened))
        },
        Watcher::update,
        Watcher::view,
    )
    .title("Mic Noize")
    .theme(|_: &Watcher, _: Id| Theme::Dark)
    .default_font(iced::Font::with_name("Segoe UI"))
    .run()
    .map_err(|e| e.to_string())?;
    match error.borrow_mut().take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
