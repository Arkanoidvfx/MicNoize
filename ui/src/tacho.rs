//! "Тахометр": the segmented slider of the 0.2.8 design. Skewed shapes are anti-aliased
//! tiny-skia paths (its `geometry` feature), so they look like the mockup at any scale. Every
//! animation is time-based and self-scheduled through redraw requests, so a still, unfocused
//! or hidden window draws nothing extra.
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::text::{self, Paragraph as _, Renderer as _, Text};
use iced::Renderer;
use iced_tiny_skia::Geometry;
use iced_tiny_skia::geometry::{Cache, Frame};
use iced_tiny_skia::graphics::cache::{Cached as _, Group};
use iced_tiny_skia::graphics::geometry::{Path, Renderer as _, Stroke, frame::Backend as _, gradient};
use std::cell::RefCell;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell};
use iced::keyboard;
use iced::mouse;
use iced::window::{self, RedrawRequest};
use iced::{Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Size, Theme};
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

const SKEW: f32 = 0.364; // tan 20°
const WAVE_CYCLE: f32 = 8000.0;
const WAVE_STEP: f32 = 64.0;
const WAVE_LEN: f32 = 568.0;
const STAGGER: f32 = 9.0;
const IGNITE_STEP: f32 = 14.0;
const IDLE_AFTER: Duration = Duration::from_millis(1500);
/// Animation frame cap. `RedrawRequest::NextFrame` is unthrottled on the tiny-skia backend:
/// it redraws as fast as the CPU allows, which starved the UI and even the audio engine.
const FRAME: Duration = Duration::from_millis(16);
fn frame_after(now: Instant) -> RedrawRequest {
    RedrawRequest::At(now + FRAME)
}
const OFF: Color = Color::from_rgb8(0x1E, 0x1F, 0x22);
const OFF_RED: Color = Color::from_rgb8(0x2C, 0x1A, 0x1A);
const OFF_EDGE: Color = Color::from_rgb8(0x2A, 0x2B, 0x30);
const GLOW: Color = Color::from_rgb8(0xFF, 0xBE, 0x8C);
const LOW: [f32; 3] = [196.0, 108.0, 44.0];
const HIGH: [f32; 3] = [255.0, 176.0, 112.0];
const HEAD: Color = Color::from_rgb8(0xFF, 0xE3, 0xC7);
pub const HOT: Color = Color::from_rgb8(0xFF, 0x5A, 0x4E);
const TAG: Color = Color::from_rgb8(0xFF, 0x9F, 0x56);
const INK: Color = Color::from_rgb8(242, 237, 227);
const DARK: Color = Color::from_rgb8(0x1A, 0x12, 0x06);

/// The numbers font of the design: Windows' own condensed DIN-like face.
pub fn numbers() -> Font {
    Font { weight: iced::font::Weight::Bold, ..Font::with_name("Bahnschrift") }
}

/// Shared clocks: `epoch` keeps the idle waves of all sliders in step; `opened` starts the
/// warm-up sweep once per shown window.
#[derive(Clone, Copy)]
pub struct Clock {
    pub epoch: Instant,
    pub opened: Option<Instant>,
    /// False while the window is unfocused: nothing keeps animating then.
    pub animate: bool,
    /// The idle wave («Визуальные эффекты»); the warm-up follows `opened`.
    pub idle: bool,
}

pub struct Tacho<'a, Message> {
    range: RangeInclusive<f32>,
    value: f32,
    step: f32,
    default: f32,
    on_change: Box<dyn Fn(f32) -> Message + 'a>,
    format: Box<dyn Fn(f32) -> String + 'a>,
    segments: usize,
    compact: bool,
    /// Fill from this value instead of the left edge (signed pitch).
    origin: Option<f32>,
    /// Values strictly above this are the red zone.
    red_above: Option<f32>,
    phase: f32,
    clock: Clock,
    enabled: bool,
    /// Overload is on (boost's hard clipping): the lit segments run hot and judder.
    overdrive: bool,
    /// The mouse wheel moves it a step per notch; off inside scrolling lists.
    wheel: bool,
    width: Length,
}
/// Drag pixels per step where the track has fewer than this many pixels per step: there the
/// value follows the drag's distance, not the cursor, so every step can be hit.
const FINE_PX: f32 = 2.0;
/// Touchpad pixels per wheel step.
const WHEEL_PX: f32 = 24.0;

pub fn tacho<'a, Message>(
    range: RangeInclusive<f32>,
    value: f32,
    on_change: impl Fn(f32) -> Message + 'a,
    clock: Clock,
) -> Tacho<'a, Message> {
    let default = *range.start();
    Tacho {
        range,
        value,
        step: 1.0,
        default,
        on_change: Box::new(on_change),
        format: Box::new(|v| format!("{v:.0}%")),
        segments: 20,
        compact: false,
        origin: None,
        red_above: None,
        phase: 0.0,
        clock,
        enabled: true,
        overdrive: false,
        wheel: true,
        width: Length::Fill,
    }
}

impl<'a, Message> Tacho<'a, Message> {
    pub fn step(mut self, step: f32) -> Self { self.step = step; self }
    pub fn default(mut self, default: f32) -> Self { self.default = default; self }
    pub fn format(mut self, format: impl Fn(f32) -> String + 'a) -> Self { self.format = Box::new(format); self }
    pub fn segments(mut self, n: usize) -> Self { self.segments = n.max(2); self }
    pub fn compact(mut self) -> Self { self.compact = true; self }
    pub fn origin(mut self, origin: f32) -> Self { self.origin = Some(origin); self }
    pub fn red_above(mut self, value: f32) -> Self { self.red_above = Some(value); self }
    /// Milliseconds the idle wave lags behind the shared clock, to chain sliders.
    pub fn phase(mut self, ms: f32) -> Self { self.phase = ms; self }
    pub fn enabled(mut self, enabled: bool) -> Self { self.enabled = enabled; self }
    pub fn overdrive(mut self, on: bool) -> Self { self.overdrive = on; self }
    pub fn wheel(mut self, on: bool) -> Self { self.wheel = on; self }
    pub fn width(mut self, width: impl Into<Length>) -> Self { self.width = width.into(); self }

    fn frac(&self, v: f32) -> f32 {
        let (min, max) = (*self.range.start(), *self.range.end());
        if max <= min { 0.0 } else { ((v - min) / (max - min)).clamp(0.0, 1.0) }
    }
    /// Lit segments `[lo, hi)` and the head index.
    fn lit(&self, v: f32) -> (i32, i32, i32) {
        let n = self.segments as f32;
        match self.origin {
            Some(o) => {
                let mid = (self.frac(o) * n).round() as i32;
                let pos = (self.frac(v) * n).round() as i32;
                if pos < mid { (pos, mid, pos) } else { (mid, pos, pos - 1) }
            }
            None => {
                let pos = (self.frac(v) * n - 1e-4).ceil().max(0.0) as i32;
                (0, pos, pos - 1)
            }
        }
    }
    fn red_from(&self) -> i32 {
        self.red_above
            .map(|r| (self.frac(r) * self.segments as f32).floor() as i32)
            .unwrap_or(i32::MAX)
    }
    fn in_red(&self, v: f32) -> bool {
        self.red_above.is_some_and(|r| v > r)
    }
    fn track(&self, bounds: Rectangle) -> Rectangle {
        let right = if self.compact { 64.0 } else { 6.0 };
        let (h, bottom) = if self.compact { (16.0, 7.0) } else { (24.0, 6.0) };
        Rectangle {
            x: bounds.x + 6.0,
            y: bounds.y + bounds.height - bottom - h,
            width: (bounds.width - 6.0 - right).max(1.0),
            height: h,
        }
    }
    fn seg_width(&self) -> f32 {
        if self.compact { 7.0 } else { 14.0 }
    }
    fn seg_x(&self, track: Rectangle, i: usize) -> f32 {
        let w = self.seg_width();
        track.x + (track.width - w) * i as f32 / (self.segments - 1) as f32
    }
    fn locate(&self, track: Rectangle, x: f32) -> f32 {
        let (min, max) = (*self.range.start(), *self.range.end());
        let t = ((x - track.x) / track.width).clamp(0.0, 1.0);
        self.snap(min + t * (max - min))
    }
    /// The nearest step, inside the range.
    fn snap(&self, v: f32) -> f32 {
        let (min, max) = (*self.range.start(), *self.range.end());
        (((v - min) / self.step).round() * self.step + min).clamp(min, max)
    }
    /// The value a drag to `x` sets, and the anchor for the next move. The press anchors the
    /// drag at (x, value). On a dense track the value moves a step per `FINE_PX` from there;
    /// past an end the anchor moves along, so turning back answers at once.
    fn dragged(&self, track: Rectangle, anchor: (f32, f32), x: f32) -> (f32, (f32, f32)) {
        let steps = ((*self.range.end() - *self.range.start()) / self.step).max(1.0);
        if track.width / steps >= FINE_PX {
            return (self.locate(track, x), anchor);
        }
        let (x0, v0) = anchor;
        let v = v0 + ((x - x0) / FINE_PX).round() * self.step;
        let snapped = self.snap(v);
        (snapped, if (snapped - v).abs() > self.step * 0.5 { (x, snapped) } else { anchor })
    }
}

#[derive(Default)]
struct State {
    ready: bool,
    drag: bool,
    mods: keyboard::Modifiers,
    shown: f32,
    last: f32,
    old: (i32, i32),
    changed: Option<Instant>,
    head_at: Option<Instant>,
    peak: Option<(i32, Instant)>,
    touched: Option<Instant>,
    frame: Option<Instant>,
    /// Where the drag was pressed or last re-anchored: (x, value).
    anchor: (f32, f32),
    /// Wheel notches not applied yet (touchpads send fractions).
    wheel: f32,
    painted: Painted,
}

impl<'a, Message> Widget<Message, Theme, Renderer> for Tacho<'a, Message> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<State>() }
    fn state(&self) -> tree::State { tree::State::new(State::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: self.width, height: Length::Fixed(if self.compact { 30.0 } else { 66.0 }) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, self.width, if self.compact { 30.0 } else { 66.0 })
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _: &Renderer,
        _: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let track = self.track(bounds);
        let hit = Rectangle { width: track.width + 12.0, x: track.x - 6.0, ..bounds };
        let publish = |v: f32, shell: &mut Shell<'_, Message>| {
            let v = v.clamp(*self.range.start(), *self.range.end());
            if (v - self.value).abs() > f32::EPSILON {
                shell.publish((self.on_change)(v));
            }
        };
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if self.enabled => {
                if let Some(p) = cursor.position_over(hit) {
                    if state.mods.command() {
                        publish(self.default, shell);
                    } else {
                        let v = self.locate(track, p.x);
                        publish(v, shell);
                        state.drag = true;
                        state.anchor = (p.x, v);
                    }
                    state.touched = Some(Instant::now());
                    shell.capture_event();
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.drag => {
                state.drag = false;
                state.touched = Some(Instant::now());
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if state.drag => {
                let (v, anchor) = self.dragged(track, state.anchor, position.x);
                state.anchor = anchor;
                publish(v, shell);
                state.touched = Some(Instant::now());
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if self.enabled && self.wheel && cursor.is_over(hit) => {
                state.wheel += match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / WHEEL_PX,
                };
                let whole = state.wheel.trunc();
                if whole != 0.0 {
                    state.wheel -= whole;
                    publish(self.snap(self.value + whole * self.step), shell);
                    state.touched = Some(Instant::now());
                    shell.request_redraw();
                }
                // Over a slider the wheel is the slider's: the page does not scroll under it.
                shell.capture_event();
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => state.mods = *m,
            Event::Window(window::Event::RedrawRequested(now)) => {
                let now = *now;
                let v = self.value;
                if !state.ready {
                    state.ready = true;
                    state.shown = v;
                    state.last = v;
                    let (lo, hi, _) = self.lit(v);
                    state.old = (lo, hi);
                }
                if (v - state.last).abs() > f32::EPSILON {
                    let (lo, hi, head) = self.lit(state.last);
                    let (_, _, new_head) = self.lit(v);
                    state.old = (lo, hi);
                    state.changed = Some(now);
                    state.touched = Some(now);
                    if new_head != head {
                        state.head_at = Some(now);
                        if self.origin.is_none() && new_head < head {
                            let top = state.peak.map_or(head, |(p, _)| p.max(head));
                            state.peak = Some((top, now));
                        }
                    }
                    state.last = v;
                }
                let dt = state.frame.map_or(0.0, |f| now.saturating_duration_since(f).as_secs_f32());
                state.frame = Some(now);
                let k = 1.0 - (-dt / 0.06).exp();
                state.shown += (v - state.shown) * k;
                if (v - state.shown).abs() < self.step * 0.5 {
                    state.shown = v;
                }
                if state.peak.is_some_and(|(_, at)| now.saturating_duration_since(at) > Duration::from_millis(600)) {
                    state.peak = None;
                }
                if let Some(next) = self.next_frame(state, now) {
                    shell.request_redraw_at(next);
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _: &Theme,
        _: &renderer::Style,
        layout: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let now = Instant::now();
        let bounds = layout.bounds();
        let track = self.track(bounds);
        let n = self.segments;
        let alpha = if self.enabled { 1.0 } else { 0.35 };
        let (lo, hi, head) = self.lit(self.value);
        let red_from = self.red_from();
        let in_red = self.in_red(self.value);
        let ignition = self.ignition(now);
        let idle = self.idle(state, now);
        let since_change = state.changed.map(|c| now.saturating_duration_since(c).as_secs_f32() * 1000.0);
        let flash = state.head_at
            .map(|a| now.saturating_duration_since(a).as_secs_f32() / 0.3)
            .filter(|t| *t < 1.0);
        let pulse = 0.85 + 0.15 * (now.saturating_duration_since(self.clock.epoch).as_secs_f32() * std::f32::consts::TAU / 1.8).cos();
        let press = if state.drag { 1.2 } else { 1.0 };
        let w = self.seg_width();
        let mut shapes = Vec::with_capacity(n * 6);
        // Overdrive judders at about 15 frames a second, like a clipping signal.
        let judder = (now.saturating_duration_since(self.clock.epoch).as_millis() / 66) as u32;
        let overdrive = self.overdrive && self.enabled;
        if overdrive && ignition.is_none() {
            // The ceiling the clipped signal keeps hitting.
            slant(&mut shapes, track.x - 2.0, track.y - 4.0, track.width + 4.0, 1.5, 0.0, Color { a: 0.55, ..HOT });
        }
        for i in 0..n {
            let ii = i as i32;
            let mut lit = ii >= lo && ii < hi;
            let mut is_head = lit && ii == head;
            if let (Some(ms), false) = (since_change, ignition.is_some()) {
                // Segments change in a ripple that starts at the new head.
                if ms < (ii - head).unsigned_abs() as f32 * STAGGER {
                    lit = ii >= state.old.0 && ii < state.old.1;
                    is_head = false;
                }
            }
            if let Some(sweep) = ignition {
                match sweep {
                    Sweep::Up(t) => { lit = t >= i as f32 * IGNITE_STEP; is_head = false; }
                    Sweep::Down(t) => {
                        if t < (n - i) as f32 * IGNITE_STEP { lit = true; is_head = false; }
                    }
                }
            }
            let red = ii >= red_from;
            let hot = lit && red && !is_head;
            let ghost = !lit && state.peak.is_some_and(|(p, _)| p == ii);
            let mut color = if lit {
                if is_head { HEAD } else if red { HOT } else { lerp(i as f32 / n as f32) }
            } else if ghost {
                Color { a: 0.4, ..HEAD }
            } else if red { OFF_RED } else { OFF };
            if hot {
                color = scale(color, pulse);
            }
            let x = self.seg_x(track, i);
            let mut height = track.height * press;
            if overdrive && lit && ignition.is_none() {
                // Hot from the first lit segment, with ragged, flickering tops.
                let t = (i as f32 / n as f32).powf(0.7);
                let base = if is_head { Color::from_rgb8(0xFF, 0xD0, 0xC8) } else { Color { r: TAG.r + (HOT.r - TAG.r) * t, g: TAG.g + (HOT.g - TAG.g) * t, b: TAG.b + (HOT.b - TAG.b) * t, a: 1.0 } };
                color = brighten(base, grain(i as u32 + 31, judder) * 0.5);
                height *= 0.8 + 0.2 * grain(i as u32, judder);
            }
            let mut lift = 0.0;
            let mut glow = 0.0;
            if is_head && let Some(t) = flash {
                height *= 1.0 + 0.5 * (1.0 - t);
                glow = 1.0 - t;
            }
            if idle {
                let (dy, b) = wave(self.wave_time(now, i), self.compact);
                lift = dy;
                glow = glow.max(b);
            }
            if glow > 0.0 {
                color = brighten(color, glow);
            }
            let y = track.y + track.height + lift - height;
            if overdrive && is_head {
                halo(&mut shapes, x, y, w, height, if self.compact { 8.0 } else { 12.0 }, Color { a: 0.6, ..HOT });
            } else if self.enabled && is_head {
                halo(&mut shapes, x, y, w, height, if self.compact { 7.0 } else { 11.0 }, Color { a: 0.6, ..GLOW });
            } else if self.enabled && hot {
                halo(&mut shapes, x, y, w, height, 6.0, Color { a: 0.35 * pulse, ..HOT });
            }
            if lit {
                segment(&mut shapes, x, y, w, height, Color { a: color.a * alpha, ..color });
            } else {
                // Unlit segments are outlined, as in the mockup.
                segment(&mut shapes, x, y, w, height, Color { a: alpha, ..OFF_EDGE });
                segment(&mut shapes, x + 1.0, y + 1.0, w - 2.0, height - 2.0, Color { a: color.a * alpha, ..color });
            }
        }
        let text_value = if matches!(ignition, Some(Sweep::Up(_))) {
            "MAX".to_owned()
        } else {
            let shown = if state.ready { state.shown } else { self.value };
            (self.format)((shown / self.step).round() * self.step)
        };
        let red_text = (in_red || overdrive) && ignition.is_none();
        if self.compact {
            state.painted.draw(renderer, bounds.expand(24.0), shapes);
            put(
                renderer,
                label(text_value, 15.0, Size::new(58.0, bounds.height), text::Alignment::Right),
                Point::new(bounds.x + bounds.width, bounds.center_y()),
                Color { a: alpha, ..if red_text { HOT } else { INK } },
                bounds,
            );
        } else {
            let text = label(text_value, 14.0, Size::new(120.0, 23.0), text::Alignment::Center);
            let tw = measure(&text).width + 24.0;
            let at = if hi > lo { head as f32 + 0.5 } else { lo as f32 };
            let cx = track.x + w / 2.0 + (track.width - w) * (at - 0.5).max(0.0) / (n - 1) as f32;
            let tx = (cx - tw / 2.0).clamp(bounds.x, bounds.x + bounds.width - tw);
            let ty = bounds.y + if state.drag { -4.0 } else { 0.0 };
            // The tag's slant matches the mockup's clip-path: 7 px over its height.
            slant(&mut shapes, tx, ty, tw - 7.0, 23.0, 7.0, Color { a: alpha, ..if red_text { HOT } else { TAG } });
            state.painted.draw(renderer, bounds.expand(24.0), shapes);
            put(renderer, text, Point::new(tx + tw / 2.0, ty + 11.5), Color { a: alpha, ..DARK }, bounds.expand(8.0));
        }
    }

    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, _: &Rectangle, _: &Renderer) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        if self.enabled && (state.drag || cursor.is_over(layout.bounds())) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

#[derive(Clone, Copy)]
enum Sweep {
    Up(f32),
    Down(f32),
}

impl<Message> Tacho<'_, Message> {
    fn ignition(&self, now: Instant) -> Option<Sweep> {
        let opened = self.clock.opened?;
        let t = now.saturating_duration_since(opened).as_secs_f32() * 1000.0 - 300.0;
        let up = self.segments as f32 * IGNITE_STEP + 260.0;
        let down = self.segments as f32 * IGNITE_STEP + 60.0;
        if t < 0.0 {
            None
        } else if t < up {
            Some(Sweep::Up(t))
        } else if t < up + down {
            Some(Sweep::Down(t - up))
        } else {
            None
        }
    }
    fn idle(&self, state: &State, now: Instant) -> bool {
        self.clock.animate
            && self.clock.idle
            && self.enabled
            && !state.drag
            && self.ignition(now).is_none()
            && state.touched.is_none_or(|t| now.saturating_duration_since(t) >= IDLE_AFTER)
    }
    fn wave_time(&self, now: Instant, i: usize) -> f32 {
        let t = now.saturating_duration_since(self.clock.epoch).as_secs_f32() * 1000.0
            - self.phase
            - i as f32 * WAVE_STEP;
        t.rem_euclid(WAVE_CYCLE)
    }
    /// When the next frame is needed, if at all.
    fn next_frame(&self, state: &State, now: Instant) -> Option<RedrawRequest> {
        if !self.clock.animate {
            return None;
        }
        let soon = |ms: u64| RedrawRequest::At(now + Duration::from_millis(ms));
        let busy = state.drag
            || (self.value - state.shown).abs() > f32::EPSILON
            || state.changed.is_some_and(|c| now.saturating_duration_since(c) < Duration::from_millis(self.segments as u64 * STAGGER as u64 + 20))
            || state.head_at.is_some_and(|a| now.saturating_duration_since(a) < Duration::from_millis(320))
            || state.peak.is_some()
            || self.ignition(now).is_some()
            || self.clock.opened.is_some_and(|o| now.saturating_duration_since(o) < Duration::from_millis(300));
        if busy {
            return Some(frame_after(now));
        }
        if (self.in_red(self.value) || self.overdrive) && self.enabled {
            return Some(soon(33));
        }
        if let Some(t) = state.touched {
            let quiet = now.saturating_duration_since(t);
            if quiet < IDLE_AFTER {
                return Some(RedrawRequest::At(t + IDLE_AFTER));
            }
        }
        // Idle: frames only while the wave crosses this slider, then sleep until it returns.
        let first = self.wave_time(now, 0);
        let span = (self.segments - 1) as f32 * WAVE_STEP + WAVE_LEN;
        if first < span {
            Some(soon(33))
        } else {
            Some(RedrawRequest::At(now + Duration::from_millis((WAVE_CYCLE - first) as u64)))
        }
    }
}

/// Lift in px and brightening of the idle wave at `t` ms into a segment's cycle.
fn wave(t: f32, compact: bool) -> (f32, f32) {
    let keys: [(f32, f32, f32); 5] = if compact {
        [(0.0, 0.0, 0.0), (144.0, -2.0, 0.2), (284.0, -6.0, 0.7), (424.0, -2.0, 0.2), (WAVE_LEN, 0.0, 0.0)]
    } else {
        [(0.0, 0.0, 0.0), (144.0, -3.0, 0.2), (284.0, -10.0, 0.7), (424.0, -3.0, 0.2), (WAVE_LEN, 0.0, 0.0)]
    };
    if t >= WAVE_LEN {
        return (0.0, 0.0);
    }
    for pair in keys.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if t <= b.0 {
            let k = (t - a.0) / (b.0 - a.0);
            return (a.1 + (b.1 - a.1) * k, (a.2 + (b.2 - a.2) * k) * 0.6);
        }
    }
    (0.0, 0.0)
}

pub fn lerp(t: f32) -> Color {
    let c = |i: usize| (LOW[i] + (HIGH[i] - LOW[i]) * t) / 255.0;
    Color::from_rgb(c(0), c(1), c(2))
}
fn brighten(c: Color, k: f32) -> Color {
    let k = k.clamp(0.0, 1.0) * 0.45;
    Color { r: c.r + (1.0 - c.r) * k, g: c.g + (1.0 - c.g) * k, b: c.b + (1.0 - c.b) * k, a: c.a }
}
fn scale(c: Color, k: f32) -> Color {
    Color { a: c.a * k, ..c }
}

/// A parallelogram whose top edge sits `lean` px right of its bottom edge.
#[derive(Clone, Copy, PartialEq)]
struct Shape {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    lean: f32,
    /// Corner rounding, for glow rings.
    round: f32,
    color: Color,
}

fn slant(shapes: &mut Vec<Shape>, x: f32, y: f32, w: f32, h: f32, lean: f32, color: Color) {
    if w > 0.0 && h > 0.0 && color.a > 0.0 {
        shapes.push(Shape { x, y, w, h, lean, round: 0.0, color });
    }
}

fn geometry(clip: Rectangle, shapes: &[Shape]) -> Geometry {
    let mut frame = Frame::new(clip);
    fill_shapes(&mut frame, shapes);
    frame.into_geometry()
}

fn fill_shapes(frame: &mut Frame, shapes: &[Shape]) {
    for &Shape { x, y, w, h, lean, round, color } in shapes {
        let corners = [Point::new(x + lean, y), Point::new(x + lean + w, y), Point::new(x + w, y + h), Point::new(x, y + h)];
        let path = Path::new(|p| {
            if round > 0.0 {
                let [a, b, ..] = corners;
                p.move_to(Point::new((a.x + b.x) / 2.0, y));
                for i in 1..=4 {
                    p.arc_to(corners[i % 4], corners[(i + 1) % 4], round);
                }
            } else {
                p.move_to(corners[0]);
                for &c in &corners[1..] {
                    p.line_to(c);
                }
            }
            p.close();
        });
        frame.fill(&path, color);
    }
}

/// The last drawn shapes. tiny-skia treats uncached geometry as changed on every frame, which
/// would repaint each slider whenever anything else in the window redraws; an unchanged frame
/// reuses the cache instead and costs nothing.
#[derive(Default)]
struct Painted(RefCell<Option<(Rectangle, Vec<Shape>, Cache)>>);

impl Painted {
    fn draw(&self, renderer: &mut Renderer, clip: Rectangle, shapes: Vec<Shape>) {
        let mut last = self.0.borrow_mut();
        if !last.as_ref().is_some_and(|(c, s, _)| *c == clip && *s == shapes) {
            let cache = geometry(clip, &shapes).cache(Group::unique(), None);
            *last = Some((clip, shapes, cache));
        }
        if let Some((_, _, cache)) = last.as_ref() {
            renderer.draw_geometry(Geometry::load(cache));
        }
    }
}

/// A box skewed by 20° around its centre, like the mockup's `skewX(-20deg)`.
fn segment(shapes: &mut Vec<Shape>, x: f32, y: f32, w: f32, h: f32, color: Color) {
    slant(shapes, x - h * SKEW / 2.0, y, w, h, h * SKEW, color);
}

/// A soft glow around a segment: stacked, growing translucent copies of its shape stand in
/// for the mockup's blurred box-shadow, which tiny-skia cannot blur.
/// Each ring is the segment grown by `e` with corners rounded by `e`, the outline a blur's
/// level lines follow, so the glow has no spikes at the slanted corners.
fn halo(shapes: &mut Vec<Shape>, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
    const RINGS: usize = 8;
    let cos = 1.0 / (1.0 + SKEW * SKEW).sqrt();
    for k in 1..=RINGS {
        let e = radius * k as f32 / RINGS as f32;
        let (gh, side) = (h + 2.0 * e, e / cos);
        shapes.push(Shape {
            x: x - h * SKEW / 2.0 - side - e * SKEW,
            y: y - e,
            w: w + 2.0 * side,
            h: gh,
            lean: gh * SKEW,
            round: e,
            color: Color { a: color.a * 0.09, ..color },
        });
    }
}

fn text_width(content: &str, size: f32, font: Font) -> f32 {
    measure(&Text {
        content: content.to_owned(),
        bounds: Size::INFINITE,
        size: Pixels(size),
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    })
    .width
}

fn measure(text: &Text<String, Font>) -> Size {
    <Renderer as text::Renderer>::Paragraph::with_text(Text {
        content: text.content.as_str(),
        bounds: text.bounds,
        size: text.size,
        line_height: text.line_height,
        font: text.font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text.shaping,
        wrapping: text.wrapping,
    })
    .min_bounds()
}

/// `fill_text` anchored by the text's own alignment. tiny-skia repaints only damaged regions and
/// records a text as starting at its position, so centred or right-aligned text that moves or
/// changes would leave stale pixels behind; drawing it from its measured top-left avoids that.
fn put(renderer: &mut Renderer, text: Text<String, Font>, at: Point, color: Color, clip: Rectangle) {
    let size = measure(&text);
    let x = match text.align_x {
        text::Alignment::Center => at.x - size.width / 2.0,
        text::Alignment::Right => at.x - size.width,
        _ => at.x,
    };
    let y = match text.align_y {
        iced::alignment::Vertical::Center => at.y - size.height / 2.0,
        iced::alignment::Vertical::Bottom => at.y - size.height,
        iced::alignment::Vertical::Top => at.y,
    };
    renderer.fill_text(
        Text {
            bounds: Size::new(size.width + 2.0, size.height),
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            ..text
        },
        Point::new(x, y),
        color,
        clip,
    );
}

fn label(content: String, size: f32, bounds: Size, align: text::Alignment) -> Text<String, Font> {
    Text {
        content,
        bounds,
        size: Pixels(size),
        line_height: text::LineHeight::default(),
        font: numbers(),
        align_x: align,
        align_y: iced::alignment::Vertical::Center,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    }
}

/// Police tape behind a card whose value is in the red zone: two hatched bands with the
/// word crawling along them. It rolls out once, then only the text moves.
pub fn caution<'a, Message: 'a>(clock: Clock, shown: bool) -> Element<'a, Message> {
    Element::new(Caution { clock, shown })
}
struct Caution {
    clock: Clock,
    /// Hidden tapes stay in the tree so the card's content keeps its widget state.
    shown: bool,
}
#[derive(Default)]
struct Born(Option<Instant>);
impl<Message> Widget<Message, Theme, Renderer> for Caution {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<Born>() }
    fn state(&self) -> tree::State { tree::State::new(Born::default()) }
    fn size(&self) -> Size<Length> { Size { width: Length::Fill, height: Length::Fill } }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let born = tree.state.downcast_mut::<Born>();
            if !self.shown {
                born.0 = None;
                return;
            }
            born.0.get_or_insert(*now);
            if self.clock.animate {
                shell.request_redraw_at(RedrawRequest::At(*now + Duration::from_millis(40)));
            }
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let now = Instant::now();
        if !self.shown {
            return;
        }
        // Without a frame event yet (a headless render) the tapes are shown fully rolled out.
        let age = tree.state.downcast_ref::<Born>().0.map_or(1.0, |born| now.saturating_duration_since(born).as_secs_f32());
        let t = now.saturating_duration_since(self.clock.epoch).as_secs_f32();
        renderer.with_layer(b, |renderer| {
            renderer.fill_quad(
                Quad { bounds: b, border: Border { radius: 10.0.into(), ..Border::default() }, ..Quad::default() },
                Color { a: 0.035, ..HOT },
            );
            let word = "ЭКСПЕРИМЕНТАЛЬНО   ///   ";
            let run = text_width(word, 11.0, numbers());
            let mut frame = Frame::new(b);
            // Crossed like police tape, not mirrored: they cross high on the card and stay above
            // the slider's track, so the slider hides little of them.
            let cross = Point::new(b.x + b.width * 0.62, b.y + b.height * 0.30);
            for (k, degrees) in [-7.0_f32, 11.0].into_iter().enumerate() {
                let fade = ((age - k as f32 * 0.12) / 0.55).clamp(0.0, 1.0);
                if fade <= 0.0 {
                    continue;
                }
                let (h, len) = (24.0, b.width * 1.8);
                let (x0, y0) = (-len / 2.0, -h / 2.0);
                let mut shapes = vec![Shape { x: x0, y: y0, w: len, h, lean: 0.0, round: 0.0, color: Color { a: 0.08 * fade, ..HOT } }];
                // 45° hatching like the mockup's repeating gradient: 14 px stripes across.
                let (stripe, period) = (14.0 * std::f32::consts::SQRT_2, 28.0 * std::f32::consts::SQRT_2);
                let mut x = x0 - h - period + (t * 6.0) % period;
                while x < x0 + len {
                    slant(&mut shapes, x, y0, stripe, h, h, Color { a: 0.13 * fade, ..HOT });
                    x += period;
                }
                for edge in [y0, y0 + h - 1.0] {
                    slant(&mut shapes, x0, edge, len, 1.0, 0.0, Color { a: 0.35 * fade, ..HOT });
                }
                frame.push_transform();
                frame.translate(iced::Vector::new(cross.x, cross.y));
                frame.rotate(degrees.to_radians());
                fill_shapes(&mut frame, &shapes);
                // The word crawls; the two tapes move in opposite directions.
                let speed = if k == 0 { -11.0 } else { 9.0 };
                let start = x0 - run + (t * speed).rem_euclid(run);
                let repeat = (len / run) as usize + 2;
                frame.fill_text(tape_text(word.repeat(repeat), Point::new(start, 0.0), Color { a: 0.5 * fade, ..Color::from_rgb8(0xFF, 0x82, 0x78) }));
                frame.pop_transform();
            }
            renderer.draw_geometry(frame.into_geometry());
        });
    }
}

/// A tape's caption; rotated geometry text is drawn as glyph outlines, clipped with the card.
fn tape_text(content: String, position: Point, color: Color) -> iced_tiny_skia::graphics::geometry::Text {
    iced_tiny_skia::graphics::geometry::Text {
        content,
        position,
        max_width: f32::INFINITY,
        color,
        size: Pixels(11.0),
        line_height: text::LineHeight::default(),
        font: numbers(),
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Center,
        shaping: text::Shaping::Basic,
    }
}

/// The reverse row's practice word: its letters fly over to the mirrored places and back.
/// Hovering holds it reversed so it can be read aloud.
pub fn reverse_word<'a, Message: 'a>(word: &str, clock: Clock) -> Element<'a, Message> {
    let word: Vec<char> = word.trim().to_uppercase().chars().take(12).collect();
    let word: Vec<char> = if word.is_empty() { "ПРИВЕТ".chars().collect() } else { word };
    let widths = word.iter().map(|c| text_width(&c.to_string(), 15.0, numbers())).collect();
    Element::new(Reverse { word, widths, clock })
}
struct Reverse {
    word: Vec<char>,
    /// Each letter's own advance, so Ж and Г keep natural spacing both ways round.
    widths: Vec<f32>,
    clock: Clock,
}
#[derive(Default)]
struct Hover(bool);
const LETTER_GAP: f32 = 1.5;
const REV_CYCLE: f32 = 4000.0;
/// Frames while the letters fly: a 144 Hz pace for the short flights (600 ms each); only the
/// word's own bounds repaint.
const REV_FRAME: Duration = Duration::from_millis(7);
impl Reverse {
    fn cycle(&self, now: Instant) -> f32 {
        (now.saturating_duration_since(self.clock.epoch).as_secs_f32() * 1000.0).rem_euclid(REV_CYCLE) / REV_CYCLE
    }
    /// 0 plain, 1 reversed, in between while the letters fly.
    fn progress(&self, now: Instant, hover: bool) -> f32 {
        if hover {
            return 1.0;
        }
        let t = self.cycle(now);
        let ease = |x: f32| x * x * (3.0 - 2.0 * x);
        if t < 0.25 {
            0.0
        } else if t < 0.40 {
            ease((t - 0.25) / 0.15)
        } else if t < 0.75 {
            1.0
        } else if t < 0.90 {
            1.0 - ease((t - 0.75) / 0.15)
        } else {
            0.0
        }
    }
    fn width(&self) -> f32 {
        24.0 + self.widths.iter().map(|w| w + LETTER_GAP).sum::<f32>() + 4.0
    }
}
impl<Message> Widget<Message, Theme, Renderer> for Reverse {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<Hover>() }
    fn state(&self) -> tree::State { tree::State::new(Hover::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fixed(self.width()), height: Length::Fixed(28.0) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fixed(self.width()), Length::Fixed(28.0))
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        let hover = &mut tree.state.downcast_mut::<Hover>().0;
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let over = cursor.is_over(layout.bounds());
                if over != *hover {
                    *hover = over;
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) if self.clock.animate && !*hover => {
                let t = self.cycle(*now);
                let moving = (0.25..0.40).contains(&t) || (0.75..0.90).contains(&t);
                if moving {
                    shell.request_redraw_at(RedrawRequest::At(*now + REV_FRAME));
                } else {
                    let edge = [0.25, 0.75, 1.25].into_iter().find(|e| *e > t).unwrap_or(1.25);
                    let wait = ((edge - t) * REV_CYCLE).max(16.0);
                    shell.request_redraw_at(RedrawRequest::At(*now + Duration::from_millis(wait as u64)));
                }
            }
            _ => {}
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let hover = tree.state.downcast_ref::<Hover>().0;
        let p = self.progress(Instant::now(), hover);
        let tint = Color {
            r: INK.r + (TAG.r - INK.r) * p,
            g: INK.g + (TAG.g - INK.g) * p,
            b: INK.b + (TAG.b - INK.b) * p,
            a: 1.0,
        };
        let glyph = |content: String, size: f32, font: Font| Text {
            content,
            bounds: Size::new(40.0, 28.0),
            size: Pixels(size),
            line_height: text::LineHeight::default(),
            font,
            align_x: text::Alignment::Center,
            align_y: iced::alignment::Vertical::Center,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::None,
        };
        let clip = Rectangle { y: b.y - 12.0, height: b.height + 24.0, ..b };
        put(
            renderer,
            glyph("\u{E7A7}".into(), 12.0, Font::with_name("Segoe MDL2 Assets")),
            Point::new(b.x + 8.0, b.center_y()),
            if p > 0.5 { TAG } else { Color::from_rgb8(0x85, 0x86, 0x8D) },
            clip,
        );
        let arc = (p * std::f32::consts::PI).sin();
        let advance = |w: &f32| w + LETTER_GAP;
        // In flight the letters are glyph outlines: those sit at fractional positions, where text
        // snaps to whole pixels and the flight looked short of frames.
        let flying = p > 0.0 && p < 1.0;
        let mut frame = flying.then(|| Frame::new(clip));
        for (i, ch) in self.word.iter().enumerate() {
            // Letter i starts after the letters before it, and ends up after those behind it.
            let from = self.widths[..i].iter().map(advance).sum::<f32>();
            let to = self.widths[i + 1..].iter().map(advance).sum::<f32>();
            let d = to - from;
            let lift = if d.abs() < 0.5 { -4.0 } else { -d.signum() * (3.0 + d.abs() / 10.0) };
            let x = b.x + 24.0 + from + self.widths[i] / 2.0 + d * p;
            let y = b.center_y() + lift * arc;
            match frame.as_mut() {
                Some(frame) => frame.fill_text(iced_tiny_skia::graphics::geometry::Text {
                    content: ch.to_string(),
                    position: Point::new(x, y),
                    max_width: f32::INFINITY,
                    color: tint,
                    size: Pixels(15.0),
                    line_height: text::LineHeight::default(),
                    font: numbers(),
                    align_x: text::Alignment::Center,
                    align_y: iced::alignment::Vertical::Center,
                    shaping: text::Shaping::Basic,
                }),
                None => put(renderer, glyph(ch.to_string(), 15.0, numbers()), Point::new(x, y), tint, clip),
            }
        }
        if let Some(frame) = frame {
            renderer.with_layer(clip, |renderer| renderer.draw_geometry(frame.into_geometry()));
        }
    }
    fn mouse_interaction(&self, _: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, _: &Rectangle, _: &Renderer) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) { mouse::Interaction::Text } else { mouse::Interaction::default() }
    }
}

/// A hotkey keycap that sinks and lights up while its key is held, as in the mockup's `kpress`:
/// a quick dip past the rest depth, then it settles; releasing springs back with a small lift.
pub fn keycap<'a, Message: 'a>(label: &str, down: bool) -> Element<'a, Message> {
    let width = text_width(label, 11.0, Font::with_name("Consolas")) + 14.0;
    Element::new(Keycap { label: label.to_owned(), down, width })
}
struct Keycap {
    label: String,
    down: bool,
    width: f32,
}
#[derive(Default)]
struct KeyMotion {
    down: bool,
    since: Option<Instant>,
}
const CAP_H: f32 = 19.0;
const TRAVEL: f32 = 3.0;
const PRESS_MS: f32 = 280.0;
const RELEASE_MS: f32 = 240.0;
impl Keycap {
    /// Depth in px, cap scale and orange mix for the current moment.
    fn pose(&self, motion: &KeyMotion, now: Instant) -> (f32, f32, f32) {
        let ms = motion.since.map_or(f32::MAX, |s| now.saturating_duration_since(s).as_secs_f32() * 1000.0);
        let ease = |x: f32| 1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3);
        if self.down {
            let t = ms / PRESS_MS;
            if t >= 1.0 {
                return (TRAVEL, 1.0, 1.0);
            }
            let (depth, scale) = if t < 0.4 {
                let k = ease(t / 0.4);
                (TRAVEL * 1.5 * k, 1.0 - 0.1 * k)
            } else {
                let k = ease((t - 0.4) / 0.6);
                (TRAVEL * (1.5 - 0.5 * k), 0.9 + 0.1 * k)
            };
            (depth, scale, ease(t / 0.35))
        } else {
            let t = ms / RELEASE_MS;
            if t >= 1.0 {
                return (0.0, 1.0, 0.0);
            }
            let depth = if t < 0.5 { TRAVEL - (TRAVEL + 1.2) * ease(t / 0.5) } else { -1.2 * (1.0 - ease((t - 0.5) / 0.5)) };
            (depth, 1.0, 1.0 - ease(t / 0.5))
        }
    }
}
impl<Message> Widget<Message, Theme, Renderer> for Keycap {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<KeyMotion>() }
    fn state(&self) -> tree::State { tree::State::new(KeyMotion { down: self.down, since: None }) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fixed(self.width), height: Length::Fixed(CAP_H + TRAVEL + 2.0) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fixed(self.width), Length::Fixed(CAP_H + TRAVEL + 2.0))
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let motion = tree.state.downcast_mut::<KeyMotion>();
            if motion.down != self.down {
                motion.down = self.down;
                motion.since = Some(*now);
            }
            let span = if self.down { PRESS_MS } else { RELEASE_MS };
            if motion.since.is_some_and(|s| now.saturating_duration_since(s).as_secs_f32() * 1000.0 < span) {
                shell.request_redraw_at(frame_after(*now));
            }
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let (depth, scale, lit) = self.pose(tree.state.downcast_ref::<KeyMotion>(), Instant::now());
        let mix = |a: Color, b: Color| Color { r: a.r + (b.r - a.r) * lit, g: a.g + (b.g - a.g) * lit, b: a.b + (b.b - a.b) * lit, a: 1.0 };
        let top = b.y + 1.0;
        let (w, h) = (b.width * scale, CAP_H * scale);
        let cap = Rectangle { x: b.center_x() - w / 2.0, y: top + depth.max(-1.2) + (CAP_H - h) / 2.0, width: w, height: h };
        for e in [6.0_f32, 4.0, 2.0] {
            if lit > 0.0 {
                renderer.fill_quad(
                    Quad { bounds: cap.expand(e), border: Border { radius: (5.0 + e).into(), ..Border::default() }, ..Quad::default() },
                    Color { a: 0.12 * lit, ..TAG },
                );
            }
        }
        // The key's side below the cap; a held cap sinks into it.
        renderer.fill_quad(
            Quad { bounds: Rectangle { x: b.x, y: top + TRAVEL, width: b.width, height: CAP_H }, border: Border { radius: 5.0.into(), ..Border::default() }, ..Quad::default() },
            mix(Color::from_rgb8(0x11, 0x12, 0x14), Color::from_rgb8(0x8A, 0x4E, 0x1F)),
        );
        renderer.fill_quad(
            Quad { bounds: cap, border: Border { color: mix(Color::from_rgb8(0x3A, 0x3B, 0x41), Color::from_rgb8(0xC9, 0x72, 0x2F)), width: 1.0, radius: 5.0.into() }, ..Quad::default() },
            mix(Color::from_rgb8(0x2A, 0x2B, 0x30), TAG),
        );
        put(
            renderer,
            Text {
                content: self.label.clone(),
                bounds: Size::new(b.width, CAP_H),
                size: Pixels(11.0 * scale),
                line_height: text::LineHeight::default(),
                font: Font::with_name("Consolas"),
                align_x: text::Alignment::Center,
                align_y: iced::alignment::Vertical::Center,
                shaping: text::Shaping::Basic,
                wrapping: text::Wrapping::None,
            },
            cap.center(),
            mix(Color::from_rgb8(0xE8, 0xE3, 0xD9), DARK),
            b.expand(8.0),
        );
    }
}

/// The До / После level bars. The raw microphone is drawn as ragged, gently flickering bars; the denoised output as crisp slanted segments in the sliders'
/// style, with a glowing head. `level` is 0..1.
pub fn level_meter<'a, Message: 'a>(level: f32, noisy: bool, color: Color) -> Element<'a, Message> {
    Element::new(LevelMeter { level: level.clamp(0.0, 1.0), noisy, color })
}
struct LevelMeter {
    level: f32,
    noisy: bool,
    color: Color,
}
const METER_H: f32 = 14.0;
const CLEAN_SEGMENTS: usize = 40;
#[derive(Default)]
struct MeterState {
    painted: Painted,
}
/// Stable pseudo-random 0..1 for bar `i` in flicker frame `t`.
fn grain(i: u32, t: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B1) ^ t.wrapping_mul(0x85EB_CA77);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}
impl<Message> Widget<Message, Theme, Renderer> for LevelMeter {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<MeterState>() }
    fn state(&self) -> tree::State { tree::State::new(MeterState::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fixed(METER_H) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fixed(METER_H))
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let track = Color::from_rgb8(0x26, 0x27, 0x2B);
        if !self.noisy {
            let m = tree.state.downcast_ref::<MeterState>();
            let n = CLEAN_SEGMENTS;
            let (gap, h) = (3.0, METER_H - 2.0);
            let w = (b.width - 8.0 - gap * (n - 1) as f32) / n as f32;
            let lit = (self.level * n as f32).round() as usize;
            let dim = Color { r: self.color.r * 0.38, g: self.color.g * 0.38, b: self.color.b * 0.38, a: 1.0 };
            let mut shapes = Vec::with_capacity(n * 2 + 8);
            for i in 0..n {
                let x = b.x + 4.0 + i as f32 * (w + gap);
                let y = b.y + 1.0;
                if i < lit {
                    // A calm gradient from deep to full colour; the newest segment glows.
                    let t = i as f32 / n as f32;
                    let c = Color { r: dim.r + (self.color.r - dim.r) * t, g: dim.g + (self.color.g - dim.g) * t, b: dim.b + (self.color.b - dim.b) * t, a: 1.0 };
                    if i + 1 == lit {
                        halo(&mut shapes, x, y, w, h, 6.0, Color { a: 0.55, ..self.color });
                        segment(&mut shapes, x, y, w, h, brighten(self.color, 0.9));
                    } else {
                        segment(&mut shapes, x, y, w, h, c);
                    }
                } else {
                    segment(&mut shapes, x, y, w, h, OFF_EDGE);
                    segment(&mut shapes, x + 1.0, y + 1.0, w - 2.0, h - 2.0, OFF);
                }
            }
            m.painted.draw(renderer, b.expand(10.0), shapes);
            return;
        }
        // The meter redraws with every level update; the flicker frame follows real time,
        // slow enough to read as a restless signal rather than strobing.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| (d.as_millis() / 160) as u32);
        const BARS: u32 = 60;
        let gap = 2.0;
        let w = (b.width - gap * (BARS - 1) as f32) / BARS as f32;
        let lit = (self.level * BARS as f32).round() as u32;
        for i in 0..BARS {
            let x = b.x + i as f32 * (w + gap);
            let r = grain(i, t);
            if i < lit {
                // Ragged heights and uneven brightness: hiss, not a clean signal.
                let h = METER_H * (0.55 + 0.45 * r);
                let k = 0.85 + 0.3 * grain(i + 97, t);
                let c = Color { r: (self.color.r * k).min(1.0), g: (self.color.g * k).min(1.0), b: (self.color.b * k).min(1.0), a: 1.0 };
                renderer.fill_quad(Quad { bounds: Rectangle { x, y: b.y + (METER_H - h) / 2.0, width: w, height: h }, ..Quad::default() }, c);
            } else {
                renderer.fill_quad(Quad { bounds: Rectangle { x, y: b.y + METER_H / 2.0 - 1.0, width: w, height: 2.0 }, ..Quad::default() }, track);
            }
        }
    }
}

/// The app mark from the design canvas: grey noise specks on the left turning into clean orange
/// voice bars on a dark tile. The bars rise a little with the output `level` (0..1).
pub fn logo<'a, Message: 'a>(size: f32, level: f32) -> Element<'a, Message> {
    Element::new(Logo { size, level: level.clamp(0.0, 1.0) })
}
struct Logo {
    size: f32,
    level: f32,
}
/// x, y, w, h, corner radius and grey level on the mark's 64-unit grid; grey 0 means orange.
const LOGO_BARS: [(f32, f32, f32, f32, f32, u8); 9] = [
    (8.0, 29.0, 4.0, 4.0, 1.0, 0x4A),
    (13.0, 21.0, 4.0, 4.0, 1.0, 0x5E),
    (13.0, 37.0, 3.0, 3.0, 1.0, 0x5E),
    (18.0, 28.0, 4.0, 4.0, 1.0, 0x7A),
    (24.0, 20.0, 5.0, 10.0, 2.5, 0x8E),
    (24.0, 33.0, 5.0, 11.0, 2.5, 0x8E),
    (33.0, 24.0, 6.0, 16.0, 3.0, 0),
    (42.0, 13.0, 6.0, 38.0, 3.0, 0),
    (51.0, 20.0, 6.0, 24.0, 3.0, 0),
];
impl<Message> Widget<Message, Theme, Renderer> for Logo {
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fixed(self.size), height: Length::Fixed(self.size) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fixed(self.size), Length::Fixed(self.size))
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let k = self.size / 64.0;
        renderer.fill_quad(
            Quad { bounds: b, border: Border { color: Color::from_rgb8(0x2A, 0x2B, 0x30), width: 1.0, radius: (14.0 * k).into() }, ..Quad::default() },
            Color::from_rgb8(0x1B, 0x1C, 0x1F),
        );
        for (x, y, w, h, r, grey) in LOGO_BARS {
            let (y, h, color) = if grey == 0 {
                // Voice bars grow about their centre with the level, never past the tile.
                let grown = (h * (1.0 + 0.4 * self.level)).min(54.0);
                (y + (h - grown) / 2.0, grown, TAG)
            } else {
                (y, h, Color::from_rgb8(grey, grey + 1, grey + 7))
            };
            renderer.fill_quad(
                Quad {
                    bounds: Rectangle { x: b.x + x * k, y: b.y + y * k, width: w * k, height: h * k },
                    border: Border { radius: (r * k).into(), ..Border::default() },
                    snap: false,
                    ..Quad::default()
                },
                color,
            );
        }
    }
}

/// A page painted offscreen at 1/[`MOSAIC_CELL`] scale: one RGB cell per 4×4 logical px.
pub struct Mosaic {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<[u8; 3]>,
}
pub const MOSAIC_CELL: f32 = 4.0;
const BLOCK_MAX: f32 = 48.0;
/// The page switch: the new page is drawn sharp from the very first frame, as without the
/// effect, and its own mosaic lies over it and dissolves in a wave from left to right while the
/// blocks shrink. Nothing waits for the animation; it only marks the change. Two bare steps
/// read as a rendering glitch, so the dissolve takes ten.
const SHIFT_STEPS: usize = 10;
/// How long each step stays: the whole switch takes 10 × 12 ms. A late frame skips ahead.
const SHIFT_STEP: Duration = Duration::from_millis(12);
/// Step `step`: the block side in logical px, and the mosaic's opacity at `u` (0 left edge, 1 right).
fn shift_step(step: usize, u: f32) -> (f32, f32) {
    let t = step as f32 / (SHIFT_STEPS - 1) as f32;
    let ease = t * t * (3.0 - 2.0 * t);
    let block = 16.0 - 11.0 * ease;
    // The wave crosses the page over the whole switch; each column fades evenly behind it.
    let local = (t * 1.5 - u * 0.5).clamp(0.0, 1.0);
    // Opacity in steps of 0.1: neighbours in one band merge into one shape.
    let alpha = (0.9 * (1.0 - local) * 10.0).round() / 10.0;
    (block, alpha)
}
/// The page area's last laid-out size, so pages can be painted offscreen at the same size.
// ponytail: one window, one page area; a per-window map if the UI ever opens a second one.
static PAGE_AREA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn page_area() -> Option<Size> {
    let v = PAGE_AREA.load(std::sync::atomic::Ordering::Relaxed);
    let size = Size::new(f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32));
    (size.width >= 1.0 && size.height >= 1.0).then_some(size)
}

/// The page switch ([`SHIFT`]) over the new page. `shift` holds the new page's mosaic and the
/// switch time; `done` is sent when it has played out.
pub fn page_shift<'a, Message: Clone + 'a>(shift: Option<&(std::sync::Arc<Mosaic>, Instant)>, done: Message) -> Element<'a, Message> {
    Element::new(PageShift { shift: shift.cloned(), done })
}
struct PageShift<Message> {
    shift: Option<(std::sync::Arc<Mosaic>, Instant)>,
    done: Message,
}
/// The clock starts at the switch's first window frame, so that frame always shows the first
/// step; later steps follow the clock, and a late frame skips ahead instead of stretching it.
#[derive(Default)]
struct ShiftState {
    /// The switch being played, its first frame and the step that frame logic chose.
    start: Option<Instant>,
    first: Option<Instant>,
    step: usize,
}
impl ShiftState {
    /// The step to show at the window frame `now`; past the end when it has played out. The
    /// 2 ms cover a timer that wakes a hair early.
    fn frame(&mut self, start: Instant, now: Instant) -> usize {
        if self.start != Some(start) {
            *self = ShiftState { start: Some(start), first: Some(now), step: 0 };
        }
        let first = self.first.unwrap_or(now);
        self.step = ((now.saturating_duration_since(first) + Duration::from_millis(2)).as_millis() / SHIFT_STEP.as_millis()) as usize;
        self.step
    }
}
impl<Message: Clone> Widget<Message, Theme, Renderer> for PageShift<Message> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<ShiftState>() }
    fn state(&self) -> tree::State { tree::State::new(ShiftState::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        let node = layout::atomic(limits, Length::Fill, Length::Fill);
        let size = node.size();
        PAGE_AREA.store(((size.width.to_bits() as u64) << 32) | size.height.to_bits() as u64, std::sync::atomic::Ordering::Relaxed);
        node
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let (Event::Window(window::Event::RedrawRequested(now)), Some((_, start))) = (event, &self.shift) {
            let state = tree.state.downcast_mut::<ShiftState>();
            let step = state.frame(*start, *now);
            match state.first {
                Some(first) if step < SHIFT_STEPS => shell.request_redraw_at(RedrawRequest::At(first + SHIFT_STEP * (step as u32 + 1))),
                _ => shell.publish(self.done.clone()),
            }
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let Some((page, start)) = &self.shift else { return };
        let b = layout.bounds();
        let state = tree.state.downcast_ref::<ShiftState>();
        // Painted without window frames (the design tests), the step follows the switch time.
        let step = if state.start == Some(*start) {
            state.step
        } else {
            (Instant::now().saturating_duration_since(*start).as_millis() / SHIFT_STEP.as_millis()) as usize
        };
        if step >= SHIFT_STEPS {
            return;
        }
        // Any whole-pixel block size, sampled from the fixed small mosaic.
        let side = shift_step(step, 0.0).0.round().max(MOSAIC_CELL);
        let (cols, rows) = ((b.width / side).ceil() as usize, (b.height / side).ceil() as usize);
        // One cached geometry for the whole mosaic: the window then repaints a single region per
        // frame. Thousands of separate quads made it repaint the page dozens of times a frame.
        let mut shapes = Vec::new();
        for row in 0..rows {
            // Merge equal neighbours of one opacity band into one shape.
            let mut run: Option<(usize, [u8; 3], f32)> = None;
            for col in 0..=cols {
                let cell = (col < cols).then(|| {
                    let (u0, v0) = (col as f32 * side / b.width, row as f32 * side / b.height);
                    let (u1, v1) = (((col + 1) as f32 * side / b.width).min(1.0), ((row + 1) as f32 * side / b.height).min(1.0));
                    (sample(page, u0, v0, u1, v1).map(|v| v.round() as u8), shift_step(step, (u0 + u1) / 2.0).1)
                });
                let same = |a: [u8; 3], z: [u8; 3]| a.iter().zip(z).all(|(x, y)| x.abs_diff(y) <= 3);
                match (run, cell) {
                    (Some((_, c, a)), Some((next, alpha))) if a == alpha && same(c, next) => {}
                    (current, next) => {
                        if let Some((first, c, alpha)) = current.filter(|r| r.2 > 0.0) {
                            // A hair of overlap hides anti-aliased seams.
                            slant(
                                &mut shapes,
                                b.x + first as f32 * side,
                                b.y + row as f32 * side,
                                (col - first) as f32 * side + 0.6,
                                side + 0.6,
                                0.0,
                                Color { a: alpha, ..Color::from_rgb8(c[0], c[1], c[2]) },
                            );
                        }
                        run = next.map(|(c, alpha)| (col, c, alpha));
                    }
                }
            }
        }
        let cache = geometry(b, &shapes).cache(Group::unique(), None);
        renderer.draw_geometry(Geometry::load(&cache));
    }
}

/// The colour Windows keys out of a colour-keyed (layered) window: pixels of exactly this colour
/// are transparent and let clicks through. Pure black, because that is also what a window shows
/// where it has not painted yet: the new version's window, shown before its first frame (or with
/// a frame Windows dropped while it was fully transparent), stays see-through instead of flashing
/// a black rectangle. Nothing in the palette is pure black.
pub const KEY: Color = Color::from_rgb8(0, 0, 0);

/// What the update window's bar shows.
#[derive(Clone, Copy, PartialEq)]
pub enum BarStage {
    /// Empty: shown until the window is on screen, so every process draws the same frame.
    Waiting,
    /// A solid block of lit segments runs across from this moment; no fading trail.
    Running(Instant),
    Done,
    /// Installed; the new version is starting. The full green bar carries a sweeping gleam timed
    /// by the wall clock, so the watcher and the new UI draw the very same frame at hand-over.
    Launching,
    /// A known share (0..1) lit from the left, its last segment brightest.
    Filled(f32),
}
const GLEAM_MS: u128 = 1400;
/// Where the gleam is (0..1 over the bar, beyond it between sweeps), from the wall clock.
fn gleam() -> f32 {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    (ms % GLEAM_MS) as f32 / GLEAM_MS as f32 * 1.6 - 0.3
}

/// The update window's bar in the sliders' segment style.
pub fn run_bar<'a, Message: 'a>(stage: BarStage) -> Element<'a, Message> {
    Element::new(RunBar { stage, compact: false })
}
/// The setup card's bar: the same segments, smaller, as many as fit the width.
pub fn progress_bar<'a, Message: 'a>(stage: BarStage) -> Element<'a, Message> {
    Element::new(RunBar { stage, compact: true })
}
struct RunBar {
    stage: BarStage,
    compact: bool,
}
const BAR_SEGMENTS: usize = 24;
const BAR_BLOCK: usize = 5;
const BAR_STEP_MS: u64 = 30;
impl RunBar {
    /// Segment width and height.
    fn cell(&self) -> (f32, f32) {
        if self.compact { (9.0, 12.0) } else { (13.0, 20.0) }
    }
    fn segments(&self, width: f32) -> usize {
        let (_, h) = self.cell();
        if self.compact { ((width - h * SKEW) / 13.0).floor().max(8.0) as usize } else { BAR_SEGMENTS }
    }
    /// The running block's head (one past its last lit segment) over `n` segments.
    fn position(&self, now: Instant, n: usize) -> usize {
        match self.stage {
            BarStage::Running(since) => (now.saturating_duration_since(since).as_millis() as u64 / BAR_STEP_MS) as usize % (n + n * BAR_BLOCK / BAR_SEGMENTS),
            _ => 0,
        }
    }
}
impl<Message> Widget<Message, Theme, Renderer> for RunBar {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<Painted>() }
    fn state(&self) -> tree::State { tree::State::new(Painted::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fixed(self.cell().1) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fixed(self.cell().1))
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let (Event::Window(window::Event::RedrawRequested(now)), BarStage::Launching) = (event, self.stage) {
            shell.request_redraw_at(RedrawRequest::At(*now + Duration::from_millis(33)));
        }
        if let (Event::Window(window::Event::RedrawRequested(now)), BarStage::Running(since)) = (event, self.stage) {
            let step = Duration::from_millis(BAR_STEP_MS);
            let elapsed = now.saturating_duration_since(since);
            let next = since + step * (elapsed.as_millis() as u32 / BAR_STEP_MS as u32 + 1);
            shell.request_redraw_at(RedrawRequest::At(next));
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let (w, h) = self.cell();
        let n = self.segments(b.width);
        let block = n * BAR_BLOCK / BAR_SEGMENTS;
        let pos = self.position(Instant::now(), n);
        let mut shapes = Vec::with_capacity(n * 2);
        let green = |t: f32| {
            let (lo, hi) = ([36.0, 84.0, 52.0], [111.0, 217.0, 138.0]);
            let c = |i: usize| (lo[i] + (hi[i] - lo[i]) * t) / 255.0;
            Color::from_rgb(c(0), c(1), c(2))
        };
        for i in 0..n {
            let t = i as f32 / n as f32;
            let x = b.x + (h * SKEW / 2.0) + (b.width - w - h * SKEW) * i as f32 / (n - 1) as f32;
            let lit = match self.stage {
                BarStage::Running(_) => (i < pos && i + block >= pos).then(|| if i + 1 == pos { HEAD } else { lerp(t) }),
                BarStage::Done => Some(if i + 1 == n { Color::from_rgb8(0xD9, 0xFF, 0xE2) } else { green(t) }),
                BarStage::Launching => {
                    let d = ((i as f32 + 0.5) / n as f32 - gleam()).abs() / 0.12;
                    Some(brighten(green(0.35 + 0.65 * t), (1.0 - d).max(0.0) * 1.6))
                }
                BarStage::Filled(share) => {
                    let lit = (share.clamp(0.0, 1.0) * n as f32).ceil() as usize;
                    (i < lit).then(|| if i + 1 == lit && lit < n { HEAD } else { lerp(t) })
                }
                BarStage::Waiting => None,
            };
            match lit {
                Some(color) => segment(&mut shapes, x, b.y, w, h, color),
                None => {
                    segment(&mut shapes, x, b.y, w, h, OFF_EDGE);
                    segment(&mut shapes, x + 1.0, b.y + 1.0, w - 2.0, h - 2.0, OFF);
                }
            }
        }
        tree.state.downcast_ref::<Painted>().draw(renderer, b.expand(8.0), shapes);
    }
}

/// A window turning into another: the frame moves between two rectangles while its content is a
/// mosaic blending the two pictures; outside the frame is [`KEY`], see-through on a keyed window.
#[derive(Clone)]
pub struct Morph<Message> {
    pub from: std::sync::Arc<Mosaic>,
    pub to: std::sync::Arc<Mosaic>,
    /// Frame rectangles relative to the window's top-left.
    pub from_rect: Rectangle,
    pub to_rect: Rectangle,
    pub start: Instant,
    pub timeline: MorphTimeline,
    /// Sent once each when their time (ms) has passed; the last one ends the morph.
    pub events: Vec<(f32, Message)>,
}
/// Times in ms: the frame moves over `move_ms`, pixels grow to `BLOCK_MAX` over `pixelate`,
/// hold, and sharpen over `sharpen`; `blend` crosses the pictures; `fade_in`/`fade_out` blend
/// the mosaic with the real window underneath at both ends.
#[derive(Clone, Copy)]
pub struct MorphTimeline {
    pub fade_in: f32,
    pub move_ms: (f32, f32),
    pub pixelate: f32,
    pub sharpen: (f32, f32),
    pub blend: (f32, f32),
    pub fade_out: (f32, f32),
}
impl MorphTimeline {
    /// The app shrinking into the update window.
    pub const SHRINK: Self = Self { fade_in: 120.0, move_ms: (120.0, 570.0), pixelate: 320.0, sharpen: (570.0, 920.0), blend: (320.0, 500.0), fade_out: (800.0, 920.0) };
    /// The update window growing into the app.
    pub const GROW: Self = Self { fade_in: 100.0, move_ms: (0.0, 500.0), pixelate: 300.0, sharpen: (520.0, 950.0), blend: (230.0, 430.0), fade_out: (820.0, 950.0) };
}
#[derive(Default)]
struct MorphState {
    /// The morph being played: the layer's tree state outlives it, and a count carried over
    /// from the previous morph (the grow after an update) left the next shrink without events.
    start: Option<Instant>,
    fired: usize,
}
impl MorphState {
    /// Marks the morph's events due `ms` after its start as fired; returns the index of the
    /// first newly fired one (`fired` is past the last).
    fn due<M>(&mut self, m: &Morph<M>, ms: f32) -> usize {
        if self.start != Some(m.start) {
            *self = MorphState { start: Some(m.start), fired: 0 };
        }
        let first = self.fired;
        while m.events.get(self.fired).is_some_and(|(at, _)| ms >= *at) {
            self.fired += 1;
        }
        first
    }
}
/// «Обновление готово» celebrates once (canvas D + B + A): a slanted «НОВОЕ» tag drops into
/// the rail, bounces, wiggles and opens into the card, which assembles from a pixel mosaic;
/// the version rolls to the new one and flashes green, the button breathes with a ring, then
/// confetti bursts from the button over the whole window and a gleam crosses the button.
/// Milliseconds from the start.
pub mod ready {
    pub const DROP: (f32, f32) = (0.0, 420.0);
    pub const WIGGLE: (f32, f32) = (420.0, 650.0);
    pub const OPEN: (f32, f32) = (600.0, 760.0);
    pub const MOSAIC: (f32, f32) = (700.0, 1200.0);
    pub const ROLL: (f32, f32) = (1200.0, 1500.0);
    pub const FLASH: (f32, f32) = (1500.0, 2100.0);
    pub const RING: (f32, f32) = (1500.0, 1850.0);
    pub const CONFETTI: (f32, f32) = (1650.0, 3250.0);
    pub const GLEAM: (f32, f32) = (1850.0, 2350.0);
    pub const END: f32 = 3300.0;
    /// The card's fixed size (its mosaic maps onto it) and its button's height.
    pub const CARD: iced::Size = iced::Size::new(176.0, 124.0);
    pub const BUTTON_H: f32 = 30.0;
}
const FLASH_GREEN: Color = Color::from_rgb8(0x6F, 0xE1, 0x8B);
/// Where the card stood in the last frame: the window layer draws the tag and the confetti
/// around it, outside the card's own bounds (drawing there from the card would leave stale
/// pixels behind, tiny-skia repaints only a widget's damaged bounds).
static READY_CARD: std::sync::Mutex<Option<Rectangle>> = std::sync::Mutex::new(None);
fn ready_ms(start: Instant, now: Instant) -> f32 {
    now.saturating_duration_since(start).as_secs_f32() * 1000.0
}
fn ready_span(ms: f32, (a, b): (f32, f32)) -> f32 {
    ((ms - a) / (b - a)).clamp(0.0, 1.0)
}
fn smooth(x: f32) -> f32 {
    x * x * (3.0 - 2.0 * x)
}
fn bounce(mut x: f32) -> f32 {
    let (n, d) = (7.5625, 2.75);
    if x < 1.0 / d {
        n * x * x
    } else if x < 2.0 / d {
        x -= 1.5 / d;
        n * x * x + 0.75
    } else if x < 2.5 / d {
        x -= 2.25 / d;
        n * x * x + 0.9375
    } else {
        x -= 2.625 / d;
        n * x * x + 0.984375
    }
}
fn ready_button(card: Rectangle) -> Rectangle {
    Rectangle { x: card.x + 12.0, y: card.y + card.height - 12.0 - ready::BUTTON_H, width: card.width - 24.0, height: ready::BUTTON_H }
}

/// The card's own layer: hides it while the tag falls, then its mosaic dissolves in a wave
/// from the bottom left; later a ring around the button and a gleam across it.
pub fn ready_fx<'a, Message: 'a>(start: Option<Instant>, mosaic: Option<std::sync::Arc<Mosaic>>, cover: Color) -> Element<'a, Message> {
    Element::new(ReadyFx { start, mosaic, cover })
}
struct ReadyFx {
    start: Option<Instant>,
    mosaic: Option<std::sync::Arc<Mosaic>>,
    cover: Color,
}
impl<Message> Widget<Message, Theme, Renderer> for ReadyFx {
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let (Event::Window(window::Event::RedrawRequested(now)), Some(start)) = (event, self.start)
            && ready_ms(start, *now) < ready::END
        {
            shell.request_redraw_at(frame_after(*now));
        }
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        if let Ok(mut card) = READY_CARD.lock() {
            *card = Some(b);
        }
        let Some(start) = self.start else { return };
        let ms = ready_ms(start, Instant::now());
        if ms >= ready::END {
            return;
        }
        let mut shapes = Vec::new();
        if ms < ready::MOSAIC.0 {
            slant(&mut shapes, b.x - 1.0, b.y - 1.0, b.width + 2.0, b.height + 2.0, 0.0, self.cover);
        } else if ms < ready::MOSAIC.1
            && let Some(m) = &self.mosaic
        {
            let p = ready_span(ms, ready::MOSAIC);
            let side = 11.0;
            let (cols, rows) = ((b.width / side).ceil() as usize, (b.height / side).ceil() as usize);
            for r in 0..rows {
                for c in 0..cols {
                    let u = ((c as f32 + 0.5) / cols as f32 + (rows - 1 - r) as f32 / rows as f32) / 2.0;
                    let local = (p * 1.6 - u * 0.6).clamp(0.0, 1.0);
                    let alpha = ((1.0 - local) * 10.0).round() / 10.0;
                    let (u0, v0) = (c as f32 / cols as f32, r as f32 / rows as f32);
                    let (u1, v1) = ((c + 1) as f32 / cols as f32, (r + 1) as f32 / rows as f32);
                    let [red, green, blue] = sample(m, u0, v0, u1, v1).map(|v| v.round() as u8);
                    slant(&mut shapes, b.x + c as f32 * side, b.y + r as f32 * side, side + 0.6, side + 0.6, 0.0, Color { a: alpha, ..Color::from_rgb8(red, green, blue) });
                }
            }
        }
        let button = ready_button(b);
        renderer.with_layer(b, |renderer| {
            let mut frame = Frame::new(b);
            fill_shapes(&mut frame, &shapes);
            let ring = ready_span(ms, ready::RING);
            if ring > 0.0 && ring < 1.0 {
                let grow = 6.0 * (1.0 - (1.0 - ring).powi(3));
                let path = Path::rounded_rectangle(
                    Point::new(button.x - grow, button.y - grow),
                    Size::new(button.width + 2.0 * grow, button.height + 2.0 * grow),
                    (8.0 + grow).into(),
                );
                frame.stroke(&path, iced_tiny_skia::graphics::geometry::Stroke::default().with_width(2.0).with_color(Color { a: 0.9 * (1.0 - ring), ..TAG }));
            }
            renderer.draw_geometry(frame.into_geometry());
            let gleam = ready_span(ms, ready::GLEAM);
            if gleam > 0.0 && gleam < 1.0 {
                renderer.with_layer(button, |renderer| {
                    let x = button.x - 34.0 + (button.width + 48.0) * smooth(gleam);
                    let band = [Shape { x, y: button.y, w: 20.0, h: button.height, lean: 12.0, round: 0.0, color: Color { a: 0.55, ..HEAD } }];
                    renderer.draw_geometry(geometry(button, &band));
                });
            }
        });
    }
}

/// The window's layer over everything: the falling tag, then the confetti from the button,
/// flying high over the whole app. Draws nothing outside the celebration.
pub fn celebrate<'a, Message: 'a>(start: Option<Instant>) -> Element<'a, Message> {
    Element::new(Celebrate { start })
}
struct Celebrate {
    start: Option<Instant>,
}
impl<Message> Widget<Message, Theme, Renderer> for Celebrate {
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let (Event::Window(window::Event::RedrawRequested(now)), Some(start)) = (event, self.start)
            && ready_ms(start, *now) < ready::END
        {
            shell.request_redraw_at(frame_after(*now));
        }
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let Some(start) = self.start else { return };
        let Some(card) = READY_CARD.lock().ok().and_then(|c| *c) else { return };
        let ms = ready_ms(start, Instant::now());
        if ms >= ready::END {
            return;
        }
        let window = layout.bounds();
        let mut frame = Frame::new(window);
        if ms < ready::OPEN.1 {
            let wiggle = ready_span(ms, ready::WIGGLE);
            let open = ready_span(ms, ready::OPEN);
            let x = card.x + card.width / 2.0;
            let y = card.y + 26.0 - 170.0 * (1.0 - bounce(ready_span(ms, ready::DROP)));
            let degrees = 7.0 * (wiggle * 4.0 * std::f32::consts::PI).sin() * (1.0 - wiggle);
            frame.push_transform();
            frame.translate(iced::Vector::new(x, y));
            frame.rotate(degrees.to_radians());
            frame.scale(1.0 - 0.8 * open);
            fill_shapes(&mut frame, &[Shape { x: -32.0, y: -11.0, w: 58.0, h: 22.0, lean: 6.0, round: 0.0, color: Color { a: 1.0 - open, ..TAG } }]);
            frame.fill_text(iced_tiny_skia::graphics::geometry::Text {
                content: "НОВОЕ".into(),
                position: Point::ORIGIN,
                max_width: f32::INFINITY,
                color: Color { a: 1.0 - open, ..DARK },
                size: Pixels(11.0),
                line_height: text::LineHeight::default(),
                font: numbers(),
                align_x: text::Alignment::Center,
                align_y: iced::alignment::Vertical::Center,
                shaping: text::Shaping::Basic,
            });
            frame.pop_transform();
        }
        let age = ms - ready::CONFETTI.0;
        let life = ready::CONFETTI.1 - ready::CONFETTI.0;
        if age >= 0.0 && age < life {
            let button = ready_button(card);
            let (ox, oy) = (button.x + button.width / 2.0, button.y + button.height / 2.0);
            let palette = [TAG, HEAD, FLASH_GREEN, Color::from_rgb8(0xFF, 0xB0, 0x70), INK];
            let fade = if age > 0.65 * life { 1.0 - (age - 0.65 * life) / (0.35 * life) } else { 1.0 };
            // A burst the air slows down, then paper fluttering down: fast only for its first
            // moments, so it reads smooth (flying at up to 1.5 px/ms all the way, the pieces
            // jumped 25 px a frame). `settled`: how much of the burst's speed is gone, the sway
            // grows with it; `spent`: its travel so far, in ms at the starting speed.
            let settled = 1.0 - (-age / CONFETTI_DRAG).exp();
            let spent = CONFETTI_DRAG * settled;
            let mut pieces: [Vec<[Point; 4]>; 5] = Default::default();
            for i in 0..70u32 {
                let angle = -std::f32::consts::FRAC_PI_2 + (i as f32 / 69.0 - 0.5) * 1.9 + (grain(i, 1) - 0.5) * 0.25;
                let speed = 1.0 + 0.8 * grain(i, 2);
                let swing = (age / (240.0 + 160.0 * grain(i, 6)) + std::f32::consts::TAU * grain(i, 7)).sin();
                let x = ox + angle.cos() * speed * spent + (8.0 + 10.0 * grain(i, 5)) * swing * settled;
                let y = oy + angle.sin() * speed * spent + CONFETTI_FALL * (age - spent);
                let turn = (grain(i, 3) - 0.5) * 0.03 * spent + 0.5 * swing * settled;
                // The paper turning over: its width breathes.
                let flip = 0.3 + 0.7 * (age / (170.0 + 90.0 * grain(i, 8)) + std::f32::consts::TAU * grain(i, 9)).cos().abs();
                let (sin, cos) = turn.sin_cos();
                let corner = |cx: f32, cy: f32| Point::new(x + cx * flip * cos - cy * sin, y + cx * flip * sin + cy * cos);
                pieces[i as usize % pieces.len()].push([
                    corner(-3.0 + 6.0 * SKEW, -6.0),
                    corner(3.0 + 6.0 * SKEW, -6.0),
                    corner(3.0 - 6.0 * SKEW, 6.0),
                    corner(-3.0 - 6.0 * SKEW, 6.0),
                ]);
            }
            // One path per colour: a frame repaints the cloud as one patch, not dozens.
            for (color, pieces) in palette.into_iter().zip(&pieces) {
                let path = Path::new(|p| {
                    for [a, b, c, d] in pieces {
                        p.move_to(*a);
                        p.line_to(*b);
                        p.line_to(*c);
                        p.line_to(*d);
                        p.close();
                    }
                });
                frame.fill(&path, Color { a: fade, ..color });
            }
        }
        renderer.draw_geometry(frame.into_geometry());
    }
}
/// The confetti's air: its burst slows with this time constant (ms), then it drifts down at
/// this speed (px/ms).
const CONFETTI_DRAG: f32 = 380.0;
const CONFETTI_FALL: f32 = 0.11;

/// «Перезапустить» clicked: the button sinks into its slot and falls out one of four ways, then
/// air from all over the window rushes into the hole it left while the restart is prepared. The
/// window itself never moves: its own morph into the update window follows. Milliseconds from
/// the click.
pub mod restart {
    use std::time::Duration;
    pub const PRESS: f32 = 170.0;
    pub const DROP: (f32, f32) = (170.0, 890.0);
    /// The air speeds up over this span, then blows steadily.
    pub const WIND: (f32, f32) = (820.0, 1300.0);
    /// The restart waits at least this long after the click: the button has fallen out and the
    /// air rushes in before the window morphs.
    pub const HOLD: Duration = Duration::from_millis(1600);
}
/// How the button leaves its slot: sinks turning like into a drain, is knocked out and falls
/// off the window, swings in like a hatch, or cracks into slanted segments that drop one by one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fall {
    Sink,
    KnockOut,
    Hatch,
    Crack,
}
impl Fall {
    pub const ALL: [Fall; 4] = [Fall::Sink, Fall::KnockOut, Fall::Hatch, Fall::Crack];
    /// A different one per restart.
    pub fn random() -> Self {
        let micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_micros());
        Self::ALL[micros as usize % Self::ALL.len()]
    }
}
/// A running restart: the click, how the button falls, and the preparation's step (0 newest
/// version, 1 rollback copy, 2 stopping the sound) with when it began.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Restart {
    pub start: Instant,
    pub fall: Fall,
    pub step: u8,
    pub step_at: Instant,
}
impl Restart {
    pub fn new(now: Instant, fall: Fall) -> Self {
        Self { start: now, fall, step: 0, step_at: now }
    }
    pub fn advance(&mut self, step: u8, now: Instant) {
        if step > self.step {
            (self.step, self.step_at) = (step, now);
        }
    }
    fn ms(&self, now: Instant) -> f32 {
        ready_ms(self.start, now)
    }
    /// The share the hole's floor shows: each step fills its third, slowing as it goes.
    fn progress(&self, now: Instant) -> f32 {
        let since = now.saturating_duration_since(self.step_at).as_secs_f32();
        (self.step.min(2) as f32 + 1.0 - (-since / 1.2).exp()) / 3.0
    }
    /// The air's strength: none until the button is out, then more with each step.
    fn wind(&self, now: Instant) -> f32 {
        let level = |step: u8| [0.7, 0.85, 1.0][step.min(2) as usize];
        let since = now.saturating_duration_since(self.step_at).as_secs_f32() * 1000.0;
        let from = level(self.step.saturating_sub(1));
        (from + (level(self.step) - from) * smooth((since / 400.0).min(1.0))) * smooth(ready_span(self.ms(now), restart::WIND))
    }
    /// The air's own clock: milliseconds at its speed, a third at first and full once it blows.
    fn tau(&self, now: Instant) -> f32 {
        let (a, b) = restart::WIND;
        let t = (self.ms(now) - a).max(0.0);
        let x = t / (b - a);
        // The integral of the smoothstep speed-up, then a steady 1.
        let ramp = if x < 1.0 { x * x * x - x * x * x * x / 2.0 } else { x - 0.5 };
        0.35 * t + 0.65 * (b - a) * ramp
    }
}
const PRESSED: Color = Color::from_rgb8(0xE3, 0x81, 0x3E);
const BUTTON_LABEL: Color = Color::from_rgb8(0x3A, 0x22, 0x0E);
fn mix(a: Color, b: Color, k: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * k, g: a.g + (b.g - a.g) * k, b: a.b + (b.b - a.b) * k, a: a.a + (b.a - a.a) * k }
}
/// Where the slot stood in the last frame: the window layer aims the air at it and drops the
/// button from it.
static RESTART_SLOT: std::sync::Mutex<Option<Rectangle>> = std::sync::Mutex::new(None);
/// The restart button's face around `center`, turned and scaled, with its label.
fn restart_button(frame: &mut Frame, center: Point, size: Size, degrees: f32, scale: f32, face: Color, label: f32) {
    frame.push_transform();
    frame.translate(iced::Vector::new(center.x, center.y));
    frame.rotate(degrees.to_radians());
    frame.scale(scale);
    frame.fill(&Path::rounded_rectangle(Point::new(-size.width / 2.0, -size.height / 2.0), size, 8.0.into()), face);
    if label > 0.0 {
        restart_label(frame, Point::ORIGIN, label * face.a);
    }
    frame.pop_transform();
}
fn restart_label(frame: &mut Frame, at: Point, alpha: f32) {
    frame.fill_text(iced_tiny_skia::graphics::geometry::Text {
        content: "Перезапустить".into(),
        position: at,
        max_width: f32::INFINITY,
        color: Color { a: alpha, ..BUTTON_LABEL },
        size: Pixels(13.0),
        line_height: text::LineHeight::default(),
        font: Font::with_name("Segoe UI"),
        align_x: text::Alignment::Center,
        align_y: iced::alignment::Vertical::Center,
        shaping: text::Shaping::Basic,
    });
}

/// The restart button's slot in the card: the pressed button, then the dark hole it leaves,
/// hot at the rim for a moment, with the preparation's progress glowing on its floor.
pub fn restart_slot<'a, Message: 'a>(restart: Restart) -> Element<'a, Message> {
    Element::new(RestartSlot(restart))
}
struct RestartSlot(Restart);
impl<Message> Widget<Message, Theme, Renderer> for RestartSlot {
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fixed(ready::BUTTON_H) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, ready::BUTTON_H)
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            shell.request_redraw_at(frame_after(*now));
        }
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        if let Ok(mut slot) = RESTART_SLOT.lock() {
            *slot = Some(b);
        }
        let now = Instant::now();
        let r = self.0;
        let ms = r.ms(now);
        let drop = ready_span(ms, restart::DROP);
        let mut frame = Frame::new(b.expand(6.0));
        let hole = Path::rounded_rectangle(b.position(), b.size(), 8.0.into());
        // Black under its top edge, a little lighter at the floor, lit at the lower lip.
        frame.fill(&hole, gradient::Linear::new(Point::new(b.x, b.y), Point::new(b.x, b.y + b.height)).add_stop(0.0, Color::BLACK).add_stop(1.0, Color::from_rgb8(0x12, 0x12, 0x14)));
        frame.fill_rectangle(Point::new(b.x + 8.0, b.y + b.height + 0.5), Size::new(b.width - 16.0, 1.0), Color { a: 0.08, ..Color::WHITE });
        let heat = if ms < restart::DROP.1 {
            smooth(((drop - 0.18) / 0.25).clamp(0.0, 1.0))
        } else {
            let s = (ms - restart::DROP.1) / 1000.0;
            0.25 + 0.75 * (-s * 2.5).exp() + 0.06 * (ms / 60.0).sin()
        };
        if heat > 0.01 {
            let rim = Path::rounded_rectangle(Point::new(b.x - 1.5, b.y - 1.5), Size::new(b.width + 3.0, b.height + 3.0), 9.5.into());
            frame.stroke(&rim, Stroke::default().with_width(4.0).with_color(Color { a: 0.2 * heat, ..TAG }));
            frame.stroke(&hole, Stroke::default().with_width(1.0).with_color(Color { a: heat, ..TAG }));
        }
        if ms >= restart::DROP.0 {
            let w = (b.width - 14.0) * r.progress(now);
            frame.fill_rectangle(Point::new(b.x + 7.0, b.y + b.height - 7.0), Size::new(w, 6.0), Color { a: 0.18, ..TAG });
            frame.fill_rectangle(Point::new(b.x + 7.0, b.y + b.height - 5.0), Size::new(w, 2.0), TAG);
        }
        let center = Point::new(b.center_x(), b.center_y());
        if ms < restart::PRESS {
            // Pressed in: a dark gap shows around it.
            let press = smooth((ms / restart::PRESS * 2.0).min(1.0));
            let face = mix(Color::from_rgb8(0xFF, 0xB0, 0x70), PRESSED, press);
            restart_button(&mut frame, Point::new(center.x, center.y + 1.5 * press), b.size(), 0.0, 1.0 - 0.035 * press, face, 1.0);
        } else if ms < restart::DROP.1 {
            match r.fall {
                Fall::Sink => {
                    let q = smooth(drop);
                    let face = Color { a: 1.0 - ((drop - 0.78) / 0.22).clamp(0.0, 1.0), ..mix(PRESSED, Color::from_rgb8(40, 26, 14), q) };
                    restart_button(&mut frame, Point::new(center.x, center.y + 1.5 + 3.0 * q), b.size(), -32.0 * q, 0.965 * (1.0 - 0.82 * q), face, 1.0 - q);
                }
                Fall::Hatch => {
                    // Hinged at the bottom edge, its top falls away from the viewer.
                    let q = drop.powf(1.7);
                    let angle = (96.0 * q).to_radians();
                    if angle < 88f32.to_radians() {
                        let (h, w) = (b.height - 1.0, b.width * 0.965);
                        let base = b.y + b.height;
                        let top = base - h * angle.cos();
                        let narrow = 220.0 / (220.0 + h * angle.sin());
                        let quad = Path::new(|p| {
                            p.move_to(Point::new(center.x - w / 2.0, base));
                            p.line_to(Point::new(center.x + w / 2.0, base));
                            p.line_to(Point::new(center.x + narrow * w / 2.0, top));
                            p.line_to(Point::new(center.x - narrow * w / 2.0, top));
                            p.close();
                        });
                        frame.fill(&quad, mix(PRESSED, Color::from_rgb8(70, 42, 22), q));
                        if q < 0.5 {
                            restart_label(&mut frame, Point::new(center.x, (top + base) / 2.0), 1.0 - 2.0 * q);
                        }
                    }
                }
                // These leave the slot: the window layer draws them.
                Fall::KnockOut | Fall::Crack => {}
            }
        }
        renderer.draw_geometry(frame.into_geometry());
    }
}

/// The window's layer for the restart: the button falling off the window or crumbling into
/// segments, and the air: streaks and specks from all over the window rushing into the hole.
/// Draws nothing else, and never moves the window's own content.
pub fn restart_wind<'a, Message: 'a>(restart: Option<Restart>) -> Element<'a, Message> {
    Element::new(RestartWind(restart))
}
struct RestartWind(Option<Restart>);
impl<Message> Widget<Message, Theme, Renderer> for RestartWind {
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let (Event::Window(window::Event::RedrawRequested(now)), Some(_)) = (event, self.0) {
            shell.request_redraw_at(frame_after(*now));
        }
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let Some(r) = self.0 else { return };
        let Some(slot) = RESTART_SLOT.lock().ok().and_then(|s| *s) else { return };
        let now = Instant::now();
        let ms = r.ms(now);
        let window = layout.bounds();
        let mut frame = Frame::new(window);
        if ms >= restart::DROP.0 && ms < restart::DROP.1 {
            let lt = ms - restart::DROP.0;
            match r.fall {
                Fall::KnockOut => {
                    // Pops out towards the viewer, tumbles and falls off the window.
                    let up = if lt < 150.0 { smooth(lt / 150.0) } else { 1.0 };
                    let fall = (lt - 150.0).max(0.0);
                    let center = Point::new(slot.center_x() + 0.022 * lt, slot.center_y() - 11.0 * up + 0.00042 * fall * fall);
                    let degrees = -5.0 * up + 0.085 * fall;
                    let scale = 1.0 + 0.05 * up;
                    restart_button(&mut frame, Point::new(center.x, center.y + 4.0 + 6.0 * up), slot.size(), degrees, scale, Color { a: 0.35, ..Color::BLACK }, 0.0);
                    restart_button(&mut frame, center, slot.size(), degrees, scale, TAG, 1.0);
                }
                Fall::Crack => {
                    // Seven slanted segments, like a slider's, drop one after another.
                    let crack = smooth((lt / 60.0).min(1.0));
                    let (piece, lean) = (25.0 - 6.0, 6.0);
                    let step = (slot.width + 2.0 - 25.0) / 6.0;
                    for i in 0..7u32 {
                        let k = i as f32 - 3.0;
                        let local = (lt - 60.0 - i as f32 * 50.0).max(0.0);
                        let y = 1.5 + 0.00062 * local * local * (0.85 + 0.3 * grain(i, 3)) - 2.0 * crack * (i % 2) as f32;
                        let x = k * 0.02 * local + k * 0.6 * crack;
                        let degrees = if i % 2 == 1 { 1.0 } else { -1.0 } * (8.0 + grain(i, 4) * 22.0) * local / 300.0;
                        let left = slot.x - 2.0 + i as f32 * step;
                        frame.push_transform();
                        frame.translate(iced::Vector::new(left + 12.5 + x, slot.center_y() + y));
                        frame.rotate(degrees.to_radians());
                        fill_shapes(&mut frame, &[Shape { x: -12.5, y: -slot.height / 2.0, w: piece, h: slot.height, lean, round: 0.0, color: lerp(i as f32 / 6.0) }]);
                        frame.pop_transform();
                    }
                    if crack < 1.0 {
                        restart_label(&mut frame, Point::new(slot.center_x(), slot.center_y() + 1.5), 1.0 - crack);
                    }
                }
                Fall::Sink | Fall::Hatch => {}
            }
        }
        let strength = r.wind(now);
        if strength > 0.001 {
            restart_air(&mut frame, window, slot, r.tau(now), strength);
        }
        renderer.draw_geometry(frame.into_geometry());
    }
}

/// Gusts rushing into the hole: each one a few parallel streaks and a speck or two of the
/// interface, starting somewhere in the window and speeding up towards a point along the slot.
/// Few gusts, each one path: tiny-skia pays for every repainted patch (it rebuilds a window-sized
/// clip mask per layer and per patch), so a frame repaints a handful of patches; streaks
/// scattered one by one cost 70 ms a frame, a dozen thin gusts still 19 ms.
fn restart_air(frame: &mut Frame, window: Rectangle, slot: Rectangle, tau: f32, strength: f32) {
    const GUSTS: u32 = 6;
    let (cx, cy) = (slot.center_x(), slot.center_y());
    let spawn = |seed: u32| {
        for k in 0..8 {
            let x = window.x + 6.0 + grain(seed, 20 + k) * (window.width - 12.0);
            let y = window.y + 4.0 + grain(seed, 30 + k) * (window.height - 8.0);
            if (x - cx).hypot((y - cy) * 1.6) > 90.0 {
                return Point::new(x, y);
            }
        }
        Point::new(window.x + window.width - 30.0, window.y + 60.0)
    };
    let specks = [TAG, INK, Color::from_rgb8(163, 163, 169), Color::from_rgb8(94, 95, 102)];
    let active = GUSTS as f32 * (strength + 0.25).min(1.0);
    for i in 0..GUSTS {
        let gate = (active - i as f32).clamp(0.0, 1.0);
        if gate <= 0.0 {
            break;
        }
        let q = tau / (900.0 * (0.8 + 0.4 * grain(i, 3))) + i as f32 / GUSTS as f32 + 0.1 * grain(i, 9);
        let (round, u) = (q.floor(), q.fract());
        if round < 1.0 {
            continue; // not blown yet: every gust starts from its own spot, not mid-flight
        }
        let seed = i.wrapping_mul(977).wrapping_add((round as u32).wrapping_mul(131));
        let from = spawn(seed);
        let to = Point::new(cx + (grain(seed, 40) - 0.5) * (slot.width - 28.0), cy + (grain(seed, 41) - 0.5) * slot.height * 0.4);
        let (r0, a0) = ((from.x - to.x).hypot(from.y - to.y), (from.y - to.y).atan2(from.x - to.x));
        let bend = (grain(seed, 42) - 0.5) * 0.3;
        let at = |u: f32| {
            let (r, a) = (r0 * (1.0 - u).sqrt(), a0 + bend * u);
            Point::new(to.x + r * a.cos(), to.y + r * a.sin())
        };
        let fade = (u / 0.14).clamp(0.0, 1.0) * ((1.0 - u) / 0.1).clamp(0.0, 1.0) * gate;
        let (head, back) = (at(u), at((u - 0.045).max(0.0)));
        let (dx, dy) = (head.x - back.x, head.y - back.y);
        let moved = dx.hypot(dy);
        if moved < 0.01 {
            continue;
        }
        // Along and across the gust; its lanes close in as it nears the slot.
        let (ux, uy) = (dx / moved, dy / moved);
        let (nx, ny) = (-uy, ux);
        let spread = (1.0 - u).sqrt();
        let length = (moved * 3.0).clamp(10.0, 64.0);
        let lanes = 5 + (grain(seed, 46) * 4.0) as u32;
        let streaks = Path::new(|p| {
            for j in 0..lanes {
                let off = ((j as f32 - (lanes - 1) as f32 / 2.0) * 7.0 + (grain(seed, 50 + j) - 0.5) * 5.0) * spread;
                let lag = grain(seed, 60 + j) * 0.35 * length;
                let h = Point::new(head.x + nx * off - ux * lag, head.y + ny * off - uy * lag);
                let half = (1.1 + 0.7 * grain(seed, 80 + j)) / 2.0;
                let reach = length * (0.6 + 0.5 * grain(seed, 70 + j));
                p.move_to(Point::new(h.x + nx * half, h.y + ny * half));
                p.line_to(Point::new(h.x - nx * half, h.y - ny * half));
                p.line_to(Point::new(h.x - ux * reach, h.y - uy * reach));
                p.close();
            }
        });
        let color = if i % 3 == 0 { TAG } else if i % 7 == 0 { HEAD } else { INK };
        let alpha = (0.3 + 0.6 * u) * fade;
        let tail = Point::new(head.x - ux * length * 1.3, head.y - uy * length * 1.3);
        frame.fill(&streaks, gradient::Linear::new(head, tail).add_stop(0.0, Color { a: alpha, ..color }).add_stop(1.0, Color { a: 0.0, ..color }));
        // Specks of the interface carried along, a little behind.
        for j in 0..2 + (grain(seed, 90) * 3.0) as u32 {
            let at_speck = at((u - 0.03 - 0.05 * grain(seed, 91 + j)).max(0.0));
            let off = (grain(seed, 95 + j) - 0.5) * 7.0 * lanes as f32 * spread;
            let side = 2.0 + (grain(seed, 44 + j) * 3.0).floor();
            let spin = if (i + j) % 2 == 1 { 1.0 } else { -1.0 } * u * 600.0;
            frame.push_transform();
            frame.translate(iced::Vector::new(at_speck.x + nx * off, at_speck.y + ny * off));
            frame.rotate(spin.to_radians());
            frame.fill_rectangle(Point::new(-side / 2.0, -side / 2.0), Size::new(side, side), Color { a: 0.9 * fade, ..specks[((i + j) % 4) as usize] });
            frame.pop_transform();
        }
    }
}

/// A version number whose differing tail rolls from the old one to the new one, then flashes
/// green; without a start it simply shows the new one.
pub fn roll<'a, Message: 'a>(from: &str, to: &str, start: Option<Instant>, size: f32, color: Color) -> Element<'a, Message> {
    Element::new(Roll { from: from.to_owned(), to: to.to_owned(), start, size, color })
}
struct Roll {
    from: String,
    to: String,
    start: Option<Instant>,
    size: f32,
    color: Color,
}
impl Roll {
    fn parts(&self) -> (String, String, String) {
        let common = self.from.chars().zip(self.to.chars()).take_while(|(a, b)| a == b).count();
        let prefix: String = self.to.chars().take(common).collect();
        (prefix, self.from.chars().skip(common).collect(), self.to.chars().skip(common).collect())
    }
    fn text(&self, content: String) -> Text<String, Font> {
        Text { align_y: iced::alignment::Vertical::Top, ..label(content, self.size, Size::INFINITE, text::Alignment::Left) }
    }
    fn width(&self) -> f32 {
        text_width(&self.to, self.size, numbers()).max(text_width(&self.from, self.size, numbers())) + 2.0
    }
}
impl<Message> Widget<Message, Theme, Renderer> for Roll {
    fn size(&self) -> Size<Length> {
        Size { width: Length::Shrink, height: Length::Shrink }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Shrink, Length::Shrink, Size::new(self.width(), (self.size * 1.3).ceil())))
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        if let (Event::Window(window::Event::RedrawRequested(now)), Some(start)) = (event, self.start)
            && ready_ms(start, *now) < ready::FLASH.1
        {
            shell.request_redraw_at(frame_after(*now));
        }
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let ms = self.start.map_or(f32::MAX, |start| ready_ms(start, Instant::now()));
        let (prefix, old, new) = self.parts();
        let roll = smooth(ready_span(ms, ready::ROLL));
        let flash = ready_span(ms, ready::FLASH);
        let fresh = if flash > 0.0 && flash < 1.0 {
            let k = flash * flash;
            Color { r: FLASH_GREEN.r + (self.color.r - FLASH_GREEN.r) * k, g: FLASH_GREEN.g + (self.color.g - FLASH_GREEN.g) * k, b: FLASH_GREEN.b + (self.color.b - FLASH_GREEN.b) * k, a: self.color.a }
        } else {
            self.color
        };
        let x = b.x + if prefix.is_empty() { 0.0 } else { text_width(&prefix, self.size, numbers()) };
        put(renderer, self.text(prefix), Point::new(b.x, b.y), self.color, b);
        if roll <= 0.0 || roll >= 1.0 {
            let tail = if roll >= 1.0 { new } else { old };
            put(renderer, self.text(tail), Point::new(x, b.y), fresh, b);
            return;
        }
        // While rolling, the tails are glyph outlines: a layer clips geometry to the line, but
        // not ordinary text.
        renderer.with_layer(b, |renderer| {
            let mut frame = Frame::new(b);
            for (content, y, color) in [(old, b.y - roll * b.height, self.color), (new, b.y + (1.0 - roll) * b.height, fresh)] {
                frame.fill_text(iced_tiny_skia::graphics::geometry::Text {
                    content,
                    position: Point::new(x, y),
                    max_width: f32::INFINITY,
                    color,
                    size: Pixels(self.size),
                    line_height: text::LineHeight::default(),
                    font: numbers(),
                    align_x: text::Alignment::Left,
                    align_y: iced::alignment::Vertical::Top,
                    shaping: text::Shaping::Basic,
                });
            }
            renderer.draw_geometry(frame.into_geometry());
        });
    }
}

/// The strength tune's sweep in the sliders' segments: a slanted bar per swept strength, as
/// tall as the noise left after the denoiser (−100…−30 dB). Bars at or under the silence line
/// (dashed) are green, the chosen one bright; the strength being measured now blinks.
pub fn tune_curve<'a, Message: 'a>(points: Vec<(u8, f32)>, steps: &'static [u8], chosen: Option<u8>, current: Option<u8>, silent_db: f32) -> Element<'a, Message> {
    Element::new(TuneCurve { points, steps, chosen, current, silent_db })
}
struct TuneCurve {
    points: Vec<(u8, f32)>,
    steps: &'static [u8],
    chosen: Option<u8>,
    current: Option<u8>,
    silent_db: f32,
}
const CURVE_H: f32 = 62.0;
impl<Message> Widget<Message, Theme, Renderer> for TuneCurve {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<Painted>() }
    fn state(&self) -> tree::State { tree::State::new(Painted::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fixed(CURVE_H) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, CURVE_H)
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let (top, bottom) = (b.y + 2.0, b.y + b.height - 15.0);
        let tall = |db: f32| ((db + 100.0) / 70.0).clamp(0.06, 1.0) * (bottom - top);
        let slot = b.width / self.steps.len() as f32;
        let w = (slot * 0.5).min(13.0);
        // The tick clock blinks the bar being measured (the UI redraws every tick meanwhile).
        let blink = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()) / 190).is_multiple_of(2);
        let mut shapes = Vec::with_capacity(self.steps.len() * 3 + 60);
        let line = bottom - tall(self.silent_db);
        let mut x = b.x;
        while x < b.x + b.width {
            slant(&mut shapes, x, line, 4.0, 1.0, 0.0, Color { a: 0.55, ..FLASH_GREEN });
            x += 8.0;
        }
        for (i, &strength) in self.steps.iter().enumerate() {
            let x = b.x + slot * (i as f32 + 0.5) - w / 2.0 + (bottom - top) * SKEW / 2.0;
            segment(&mut shapes, x, top, w, bottom - top, OFF_EDGE);
            segment(&mut shapes, x + 1.0, top + 1.0, w - 2.0, bottom - top - 2.0, OFF);
            if let Some(&(_, residual)) = self.points.iter().find(|p| p.0 == strength) {
                let h = tall(residual);
                let color = if self.chosen == Some(strength) {
                    HEAD
                } else if residual <= self.silent_db {
                    Color { a: 0.85, ..FLASH_GREEN }
                } else {
                    lerp(0.35 + 0.65 * i as f32 / self.steps.len() as f32)
                };
                segment(&mut shapes, x + (bottom - top - h) * SKEW, bottom - h, w, h, color);
            } else if self.current == Some(strength) && blink {
                segment(&mut shapes, x, top, w, bottom - top, Color { a: 0.35, ..HEAD });
            }
        }
        tree.state.downcast_ref::<Painted>().draw(renderer, b.expand(4.0), shapes);
        for (i, &strength) in self.steps.iter().enumerate() {
            let at = Point::new(b.x + slot * (i as f32 + 0.5), bottom + 3.0);
            let lit = self.chosen == Some(strength) || self.current == Some(strength);
            let text = Text { align_x: text::Alignment::Center, align_y: iced::alignment::Vertical::Top, ..label(strength.to_string(), 10.0, Size::INFINITE, text::Alignment::Center) };
            put(renderer, text, at, if lit { INK } else { Color::from_rgb8(0x85, 0x86, 0x8D) }, b.expand(2.0));
        }
    }
}

/// While the pointer rests on «by ARKANOID» the whole window glitches: bands of its own picture
/// (a mosaic painted when the hover began) slip sideways with colour fringes, and stray blocks
/// and scanlines flicker. A new pattern every 70 ms; nothing is drawn otherwise.
///
/// Cheap by construction: each band, block and scanline is its own cached piece of geometry,
/// rebuilt only when the pattern changes. Between changes nothing is repainted, and a change
/// repaints only the pieces' own rectangles, never the whole window (a full-window layer redrawn
/// at every frame of the signature's glow is what made the first version lag).
pub fn glitch<'a, Message: 'a>(start: Option<Instant>, mosaic: Option<std::sync::Arc<Mosaic>>) -> Element<'a, Message> {
    Element::new(Glitch { start, mosaic })
}
struct Glitch {
    start: Option<Instant>,
    mosaic: Option<std::sync::Arc<Mosaic>>,
}
/// The pieces on screen, each with the epoch it was built for.
#[derive(Default)]
struct GlitchState(RefCell<Vec<(u32, Cache)>>);
const GLITCH_TICK: Duration = Duration::from_millis(70);
const GLITCH_CYAN: Color = Color::from_rgb8(0x56, 0xE0, 0xFF);
/// The window's size as last drawn (the glitch layer always spans it), for the picture the
/// glitch slices: painted at another size, its bands would not line up with the window.
static WINDOW_AREA: std::sync::Mutex<Option<Size>> = std::sync::Mutex::new(None);
pub fn window_area() -> Option<Size> {
    WINDOW_AREA.lock().ok().and_then(|area| *area)
}
fn glitch_tick(start: Instant, now: Instant) -> u32 {
    (now.saturating_duration_since(start).as_millis() / GLITCH_TICK.as_millis()) as u32 + 1
}
/// Pieces of the glitch: full-width bands of the window's picture, stray blocks, scanlines.
const GLITCH_BANDS: u32 = 6;
const GLITCH_BLOCKS: u32 = 14;
const GLITCH_PIECES: u32 = GLITCH_BANDS + GLITCH_BLOCKS + 2;
/// Each piece keeps its look for 2 to 5 patterns, staggered: a new pattern changes only a few
/// pieces, so it repaints a few strips and blocks, never most of the window.
fn glitch_epoch(piece: u32, tick: u32) -> u32 {
    let life = 2 + piece % 4;
    (tick + piece * 3) / life
}
/// Piece `k` in its `epoch` over `b`: its rectangle and shapes (none while it rests).
fn glitch_piece(b: Rectangle, m: &Mosaic, k: u32, epoch: u32) -> (Rectangle, Vec<Shape>) {
    let rnd = |j: u32| grain(k * 97 + j, epoch);
    let (cell, row_h) = (b.width / m.width as f32, b.height / m.height as f32);
    let rgb = |c: [u8; 3]| Color::from_rgb8(c[0], c[1], c[2]);
    let mut shapes = Vec::new();
    if k < GLITCH_BANDS {
        // A band of the window slipping sideways, sometimes with red/cyan fringes.
        let y0 = b.y + rnd(1) * (b.height - 4.0);
        let h = (3.0 + rnd(2) * 17.0).min(b.y + b.height - y0);
        let rect = Rectangle { x: b.x, y: y0, width: b.width, height: h };
        if rnd(0) < 0.3 {
            return (rect, shapes);
        }
        let dx = (rnd(3) - 0.5) * 140.0;
        let close = |a: [u8; 3], z: [u8; 3]| a.iter().zip(z).all(|(x, y)| x.abs_diff(y) <= 6);
        let r0 = (((y0 - b.y) / row_h) as usize).min(m.height);
        let r1 = (((y0 + h - b.y) / row_h).ceil() as usize).min(m.height);
        for r in r0..r1 {
            let y = b.y + r as f32 * row_h;
            let mut run: Option<(usize, [u8; 3])> = None;
            for c in 0..=m.width {
                let color = (c < m.width).then(|| m.cells[r * m.width + c]);
                match (run, color) {
                    (Some((_, current)), Some(next)) if close(current, next) => {}
                    (current, next) => {
                        if let Some((first, color)) = current {
                            slant(&mut shapes, b.x + first as f32 * cell + dx, y, (c - first) as f32 * cell + 0.5, row_h + 0.5, 0.0, rgb(color));
                        }
                        run = next.map(|n| (c, n));
                    }
                }
            }
        }
        if rnd(4) > 0.45 {
            slant(&mut shapes, b.x + dx - 6.0, y0, b.width, h, 0.0, Color { a: 0.28, ..GLITCH_CYAN });
            slant(&mut shapes, b.x + dx + 6.0, y0, b.width, h, 0.0, Color { a: 0.22, ..HOT });
        }
        (rect, shapes)
    } else if k < GLITCH_BANDS + GLITCH_BLOCKS {
        // A stray block: an accent, or a piece of the picture from elsewhere.
        let side = 5.0 + rnd(1) * 18.0;
        let h = side * (0.4 + rnd(2));
        let (x, y) = (b.x + rnd(3) * (b.width - side), b.y + rnd(4) * (b.height - h));
        let pick = rnd(5);
        let color = if pick < 0.3 {
            TAG
        } else if pick < 0.5 {
            GLITCH_CYAN
        } else if pick < 0.6 {
            INK
        } else {
            let (u, v) = (rnd(6), rnd(7));
            rgb(m.cells[((v * m.height as f32) as usize).min(m.height - 1) * m.width + ((u * m.width as f32) as usize).min(m.width - 1)])
        };
        slant(&mut shapes, x, y, side, h, 0.0, Color { a: 0.55 + 0.4 * rnd(8), ..color });
        (Rectangle { x, y, width: side, height: h }, shapes)
    } else {
        let y = b.y + rnd(1) * (b.height - 1.0);
        slant(&mut shapes, b.x, y, b.width, 1.0, 0.0, Color { a: 0.14, ..INK });
        (Rectangle { x: b.x, y, width: b.width, height: 1.0 }, shapes)
    }
}
impl<Message> Widget<Message, Theme, Renderer> for Glitch {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<GlitchState>() }
    fn state(&self) -> tree::State { tree::State::new(GlitchState::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }
    fn update(&mut self, _: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        // One frame per pattern, on the pattern's own clock.
        if let (Event::Window(window::Event::RedrawRequested(now)), Some(start)) = (event, self.start) {
            shell.request_redraw_at(RedrawRequest::At(start + GLITCH_TICK * glitch_tick(start, *now)));
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        if let Ok(mut area) = WINDOW_AREA.lock() {
            *area = Some(b.size());
        }
        let state = tree.state.downcast_ref::<GlitchState>();
        let mut pieces = state.0.borrow_mut();
        let (Some(start), Some(m)) = (self.start, &self.mosaic) else {
            pieces.clear();
            return;
        };
        let tick = glitch_tick(start, Instant::now());
        // Only the pieces whose epoch moved on are rebuilt; the rest keep their cache.
        for k in 0..GLITCH_PIECES {
            let epoch = glitch_epoch(k, tick);
            let slot = k as usize;
            if pieces.get(slot).is_none_or(|(built, _)| *built != epoch) {
                let (rect, shapes) = glitch_piece(b, m, k, epoch);
                let cache = geometry(rect, &shapes).cache(Group::unique(), None);
                if slot < pieces.len() {
                    pieces[slot] = (epoch, cache);
                } else {
                    pieces.push((epoch, cache));
                }
            }
        }
        for (_, cache) in pieces.iter() {
            renderer.draw_geometry(Geometry::load(cache));
        }
    }
}

/// Frames drawn by the window (the morph layer is always in it): the update intro waits for
/// the card to be drawn before its window becomes visible.
static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn frames() -> u64 {
    FRAMES.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn morph<'a, Message: Clone + 'a>(morph: Option<&Morph<Message>>) -> Element<'a, Message> {
    Element::new(MorphWidget { morph: morph.cloned() })
}
struct MorphWidget<Message> {
    morph: Option<Morph<Message>>,
}
/// Average colour of the part of `m` under the normalised rectangle `[u0, u1) × [v0, v1)`.
fn sample(m: &Mosaic, u0: f32, v0: f32, u1: f32, v1: f32) -> [f32; 3] {
    let x0 = ((u0 * m.width as f32) as usize).min(m.width - 1);
    let y0 = ((v0 * m.height as f32) as usize).min(m.height - 1);
    let x1 = ((u1 * m.width as f32).ceil() as usize).clamp(x0 + 1, m.width);
    let y1 = ((v1 * m.height as f32).ceil() as usize).clamp(y0 + 1, m.height);
    let mut sum = [0.0f32; 3];
    for y in y0..y1 {
        for x in x0..x1 {
            let c = m.cells[y * m.width + x];
            for i in 0..3 {
                sum[i] += c[i] as f32;
            }
        }
    }
    let n = ((x1 - x0) * (y1 - y0)) as f32;
    sum.map(|v| v / n)
}
impl<Message: Clone> Widget<Message, Theme, Renderer> for MorphWidget<Message> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<MorphState>() }
    fn state(&self) -> tree::State { tree::State::new(MorphState::default()) }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, _: Layout<'_>, _: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        let (Event::Window(window::Event::RedrawRequested(now)), Some(m)) = (event, &self.morph) else { return };
        let state = tree.state.downcast_mut::<MorphState>();
        let ms = now.saturating_duration_since(m.start).as_secs_f32() * 1000.0;
        for (_, message) in &m.events[state.due(m, ms)..state.fired] {
            shell.publish(message.clone());
        }
        if state.fired < m.events.len() {
            shell.request_redraw_at(frame_after(*now));
        }
    }
    fn draw(&self, _: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(m) = &self.morph else { return };
        let b = layout.bounds();
        let ms = Instant::now().saturating_duration_since(m.start).as_secs_f32() * 1000.0;
        let tl = m.timeline;
        let span = |a: f32, z: f32| ((ms - a) / (z - a)).clamp(0.0, 1.0);
        let ease = |x: f32| x * x * (3.0 - 2.0 * x);
        let k = ease(span(tl.move_ms.0, tl.move_ms.1));
        let block = if ms < tl.sharpen.0 {
            MOSAIC_CELL + (BLOCK_MAX - MOSAIC_CELL) * ease(span(0.0, tl.pixelate))
        } else {
            BLOCK_MAX - (BLOCK_MAX - MOSAIC_CELL) * ease(span(tl.sharpen.0, tl.sharpen.1))
        };
        let blend = ease(span(tl.blend.0, tl.blend.1));
        let alpha = span(0.0, tl.fade_in).min(1.0 - span(tl.fade_out.0, tl.fade_out.1));
        let lerp_rect = |a: Rectangle, z: Rectangle| Rectangle {
            x: b.x + a.x + (z.x - a.x) * k,
            y: b.y + a.y + (z.y - a.y) * k,
            width: a.width + (z.width - a.width) * k,
            height: a.height + (z.height - a.height) * k,
        };
        let frame = lerp_rect(m.from_rect, m.to_rect);
        let mut shapes = Vec::new();
        // Keyed surroundings are pixel-snapped quads, never anti-aliased: an edge blended with the
        // key rounds to a near-key colour Windows would not cut out, leaving hairlines.
        for (x, y, w, h) in [
            (b.x, b.y, b.width, frame.y - b.y),
            (b.x, frame.y + frame.height, b.width, b.y + b.height - frame.y - frame.height),
            (b.x, frame.y, frame.x - b.x, frame.height),
            (frame.x + frame.width, frame.y, b.x + b.width - frame.x - frame.width, frame.height),
        ] {
            if w > 0.01 && h > 0.01 {
                renderer.fill_quad(Quad { bounds: Rectangle { x, y, width: w, height: h }, snap: true, ..Quad::default() }, KEY);
            }
        }
        let side = block.round().max(MOSAIC_CELL);
        let cols = (frame.width / side).ceil().max(1.0) as usize;
        let rows = (frame.height / side).ceil().max(1.0) as usize;
        for row in 0..rows {
            let mut run: Option<(usize, [u8; 3])> = None;
            for col in 0..=cols {
                let color = (col < cols).then(|| {
                    let (u0, v0) = (col as f32 * side / frame.width, row as f32 * side / frame.height);
                    let (u1, v1) = (((col + 1) as f32 * side / frame.width).min(1.0), ((row + 1) as f32 * side / frame.height).min(1.0));
                    let a = sample(&m.from, u0, v0, u1, v1);
                    let z = sample(&m.to, u0, v0, u1, v1);
                    [0, 1, 2].map(|i| (a[i] + (z[i] - a[i]) * blend).round() as u8)
                });
                let same = |a: [u8; 3], z: [u8; 3]| a.iter().zip(z).all(|(x, y)| x.abs_diff(y) <= 3);
                match (run, color) {
                    (Some((_, c)), Some(next)) if same(c, next) => {}
                    (current, next) => {
                        if let Some((first, c)) = current {
                            let x = frame.x + first as f32 * side;
                            let y = frame.y + row as f32 * side;
                            let w = ((col - first) as f32 * side).min(frame.x + frame.width - x);
                            let h = side.min(frame.y + frame.height - y);
                            slant(&mut shapes, x, y, w + 0.6, h + 0.6, 0.0, Color { a: alpha, ..Color::from_rgb8(c[0], c[1], c[2]) });
                        }
                        run = next.map(|c| (col, c));
                    }
                }
            }
        }
        let cache = geometry(b, &shapes).cache(Group::unique(), None);
        renderer.draw_geometry(Geometry::load(&cache));
    }
}

/// "by ARKANOID" in the sliders' tag style: a quiet slanted chip for the title bar. Hovered, it
/// glows (a pulsing orange aura, brighter letters) and tells the app, which glitches the window.
pub fn signature<'a, Message: 'a>(on_hover: impl Fn(bool) -> Message + 'a) -> Element<'a, Message> {
    let by = text_width("by", 11.0, Font::with_name("Segoe UI"));
    let name = text_width("ARKANOID", 11.0, numbers());
    Element::new(Signature { by, name, on_hover: Box::new(on_hover) })
}
struct Signature<'a, Message> {
    by: f32,
    name: f32,
    on_hover: Box<dyn Fn(bool) -> Message + 'a>,
}
#[derive(Default)]
struct SignatureState {
    painted: Painted,
    hover: Option<Instant>,
}
const SIGNATURE_H: f32 = 20.0;
const SIGNATURE_LEAN: f32 = 6.0;
impl<Message> Signature<'_, Message> {
    fn width(&self) -> f32 {
        SIGNATURE_LEAN + 9.0 + self.by + 5.0 + self.name + 9.0
    }
}
impl<Message> Widget<Message, Theme, Renderer> for Signature<'_, Message> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<SignatureState>() }
    fn state(&self) -> tree::State { tree::State::new(SignatureState::default()) }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, _: &Renderer, _: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, _: &Rectangle) {
        let state = tree.state.downcast_mut::<SignatureState>();
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => {
                let over = cursor.is_over(layout.bounds());
                if over != state.hover.is_some() {
                    state.hover = over.then(Instant::now);
                    shell.publish((self.on_hover)(over));
                    shell.request_redraw();
                }
            }
            // The glow breathes slowly: 30 frames a second are plenty, and each frame is cheap.
            Event::Window(window::Event::RedrawRequested(now)) if state.hover.is_some() => shell.request_redraw_at(RedrawRequest::At(*now + Duration::from_millis(33))),
            _ => {}
        }
    }
    fn size(&self) -> Size<Length> {
        Size { width: Length::Fixed(self.width()), height: Length::Fixed(SIGNATURE_H) }
    }
    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fixed(self.width()), Length::Fixed(SIGNATURE_H))
    }
    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _: &Theme, _: &renderer::Style, layout: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {
        let b = layout.bounds();
        let state = tree.state.downcast_ref::<SignatureState>();
        // 0 idle, up to 1 hovered: ramps in over 200 ms, then breathes.
        let glow = state.hover.map_or(0.0, |since| {
            let ms = Instant::now().saturating_duration_since(since).as_secs_f32() * 1000.0;
            (ms / 200.0).min(1.0) * (0.78 + 0.22 * (ms / 140.0).sin())
        });
        let mut shapes = Vec::with_capacity(10);
        let w = b.width - SIGNATURE_LEAN;
        for k in (1..=7).rev() {
            let e = k as f32 * 1.4;
            let h = SIGNATURE_H + 2.0 * e;
            slant(&mut shapes, b.x - e - e * SIGNATURE_LEAN / SIGNATURE_H, b.y - e, w + 2.0 * e, h, SIGNATURE_LEAN * h / SIGNATURE_H, Color { a: 0.09 * glow, ..TAG });
        }
        let edge = Color { r: OFF_EDGE.r + (TAG.r - OFF_EDGE.r) * glow, g: OFF_EDGE.g + (TAG.g - OFF_EDGE.g) * glow, b: OFF_EDGE.b + (TAG.b - OFF_EDGE.b) * glow, a: 1.0 };
        slant(&mut shapes, b.x, b.y, w, SIGNATURE_H, SIGNATURE_LEAN, edge);
        slant(&mut shapes, b.x + 1.2, b.y + 1.0, w - 2.4, SIGNATURE_H - 2.0, SIGNATURE_LEAN * (SIGNATURE_H - 2.0) / SIGNATURE_H, Color::from_rgb8(0x1B, 0x1C, 0x1F));
        state.painted.draw(renderer, b.expand(14.0), shapes);
        let text = |content: &str, font: Font| Text {
            content: content.to_owned(),
            bounds: Size::new(80.0, SIGNATURE_H),
            size: Pixels(11.0),
            line_height: text::LineHeight::default(),
            font,
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Center,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::None,
        };
        let x = b.x + SIGNATURE_LEAN / 2.0 + 9.0;
        let by = Color::from_rgb8(0x85, 0x86, 0x8D);
        put(renderer, text("by", Font::with_name("Segoe UI")), Point::new(x, b.center_y()), brighten(by, glow), b.expand(4.0));
        let name = Color { r: INK.r + (HEAD.r - INK.r) * glow, g: INK.g + (TAG.g - INK.g) * glow * 0.5, b: INK.b + (TAG.b - INK.b) * glow * 0.6, a: 1.0 };
        put(renderer, text("ARKANOID", numbers()), Point::new(x + self.by + 5.0, b.center_y()), name, b.expand(4.0));
    }
}

impl<'a, Message: 'a> From<Tacho<'a, Message>> for Element<'a, Message> {
    fn from(t: Tacho<'a, Message>) -> Self {
        Element::new(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(range: RangeInclusive<f32>, v: f32) -> Tacho<'static, ()> {
        tacho(range, v, |_| (), Clock { epoch: Instant::now(), opened: None, animate: false, idle: false })
    }
    #[test]
    fn red_zone_starts_strictly_above_the_threshold() {
        let s = t(0.0..=200.0, 100.0).segments(20).red_above(100.0);
        assert_eq!(s.lit(100.0), (0, 10, 9));
        assert!(!s.in_red(100.0));
        assert_eq!(s.lit(101.0), (0, 11, 10));
        assert!(s.in_red(101.0));
        assert_eq!(s.red_from(), 10);
        assert_eq!(s.lit(0.0), (0, 0, -1));
    }
    #[test]
    fn signed_fill_grows_from_the_origin() {
        let s = t(-12.0..=12.0, 0.0).segments(16).origin(0.0);
        assert_eq!(s.lit(0.0).0, s.lit(0.0).1);
        assert_eq!(s.lit(-3.0), (6, 8, 6));
        assert_eq!(s.lit(6.0), (8, 12, 11));
    }
    #[test]
    fn version_rolls_only_its_changed_tail() {
        let roll = Roll { from: "0.3.19".into(), to: "0.3.20".into(), start: None, size: 13.0, color: INK };
        assert_eq!(roll.parts(), ("0.3.".into(), "19".into(), "20".into()));
        let same = Roll { from: "0.3.14".into(), to: "0.3.15".into(), start: None, size: 13.0, color: INK };
        assert_eq!(same.parts(), ("0.3.1".into(), "4".into(), "5".into()));
        assert!(ready::DROP.1 <= ready::MOSAIC.0 && ready::MOSAIC.1 <= ready::ROLL.0 && ready::ROLL.1 <= ready::CONFETTI.0 && ready::CONFETTI.1 < ready::END, "D, then B, then A");
    }
    #[test]
    fn dense_sliders_drag_a_step_at_a_time() {
        let clock = Clock { epoch: Instant::now(), opened: None, animate: false, idle: false };
        let track = Rectangle { x: 0.0, y: 0.0, width: 190.0, height: 16.0 };
        // Boost: 1900 steps on 190 px; the press lands on 300, then every 2 px is one step.
        let boost = tacho(100.0..=2000.0, 300.0, |v| v, clock);
        let anchor = (20.0, 300.0);
        assert_eq!(boost.dragged(track, anchor, 22.0).0, 301.0);
        assert_eq!(boost.dragged(track, anchor, 20.8).0, 300.0, "less than a step stays");
        assert_eq!(boost.dragged(track, anchor, 12.0).0, 296.0);
        let (end, moved) = boost.dragged(track, anchor, -2000.0);
        assert_eq!(end, 100.0);
        assert_eq!(boost.dragged(track, moved, moved.0 + 2.0).0, 101.0, "turning back at the end answers at once");
        // Pitch: 24 steps on 190 px follow the cursor itself.
        let pitch = tacho(-12.0..=12.0, 0.0, |v| v, clock);
        assert_eq!(pitch.dragged(track, (95.0, 0.0), 190.0).0, 12.0);
        assert_eq!(boost.snap(299.6), 300.0);
        assert_eq!(boost.snap(5000.0), 2000.0);
    }
    #[test]
    fn page_shift_starts_at_its_first_frame() {
        let start = Instant::now();
        let ms = |v: u64| start + Duration::from_millis(v);
        let mut s = ShiftState::default();
        assert_eq!(s.frame(start, ms(9)), 0, "a late first frame still shows the first step");
        assert_eq!(s.frame(start, ms(14)), 0, "an early redraw keeps the step");
        assert_eq!(s.frame(start, ms(19)), 1, "2 ms early still counts");
        assert!(s.frame(start, ms(200)) >= SHIFT_STEPS, "a late frame ends the fade, never stretches it");
        assert_eq!(s.frame(start + Duration::from_millis(1), ms(210)), 0, "a new switch starts over");
        for u in [0.0, 0.5, 1.0] {
            let steps: Vec<_> = (0..SHIFT_STEPS).map(|i| shift_step(i, u)).collect();
            assert!(steps.windows(2).all(|w| w[0].0 >= w[1].0 && w[0].1 >= w[1].1), "blocks and opacity only resolve at {u}");
            assert!(steps[0].1 < 1.0, "the new page shows through from the first frame");
            assert_eq!(steps[SHIFT_STEPS - 1].1, 0.0, "the last step leaves the page clean");
        }
        assert!(shift_step(4, 0.0).1 < shift_step(4, 1.0).1, "the left edge dissolves first");
    }
    #[test]
    fn morph_events_restart_with_each_morph() {
        let mosaic = std::sync::Arc::new(Mosaic { width: 1, height: 1, cells: vec![[0; 3]] });
        let start = Instant::now();
        let morph = |start| Morph { from: mosaic.clone(), to: mosaic.clone(), from_rect: Rectangle::default(), to_rect: Rectangle::default(), start, timeline: MorphTimeline::GROW, events: vec![(100.0, 1), (950.0, 2)] };
        let (grow, mut s) = (morph(start), MorphState::default());
        assert_eq!((s.due(&grow, 50.0), s.fired), (0, 0));
        assert_eq!((s.due(&grow, 960.0), s.fired), (0, 2));
        assert_eq!((s.due(&grow, 990.0), s.fired), (2, 2), "fired events stay fired");
        let shrink = morph(start + Duration::from_secs(60));
        assert_eq!((s.due(&shrink, 120.0), s.fired), (0, 1), "the next morph plays its own events");
    }
    #[test]
    fn wave_lifts_and_settles() {
        assert_eq!(wave(0.0, false).0, 0.0);
        assert!(wave(284.0, false).0 < -9.0);
        assert_eq!(wave(WAVE_LEN + 1.0, true), (0.0, 0.0));
    }
}
