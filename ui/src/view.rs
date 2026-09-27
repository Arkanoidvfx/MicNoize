use super::*;
use crate::tacho::{self, Clock, tacho};
use iced::advanced::widget::{Id, Operation, operation};
use iced::widget::{
    self, Space, button, column, container, mouse_area, pick_list, row, scrollable, text,
    text_input,
};
use iced::{Border, Color, Length};

// Keep an off-screen keyboard target mounted without mounting every row on the way to it.
fn sound_rows(count: usize, scroll: f32, viewport: f32, pitch: f32, focus: Option<usize>) -> Vec<usize> {
    // The stored offset can outlive a shorter section/filter until Iced clamps its scrollable.
    let scroll = scroll.min((count as f32 * pitch - viewport).max(0.0));
    let first = (((scroll - 96.0) / pitch).floor().max(0.0) as usize).min(count);
    let last = (((scroll + viewport + 96.0) / pitch).ceil() as usize).min(count).max(first);
    let mut rows: Vec<_> = (first..last).collect();
    if let Some(at) = focus.filter(|&at| at < count)
        && let Err(pos) = rows.binary_search(&at)
    {
        rows.insert(pos, at);
    }
    rows
}

// Query actual layout so keyboard focus remains visible at every window size/DPI.
pub fn reveal_focus() -> Task<Msg> {
    #[derive(Default)]
    struct FocusBounds {
        target: Option<iced::Rectangle>,
        viewport: Option<(iced::Rectangle, iced::Vector)>,
    }
    impl Operation<f32> for FocusBounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<f32>)) {
            operate(self);
        }
        fn container(&mut self, id: Option<&Id>, bounds: iced::Rectangle) {
            if id == Some(&Id::new("focused-control")) && self.viewport.is_some() {
                self.target = Some(bounds);
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            bounds: iced::Rectangle,
            _: iced::Rectangle,
            translation: iced::Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&Id::new("body")) {
                self.viewport = Some((bounds, translation));
            }
        }
        fn finish(&self) -> operation::Outcome<f32> {
            match (self.target, self.viewport) {
                (Some(target), Some((viewport, translation))) => {
                    operation::Outcome::Some(focus_scroll_delta(
                        target.y - translation.y,
                        target.height,
                        viewport.y,
                        viewport.height,
                    ))
                }
                _ => operation::Outcome::None,
            }
        }
    }
    iced::advanced::widget::operate(FocusBounds::default()).then(|y| {
        iced::widget::operation::scroll_by(
            "body",
            iced::widget::operation::AbsoluteOffset { x: 0.0, y },
        )
    })
}
pub fn studio_reveal(note: u8, zoom: u8, viewport: f32) -> Task<Msg> {
    iced::widget::operation::scroll_to(
        "studio-roll",
        iced::widget::operation::AbsoluteOffset { x: None, y: Some(studio_offset(note, zoom, viewport)) },
    )
}
pub fn studio_offset(note: u8, zoom: u8, viewport: f32) -> f32 {
    // The roll descends from B5. Keep the selected note near the middle at each scale.
    let row = (studio::FIRST_NOTE + studio::NOTES as u8 - 1 - note) as f32;
    let pitch = 28.0 * zoom as f32 / 100.0;
    (row * pitch - (viewport - pitch) / 2.0).max(0.0)
}
fn focus_scroll_delta(top: f32, height: f32, viewport_top: f32, viewport_height: f32) -> f32 {
    if top < viewport_top + 8.0 {
        top - viewport_top - 8.0
    } else {
        (top + height + 8.0 - viewport_top - viewport_height).max(0.0)
    }
}

// 0.2.8 palette: graphite surfaces, one warm accent, red only for the risky zone.
pub const BG: Color = Color::from_rgb8(0x15, 0x16, 0x19);
const RAIL: Color = Color::from_rgb8(0x11, 0x12, 0x14);
const CARD: Color = Color::from_rgb8(0x1B, 0x1C, 0x1F);
const CARD2: Color = Color::from_rgb8(0x22, 0x23, 0x27);
const HOVER: Color = Color::from_rgb8(0x2A, 0x2B, 0x30);
const LINE: Color = Color::from_rgb8(0x26, 0x27, 0x2B);
const EDGE: Color = Color::from_rgb8(0x3A, 0x3B, 0x41);
pub const INK: Color = Color::from_rgb8(242, 237, 227);
const DIM: Color = Color::from_rgb8(0xA3, 0xA3, 0xA9);
const FAINT: Color = Color::from_rgb8(0x85, 0x86, 0x8D);
pub const ORANGE: Color = Color::from_rgb8(255, 159, 86);
const ORANGE_DARK: Color = Color::from_rgb8(0x1A, 0x12, 0x06);
pub const GREEN: Color = Color::from_rgb8(111, 225, 139);
pub const RED: Color = Color::from_rgb8(255, 119, 118);
const LIVE_BG: Color = Color::from_rgb8(0x1F, 0x1B, 0x18);

/// Segoe MDL2 Assets ships with Windows 10 and 11; its glyphs replace hand-drawn icons.
mod glyph {
    pub const MIC: &str = "\u{E720}";
    pub const HEADPHONES: &str = "\u{E7F6}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const EFFECTS: &str = "\u{E945}";
    pub const SOUNDPAD: &str = "\u{E8A9}";
    pub const STUDIO: &str = "\u{E8D6}";
    pub const VOICE: &str = "\u{E77B}";
    pub const CHIP: &str = "\u{E9F5}";
    pub const OUTPUT: &str = "\u{E8BD}";
    pub const LOCK: &str = "\u{E72E}";
    pub const RIGHT: &str = "\u{E76C}";
    pub const DOWN: &str = "\u{E70D}";
    pub const PLAY: &str = "\u{E768}";
    pub const STOP: &str = "\u{E71A}";
    pub const SAVE: &str = "\u{E896}";
    pub const VOLUME: &str = "\u{E767}";
    pub const BOOST: &str = "\u{E995}";
    pub const NOTE: &str = "\u{E8D6}";
    pub const SLOW: &str = "\u{EC49}";
    pub const FAST: &str = "\u{EC4A}";
    pub const REVERSE: &str = "\u{E7A7}";
    pub const REPEAT: &str = "\u{E8EE}";
    pub const CLOSE: &str = "\u{E8BB}";
    pub const MINIMIZE: &str = "\u{E921}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const FOLDER: &str = "\u{E838}";
    pub const REFRESH: &str = "\u{E72C}";
    pub const CHECK: &str = "\u{E73E}";
    pub const LOGS: &str = "\u{E9D9}";
    pub const BACK: &str = "\u{E72B}";
    pub const PALETTE: &str = "\u{E790}";
}
fn icon<'a>(glyph: &'a str, size: u32, color: Color) -> widget::Text<'a> {
    text(glyph).size(size).color(color).font(Font::with_name("Segoe MDL2 Assets"))
}
fn label<'a>(s: impl Into<String>, size: u32, color: Color) -> widget::Text<'a> {
    text(s.into()).size(size).color(color)
}
fn bold<'a>(s: impl Into<String>, size: u32, color: Color) -> widget::Text<'a> {
    label(s, size, color).font(Font {
        weight: iced::font::Weight::Semibold,
        ..Font::with_name("Segoe UI")
    })
}
fn title<'a>(s: &'a str) -> widget::Text<'a> {
    bold(s, 22, INK)
}
fn numbers<'a>(s: impl Into<String>, size: u32, color: Color) -> widget::Text<'a> {
    label(s, size, color).font(tacho::numbers())
}
const MB: u64 = 1_048_576;
fn speed_text(bytes_per_second: f64) -> String {
    format!("{:.1} МБ/с", bytes_per_second / MB as f64).replace('.', ",")
}
/// Rounded up to 5 s under a minute, to 10 s under ten minutes, then to whole minutes.
fn eta_text(seconds: f64) -> String {
    let s = seconds.max(1.0).ceil() as u64;
    match s {
        0..60 => format!("≈ {} с", s.div_ceil(5) * 5),
        60..600 => match s.div_ceil(10) * 10 {
            s if s % 60 == 0 => format!("≈ {} мин", s / 60),
            s => format!("≈ {} мин {} с", s / 60, s % 60),
        },
        _ => format!("≈ {} мин", s.div_ceil(60)),
    }
}
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}
fn outline(focused: bool) -> Border {
    Border {
        color: if focused { ORANGE } else { EDGE },
        width: if focused { 2.0 } else { 1.0 },
        radius: 8.0.into(),
    }
}
/// A button: accent is the page's single primary action, the rest are quiet.
fn action<'a>(
    content: impl Into<Element<'a, Msg>>,
    message: Msg,
    focused: bool,
    accent: bool,
) -> widget::Button<'a, Msg> {
    button(focus_target(content, focused))
        .padding([7, 12])
        .on_press(message)
        .style(move |_, status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            let disabled = matches!(status, button::Status::Disabled);
            button::Style {
                background: Some(
                    (if accent && disabled {
                        Color { a: 0.35, ..ORANGE }
                    } else if accent && hover {
                        Color::from_rgb8(0xFF, 0xB0, 0x70)
                    } else if accent {
                        ORANGE
                    } else if disabled {
                        BG
                    } else if hover {
                        HOVER
                    } else {
                        CARD2
                    })
                    .into(),
                ),
                text_color: if accent { ORANGE_DARK } else { INK },
                border: if accent && !focused {
                    Border { radius: 8.0.into(), ..Border::default() }
                } else {
                    outline(focused)
                },
                ..Default::default()
            }
        })
}
fn icon_button<'a>(glyph: &'a str, message: Msg, focused: bool) -> widget::Button<'a, Msg> {
    button(focus_target(container(icon(glyph, 14, DIM)).center(Length::Fill), focused))
        .width(34)
        .height(34)
        .padding(0)
        .on_press(message)
        .style(move |_, status| button::Style {
            background: Some(
                (if matches!(status, button::Status::Hovered | button::Status::Pressed) { HOVER } else { CARD2 }).into(),
            ),
            text_color: INK,
            border: outline(focused),
            ..Default::default()
        })
}
fn caption_button<'a>(glyph: &'a str, message: Msg, danger: bool) -> widget::Button<'a, Msg> {
    button(container(icon(glyph, 10, DIM)).center(Length::Fill))
        .width(46)
        .height(46)
        .padding(0)
        .on_press(message)
        .style(move |_, status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: hover.then(|| (if danger { Color::from_rgb8(0xC4, 0x2B, 0x1C) } else { HOVER }).into()),
                text_color: INK,
                ..Default::default()
            }
        })
}
fn focus_target<'a>(
    content: impl Into<Element<'a, Msg>>,
    focused: bool,
) -> widget::Container<'a, Msg> {
    let content = container(content);
    if focused {
        content.id("focused-control")
    } else {
        content
    }
}
/// Any element painted offscreen at 1/[`tacho::MOSAIC_CELL`] scale, as RGB cells.
pub fn mosaic_of<'a, M: 'a>(mut element: Element<'a, M>, area: Size) -> Option<std::sync::Arc<tacho::Mosaic>> {
    use iced::advanced::{Layout, Renderer as _, graphics::Viewport};
    let (w, h) = ((area.width / tacho::MOSAIC_CELL).ceil() as u32, (area.height / tacho::MOSAIC_CELL).ceil() as u32);
    let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
    let mut tree = iced::advanced::widget::Tree::empty();
    tree.diff(element.as_widget());
    let layout = element.as_widget_mut().layout(&mut tree, &renderer, &iced::advanced::layout::Limits::new(Size::ZERO, area));
    let full = iced::Rectangle::with_size(area);
    renderer.reset(full);
    element.as_widget().draw(&tree, &mut renderer, &Theme::Dark, &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout), iced::mouse::Cursor::Unavailable, &full);
    let mut pixels = tiny_skia::Pixmap::new(w, h)?;
    let mut mask = tiny_skia::Mask::new(w, h)?;
    renderer.draw(&mut pixels.as_mut(), &mut mask, &Viewport::with_physical_size(Size::new(w, h), 1.0 / tacho::MOSAIC_CELL), &[full], BG);
    // The renderer writes BGRA.
    let cells = pixels.data().as_chunks::<4>().0.iter().map(|p| [p[2], p[1], p[0]]).collect();
    Some(std::sync::Arc::new(tacho::Mosaic { width: w as usize, height: h as usize, cells }))
}

/// The update window (design variant A): the app's mark, what is happening, the versions in
/// large type and the running bar.
pub const UPDATE_CARD: Size = Size::new(440.0, 176.0);
pub fn update_card<'a, M: 'a>(stage: tacho::BarStage, from: &str, to: &str) -> Element<'a, M> {
    let (title_text, version, color) = match stage {
        tacho::BarStage::Done => ("Готово", to.to_owned(), GREEN),
        tacho::BarStage::Launching => ("Запускаем Mic Noize", to.to_owned(), GREEN),
        _ => ("Обновляем Mic Noize", format!("{from} → {to}"), ORANGE),
    };
    container(column![
        container(row![tacho::logo(18.0, 0.0), bold("Mic Noize", 13, INK)].spacing(10).align_y(iced::Center)).padding([0, 14]).center_y(38),
        container(Space::new().height(1)).width(Length::Fill).style(|_| container::Style { background: Some(LINE.into()), ..Default::default() }),
        column![column![label(title_text, 14, DIM), numbers(version, 28, color)].spacing(2), tacho::run_bar(stage)]
            .spacing(16)
            .padding(iced::Padding { top: 18.0, right: 20.0, bottom: 22.0, left: 20.0 }),
    ])
    .width(UPDATE_CARD.width)
    .height(UPDATE_CARD.height)
    .style(|_| container::Style {
        background: Some(Color::from_rgb8(0x15, 0x16, 0x19).into()),
        border: Border { color: Color::from_rgb8(0x2A, 0x2B, 0x30), width: 1.0, radius: 12.0.into() },
        ..Default::default()
    })
    .into()
}
/// The update card centred on the colour key, so a keyed window shows only the card.
pub fn update_card_on_key<'a, M: 'a>(stage: tacho::BarStage, from: &str, to: &str) -> Element<'a, M> {
    container(update_card(stage, from, to))
        .center(Length::Fill)
        .style(|_| container::Style { background: Some(tacho::KEY.into()), ..Default::default() })
        .into()
}

/// tiny-skia repaints only damaged regions and places vertically centred control text from its
/// anchor down, so a pick_list or text_input whose label changes would keep the top half of
/// the old one (hovering repaints it). An invisible background that changes with the label
/// damages the whole control instead.
fn repaint<'a>(key: impl std::hash::Hash, content: impl Into<Element<'a, Msg>>) -> Element<'a, Msg> {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    let h = BuildHasherDefault::<DefaultHasher>::default().hash_one(key);
    let byte = |shift: u32| ((h >> shift) & 0xFF) as f32 / 255.0;
    let tint = Color { r: byte(0), g: byte(8), b: byte(16), a: 0.0 };
    container(content).style(move |_| container::Style { background: Some(tint.into()), ..Default::default() }).into()
}
fn frame<'a>(content: impl Into<Element<'a, Msg>>, focused: bool) -> Element<'a, Msg> {
    focus_target(content, focused)
        .padding(3)
        .style(move |_| container::Style {
            border: Border {
                color: if focused { ORANGE } else { Color::TRANSPARENT },
                width: if focused { 2.0 } else { 1.0 },
                radius: 6.0.into(),
            },
            ..Default::default()
        })
        .into()
}
fn card<'a>(content: impl Into<Element<'a, Msg>>) -> widget::Container<'a, Msg> {
    container(content).padding([14, 16]).width(Length::Fill).style(|_| container::Style {
        background: Some(CARD.into()),
        border: Border { color: LINE, width: 1.0, radius: 10.0.into() },
        ..Default::default()
    })
}
fn tile<'a>(glyph: &'a str, size: f32, accent: bool) -> Element<'a, Msg> {
    container(icon(glyph, (size * 0.45) as u32, if accent { ORANGE } else { FAINT }))
        .width(size)
        .height(size)
        .center(size)
        .style(move |_| container::Style {
            background: Some(
                (if accent { Color { a: 0.13, ..ORANGE } } else { Color::from_rgb8(0x25, 0x26, 0x2A) }).into(),
            ),
            border: Border { radius: 9.0.into(), ..Border::default() },
            ..Default::default()
        })
        .into()
}
fn heading_row<'a>(glyph: &'a str, name: &'a str, right: Element<'a, Msg>) -> Element<'a, Msg> {
    row![icon(glyph, 14, DIM), bold(name, 13, INK), Space::new().width(Length::Fill), right]
        .spacing(8)
        .height(24)
        .align_y(iced::Center)
        .into()
}
fn device_style(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let open = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
    pick_list::Style {
        text_color: INK,
        placeholder_color: FAINT,
        handle_color: DIM,
        background: BG.into(),
        border: Border {
            color: if open { ORANGE } else { Color::from_rgb8(0x45, 0x46, 0x4D) },
            width: 1.0,
            radius: 7.0.into(),
        },
    }
}
fn input_style(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    let hover = matches!(status, text_input::Status::Hovered);
    text_input::Style {
        background: BG.into(),
        border: Border {
            color: if focused { ORANGE } else if hover { Color::from_rgb8(0x6B, 0x6C, 0x73) } else { Color::from_rgb8(0x2E, 0x2F, 0x34) },
            width: 1.0,
            radius: 7.0.into(),
        },
        icon: DIM,
        placeholder: FAINT,
        value: INK,
        selection: Color { a: 0.35, ..ORANGE },
    }
}
/// tiny-skia repaints a changed quad only inside its bounds, so the pill's anti-aliased edge
/// kept the old colour: a ring around a switch that was just flipped. A padded invisible
/// background keyed on the state (see [`repaint`]) repaints the edge with it.
fn switch<'a>(on: bool, message: impl Fn(bool) -> Msg + 'a, enabled: bool) -> Element<'a, Msg> {
    repaint((on, enabled), container(toggler(on, message, enabled)).padding(2))
}
fn toggler<'a>(on: bool, message: impl Fn(bool) -> Msg + 'a, enabled: bool) -> widget::Toggler<'a, Msg> {
    widget::toggler(on)
        .size(18)
        .on_toggle_maybe(enabled.then_some(message))
        .style(|_, status| {
            use widget::toggler::Status::*;
            let (Active { is_toggled } | Hovered { is_toggled } | Disabled { is_toggled }) = status;
            widget::toggler::Style {
                background: (if is_toggled { ORANGE } else { Color::from_rgb8(0x34, 0x35, 0x3A) }).into(),
                background_border_width: 0.0,
                background_border_color: Color::TRANSPARENT,
                foreground: (if is_toggled { ORANGE_DARK } else { DIM }).into(),
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: Some(DIM),
                border_radius: None,
                padding_ratio: 0.18,
            }
        })
}
/// Segmented level meter, the same vocabulary as the sliders.
fn meter<'a>(level: f32, color: Color) -> Element<'a, Msg> {
    let segments = 40usize;
    let lit = (level.clamp(0.0, 1.0) * segments as f32).round() as usize;
    widget::Row::with_children((0..segments).map(|i| {
        let fill = if i < lit { color } else { Color::from_rgb8(0x26, 0x27, 0x2B) };
        container(Space::new().width(Length::Fill).height(10))
            .width(Length::Fill)
            .style(move |_| container::Style { background: Some(fill.into()), ..Default::default() })
            .into()
    }))
    .spacing(2)
    .width(Length::Fill)
    .into()
}
/// The hero recording's bars, as in the mockup: a stable pseudo-waveform per clip (the real
/// samples are not decoded for the UI), filled with the slider gradient as it plays.
fn waveform<'a>(name: &str, progress: Option<f32>) -> Element<'a, Msg> {
    const BARS: usize = 30;
    let seed = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 997;
    let seed = seed as f32 / 97.0;
    let done = progress.map_or(0, |p| (p * BARS as f32).round() as usize);
    widget::Row::with_children((0..BARS).map(|i| {
        let t = i as f32;
        let h = 5.0 + 24.0 * ((t * 0.55 + seed).sin() * (t * 0.21 + seed * 1.7).cos()).abs();
        let fill = if i < done {
            tacho::lerp(t / BARS as f32)
        } else if progress.is_some() {
            Color::from_rgb8(0x5A, 0x5B, 0x61)
        } else {
            Color::from_rgb8(0x4A, 0x4B, 0x51)
        };
        container(Space::new().width(Length::Fill).height(h.round()))
            .width(Length::Fill)
            .style(move |_| container::Style { background: Some(fill.into()), border: Border { radius: 1.0.into(), ..Border::default() }, ..Default::default() })
            .into()
    }))
    .spacing(3)
    .align_y(iced::Center)
    .width(Length::Fill)
    .into()
}
fn panel_row<'a>(name: Element<'a, Msg>, control: Element<'a, Msg>) -> widget::Row<'a, Msg> {
    row![container(name).width(112), control].spacing(12).align_y(iced::Center).height(34)
}
fn effect_grid<'a>(a: Element<'a, Msg>, b: Element<'a, Msg>, c: Element<'a, Msg>, d: Element<'a, Msg>, e: Element<'a, Msg>) -> widget::Row<'a, Msg> {
    row![
        container(a).width(36),
        container(b).width(128),
        container(c).width(Length::Fill),
        container(d).width(150),
        container(e).width(150),
    ]
    .spacing(14)
    .align_y(iced::Center)
}
fn db(peak: f32) -> f32 {
    if peak > 0.000001 { 20.0 * peak.log10() } else { -100.0 }
}
fn db_text(peak: f32) -> String {
    let v = db(peak);
    if v <= -99.0 { "−∞ dB".into() } else { format!("{:.0} dB", v).replace('-', "−") }
}

impl App {
    pub fn clock(&self) -> Clock {
        Clock {
            epoch: self.epoch,
            opened: self.opened_at.filter(|_| self.slider_idle),
            animate: self.ui_active(),
            idle: self.slider_idle,
        }
    }
    fn ring(&self, focused: bool) -> bool {
        focused && self.focus_visible
    }
    fn key_held(&self, vk: u32) -> bool {
        self.keys_down[(vk / 64 % 4) as usize] >> (vk % 64) & 1 != 0
    }
    fn page_main(&self) -> bool {
        !self.details && !self.rvc_page && !self.soundpad_page && !self.logs_page && !self.effects_page && !self.studio_page
    }

    /// The window: the app, with the update morph layer on top (empty unless morphing). The
    /// layer is always there so the app's widgets keep their state when a morph starts.
    pub fn view(&self, _: window::Id) -> Element<'_, Msg> {
        let (base, anim): (Element<'_, Msg>, _) = match &self.morph {
            None => (self.root(), None),
            Some(m) => (
                match m.base {
                    MorphBase::Root => self.root(),
                    MorphBase::Card(stage) => update_card_on_key(stage, &m.from_version, &m.to_version),
                    MorphBase::Key => container(Space::new()).width(Length::Fill).height(Length::Fill).style(|_| container::Style { background: Some(tacho::KEY.into()), ..Default::default() }).into(),
                },
                m.anim.as_ref(),
            ),
        };
        // The glitch layer stays (it keeps track of the window's size for its picture); the
        // others are there only while they play: tiny-skia rebuilds a window-sized clip mask for
        // every layer, empty or not, in every patch it repaints.
        let glitch = tacho::glitch(self.glitch.filter(|_| self.morph.is_none()), self.glitch_mosaic.as_ref().map(|(_, _, m)| m.clone()));
        let mut layers = widget::stack![base, glitch].width(Length::Fill).height(Length::Fill);
        if self.morph.is_none() {
            // The ready card's tag and confetti fly over the whole window.
            if self.ready_fx.is_some() {
                layers = layers.push(tacho::celebrate(self.ready_fx));
            }
            // The restart's air stops where the morph takes over: under its colour key it
            // would float over the desktop.
            if self.restart.is_some() {
                layers = layers.push(tacho::restart_wind(self.restart));
            }
        }
        if anim.is_some() {
            layers = layers.push(tacho::morph(anim));
        }
        layers.into()
    }

    /// The whole app window without any morph.
    fn root(&self) -> Element<'_, Msg> {
        let voice = ((db(self.peak) + 72.0) / 72.0).clamp(0.0, 1.0);
        let logo = tacho::logo(26.0, voice);
        let titlebar = row![
            mouse_area(
                container(
                    row![
                        logo,
                        bold("MicNoize", 15, INK),
                        // 40 % smaller than before and 75 % transparent: there, not loud.
                        numbers(env!("CARGO_PKG_VERSION"), 9, Color { a: 0.25, ..INK }),
                        Space::new().width(Length::Fill),
                        tacho::signature(Msg::SignatureHover),
                    ]
                    .spacing(10)
                    .align_y(iced::Center),
                )
                    .padding([0, 16])
                    .width(Length::Fill)
                    .height(46)
                    .center_y(46),
            )
            .on_press(Msg::Drag),
            caption_button(glyph::MINIMIZE, Msg::Minimize, false),
            caption_button(glyph::CLOSE, Msg::Hide, true),
        ]
        .align_y(iced::Center);
        let titlebar = container(titlebar).width(Length::Fill).style(|_| container::Style {
            border: Border { color: LINE, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        });

        let body = widget::stack![
            self.body(),
            tacho::page_shift(self.page_shift.as_ref(), Msg::PageShiftDone),
        ]
        .width(Length::Fill)
        .height(Length::Fill);
        let root = column![
            titlebar,
            container(Space::new().height(1)).width(Length::Fill).style(|_| container::Style {
                background: Some(LINE.into()),
                ..Default::default()
            }),
            row![self.rail(), body].height(Length::Fill),
        ];
        container(root)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(BG.into()),
                text_color: Some(INK),
                border: Border { color: Color::from_rgb8(0x2A, 0x2B, 0x30), width: 1.0, radius: 12.0.into() },
                ..Default::default()
            })
            .into()
    }

    /// The current page with its error banner and scrolling, right of the rail.
    fn body(&self) -> Element<'_, Msg> {
        let content: Element<'_, Msg> = if self.logs_page {
            self.logs_view()
        } else if self.studio_page {
            self.studio_view()
        } else if self.soundpad_page {
            self.soundpad_view()
        } else if self.details {
            self.settings_view()
        } else if self.rvc_page {
            self.rvc_view()
        } else if self.effects_page {
            self.effects_view()
        } else {
            self.main_view()
        };
        let mut page = column![].spacing(12).width(Length::Fill).height(Length::Fill);
        if !self.message.is_empty() {
            let (color, glyph) = match self.message.as_str() {
                DEVICE_REPAIRED => (GREEN, glyph::CHECK),
                DEVICE_REPAIRING => (DIM, glyph::REFRESH),
                _ => (RED, glyph::WARNING),
            };
            page = page.push(
                container(row![icon(glyph, 14, color), label(&self.message, 13, color)].spacing(10).align_y(iced::Center))
                    .padding([9, 14])
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Color { a: 0.08, ..color }.into()),
                        border: Border { color: Color { a: 0.45, ..color }, width: 1.0, radius: 8.0.into() },
                        ..Default::default()
                    }),
            );
        }
        // The soundpad owns its own scrollable list (and the "body" id) so its toolbar and
        // sidebar stay put while hundreds of clips scroll.
        if self.soundpad_page && self.sound_folder.is_some() {
            page.push(container(content).width(Length::Fill).height(Length::Fill)).padding([20, 26]).into()
        } else {
            page.push(
                scrollable(
                    mouse_area(container(content).padding(iced::Padding { right: 12.0, bottom: 8.0, ..Default::default() }))
                        .on_scroll(|d| Msg::Wheel("body", smooth::wheel_pixels(d))),
                )
                .id("body")
                .width(Length::Fill)
                .height(Length::Fill),
            )
            .padding(iced::Padding { top: 20.0, right: 14.0, bottom: 6.0, left: 26.0 })
            .into()
        }
    }

    /// The page area painted offscreen, small, for the page-switch pixelation.
    pub fn page_mosaic(&self) -> Option<std::sync::Arc<tacho::Mosaic>> {
        mosaic_of(self.body(), tacho::page_area()?)
    }
    /// The whole window painted offscreen, small, for the update morphs.
    pub fn window_mosaic(&self, size: Size) -> Option<std::sync::Arc<tacho::Mosaic>> {
        mosaic_of(self.root(), size)
    }
    /// Left rail: what the app does, in order of use; settings and a ready update at the bottom.
    fn rail(&self) -> Element<'_, Msg> {
        let item = |glyph: &'static str, name: &'static str, page: u8, selected: bool| {
            let focused = self.focus == focus::tab(page);
            // The row fills the 40 px button and centres vertically; the glyph gets a fixed,
            // centred cell so icons of different widths line the labels up.
            button(focus_target(
                row![
                    container(icon(glyph, 15, if selected { ORANGE } else { DIM })).center(18),
                    label(name, 14, if selected { INK } else { DIM }),
                ]
                .spacing(11)
                .height(Length::Fill)
                .align_y(iced::Center),
                focused,
            )
            .height(Length::Fill))
            .width(Length::Fill)
            .height(40)
            .padding([0, 12])
            .on_press(Msg::Page(page))
            .style(move |_, status| button::Style {
                background: Some(
                    (if selected {
                        Color::from_rgb8(0x1F, 0x20, 0x23)
                    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                        Color::from_rgb8(0x1A, 0x1B, 0x1E)
                    } else {
                        Color::TRANSPARENT
                    })
                    .into(),
                ),
                text_color: INK,
                border: Border {
                    color: if focused { ORANGE } else { Color::TRANSPARENT },
                    width: if focused { 2.0 } else { 0.0 },
                    radius: 8.0.into(),
                },
                ..Default::default()
            })
        };
        let mut rail = column![
            item(glyph::MIC, "Шумодав", 0, self.page_main()),
            item(glyph::EFFECTS, "Эффекты", 6, self.effects_page),
            item(glyph::SOUNDPAD, "Саундпад", 4, self.soundpad_page),
            item(glyph::STUDIO, "Студия", 7, self.studio_page),
            item(glyph::VOICE, "Смена голоса", 1, self.rvc_page),
            Space::new().height(Length::Fill),
        ]
        .spacing(2)
        .padding([14, 10])
        .width(196)
        .height(Length::Fill);
        if self.update_ready || self.ready_preview.is_some() {
            // The celebration's layer only while it plays: the card keeps its widget state.
            let mut card = widget::stack![self.ready_card()];
            if self.ready_fx.is_some() {
                card = card.push(tacho::ready_fx(self.ready_fx, self.ready_mosaic.clone(), RAIL));
            }
            rail = rail.push(card);
            rail = rail.push(Space::new().height(8));
        }
        rail = rail.push(item(glyph::SETTINGS, "Настройки", 2, self.details || self.logs_page));
        container(rail)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(RAIL.into()),
                border: Border { color: LINE, width: 0.0, radius: iced::border::Radius::default().bottom_left(12.0) },
                ..Default::default()
            })
            .into()
    }

    /// «Обновление готово» in the rail: the new version in large numbers (it rolls in from the
    /// installed one when the card celebrates) and the restart. Fixed size: its mosaic maps on it.
    pub fn ready_card(&self) -> Element<'_, Msg> {
        let message = if self.ready_preview.is_some() { Msg::ReadyPreviewPlay } else { Msg::ApplyUpdate };
        // After the click the button leaves a hole; the card names the preparation's step.
        let (heading, step, bottom): (_, _, Element<'_, Msg>) = match self.restart {
            Some(restart) => (
                "Готовим перезапуск",
                ["Проверяем версию…", "Сохраняем откат…", "Останавливаем звук…"][restart.step.min(2) as usize],
                tacho::restart_slot(restart),
            ),
            None => (
                "Обновление готово",
                "скачана и готова",
                action(container(label("Перезапустить", 13, ORANGE_DARK)).center(Length::Fill), message, self.focus == focus::UPDATE_BANNER, true)
                    .padding([0, 12])
                    .width(Length::Fill)
                    .height(tacho::ready::BUTTON_H)
                    .into(),
            ),
        };
        container(
            column![
                bold(heading, 13, INK),
                row![label("Версия", 12, DIM), tacho::roll(env!("CARGO_PKG_VERSION"), &self.ready_version(), self.ready_fx, 14.0, INK)]
                    .spacing(6)
                    .align_y(iced::Center),
                label(step, 12, DIM),
                Space::new().height(Length::Fill),
                bottom,
            ]
            .spacing(4),
        )
        .padding(12)
        .width(tacho::ready::CARD.width)
        .height(tacho::ready::CARD.height)
        .style(|_| container::Style {
            background: Some(CARD.into()),
            border: Border { color: Color::from_rgb8(0x2A, 0x2B, 0x30), width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
    }

    /// Шумодав: the devices you can change on top, the fixed route folded away, then strength.
    fn main_view(&self) -> Element<'_, Msg> {
        use focus::effects::*;
        let clock = self.clock();
        let mic = row![
            tile(glyph::MIC, 52.0, true),
            column![
                label("Микрофон", 12, DIM),
                frame(
                    repaint(self.input.as_ref().map(ToString::to_string), pick_list(self.inputs.as_slice(), self.input.as_ref(), Msg::Input)
                        .placeholder("Выберите микрофон")
                        .text_size(13)
                        .padding([6, 10])
                        .width(Length::Fill)
                        .style(device_style)),
                    self.focus == INPUT,
                ),
            ]
            .spacing(3)
            .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(iced::Center);
        let hp_live = matches!(self.headphone_state, 1 | 2);
        let modded = hp_live || self.headphone_reverse || self.headphone_pitch != 0;
        let gear_focused = self.focus == HEADPHONE_GEAR;
        let open = self.headphone_page;
        let gear = button(focus_target(
            widget::stack![
                container(icon(glyph::SETTINGS, 16, if open { ORANGE_DARK } else { DIM })).center(40),
                container(
                    container(Space::new().width(7).height(7)).style(move |_| container::Style {
                        background: Some((if modded { if open { ORANGE_DARK } else { ORANGE } } else { Color::TRANSPARENT }).into()),
                        border: Border { radius: 4.0.into(), ..Border::default() },
                        ..Default::default()
                    }),
                )
                .padding(iced::Padding { top: 6.0, left: 27.0, ..Default::default() }),
            ],
            gear_focused,
        ))
        .width(40)
        .height(40)
        .padding(0)
        .on_press(Msg::HeadphonePanel(!open))
        .style(move |_, status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some((if open { ORANGE } else if hover { HOVER } else { CARD }).into()),
                text_color: INK,
                border: Border {
                    color: if gear_focused || open { ORANGE } else { EDGE },
                    width: if gear_focused { 2.0 } else { 1.0 },
                    radius: 9.0.into(),
                },
                ..Default::default()
            }
        });
        let phones = row![
            tile(glyph::HEADPHONES, 52.0, true),
            column![
                label("Наушники", 12, DIM),
                frame(
                    repaint(self.headphone_output.as_ref().map(ToString::to_string), pick_list(self.headphone_outputs(), self.headphone_output.clone(), Msg::HeadphoneOutput)
                        .placeholder("Выберите наушники")
                        .text_size(13)
                        .padding([6, 10])
                        .width(Length::Fill)
                        .style(device_style)),
                    self.focus == focus::headphones::OUTPUT,
                ),
            ]
            .spacing(3)
            .width(Length::Fill),
            gear,
        ]
        .spacing(12)
        .align_y(iced::Center);
        let device_card = |content| {
            container(content).padding(10).width(Length::Fill).style(|_| container::Style {
                background: Some(CARD2.into()),
                border: Border { color: EDGE, width: 1.0, radius: 10.0.into() },
                ..Default::default()
            })
        };
        let processing = match self.denoiser.0 {
            3 => "DeepFilterNet (процессор)".to_owned(),
            4 => "Шум убран на входе".to_owned(),
            2 => "Без шумодава".to_owned(),
            _ => format!("NVIDIA Denoiser v{}", self.version),
        };
        let gpu = match &self.gpu {
            Ok((_, name)) => format!("{}, буфер {} мс", name.trim_start_matches("NVIDIA ").trim_start_matches("GeForce "), self.buffer),
            Err(_) => format!("буфер {} мс", self.buffer),
        };
        let output = self.output.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| "Не выбран".into());
        let route_focused = self.focus == ROUTE;
        let route_toggle = button(focus_target(
            row![
                icon(if self.route_open { glyph::DOWN } else { glyph::RIGHT }, 10, DIM),
                label("Обработка и выход", 12, DIM),
                Space::new().width(Length::Fill),
                label(format!("{processing}  ›  {output}"), 12, FAINT),
            ]
            .spacing(8)
            .align_y(iced::Center),
            route_focused,
        ))
        .width(Length::Fill)
        .padding([8, 14])
        .on_press(Msg::RouteToggle)
        .style(move |_, status| button::Style {
            background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| Color::from_rgb8(0x1F, 0x20, 0x23).into()),
            text_color: INK,
            border: Border {
                color: if route_focused { ORANGE } else { Color::TRANSPARENT },
                width: if route_focused { 2.0 } else { 0.0 },
                radius: iced::border::Radius::default().bottom(10.0),
            },
            ..Default::default()
        });
        let fixed = |glyph: &'static str, name: &'static str, main: String, detail: String| {
            row![
                tile(glyph, 38.0, false),
                column![
                    row![icon(glyph::LOCK, 9, FAINT), label(name, 11, FAINT)].spacing(5).align_y(iced::Center),
                    label(main, 13, Color::from_rgb8(0xCF, 0xCB, 0xC2)),
                    label(detail, 11, FAINT),
                ]
                .spacing(1),
            ]
            .spacing(10)
            .align_y(iced::Center)
            .width(Length::Fill)
        };
        let mut devices = column![
            container(row![device_card(mic), device_card(phones)].spacing(12)).padding(12),
            container(Space::new().height(1)).width(Length::Fill).style(|_| container::Style { background: Some(LINE.into()), ..Default::default() }),
            route_toggle,
        ];
        if self.route_open {
            devices = devices.push(
                container(
                    row![
                        fixed(glyph::CHIP, "Обработка", processing.clone(), gpu),
                        icon(glyph::RIGHT, 12, ORANGE),
                        fixed(glyph::OUTPUT, "Выход", output.clone(), "Выберите его микрофоном в Discord".into()),
                    ]
                    .spacing(14)
                    .align_y(iced::Center),
                )
                .padding(iced::Padding { top: 0.0, right: 14.0, bottom: 12.0, left: 14.0 }),
            );
        }
        let devices = container(devices).width(Length::Fill).style(|_| container::Style {
            background: Some(CARD.into()),
            border: Border { color: LINE, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        });

        let before = self.in_peak;
        let after = self.peak;
        // From −72 dB: the raw microphone's hiss (about −60 dB) shows on «До», while what is
        // left after the denoiser (around −80 dB) reads as silence on «После».
        let level = |p: f32| ((db(p) + 72.0) / 72.0).clamp(0.0, 1.0);
        let meters = card(
            column![
                row![label("До", 12, DIM).width(56), tacho::level_meter(level(before), true, Color::from_rgb8(0x8A, 0x8B, 0x92)), container(numbers(db_text(before), 12, DIM)).align_right(64)]
                    .spacing(12)
                    .align_y(iced::Center),
                row![label("После", 12, INK).width(56), tacho::level_meter(level(after), false, if level(after) > 0.93 { ORANGE } else { GREEN }), container(numbers(db_text(after), 12, INK)).align_right(64)]
                    .spacing(12)
                    .align_y(iced::Center),
            ]
            .spacing(10),
        )
        .padding([12, 16]);

        let strength = self.controls.intensity * 100.0;
        let risky = strength > 100.0;
        let noise_card = container(
            column![
                row![label("Шумоподавление", 13, DIM), Space::new().width(Length::Fill)].height(26).align_y(iced::Center),
                frame(
                    tacho(0.0..=200.0, strength, Msg::Intensity, clock).default(40.0).red_above(100.0).segments(20),
                    self.ring(self.focus == INTENSITY),
                ),
            ]
            .push(risky.then(|| {
                row![
                    icon(glyph::WARNING, 12, Color::from_rgb8(0xFF, 0x77, 0x76)),
                    label("Выше 100% могут наблюдаться редкие звуковые неполадки в голосе", 12, Color::from_rgb8(0xFF, 0x77, 0x76)),
                ]
                .spacing(7)
                .align_y(iced::Center)
            }))
            .spacing(10),
        )
        .padding([14, 16])
        .width(Length::Fill);
        // In the red zone the card's background carries the police tape under its content.
        // The tape layer is always in the tree (drawn only in the red zone): adding it on the fly
        // would move the slider in the widget tree and drop a drag that crosses 100%.
        let layers = widget::stack![noise_card].width(Length::Fill).push_under(tacho::caution(clock, risky));
        let noise_card = card(layers).padding(0);
        let hold_card = card(
            column![
                row![
                    label("При удержании клавиши", 13, DIM),
                    Space::new().width(Length::Fill),
                    self.bind_button(12, self.keys[12], self.focus == NOISE_BIND, false, 140.0),
                ]
                .height(26)
                .align_y(iced::Center),
                frame(
                    tacho(0.0..=200.0, self.controls.alternate_intensity * 100.0, Msg::AlternateIntensity, clock)
                        .default(10.0)
                        .red_above(100.0)
                        .segments(20)
                        .phase(20.0 * 64.0 + 128.0),
                    self.ring(self.focus == ALT_INTENSITY),
                ),
            ]
            .spacing(10),
        );
        let mut body = column![title("Шумодав")]
            .push(self.setup_card())
            .push(devices)
            .push(meters)
            .push(row![noise_card, hold_card].spacing(14))
            .push(self.tune_panel())
            .spacing(14);
        // Without NVIDIA the sliders do nothing; say so next to them, not in the error line.
        let note = if self.denoiser.0 == 2 && self.running() {
            Some((format!("Шумодав выключен: {}. Голос, эффекты и виртуальный микрофон работают.", self.denoiser.1), ORANGE))
        } else if self.denoiser.0 == 3 && self.running() {
            Some((format!("Шумодав DeepFilterNet на процессоре (+30 мс). NVIDIA: {}", self.denoiser.1), DIM))
        } else if self.denoiser.0 == 4 && self.running() {
            Some((format!("Шум уже убран на входе ({}): свой шумодав выключен, силу задаёт он.", self.denoiser.1), DIM))
        } else {
            None
        };
        if let Some((note, color)) = note {
            body = body.push(label(note, 12, color));
        }
        if !open {
            return body.into();
        }
        // The headphone settings open over the page, anchored under the gear. A stack is as
        // tall as its first layer, so the page gets room below for the panel's last rows (the
        // mixer hint, a failure and its button); it still fits a 1040×740 window unscrolled.
        let body = body.push(Space::new().height(150));
        let panel = self.headphone_panel();
        widget::stack![
            body,
            container(widget::opaque(panel))
                .padding(iced::Padding { top: 138.0, right: 12.0, ..Default::default() })
                .align_right(Length::Fill),
        ]
        .into()
    }

    /// The setup card on Шумодав (canvas variant C): a tile per step, like the device cards; the
    /// current one lit, with its numbers and bar. The header names the problem and how to solve it,
    /// or the install and about how long it takes. Every failure keeps its fix on its own tile.
    /// Shown only while something is missing, installing or blocks the voice (`setup_visible`). Compact: the
    /// page under it still fits the smallest window (960×680) without scrolling, so names stay
    /// short enough for one line of a tile there.
    fn setup_card(&self) -> Option<Element<'_, Msg>> {
        use components::{Item, Phase, Reason};
        use focus::effects::{SETUP_DRIVER, SETUP_RETRY, SETUP_SETTINGS};
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Done,
            Active,
            Waiting,
            Failed,
            /// Not needed on this PC.
            Off,
        }
        struct Tile {
            mark: Mark,
            glyph: &'static str,
            name: String,
            /// One line: the number or the short state; numeric ones use the numbers face.
            main: String,
            numeric: bool,
            bar: Option<tacho::BarStage>,
            button: Option<(&'static str, Msg, usize)>,
        }
        if !self.setup_visible() {
            return None;
        }
        let status = components::status();
        let rate = components::rate(&self.transfer);
        let arch = self.gpu.as_ref().ok().map(|(arch, _)| arch.as_str()).filter(|a| components::MODEL_ARCHS.contains(a));
        let running = matches!(self.snapshot.state, 2 | 3);
        // What install_core works on; before its first phase, the core if it is missing.
        let current = self.core_installing.then_some(match status.item {
            Item::None if self.core_present => Item::Models,
            Item::None => Item::Core,
            item => item,
        });
        let stalled = self.transfer_moved.elapsed().as_secs();
        let stalled = (current.is_some() && matches!(status.phase, Phase::Connect | Phase::Download) && stalled >= 15).then_some(stalled);
        let failed = !self.core_installing && !self.setup_error.is_empty();
        let why = components::reason(&self.setup_error);
        let why_short = match why {
            Reason::Network => "нет связи с сервером",
            Reason::Disk => "нет места на диске",
            Reason::Corrupt => "файл повредился",
            Reason::Access => "нет доступа к папке",
            Reason::Other => "не установилось",
        };
        let tile = |mark, glyph, name: String| Tile { mark, glyph, name, main: String::new(), numeric: false, bar: None, button: None };
        let with = |mut t: Tile, main: &str| {
            t.main = main.into();
            t
        };
        // The running download of `item`, whichever phase it is in.
        let installing = |glyph, name: String| -> Tile {
            let share = status.share().unwrap_or(0.0);
            let percent = format!("{}%", (share * 100.0) as u8);
            let phase = if status.item == Item::None { Phase::Connect } else { status.phase };
            let mut t = tile(Mark::Active, glyph, name);
            // The speed stands by the time left in the header; a stall is said there too.
            match phase {
                Phase::Connect => {
                    t.main = "подключаемся".into();
                    t.bar = Some(tacho::BarStage::Running(self.epoch));
                }
                Phase::Download => {
                    t.main = format!("{} / {} МБ", status.done / MB, status.total / MB);
                    t.numeric = true;
                    t.bar = Some(tacho::BarStage::Filled(share));
                }
                Phase::Verify | Phase::Unpack => {
                    t.main = format!("{} {percent}", if phase == Phase::Verify { "проверка" } else { "распаковка" });
                    t.bar = Some(tacho::BarStage::Filled(share));
                }
                Phase::Place => {
                    t.main = "установка файлов".into();
                    t.bar = Some(tacho::BarStage::Running(self.epoch));
                }
            }
            t
        };
        let done = |glyph, name: String| {
            let mut t = with(tile(Mark::Done, glyph, name), "готово");
            t.bar = Some(tacho::BarStage::Done);
            t
        };
        let retry = |glyph, name: String| {
            let mut t = with(tile(Mark::Failed, glyph, name), why_short);
            t.button = Some(("Повторить", Msg::RetryCore, SETUP_RETRY));
            t
        };

        let core_name = "Компоненты".to_owned();
        let core = if current == Some(Item::Core) {
            installing(glyph::SAVE, core_name)
        } else if self.core_present {
            done(glyph::SAVE, core_name)
        } else if failed {
            retry(glyph::SAVE, core_name)
        } else {
            with(tile(Mark::Waiting, glyph::SAVE, core_name), "ждёт")
        };
        let models_name = "Модели NVIDIA".to_owned();
        let models = if arch.is_none() {
            with(tile(Mark::Off, glyph::CHIP, models_name), "шумодав на процессоре")
        } else if current == Some(Item::Models) {
            installing(glyph::CHIP, models_name)
        } else if self.models_present {
            done(glyph::CHIP, models_name)
        } else if failed && self.core_present {
            retry(glyph::CHIP, models_name)
        } else {
            with(tile(Mark::Waiting, glyph::CHIP, models_name), "после компонентов")
        };
        let mic_name = "Вирт. микрофон".to_owned();
        let refused = self.driver_error.starts_with("Установка отменена");
        let driver = if self.driver_ready {
            done(glyph::MIC, mic_name)
        } else if self.driver_installing {
            let mut t = with(tile(Mark::Active, glyph::MIC, mic_name), "подтвердите запрос");
            t.bar = Some(tacho::BarStage::Running(self.epoch));
            t
        } else if !self.core_present || self.core_installing {
            with(tile(Mark::Waiting, glyph::MIC, mic_name), "после загрузки")
        } else if !self.driver_error.is_empty() {
            let mut t = with(tile(Mark::Failed, glyph::MIC, mic_name), if refused { "отменено" } else { "не установился" });
            t.button = Some(("Повторить", Msg::InstallDriver, SETUP_DRIVER));
            t
        } else {
            let mut t = with(tile(Mark::Active, glyph::MIC, mic_name), "нужно разрешение");
            t.button = Some(("Установить", Msg::InstallDriver, SETUP_DRIVER));
            t
        };
        let wrong_output = self.output.as_ref().is_none_or(|o| o.id != "TAG" && !o.name.contains("Voicemeeter"));
        let settings = |mut t: Tile| {
            t.button = Some(("Настройки", Msg::Page(2), SETUP_SETTINGS));
            t
        };
        let voice_name = "Обработка голоса".to_owned();
        let start = if running {
            done(glyph::VOLUME, voice_name)
        } else if self.setup_pending() {
            with(tile(Mark::Waiting, glyph::VOLUME, voice_name), "включится сама")
        } else if self.snapshot.state == 5 {
            with(tile(Mark::Failed, glyph::VOLUME, voice_name), "не запустилась")
        } else if self.input.is_none() && self.inputs.is_empty() && self.devices_known {
            settings(with(tile(Mark::Failed, glyph::VOLUME, voice_name), "микрофон не найден"))
        } else if self.input.is_none() {
            with(tile(Mark::Active, glyph::VOLUME, voice_name), "выберите микрофон")
        } else if wrong_output && !self.busy {
            settings(with(tile(Mark::Active, glyph::VOLUME, voice_name), "выберите выход TAG"))
        } else {
            let label = match self.device_state {
                engine::DeviceState::WaitingEndpoint => "ждём устройство",
                engine::DeviceState::Recovering => "восстанавливаем связь",
                engine::DeviceState::UserAction => "нужно действие",
                _ => "запускаем",
            };
            let mut t = with(tile(Mark::Active, glyph::VOLUME, voice_name), label);
            t.bar = Some(tacho::BarStage::Running(self.epoch));
            t
        };

        // The header: the problem and its fix, or the install; in priority of what needs the user.
        let drive = match self.component_root.components().next() {
            Some(std::path::Component::Prefix(prefix)) => format!(" {}", prefix.as_os_str().to_string_lossy()),
            _ => String::new(),
        };
        let (headline, line, alarm): (&str, String, bool) = if failed && !self.core_present {
            ("Установка остановилась", match why {
                Reason::Network => "Нет связи с сервером. Проверьте интернет и нажмите «Повторить», скачанное сохранится.".into(),
                Reason::Disk => format!("Не хватает места на диске{drive}. Освободите место и нажмите «Повторить»."),
                Reason::Corrupt => "Файл повредился при загрузке. Нажмите «Повторить»: он скачается заново.".into(),
                Reason::Access => "Нет доступа к папке компонентов. Нажмите «Повторить»; если не поможет, отправьте логи из настроек.".into(),
                Reason::Other => format!("{}. Нажмите «Повторить».", self.setup_error.trim_end_matches('.')),
            }, true)
        } else if failed {
            ("Модели не скачались", format!("{}. Без моделей шумодав работает на процессоре. Нажмите «Повторить».", capitalized(why_short)), true)
        } else if self.core_installing {
            let what = if current == Some(Item::Models) { "Скачиваем модели NVIDIA" } else { "Скачиваем компоненты" };
            match stalled {
                Some(seconds) => (what, format!("Сервер загрузки молчит {seconds} с. Проверьте интернет: загрузка продолжится сама."), true),
                None => (what, "Без них голос не обработать. Окно можно закрыть: загрузка продолжится в трее.".into(), false),
            }
        } else if self.driver_installing {
            ("Устанавливаем виртуальный микрофон", "Подтвердите запрос Windows: если окна не видно, оно мигает на панели задач.".into(), false)
        } else if refused {
            ("Виртуальный микрофон не установлен", "Windows не дала права администратора. Нажмите «Повторить» и подтвердите запрос.".into(), true)
        } else if !self.driver_error.is_empty() {
            ("Виртуальный микрофон не установлен", format!("{}. Нажмите «Повторить».", self.driver_error.trim_end_matches('.')), true)
        } else if !self.driver_ready {
            ("Нет виртуального микрофона", "Без него голос не дойдёт до Discord. Нажмите «Установить», Windows спросит права.".into(), false)
        } else if self.input.is_none() && self.inputs.is_empty() {
            ("Микрофон не найден", "Подключите микрофон и нажмите «Обновить устройства» в настройках.".into(), true)
        } else if self.input.is_none() {
            ("Микрофон не выбран", "Выберите его в карточке ниже: обработка включится сама.".into(), false)
        } else if wrong_output {
            ("Выход не TAG", "Автозапуск ждёт выход TAG или Voicemeeter: выберите его в настройках.".into(), false)
        } else {
            ("Запускаем обработку голоса", "Это несколько секунд.".into(), false)
        };
        // ponytail: byte phases still ahead are guessed at 150 MB/s each until they run and measure
        // themselves; the models' size is unknown until their download starts.
        let eta = rate.filter(|_| current.is_some() && status.item != Item::None && stalled.is_none()).and_then(|rate| {
            let ahead = match status.phase {
                Phase::Download => 2.0,
                Phase::Verify => 1.0,
                Phase::Unpack => 0.0,
                _ => return None,
            };
            Some(status.total.saturating_sub(status.done) as f64 / rate + ahead * status.total as f64 / 150e6)
        });

        let render = |t: Tile| -> Element<'static, Msg> {
            let (fill, edge, tint, icon_fill) = match t.mark {
                Mark::Done => (Color::from_rgb8(0x1D, 0x1F, 0x1E), Color::from_rgb8(0x2B, 0x3A, 0x2F), GREEN, Color { a: 0.12, ..GREEN }),
                Mark::Active => (Color::from_rgb8(0x24, 0x1F, 0x1B), ORANGE, ORANGE, Color { a: 0.13, ..ORANGE }),
                Mark::Failed => (Color::from_rgb8(0x24, 0x1C, 0x1D), Color { a: 0.6, ..RED }, RED, Color { a: 0.12, ..RED }),
                Mark::Waiting | Mark::Off => (CARD, Color::from_rgb8(0x2A, 0x2B, 0x30), FAINT, Color::from_rgb8(0x25, 0x26, 0x2A)),
            };
            let quiet = matches!(t.mark, Mark::Waiting | Mark::Off);
            let main_color = match t.mark {
                Mark::Done => GREEN,
                Mark::Failed => RED,
                _ if quiet => DIM,
                _ => INK,
            };
            // Icon and name on one line, then the state, then the button or the bar.
            let top = row![
                container(icon(t.glyph, 11, tint)).width(22).height(22).center(22).style(move |_| container::Style {
                    background: Some(icon_fill.into()),
                    border: Border { radius: 7.0.into(), ..Border::default() },
                    ..Default::default()
                }),
                bold(t.name, 13, if quiet { DIM } else { INK }).width(Length::Fill).wrapping(iced::widget::text::Wrapping::None),
            ]
            .push((t.mark == Mark::Done).then(|| icon(glyph::CHECK, 11, GREEN)))
            .spacing(8)
            .align_y(iced::Center);
            let state: Element<'static, Msg> = if t.numeric { numbers(t.main, 13, main_color).into() } else { label(t.main, 12, main_color).into() };
            let bottom: Element<'static, Msg> = match (t.button, t.bar) {
                (Some((text, message, id)), _) => action(label(text, 12, ORANGE_DARK), message, self.focus == id, true).padding([4, 12]).into(),
                (None, Some(stage)) => tacho::progress_bar(stage),
                (None, None) => tacho::progress_bar(tacho::BarStage::Waiting),
            };
            container(column![top, state, Space::new().height(Length::Fill), bottom].spacing(4))
                .padding(8)
                .width(Length::Fill)
                .height(90)
                .clip(true)
                .style(move |_| container::Style {
                    background: Some(fill.into()),
                    border: Border { color: edge, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                })
                .into()
        };
        let speed = rate.filter(|_| status.phase == Phase::Download).map_or(String::new(), |r| format!(" · {}", speed_text(r)));
        let header = row![
            column![bold(headline, 14, INK), label(line, 12, if alarm { RED } else { DIM })]
                .spacing(2)
                .width(Length::Fill),
        ]
        .push(eta.map(|seconds| {
            column![label(format!("осталось{speed}"), 11, FAINT), numbers(eta_text(seconds), 18, ORANGE)]
                .align_x(iced::alignment::Horizontal::Right)
        }))
        .spacing(16)
        .align_y(iced::Center);
        Some(
            card(column![header, row![render(core), render(models), render(driver), render(start)].spacing(8)].spacing(8))
                .padding([10, 14])
                .into(),
        )
    }

    /// «Подбор под микрофон» under the strength sliders: finds the weakest strength that silences
    /// this microphone's room (a quiet sweep, then a spoken check), keeps it per microphone and
    /// puts it back when the microphone changes; «Послушать себя» turns on the full-voice monitor.
    fn tune_panel(&self) -> Element<'_, Msg> {
        use focus::effects::{TUNE, TUNE_LISTEN, TUNE_UNDO};
        use tune::Phase;
        let now = Instant::now();
        let listening = self.monitor_all && matches!(self.monitor, 1 | 2);
        let ready = self.tune_ready();
        let running = self.tune.as_ref().is_some_and(tune::Tune::running);
        let mic = self.input.as_ref().map_or("микрофон не выбран".to_owned(), |d| d.name.clone());
        let saved = self.input.as_ref().and_then(|d| self.profile(&d.id));

        let listen = action(
            row![icon(glyph::HEADPHONES, 12, if listening { ORANGE_DARK } else { INK }), label(if listening { "Слушаю себя" } else { "Послушать себя" }, 13, if listening { ORANGE_DARK } else { INK })]
                .spacing(8)
                .align_y(iced::Center),
            Msg::TuneListen,
            self.focus == TUNE_LISTEN,
            listening,
        )
        .on_press_maybe(matches!(self.snapshot.state, 2 | 3).then_some(Msg::TuneListen));
        let tune_label = match &self.tune {
            Some(t) if t.running() => "Остановить",
            Some(t) if t.phase == Phase::Done => "Ещё раз",
            _ if saved.is_some() => "Подобрать заново",
            _ => "Подобрать",
        };
        let tune_button = action(label(tune_label, 13, if running { INK } else { ORANGE_DARK }), if running { Msg::TuneStop } else { Msg::TuneStart }, self.focus == TUNE, !running)
            .on_press_maybe((running || ready.is_ok()).then_some(if running { Msg::TuneStop } else { Msg::TuneStart }));
        let header = row![
            icon(glyph::MIC, 13, ORANGE),
            bold("Подбор под микрофон", 13, INK),
            label(mic, 12, FAINT).wrapping(iced::widget::text::Wrapping::None),
            Space::new().width(Length::Fill),
            listen,
            tune_button,
        ]
        .spacing(10)
        .align_y(iced::Center);

        // The three steps, when a tune runs or has run.
        let steps = |active: usize, failed: bool| -> Element<'static, Msg> {
            let names = ["Тишина", "Голос", "Готово"];
            let mut chips = row![].spacing(6).align_y(iced::Center);
            for (i, name) in names.into_iter().enumerate() {
                let (glyph_color, text_color) = if failed && i == active {
                    (RED, RED)
                } else if i < active || (i == 2 && active == 2) {
                    (GREEN, DIM)
                } else if i == active {
                    (ORANGE, INK)
                } else {
                    (EDGE, FAINT)
                };
                if i > 0 {
                    chips = chips.push(icon(glyph::RIGHT, 8, FAINT));
                }
                let mark: Element<'static, Msg> = if i < active || (i == 2 && active == 2 && !failed) {
                    icon(glyph::CHECK, 10, glyph_color).into()
                } else {
                    container(Space::new().width(6).height(6)).style(move |_| container::Style { background: Some(glyph_color.into()), border: Border { radius: 3.0.into(), ..Border::default() }, ..Default::default() }).into()
                };
                chips = chips.push(row![mark, label(name, 12, text_color)].spacing(5).align_y(iced::Center));
            }
            chips.into()
        };
        let left: Element<'_, Msg> = match &self.tune {
            Some(t) if t.phase == Phase::Quiet => column![
                steps(0, false),
                row![bold("Помолчите", 15, INK), numbers(format!("{} с", t.left(now)), 15, ORANGE)].spacing(8).align_y(iced::Center),
                label("Фон как обычно: вентилятор, комната. Ползунок сам проходит силы.", 12, DIM),
            ]
            .spacing(6)
            .into(),
            Some(t) if t.phase == Phase::Voice => column![
                steps(1, false),
                row![bold("Скажите пару слов", 15, INK), numbers(format!("{} с", t.left(now)), 15, ORANGE)].spacing(8).align_y(iced::Center),
                label("Обычным голосом, например: «раз, два, три, проверка».", 12, DIM),
            ]
            .spacing(6)
            .into(),
            Some(t) if t.phase == Phase::Done => {
                let residual = t.chosen_residual().map_or(String::new(), |r| format!(", на {} % стал {:.0} dB", t.chosen, r));
                let voice = match (t.voice_drop, t.backed_off) {
                    (_, true) => "Голос подсаживался, поэтому на шаг мягче.",
                    (Some(_), false) => "Голос проходит без потерь.",
                    (None, _) => "Голоса не было слышно: подобрано по тишине.",
                };
                column![
                    steps(2, false),
                    row![
                        numbers(format!("{} %", t.chosen), 18, GREEN),
                        label(format!("фон {:.0} dB{residual}", t.noise_db), 12, DIM),
                        Space::new().width(Length::Fill),
                        action(label(format!("Вернуть {:.0} %", t.previous * 100.0), 12, INK), Msg::TuneUndo, self.focus == TUNE_UNDO, false).padding([3, 10]),
                    ]
                    .spacing(10)
                    .align_y(iced::Center),
                    label(format!("{voice} Сила сохранена для этого микрофона."), 12, DIM),
                ]
                .spacing(6)
                .into()
            }
            Some(tune::Tune { phase: Phase::Failed(reason), .. }) => column![
                steps(if reason.contains("тишины") { 0 } else { 1 }, true),
                label(reason.clone(), 12, RED),
            ]
            .spacing(6)
            .into(),
            _ => match (ready, saved) {
                (Err(why), _) => label(why, 12, FAINT).into(),
                (Ok(()), Some(strength)) => column![
                    row![numbers(format!("{:.0} %", strength * 100.0), 18, ORANGE), label("подобрано для этого микрофона", 12, DIM)].spacing(10).align_y(iced::Center),
                    label("При смене микрофона сила ставится сама. Послушайте себя, чтобы проверить на слух.", 12, FAINT),
                ]
                .spacing(6)
                .into(),
                (Ok(()), None) => label("Помолчите 4 секунды, потом скажите пару слов: Mic Noize найдёт самую мягкую силу, при которой фон пропадает, и запомнит её для этого микрофона.", 12, DIM).into(),
            },
        };
        let (curve, chosen, current) = match &self.tune {
            Some(t) => (t.curve.clone(), (t.phase == Phase::Done).then_some(t.chosen), (t.phase == Phase::Quiet).then(|| t.strength())),
            None => (Vec::new(), None, None),
        };
        card(
            column![
                header,
                row![
                    container(left).width(Length::Fill),
                    container(tacho::tune_curve(curve, &tune::STEPS, chosen, current, tune::SILENT_DB)).width(300),
                ]
                .spacing(18)
                .align_y(iced::Center),
            ]
            .spacing(10),
        )
        .padding([12, 16])
        .into()
    }

    /// Что слышите вы: processing of the sound you hear, opened from the gear.
    fn headphone_panel(&self) -> Element<'_, Msg> {
        use focus::headphones::*;
        let clock = self.clock();
        let live = matches!(self.headphone_state, 1 | 2) || self.headphone_busy;
        let rowl = panel_row;
        let named = |name: &'static str| -> Element<'static, Msg> { label(name, 13, DIM).into() };
        let noise_name: Element<'static, Msg> = row![label("Шумодав", 13, DIM), Space::new().width(Length::Fill)].into();
        let mut content = column![
            row![
                icon(glyph::HEADPHONES, 15, ORANGE),
                bold("Что слышите вы", 13, INK),
                label("звук приложений", 12, FAINT),
                Space::new().width(Length::Fill),
                caption_button(glyph::CLOSE, Msg::HeadphonePanel(false), false).width(28).height(28),
            ]
            .spacing(8)
            .align_y(iced::Center),
            container(Space::new().height(1)).width(Length::Fill).style(|_| container::Style { background: Some(Color::from_rgb8(0x2E, 0x2F, 0x34).into()), ..Default::default() }),
            row![
                label(if live { "Обработка наушников включена" } else { "Обработка наушников выключена" }, 12, if live { GREEN } else { FAINT }),
                Space::new().width(Length::Fill),
                action(label(if live { "Остановить" } else { "Включить" }, 13, if live { INK } else { ORANGE_DARK }), Msg::HeadphoneToggle, self.focus == TOGGLE, !live),
            ]
            .align_y(iced::Center),
            rowl(
                row![noise_name, frame(switch(self.headphone_denoise, Msg::HeadphoneNoise, !self.headphone_busy), self.focus == NOISE)].align_y(iced::Center).into(),
                frame(
                    tacho(0.0..=200.0, self.headphone_intensity * 100.0, Msg::HeadphoneIntensity, clock)
                        .default(80.0)
                        .red_above(100.0)
                        .segments(16)
                        .compact()
                        .phase(3000.0)
                        .enabled(self.headphone_denoise),
                    self.ring(self.focus == INTENSITY),
                ),
            ),
            rowl(
                named("Громкость"),
                frame(
                    tacho(0.0..=100.0, self.headphone_volume * 100.0, Msg::HeadphoneVolume, clock).default(70.0).segments(16).compact().phase(4200.0),
                    self.ring(self.focus == VOLUME),
                ),
            ),
            rowl(
                named("Высота"),
                frame(
                    tacho(-12.0..=12.0, self.headphone_pitch as f32, Msg::HeadphonePitch, clock)
                        .default(0.0)
                        .origin(0.0)
                        .segments(16)
                        .compact()
                        .phase(5400.0)
                        .format(|v| format!("{:+.0} пт", v).replace('-', "−")),
                    self.ring(self.focus == PITCH),
                ),
            ),
            rowl(
                row![label("Реверс", 13, DIM), Space::new().width(Length::Fill), frame(switch(self.headphone_reverse, Msg::HeadphoneReverse, true), self.focus == REVERSE)].align_y(iced::Center).into(),
                label("куски по 0,2 с задом наперёд, +200 мс", 12, FAINT).into(),
            ),
        ]
        .spacing(10);
        if self.headphone_needs_lines() {
            content = content
                .push(label("Наушникам не хватило линии в виртуальном драйвере: на новой установке её занимает стандартная линия TAG. Освободите её, звук микрофона не пострадает.", 12, RED))
                .push(action(label("Освободить место", 13, ORANGE_DARK), Msg::HeadphoneLines, self.focus == LINES, true));
        } else if !self.headphone_message.is_empty() {
            content = content.push(label(&self.headphone_message, 12, RED));
        }
        content = content.push(label("Выход в микшере Windows: Mic Noize Headphones. После остановки верните физические наушники.", 11, FAINT));
        let panel = container(content)
            .padding([14, 16])
            .width(410)
            .style(|_| container::Style {
                background: Some(Color::from_rgb8(0x1F, 0x20, 0x23).into()),
                border: Border { color: EDGE, width: 1.0, radius: 12.0.into() },
                ..Default::default()
            });
        // A dark ring instead of a drop shadow: tiny-skia repaints shadows over partial redraws.
        container(panel)
            .padding(3)
            .style(|_| container::Style {
                background: Some(Color { a: 0.45, ..Color::BLACK }.into()),
                border: Border { radius: 15.0.into(), ..Border::default() },
                ..Default::default()
            })
            .into()
    }

    /// Эффекты: one aligned row per hold effect, then monitoring, Discord level and replays.
    fn effects_view(&self) -> Element<'_, Msg> {
        use focus::effects::*;
        let clock = self.clock();
        let grid = effect_grid;
        let head = grid(
            Space::new().into(),
            label("Эффект", 12, FAINT).into(),
            label("Сила", 12, FAINT).into(),
            row![icon(glyph::MIC, 11, FAINT), label("Мой голос", 12, FAINT)].spacing(6).align_y(iced::Center).into(),
            row![icon(glyph::OUTPUT, 11, FAINT), label("Голоса Discord", 12, FAINT)].spacing(6).align_y(iced::Center).into(),
        )
        .padding([0, 13]);
        let mut rows = column![head].spacing(6);
        for &i in EFFECT_GROUPS[self.effects_group] {
            let activity = match i { 1 => Some((1u32 << 1, 1u32 << 6)), 5..=8 => Some((1u32 << (i + 5), 1u32 << (i + 9))), _ => None };
            let (name, glyph, sub, control, bind_focus, active): (&str, &str, Element<'_, Msg>, Element<'_, Msg>, usize, bool) = match i {
                0 => (
                    "Усиление",
                    glyph::BOOST,
                    row![frame(switch(self.controls.overload, Msg::Overload, true), self.focus == OVERLOAD), label("перегрузка", 11, FAINT)]
                        .spacing(2)
                        .align_y(iced::Center)
                        .into(),
                    frame(
                        tacho(100.0..=2000.0, self.controls.boost * 100.0, Msg::Boost, clock)
                            .default(167.0)
                            .red_above(1600.0)
                            .segments(16)
                            .compact()
                            .phase(0.0)
                            .overdrive(self.controls.overload),
                        self.ring(self.focus == BOOST),
                    ),
                    BOOST_BIND,
                    self.snapshot.boost_active != 0,
                ),
                1 => (
                    "Formant Shift",
                    glyph::NOTE,
                    action(label("тон · форманты", 11, DIM), Msg::EffectDetails(1), self.focus == DETAIL_BASE + 1, false).into(),
                    frame(
                        tacho(-12.0..=12.0, self.controls.pitch as f32, Msg::Pitch, clock)
                            .default(-6.0)
                            .origin(0.0)
                            .segments(16)
                            .compact()
                            .phase(150.0)
                            .format(|v| format!("{:+.0}", v).replace('-', "−")),
                        self.ring(self.focus == PITCH),
                    ),
                    PITCH_BIND,
                    self.effect_activity & ((1 << 1) | (1 << 6)) != 0,
                ),
                // Mirrored so that, as on every other slider, more orange means a stronger
                // effect: ×0.50 (slowest) sits on the right.
                2 => (
                    "Замедление",
                    glyph::SLOW,
                    label("запись, пока держите", 11, FAINT).into(),
                    frame(
                        tacho(-95.0..=-50.0, -self.controls.slow * 100.0, |v| Msg::Slow(-v), clock)
                            .default(-67.0)
                            .segments(16)
                            .compact()
                            .phase(300.0)
                            .format(|v| format!("×{:.2}", -v / 100.0)),
                        self.ring(self.focus == SLOW),
                    ),
                    SLOW_BIND,
                    (1..=8).contains(&self.phrase_state) && self.phrase_state % 2 == 1,
                ),
                3 => (
                    "Ускорение",
                    glyph::FAST,
                    label("запись, пока держите", 11, FAINT).into(),
                    frame(
                        tacho(105.0..=200.0, self.controls.fast * 100.0, Msg::Fast, clock)
                            .default(167.0)
                            .segments(16)
                            .compact()
                            .phase(450.0)
                            .format(|v| format!("×{:.2}", v / 100.0)),
                        self.ring(self.focus == FAST),
                    ),
                    FAST_BIND,
                    (1..=8).contains(&self.phrase_state) && self.phrase_state % 2 == 0,
                ),
                4 => (
                    "Реверс",
                    glyph::REVERSE,
                    label("после отпускания", 11, FAINT).into(),
                    self.reverse_demo(),
                    REVERSE_BIND,
                    self.phrase_state >= 9,
                ),
                5 => (
                    "Эхо", glyph::REPEAT,
                    action(label("настроить", 11, DIM), Msg::EffectDetails(5), self.focus == DETAIL_BASE + 5, false).into(),
                    frame(tacho(60.0..=600.0, self.controls.effects.echo_delay_ms as f32,
                        |v| Msg::EffectOption(0, v), clock).default(220.0).compact()
                        .format(|v| format!("{v:.0} мс")), self.ring(self.focus == OPTION_BASE)),
                    NEW_MIC_BIND_BASE, self.effect_activity & ((1 << 10) | (1 << 14)) != 0,
                ),
                6 => (
                    "Застревание", glyph::REPEAT,
                    action(label("как работает", 11, DIM), Msg::EffectDetails(6), self.focus == DETAIL_BASE + 6, false).into(),
                    frame(tacho(50.0..=300.0, self.controls.effects.stutter_ms as f32,
                        |v| Msg::EffectOption(4, v), clock).default(120.0).compact()
                        .format(|v| format!("{v:.0} мс")), self.ring(self.focus == OPTION_BASE + 4)),
                    NEW_MIC_BIND_BASE + 1, self.effect_activity & ((1 << 11) | (1 << 15)) != 0,
                ),
                7 => (
                    "Granular", glyph::NOTE,
                    action(label("настроить", 11, DIM), Msg::EffectDetails(7), self.focus == DETAIL_BASE + 7, false).into(),
                    frame(tacho(30.0..=150.0, self.controls.effects.grain_ms as f32,
                        |v| Msg::EffectOption(5, v), clock).default(80.0).compact()
                        .format(|v| format!("{v:.0} мс")), self.ring(self.focus == OPTION_BASE + 5)),
                    NEW_MIC_BIND_BASE + 2, self.effect_activity & ((1 << 12) | (1 << 16)) != 0,
                ),
                _ => (
                    "AutoTune", glyph::NOTE,
                    action(label("тональность · гамма", 11, DIM), Msg::EffectDetails(8), self.focus == DETAIL_BASE + 8, false).into(),
                    frame(tacho(5.0..=150.0, self.controls.effects.tune_speed_ms as f32,
                        |v| Msg::EffectOption(10, v), clock).default(80.0).compact()
                        .format(|v| format!("{v:.0} мс")), self.ring(self.focus == OPTION_BASE + 10)),
                    NEW_MIC_BIND_BASE + 3, self.effect_activity & ((1 << 13) | (1 << 17)) != 0,
                ),
            };
            let binding = |discord: bool| {
                let lit = activity.map_or(active && self.discord_source == discord,
                    |(mic, disc)| self.effect_activity & (if discord { disc } else { mic }) != 0);
                let target = if i<5 { i + if discord { 5 } else { 0 } }
                    else { i + if discord { 12 } else { 8 } };
                self.bind_button(
                    target,
                    self.keys[target],
                    self.focus == if discord { if i<5 { DISCORD_BIND_BASE + i } else { NEW_DISCORD_BIND_BASE + i - 5 } } else { bind_focus },
                    lit,
                    150.0,
                )
            };
            let line = grid(
                container(icon(glyph, 16, if active { ORANGE } else { Color::from_rgb8(0xCF, 0xCB, 0xC2) }))
                    .width(36)
                    .height(36)
                    .center(36)
                    .style(move |_| container::Style {
                        background: Some((if active { Color { a: 0.14, ..ORANGE } } else { Color::from_rgb8(0x25, 0x26, 0x2A) }).into()),
                        border: Border { radius: 8.0.into(), ..Border::default() },
                        ..Default::default()
                    })
                    .into(),
                column![bold(name, 14, INK), sub].spacing(2).into(),
                control,
                binding(false),
                binding(true),
            );
            rows = rows.push(
                container(line)
                    .padding([8, 12])
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some((if active { LIVE_BG } else { CARD }).into()),
                        border: Border { color: if active { Color { a: 0.45, ..ORANGE } } else { LINE }, width: 1.0, radius: 10.0.into() },
                        ..Default::default()
                    }),
            );
            if self.effect_details == Some(i) {
                rows = rows.push(self.effect_detail(i));
            }
        }
        // Values mirror mic::PhraseEffect::State in src/effects.hpp: 1/2 record slow/fast,
        // 3/4 play slow/fast, 5/6 tail capture, 7/8 limit reached, 9 record reverse,
        // 10 reverse pause, 11 reverse limit, 12 play reverse.
        let status = match self.phrase_state {
            1 | 2 | 9 => format!("Запись {:.1} / 10 с · отпустите хоткей", self.phrase_seconds),
            10 => "Пауза 0,15 с перед реверсом…".into(),
            3 | 4 | 12 => format!("Воспроизведение · осталось {:.1} с", self.phrase_seconds),
            5 | 6 => "Завершаем последний слог…".into(),
            7 | 8 | 11 => "Записано 10 с · отпустите хоткей".into(),
            _ => String::new(),
        };
        if self.phrase_state != 0 {
            rows = rows.push(
                row![
                    label(status, 12, ORANGE).width(Length::Fill),
                    action(label("Отмена", 12, INK), Msg::CancelPhrase, self.focus == CANCEL_PHRASE, false),
                ]
                .spacing(8)
                .align_y(iced::Center),
            );
        }
        // A closed Discord is no fault: its hotkeys just have nothing to catch. The engine keeps
        // looking for it, and the note came and went with every try.
        if self.discord_state == 3 && !self.discord_message.contains("Откройте приложение Discord") {
            rows = rows.push(label(&self.discord_message, 12, RED));
        }
        let full_monitor = if self.monitor_all { self.monitor } else { 0 };
        let can_monitor = !self.busy && !self.quitting && matches!(self.snapshot.state, 2 | 3);
        let monitor_note = if self.monitor_all && matches!(self.monitor, 1 | 2) {
            "Сейчас слышен весь голос; режим эффектов сохранён."
        } else if self.effect_monitoring() && self.monitor == 1 {
            "Подключение наушников…"
        } else if self.effect_monitoring() && self.monitor == 3 {
            "Ошибка прослушивания — см. сообщение сверху."
        } else {
            ""
        };
        let mut hear = column![
            heading_row(glyph::HEADPHONES, "Слышать себя", Space::new().into()),
            row![
                action(
                    label(
                        match full_monitor {
                            1 => "Подключение…",
                            2 => "Слышу весь голос",
                            _ => "Весь голос",
                        },
                        13,
                        if full_monitor == 2 { ORANGE_DARK } else if can_monitor { INK } else { FAINT },
                    ),
                    Msg::Monitor,
                    self.focus == MONITOR,
                    full_monitor == 2,
                )
                .on_press_maybe(can_monitor.then_some(Msg::Monitor)),
                self.bind_button(10, self.keys[10], self.focus == MONITOR_BIND, false, 140.0),
            ]
            .spacing(8)
            .align_y(iced::Center),
            row![
                frame(switch(self.effects_monitor, Msg::EffectsMonitor, true), self.focus == EFFECTS_MONITOR),
                label("эффекты", 12, DIM),
                Space::new().width(16),
                frame(switch(self.boost_monitor, Msg::BoostMonitor, true), self.focus == BOOST_MONITOR),
                label("усиление", 12, DIM),
            ]
            .spacing(4)
            .align_y(iced::Center),
        ]
        .spacing(8);
        if !monitor_note.is_empty() {
            hear = hear.push(label(monitor_note, 11, FAINT));
        }
        let discord = column![
            heading_row(glyph::VOLUME, "Громкость Discord", label("ваш голос не трогает", 11, FAINT).into()),
            frame(
                tacho(0.0..=DISCORD_VOLUME_MAX_PERCENT, discord_volume_percent(self.controls.discord_volume), Msg::DiscordVolume, clock)
                    .default(100.0)
                    .segments(18)
                    .compact()
                    .phase(600.0),
                self.ring(self.focus == DISCORD_VOLUME),
            ),
        ]
        .spacing(8);
        let replay = column![
            heading_row(glyph::REPEAT, "Повтор последнего", self.bind_button(11, self.keys[11], self.focus == REPLAY_BIND, false, 140.0)),
            self.clips_view(),
        ]
        .spacing(8)
        .width(Length::FillPortion(135));
        let bottom = card(
            row![column![hear, discord].spacing(14).width(Length::FillPortion(100)), replay].spacing(24),
        )
        .padding([13, 16]);
        column![row![title("Эффекты"), Space::new().width(Length::Fill), self.effect_groups()].align_y(iced::Center), rows, bottom].spacing(14).into()
    }

    /// Whether effect row `i` is sounding now (from either source).
    fn effect_live(&self, i: usize) -> bool {
        match i {
            0 => self.snapshot.boost_active != 0,
            1 => self.effect_activity & ((1 << 1) | (1 << 6)) != 0,
            2 => (1..=8).contains(&self.phrase_state) && self.phrase_state % 2 == 1,
            3 => (1..=8).contains(&self.phrase_state) && self.phrase_state % 2 == 0,
            4 => self.phrase_state >= 9,
            // Echo, stutter, granular, autotune: bits i + 5 (microphone) and i + 9 (Discord).
            _ => self.effect_activity & ((1 << (i + 5)) | (1 << (i + 9))) != 0,
        }
    }

    /// The effects page's group switch, in the title row. A group whose effect is sounding while
    /// the other is shown lights up, so a hotkey never goes unseen.
    fn effect_groups(&self) -> Element<'_, Msg> {
        use focus::effects::GROUP_BASE;
        let mut switch = row![].spacing(2);
        for (group, name) in ["Голос вживую", "Фразы и повторы"].into_iter().enumerate() {
            let chosen = self.effects_group == group;
            let live = EFFECT_GROUPS[group].iter().any(|&i| self.effect_live(i));
            let focused = self.focus == GROUP_BASE + group;
            let count = numbers(EFFECT_GROUPS[group].len().to_string(), 11, if chosen { ORANGE } else { FAINT });
            let dot = container(Space::new().width(6).height(6)).style(move |_| container::Style {
                background: Some((if live { ORANGE } else { Color::TRANSPARENT }).into()),
                border: Border { radius: 3.0.into(), ..Border::default() },
                ..Default::default()
            });
            switch = switch.push(
                button(focus_target(row![dot, label(name, 13, if chosen { INK } else { DIM }), count].spacing(7).align_y(iced::Center), focused))
                    .padding([6, 12])
                    .on_press(Msg::EffectsGroup(group))
                    .style(move |_, status| {
                        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
                        button::Style {
                            background: Some((if chosen { HOVER } else if live { LIVE_BG } else if hover { CARD2 } else { Color::TRANSPARENT }).into()),
                            text_color: INK,
                            border: Border {
                                color: if focused { ORANGE } else if live && !chosen { Color { a: 0.5, ..ORANGE } } else { Color::TRANSPARENT },
                                width: if focused { 2.0 } else { 1.0 },
                                radius: 7.0.into(),
                            },
                            ..Default::default()
                        }
                    }),
            );
        }
        container(switch)
            .padding(3)
            .style(|_| container::Style {
                background: Some(CARD.into()),
                border: Border { color: LINE, width: 1.0, radius: 10.0.into() },
                ..Default::default()
            })
            .into()
    }

    fn effect_detail(&self, row: usize) -> Element<'_, Msg> {
        use focus::effects::OPTION_BASE;
        let options: &[(usize, &str)] = match row {
            1 => &[(12, "Форманты")],
            5 => &[(1, "Повторы"), (2, "Затухание"), (3, "Уровень")],
            7 => &[(6, "Разброс"), (7, "Тон зерна")],
            8 => &[(8, "Тоника"), (9, "Гамма"), (11, "Сила")],
            _ => &[],
        };
        if options.is_empty() {
            return card(label("При нажатии берутся последние 50–300 мс; фрагмент повторяется до отпускания.", 12, DIM))
                .padding([12, 16]).into();
        }
        let mut controls = row![].spacing(12);
        for &(index, name) in options {
            let (min, max) = super::engine::EffectOptions::RANGES[index];
            let knob = tacho(min as f32..=max as f32, self.controls.effects.value(index) as f32,
                move |v| Msg::EffectOption(index, v), self.clock())
                .default(super::engine::EffectOptions::default().value(index) as f32)
                .compact()
                .format(move |v| match index {
                    8 => ["C","C♯","D","D♯","E","F","F♯","G","G♯","A","A♯","B"][(v.round() as usize).min(11)].into(),
                    9 => ["Хроматика","Мажор","Минор"][(v.round() as usize).min(2)].into(),
                    7 | 12 => format!("{v:+.0}"),
                    1 => format!("{v:.0}"),
                    6 => format!("{v:.0} мс"),
                    _ => format!("{v:.0} %"),
                });
            controls = controls.push(column![label(name, 11, DIM), frame(knob, self.ring(self.focus == OPTION_BASE + index))]
                .spacing(4).width(Length::Fill));
        }
        card(controls).padding([12, 16]).into()
    }

    /// Reverse row: a practice word that turns around; click it to type your own.
    fn reverse_demo(&self) -> Element<'_, Msg> {
        if self.reverse_edit {
            return frame(
                repaint(&self.reverse_word, text_input("ваше слово", &self.reverse_word)
                    .id("reverse-word")
                    .size(14)
                    .padding([4, 8])
                    .width(180)
                    .on_input(Msg::ReverseWord)
                    .on_submit(Msg::ReverseEdit(false))
                    .style(input_style)),
                self.focus == focus::effects::REVERSE_WORD,
            );
        }
        frame(
            button(tacho::reverse_word(&self.reverse_word, self.clock()))
                .padding([2, 4])
                .on_press(Msg::ReverseEdit(true))
                .style(|_, status| button::Style {
                    background: matches!(status, button::Status::Hovered | button::Status::Pressed)
                        .then(|| Color::from_rgb8(0x22, 0x23, 0x27).into()),
                    text_color: INK,
                    border: Border { radius: 6.0.into(), ..Border::default() },
                    ..Default::default()
                }),
            self.focus == focus::effects::REVERSE_WORD,
        )
    }

    /// The newest recording large, the other five as chips; each can be played and saved.
    fn clips_view(&self) -> Element<'_, Msg> {
        use focus::effects::*;
        if self.clips.is_empty() {
            return label("Появятся после удержания голосового эффекта.", 12, FAINT).into();
        }
        let (playing, position, length) = self.sound_playing;
        let progress = |i: usize| -> Option<f32> {
            (playing == Self::clip_id(i)).then(|| if length > 0.0 { (position / length).clamp(0.0, 1.0) } else { 0.0 })
        };
        let save_button = |i: usize| {
            let focused = self.focus == CLIP_BASE + 2 * i + 1;
            let chosen = self.clip_menu == Some(i);
            button(focus_target(container(icon(glyph::SAVE, 12, if chosen { ORANGE_DARK } else { DIM })).center(Length::Fill), focused))
                .width(30)
                .height(Length::Fill)
                .padding(0)
                .on_press(Msg::ClipMenu(Some(i)))
                .style(move |_, status| {
                    let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
                    button::Style {
                        background: Some((if chosen || hover { ORANGE } else { Color::TRANSPARENT }).into()),
                        text_color: INK,
                        border: Border { color: if focused { ORANGE } else { Color::TRANSPARENT }, width: 2.0, radius: 6.0.into() },
                        ..Default::default()
                    }
                })
        };
        let hero = {
            let clip = &self.clips[0];
            let p = progress(0);
            let failed = matches!(clip.state, SoundState::Failed(_));
            let focused = self.focus == CLIP_BASE;
            let play = button(focus_target(
                container(icon(if p.is_some() { glyph::STOP } else { glyph::PLAY }, 14, ORANGE_DARK)).center(Length::Fill),
                focused,
            ))
            .width(40)
            .height(40)
            .padding(0)
            .on_press(Msg::ClipPlay(0))
            .style(move |_, status| button::Style {
                background: Some((if matches!(status, button::Status::Hovered | button::Status::Pressed) { Color::from_rgb8(0xFF, 0xB0, 0x70) } else { ORANGE }).into()),
                text_color: ORANGE_DARK,
                border: Border { color: if focused { INK } else { Color::TRANSPARENT }, width: 2.0, radius: 8.0.into() },
                ..Default::default()
            });
            let seconds = match clip.state {
                SoundState::Loaded(s) => format!("{s:.1} с").replace('.', ","),
                _ => String::new(),
            };
            let bar = waveform(&clip.name, p);
            container(
                row![
                    play,
                    column![
                        numbers(super::clip_label(&clip.name), 14, if failed { RED } else { INK }),
                        label(if seconds.is_empty() { "последняя запись".to_owned() } else { format!("последняя · {seconds}") }, 11, FAINT),
                    ]
                    .spacing(2)
                    .width(118),
                    container(bar).width(Length::Fill).center_y(40),
                    save_button(0),
                ]
                .spacing(12)
                .height(40)
                .align_y(iced::Center),
            )
            .padding([8, 10])
            .style(move |_| container::Style {
                background: Some((if p.is_some() { LIVE_BG } else { CARD2 }).into()),
                border: Border { color: if p.is_some() { Color { a: 0.6, ..ORANGE } } else { Color::from_rgb8(0x2E, 0x2F, 0x34) }, width: 1.0, radius: 10.0.into() },
                ..Default::default()
            })
        };
        let chip = |i: usize| -> Element<'_, Msg> {
            let clip = &self.clips[i];
            let p = progress(i);
            let focused = self.focus == CLIP_BASE + 2 * i;
            let failed = matches!(clip.state, SoundState::Failed(_));
            let play = button(focus_target(
                row![
                    icon(if p.is_some() { glyph::STOP } else { glyph::PLAY }, 9, if p.is_some() { ORANGE } else { DIM }),
                    numbers(super::clip_label(&clip.name), 13, if failed { RED } else if p.is_some() { ORANGE } else { INK }),
                ]
                .spacing(8)
                .align_y(iced::Center),
                focused,
            ))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([0, 10])
            .on_press(Msg::ClipPlay(i))
            .style(move |_, status| button::Style {
                background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| HOVER.into()),
                text_color: INK,
                border: Border { color: if focused { ORANGE } else { Color::TRANSPARENT }, width: 2.0, radius: 6.0.into() },
                ..Default::default()
            });
            container(row![play, save_button(i)].height(34))
                .style(move |_| container::Style {
                    background: Some((if p.is_some() { LIVE_BG } else { CARD2 }).into()),
                    border: Border { color: if p.is_some() { Color { a: 0.6, ..ORANGE } } else { Color::from_rgb8(0x2E, 0x2F, 0x34) }, width: 1.0, radius: 7.0.into() },
                    ..Default::default()
                })
                .width(Length::Fill)
                .into()
        };
        let mut list = column![hero].spacing(6);
        match self.clip_menu.filter(|i| *i < self.clips.len()) {
            Some(i) => {
                list = list.push(
                    row![
                        label(format!("Сохранить «{}»", super::clip_label(&self.clips[i].name)), 12, DIM),
                        action(label("В саундпад", 12, INK), Msg::ClipSave(i, true), self.focus == CLIP_TO_SOUNDPAD, false),
                        action(label("В папку…", 12, INK), Msg::ClipSave(i, false), self.focus == CLIP_TO_FOLDER, false),
                        caption_button(glyph::CLOSE, Msg::ClipMenu(None), false).width(30).height(30),
                    ]
                    .spacing(6)
                    .align_y(iced::Center),
                );
            }
            None if !self.clip_note.is_empty() => {
                let saved = self.clip_note.starts_with("Сохранено");
                let hint = self.clip_note.starts_with("Выберите");
                list = list.push(
                    row![
                        icon(if saved { glyph::CHECK } else { glyph::WARNING }, 12, if saved { GREEN } else if hint { DIM } else { RED }),
                        label(&self.clip_note, 12, if saved { GREEN } else if hint { DIM } else { RED }),
                    ]
                    .spacing(8)
                    .align_y(iced::Center),
                );
            }
            None => {}
        }
        let others: Vec<usize> = (1..self.clips.len()).collect();
        for line in others.chunks(3) {
            let mut cells = row![].spacing(6);
            for &i in line {
                cells = cells.push(chip(i));
            }
            for _ in line.len()..3 {
                cells = cells.push(Space::new().width(Length::Fill));
            }
            list = list.push(cells);
        }
        list.into()
    }

    fn rvc_view(&self) -> Element<'_, Msg> {
        use focus::rvc::*;
        let clock = self.clock();
        let rvc_status = match self.snapshot.rvc_state {
            1 => "Загрузка / буферизация модели…".into(),
            2 => format!(
                "Активно · задержка ~{} мс · модель {:.0} мс",
                self.controls.rvc_options.chunk + RVC_SLACK_MS,
                self.snapshot.rvc_latency_ms
            ),
            3 => "Модель не успевает · пауза".into(),
            _ if self.controls.rvc => "Включится вместе с обработкой".into(),
            _ => "Выключено".into(),
        };
        let mut top = column![
            row![
                tile(glyph::VOICE, 44.0, self.controls.rvc),
                column![
                    bold("Голос по модели RVC", 15, if self.snapshot.rvc_state == 2 { GREEN } else { INK }),
                    label(rvc_status, 12, if self.snapshot.rvc_state == 3 { RED } else { DIM }),
                ]
                .spacing(2)
                .width(Length::Fill),
                frame(switch(self.controls.rvc, Msg::Rvc, self.rvc_runtime_installed), self.focus == ENABLE),
            ]
            .spacing(12)
            .align_y(iced::Center),
        ]
        .spacing(12);
        if !self.rvc_runtime_installed {
            // Without the runtime this is the only thing on the page that does anything, so it
            // is the page's one orange button.
            top = top.push(
                action(
                    label(if self.rvc_runtime_installing { "Установка RVC…" } else { "Установить RVC runtime" }, 13, ORANGE_DARK),
                    Msg::RvcInstall,
                    self.focus == INSTALL,
                    true,
                )
                .on_press_maybe((!self.rvc_runtime_installing).then_some(Msg::RvcInstall)),
            );
        }
        let model = card(
            column![
                row![
                    container(frame(
                        repaint(self.controls.rvc_options.slot, pick_list(
                            self.rvc_models.as_slice(),
                            self.rvc_models.iter().find(|m| m.slot == self.controls.rvc_options.slot).cloned(),
                            Msg::RvcModel,
                        )
                        .placeholder("Выберите модель")
                        .text_size(13)
                        .padding([6, 10])
                        .width(Length::Fill)
                        .style(device_style)),
                        self.focus == MODEL,
                    ))
                    .width(Length::Fill),
                    action(
                        label(if self.rvc_importing { "Импорт…" } else { "Импорт модели" }, 13, if self.rvc_importing || !self.rvc_runtime_installed { FAINT } else { INK }),
                        Msg::RvcImport,
                        self.focus == IMPORT,
                        false,
                    )
                    .on_press_maybe((self.rvc_runtime_installed && !self.rvc_importing).then_some(Msg::RvcImport)),
                ]
                .spacing(10)
                .align_y(iced::Center),
                row![
                    container(frame(
                        repaint(&self.rvc_name, text_input("Название модели", &self.rvc_name)
                            .id("rvc-name")
                            .size(13)
                            .padding([6, 10])
                            .on_input_maybe(self.rvc_can_manage().then_some(Msg::RvcName))
                            .on_submit_maybe(self.rvc_can_manage().then_some(Msg::RvcRename))
                            .style(input_style)),
                        self.focus == NAME,
                    ))
                    .width(Length::Fill),
                    action(label("Переименовать", 12, if self.rvc_can_manage() { INK } else { FAINT }), Msg::RvcRename, self.focus == RENAME, false)
                        .on_press_maybe(self.rvc_can_manage().then_some(Msg::RvcRename)),
                    action(
                        label(if self.rvc_delete_confirm { "Удалить ещё раз" } else { "Удалить" }, 12, if self.rvc_can_manage() { RED } else { FAINT }),
                        Msg::RvcDelete,
                        self.focus == DELETE,
                        false,
                    )
                    .on_press_maybe(self.rvc_can_manage().then_some(Msg::RvcDelete)),
                ]
                .spacing(10)
                .align_y(iced::Center),
            ]
            .spacing(10),
        );
        let pitch = card(
            column![
                heading_row(glyph::NOTE, "Тон модели", label("±24 полутона", 11, FAINT).into()),
                frame(
                    tacho(-24.0..=24.0, self.controls.rvc_options.pitch as f32, Msg::RvcPitch, clock)
                        .default(0.0)
                        .origin(0.0)
                        .segments(16)
                        .format(|v| format!("{:+.0} пт", v).replace('-', "−")),
                    self.ring(self.focus == PITCH),
                ),
                label("Сдвигает тон готового голоса модели. Ctrl+клик — 0.", 11, FAINT),
            ]
            .spacing(6),
        )
        .height(Length::Fill);
        // The advanced settings open beside the pitch card, not below it, so the page never
        // needs scrolling at the default window size.
        let mut tuning = column![row![
            heading_row(glyph::CHIP, "Тонкая настройка", Space::new().into()),
            action(label(if self.rvc_advanced { "Скрыть" } else { "Показать" }, 12, INK), Msg::RvcAdvanced, self.focus == ADVANCED, false),
        ]
        .spacing(10)
        .align_y(iced::Center)]
        .spacing(8);
        if self.rvc_advanced {
            let index: Element<'_, Msg> = if self.rvc_has_index() {
                frame(tacho(0.0..=100.0, self.controls.rvc_options.index as f32, Msg::RvcIndex, clock).default(0.0).segments(16).compact().phase(900.0), self.ring(self.focus == INDEX))
            } else {
                label("Недоступно: у модели нет .index", 12, FAINT).into()
            };
            tuning = tuning
                .push(label("Влияние индекса", 12, DIM))
                .push(index)
                .push(label("Вход модели", 12, DIM))
                .push(frame(tacho(50.0..=300.0, self.controls.rvc_options.gain as f32, Msg::RvcGain, clock).default(100.0).segments(16).compact().phase(1800.0), self.ring(self.focus == GAIN)))
                .push(
                    row![
                        label("Блок аудио, мс", 12, DIM),
                        frame(repaint(self.controls.rvc_options.chunk, pick_list(rvc::CHUNKS, Some(self.controls.rvc_options.chunk), Msg::RvcChunk).text_size(13).style(device_style)), self.focus == CHUNK),
                        Space::new().width(Length::Fill),
                        action(label("Обновить модели", 12, INK), Msg::RvcRefresh, self.focus == REFRESH, false),
                    ]
                    .spacing(8)
                    .align_y(iced::Center),
                )
                .push(label("Задержка = блок + 200 мс. При лаге модели — тишина, не обычный голос.", 11, FAINT));
        } else {
            tuning = tuning.push(label(
                if self.rvc_has_index() { "Индекс модели, громкость входа и размер блока аудио." } else { "Громкость входа и размер блока аудио." },
                11,
                FAINT,
            ));
        }
        let mut content = column![
            title("Смена голоса"),
            top,
            model,
            row![pitch.width(Length::FillPortion(1)), card(tuning).width(Length::FillPortion(1)).height(Length::Fill)]
                .spacing(14)
                // Both cards share a height; Fill children in a Shrink row would collapse.
                .height(if self.rvc_advanced { 276 } else { 156 }),
        ]
        .spacing(14);
        if !self.rvc_import_note.is_empty() {
            content = content.push(label(&self.rvc_import_note, 12, DIM));
        }
        content = content.push(label("Только микрофон. Выключение выгружает модель; повторный запуск снова её загружает.", 11, FAINT));
        content.into()
    }

    /// One report to copy and send: the buttons first, the text exactly as it will be copied.
    fn logs_view(&self) -> Element<'_, Msg> {
        use focus::logs::*;
        let report: Element<'_, Msg> = if self.logs_text.is_empty() {
            label("Собираем отчёт…", 12, DIM).into()
        } else {
            // Consolas: the generic monospace fallback has no Cyrillic and clips underscores.
            text(&self.logs_text).size(12).color(INK).font(Font::with_name("Consolas")).line_height(1.35).into()
        };
        column![
            row![
                action(row![icon(glyph::BACK, 11, INK), label("Настройки", 13, INK)].spacing(8).align_y(iced::Center), Msg::Page(2), self.focus == BACK, false),
                title("Логи"),
            ]
            .spacing(14)
            .align_y(iced::Center),
            row![
                action(label(if self.logs_copied { "Скопировано" } else { "Копировать всё" }, 13, ORANGE_DARK), Msg::LogsCopy, self.focus == COPY, true),
                action(label("Открыть папку", 13, INK), Msg::LogsFolder, self.focus == FOLDER, false),
                Space::new().width(Length::Fill),
                action(label(if self.report_sending { "Отправляем…" } else { "Отправить разработчику" }, 13, INK), Msg::SendReport, self.focus == SEND, false)
                    .on_press_maybe((!self.report_sending).then_some(Msg::SendReport)),
            ]
            .spacing(8)
            .align_y(iced::Center),
            label("Версия, видеокарта, состояние и последние строки каждого лога. Имя пользователя Windows заменено.", 12, DIM),
            card(report),
        ]
        .spacing(12)
        .into()
    }

    fn studio_view(&self) -> Element<'_, Msg> {
        widget::responsive(|size| self.studio_layout(size.width)).height(Length::Shrink).into()
    }

    fn studio_layout(&self, width: f32) -> Element<'_, Msg> {
        use focus::studio::*;
        let compact = width < 650.0;
        let cell_width = if compact { 16.0 } else { 27.0 };
        let zoom = studio::ZOOMS[self.studio_zoom];
        let pitch = 28.0 * zoom as f32 / 100.0;
        let roll_height = if compact { 240.0 } else { 300.0 };
        let recording = self.engine.studio_recording();
        let mut library = column![
            bold("Звуки", 14, INK),
            label("Выберите сэмпл для нот", 11, DIM),
            action(label("Добавить файлы", 12, INK), Msg::StudioImport, self.focus == IMPORT, false)
                .on_press_maybe((!self.studio_busy).then_some(Msg::StudioImport)),
        ].spacing(7).width(if compact { Length::Fill } else { Length::Fixed(170.0) });
        if compact {
            library = library.push(action(label("← К сетке", 11, INK), Msg::StudioLibraryToggle, self.focus == LIBRARY, false));
        }
        if let Some(name) = &self.studio_delete {
            library = library.push(column![
                label(format!("Убрать «{}» из проекта?", name.chars().take(14).collect::<String>()), 11, INK),
                row![
                    action(label("Удалить", 11, RED), Msg::StudioDeleteConfirm, self.focus == DELETE_CONFIRM, false),
                    action(label("Отмена", 11, INK), Msg::StudioDeleteCancel, self.focus == DELETE_CANCEL, false),
                ].spacing(5),
                label("Файл останется в папке «Удалённые».", 10, DIM),
            ].spacing(5));
        }
        for (i, name) in self.studio_samples.iter().enumerate() {
            let selected = self.studio_selected.as_ref() == Some(name);
            library = library.push(
                row![
                    action(label(name.chars().take(16).collect::<String>(), 12, if selected { ORANGE_DARK } else { INK }),
                        Msg::StudioSelect(name.clone()), self.focus == SAMPLE_BASE + i, selected).width(Length::Fill),
                    action(label("×", 14, RED), Msg::StudioDeleteAsk(name.clone()),
                        self.focus == DELETE_BASE + i, false).on_press_maybe((!self.studio_busy).then_some(Msg::StudioDeleteAsk(name.clone()))),
                ].spacing(3),
            );
        }
        if self.studio_samples.is_empty() {
            library = library.push(label("Запишите микрофон или добавьте WAV, MP3, OGG, M4A.", 11, DIM));
        }
        let library = scrollable(library).height(if compact { 160 } else { 330 });
        let bar_label: Element<'_, Msg> = if compact {
            action(label("Звуки", 11, INK), Msg::StudioLibraryToggle, self.focus == LIBRARY, false).into()
        } else { label("Такт", 12, DIM).into() };
        let mut bars = row![bar_label].spacing(6).align_y(iced::Center);
        for bar in 0..studio::BARS {
            bars = bars.push(action(label(format!("{}", bar + 1), 12, if bar == self.studio_bar { ORANGE_DARK } else { INK }),
                Msg::StudioBar(bar), self.focus == BAR_BASE + bar, bar == self.studio_bar));
        }
        let mut loop_strip = row![container(label("Луп", 10, DIM)).width(50)].spacing(0).align_y(iced::Center);
        for step in 0..studio::STEPS {
            let inside = step >= self.studio_loop_start && step < self.studio_loop_end;
            let edge = step == self.studio_loop_start || step + 1 == self.studio_loop_end;
            let playing = self.sound_playing.0 == studio::TRACK_ID && self.studio_cursor as usize == step;
            let cell = container(Space::new().height(10)).width(Length::FillPortion(1)).height(10)
                .style(move |_| container::Style {
                    background: Some((if playing { INK } else if inside && edge { ORANGE }
                        else if inside && self.studio_loop { Color::from_rgb8(0xA6, 0x68, 0x39) }
                        else { CARD2 }).into()),
                    ..Default::default()
                });
            loop_strip = loop_strip.push(mouse_area(cell)
                .on_press(Msg::StudioLoopGrab(step))
                .on_right_press(Msg::StudioLoopStart(step))
                .on_enter(Msg::StudioLoopMove(step))
                .interaction(iced::mouse::Interaction::Grab));
        }
        let cursor_bar = (self.studio_cursor as usize / 16).min(studio::BARS - 1);
        let cursor_here = cursor_bar == self.studio_bar;
        let cursor_step = self.studio_cursor.min((studio::STEPS - 1) as f32) as usize % 16;
        let mut header = row![container(label("Нота", 11, DIM)).width(50)].spacing(2).align_y(iced::Center);
        for step in 0..16 {
            let active = cursor_here && step == cursor_step;
            let focused = active && self.focus == CURSOR;
            let number = container(focus_target(numbers(format!("{:02}", self.studio_bar * 16 + step + 1), 10,
                if active { ORANGE_DARK } else if step.is_multiple_of(4) { ORANGE } else { FAINT }), focused))
                .width(cell_width).height(18).center_x(cell_width).center_y(18)
                .style(move |_| container::Style {
                    background: active.then(|| ORANGE.into()),
                    border: outline(focused),
                    ..Default::default()
                });
            header = header.push(mouse_area(number)
                .on_press(Msg::StudioGrab(self.studio_bar * 16 + step))
                .on_enter(Msg::StudioDrag(self.studio_bar * 16 + step))
                .interaction(iced::mouse::Interaction::Grab));
        }
        let focused_row = self.focus.checked_sub(CELL_BASE)
            .filter(|&i| i < 16 * studio::NOTES)
            .map(|i| studio::NOTES - 1 - i % studio::NOTES);
        let mounted = sound_rows(studio::NOTES, self.studio_scroll.0, self.studio_scroll.1, pitch, focused_row);
        let mut events = [None; 16 * studio::NOTES];
        for event in self.studio_events.iter().filter(|e| (e.step as usize) < (self.studio_bar + 1) * 16
            && e.step as usize + e.length as usize > self.studio_bar * 16) {
            if let Some(row_note) = event.note.checked_sub(studio::FIRST_NOTE)
                && (row_note as usize) < studio::NOTES
            {
                for step in (event.step as usize).max(self.studio_bar * 16)
                    ..(event.step as usize + event.length as usize).min((self.studio_bar + 1) * 16) {
                    events[(step % 16) * studio::NOTES + row_note as usize] = Some(event);
                }
            }
        }
        let mut grid: widget::keyed::Column<'_, usize, Msg> = widget::keyed::Column::new().spacing(3);
        let mut next = 0;
        for at in mounted {
            if at > next {
                grid = grid.push(usize::MAX - next, Space::new().height((at - next) as f32 * pitch - 3.0));
            }
            next = at + 1;
            let row_note = studio::NOTES - 1 - at;
            let note = studio::FIRST_NOTE + row_note as u8;
            let note_color = if note == studio::ROOT_NOTE { ORANGE } else if note.is_multiple_of(12) { INK } else { DIM };
            let mut line = row![container(label(studio::note_name(note), 11, note_color)).width(50)].spacing(2).align_y(iced::Center);
            for cell in 0..16 {
                let step = self.studio_bar * 16 + cell;
                let event = events[cell * studio::NOTES + row_note];
                let active = event.is_some();
                let own = event.is_some_and(|e| Some(&e.sample) == self.studio_selected.as_ref());
                let focused = self.focus == CELL_BASE + cell * studio::NOTES + row_note;
                let beat = cell.is_multiple_of(4);
                let tile = container(focus_target(label(if event.is_some_and(|e| e.step as usize == step) { "●" }
                    else if active { "━" } else { "·" }, if zoom == 50 { 10 } else { 13 },
                    if own { ORANGE_DARK } else if active { INK } else { FAINT }), focused))
                    .width(cell_width).height(pitch - 3.0).center_x(cell_width).center_y(pitch - 3.0)
                    .style(move |_| container::Style {
                        background: Some((if own { ORANGE } else if active { Color::from_rgb8(0x62, 0x67, 0x70) }
                            else if beat { CARD2 } else { Color::from_rgb8(0x20, 0x21, 0x24) }).into()),
                        border: outline(focused),
                        ..Default::default()
                    });
                line = line.push(mouse_area(tile)
                    .on_press(Msg::StudioDrawStart(step, note))
                    .on_right_press(Msg::StudioEraseStart(step, note))
                    .on_enter(Msg::StudioCellEnter(step, note))
                    .interaction(iced::mouse::Interaction::Crosshair));
            }
            grid = grid.push(at, line);
        }
        if next < studio::NOTES {
            grid = grid.push(usize::MAX - next, Space::new().height((studio::NOTES - next) as f32 * pitch - 3.0));
        }
        let roll = scrollable(grid).id("studio-roll")
            .on_scroll(|v| Msg::StudioScroll(v.absolute_offset().y, v.bounds().height))
            .height(roll_height).width(Length::Fill);
        let roll: Element<'_, Msg> = if cursor_here {
            let x = 52.0 + (self.studio_cursor - self.studio_bar as f32 * 16.0) * (cell_width + 2.0);
            let line = row![Space::new().width(x).height(roll_height),
                container(Space::new().width(2).height(roll_height)).width(2).style(|_| container::Style {
                    background: Some(ORANGE.into()), ..Default::default()
                })].height(roll_height);
            widget::stack![roll, line].height(roll_height).width(Length::Fill).into()
        } else { roll.into() };
        let mut piano = column![
            row![bars, Space::new().width(Length::Fill), action(label("Очистить такт", 11, INK), Msg::StudioClear, self.focus == CLEAR, false)]
                .align_y(iced::Center),
        ].spacing(if compact { 5 } else { 7 });
        if !compact { piano = piano.push(label("ЛКМ: протянуть ноту · ПКМ: стереть · Линейка: перемотка", 11, DIM)); }
        let piano = piano.push(loop_strip).push(header).push(roll);
        let workspace: Element<'_, Msg> = if compact {
            if self.studio_library_open { library.into() } else { piano.into() }
        } else {
            row![library, container(piano).width(Length::Fill)].spacing(16).into()
        };
        let record = action(label(if recording { "Остановить запись" } else { "Записать звук" }, 13,
            if recording { INK } else { ORANGE_DARK }), Msg::StudioRecord, self.focus == RECORD, !recording);
        let bpm = row![
            label("BPM", 12, DIM),
            action(label("−", 15, INK), Msg::StudioBpm(self.studio_bpm.saturating_sub(1)), self.focus == BPM, false),
            numbers(format!("{}", self.studio_bpm), 17, ORANGE),
            action(label("+", 15, INK), Msg::StudioBpm(self.studio_bpm + 1), self.focus == BPM, false),
        ].spacing(7).align_y(iced::Center);
        let actions = row![
            frame(switch(self.sound_monitor, Msg::SoundpadHear, true), self.focus == HEAR),
            label("Слышать самому", 12, DIM),
            action(label("Стоп", 12, INK), Msg::SoundpadStop, self.focus == STOP, false),
            action(label("Играть в Discord", 12, if self.studio_events.is_empty() { INK } else { ORANGE_DARK }), Msg::StudioRender(false), self.focus == PLAY, !self.studio_events.is_empty())
                .on_press_maybe((!self.studio_busy && !self.studio_events.is_empty()).then_some(Msg::StudioRender(false))),
        ].spacing(7).align_y(iced::Center);
        let transport: Element<'_, Msg> = if compact {
            column![
                row![bpm, Space::new().width(Length::Fill),
                    action(label("В Discord", 12, ORANGE_DARK), Msg::StudioRender(false), self.focus == PLAY, !self.studio_events.is_empty())
                        .on_press_maybe((!self.studio_busy && !self.studio_events.is_empty()).then_some(Msg::StudioRender(false)))].align_y(iced::Center),
                row![record, Space::new().width(Length::Fill),
                    frame(switch(self.studio_loop, Msg::StudioLoopEnabled, true), self.focus == LOOP),
                    label("Луп", 11, DIM),
                    frame(switch(self.sound_monitor, Msg::SoundpadHear, true), self.focus == HEAR),
                    label("Себе", 11, DIM),
                    action(label("Стоп", 11, INK), Msg::SoundpadStop, self.focus == STOP, false)]
                    .spacing(5).align_y(iced::Center),
            ].spacing(7).into()
        } else {
            row![record, Space::new().width(16), bpm, Space::new().width(Length::Fill), actions]
                .align_y(iced::Center).into()
        };
        let loop_tools = row![
            frame(switch(self.studio_loop, Msg::StudioLoopEnabled, true), self.focus == LOOP),
            label("Луп", 12, DIM),
            action(label(format!("От {:02}", self.studio_loop_start + 1), 11, INK),
                Msg::StudioLoopStart(self.studio_cursor as usize), self.focus == LOOP_START, false),
            action(label(format!("До {:02}", self.studio_loop_end), 11, INK),
                Msg::StudioLoopEnd(self.studio_cursor as usize + 1), self.focus == LOOP_END, false),
        ].spacing(7).align_y(iced::Center);
        let zoom_tools = row![
            label("Ноты", 11, DIM),
            action(label("−", 14, INK), Msg::StudioZoom(self.studio_zoom.saturating_sub(1)), self.focus == ZOOM, false),
            numbers(format!("{}%", zoom), 12, ORANGE),
            action(label("+", 14, INK), Msg::StudioZoom((self.studio_zoom + 1).min(studio::ZOOMS.len() - 1)), self.focus == ZOOM, false),
        ].spacing(4).align_y(iced::Center);
        let volume_tools = row![
            label("Трек", 11, DIM),
            action(label("−", 14, INK), Msg::StudioVolume(self.studio_volume.saturating_sub(10)), self.focus == VOLUME, false),
            numbers(format!("{}%", self.studio_volume), 12, ORANGE),
            action(label("+", 14, INK), Msg::StudioVolume((self.studio_volume + 10).min(200)), self.focus == VOLUME, false),
        ].spacing(4).align_y(iced::Center);
        let tools: Element<'_, Msg> = if compact {
            column![loop_tools, row![zoom_tools, Space::new().width(16), volume_tools]].spacing(8).into()
        } else {
            row![loop_tools, Space::new().width(Length::Fill), zoom_tools,
                Space::new().width(18), volume_tools].align_y(iced::Center).into()
        };
        let heading = row![title("Студия"), Space::new().width(Length::Fill), label("4 такта · C2–B5", 12, DIM)].align_y(iced::Center);
        let footer = row![
            action(label("Сохранить WAV", 12, INK), Msg::StudioRender(true), self.focus == EXPORT, false)
                .on_press_maybe((!self.studio_busy && !self.studio_events.is_empty()).then_some(Msg::StudioRender(true))),
            label(&self.studio_note, 12, if self.studio_note.starts_with("Не удалось") || self.studio_note.starts_with("Сборка") { RED } else { DIM }),
        ].spacing(12).align_y(iced::Center);
        if compact {
            column![heading, card(transport), card(workspace), card(tools), footer].spacing(9).into()
        } else {
            column![heading, label("Соберите короткий трек и отправьте его в виртуальный микрофон.", 12, DIM),
                card(column![transport, tools].spacing(10)), card(workspace), footer].spacing(12).into()
        }
    }

    fn soundpad_view(&self) -> Element<'_, Msg> {
        use focus::soundpad::*;
        let clock = self.clock();
        let (playing_id, position, length) = self.sound_playing;
        let playing = playing_id != 0;
        let visible = self.visible_sounds();
        let mut toolbar = row![title("Саундпад")].spacing(10).align_y(iced::Center);
        if self.sound_folder.is_some() {
            toolbar = toolbar.push(numbers(
                if self.sound_filter.trim().is_empty() && self.section == super::Selection::All {
                    format!("{}", self.sounds.len())
                } else {
                    format!("{} из {}", visible.len(), self.sounds.len())
                },
                13,
                FAINT,
            ));
        }
        toolbar = toolbar.push(Space::new().width(Length::Fill));
        if self.sound_folder.is_some() {
            toolbar = toolbar.push(
                container(frame(
                    repaint(&self.sound_filter, text_input("Поиск", &self.sound_filter)
                        .id("sound-filter")
                        .size(13)
                        .padding([6, 10])
                        .on_input(Msg::SoundpadFilter)
                        .style(input_style)),
                    self.focus == FILTER,
                ))
                .width(220),
            );
            toolbar = toolbar.push(frame(
                repaint(self.sound_sort.to_string(), pick_list(SoundSort::ALL, Some(self.sound_sort), Msg::SoundpadSort)
                    .text_size(13)
                    .padding([6, 10])
                    .width(150)
                    .style(device_style)),
                self.focus == SORT,
            ));
        }
        toolbar = toolbar
            .push(
                action(label("Добавить звуки", 13, ORANGE_DARK), Msg::SoundpadAdd, self.focus == ADD, true)
                    .on_press_maybe((!self.sound_dialog && self.sound_folder.is_some()).then_some(Msg::SoundpadAdd)),
            )
            .push(icon_button(glyph::FOLDER, Msg::SoundpadFolder, self.focus == FOLDER).on_press_maybe((!self.sound_dialog).then_some(Msg::SoundpadFolder)))
            .push(icon_button(glyph::REFRESH, Msg::SoundpadRefresh, self.focus == REFRESH).on_press_maybe(self.sound_folder.is_some().then_some(Msg::SoundpadRefresh)));
        let folder_name = self.sound_folder.as_ref().map(|f| f.to_string_lossy().into_owned());
        let mut body = column![toolbar].spacing(12);
        if let Some(name) = folder_name {
            body = body.push(row![icon(glyph::FOLDER, 11, FAINT), label(name, 12, FAINT)].spacing(8).align_y(iced::Center));
        }
        if !self.sound_note.is_empty() {
            body = body.push(label(&self.sound_note, 12, if self.sound_note.starts_with("Добавлено") || self.sound_note.starts_with('«') { GREEN } else { DIM }));
        }
        if self.sound_folder.is_none() {
            return body
                .push(card(
                    column![
                        bold("Звуки поверх голоса в виртуальный микрофон", 15, INK),
                        label("Выберите папку с mp3, wav, ogg или m4a. Каждому звуку в списке назначается свой хоткей; звуки без хоткея запускаются кнопкой в строке.", 12, DIM),
                        label("Хоткей звука: нажатие играет, повторное нажатие останавливает, быстрое двойное перезапускает с начала.", 12, DIM),
                        action(label("Выбрать папку", 13, ORANGE_DARK), Msg::SoundpadFolder, false, true).on_press_maybe((!self.sound_dialog).then_some(Msg::SoundpadFolder)),
                    ]
                    .spacing(10),
                ))
                .push(self.soundpad_footer(clock, playing, playing_id, position, length))
                .into();
        }
        let custom = self.custom_section();
        body = body.push(
            row![self.section_sidebar(custom), self.sound_list(&visible, custom, playing_id, position, length, clock)]
                .spacing(16)
                .height(Length::Fill),
        );
        body.push(self.soundpad_footer(clock, playing, playing_id, position, length)).height(Length::Fill).into()
    }
    /// Now playing on top, the soundpad-wide controls below.
    fn soundpad_footer(&self, clock: Clock, playing: bool, playing_id: u32, position: f32, length: f32) -> Element<'_, Msg> {
        use focus::soundpad::*;
        let name = (playing_id != 0)
            .then(|| self.sounds.iter().enumerate().find(|(i, _)| super::App::sound_id(*i) == playing_id))
            .flatten()
            .map(|(_, s)| s.name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&s.name).to_owned())
            .unwrap_or_else(|| if playing { "Запись".into() } else { "Ничего не играет".into() });
        let now = row![
            button(container(icon(glyph::STOP, 12, if playing { ORANGE_DARK } else { FAINT })).center(Length::Fill))
                .width(32)
                .height(32)
                .padding(0)
                .on_press_maybe(playing.then_some(Msg::SoundpadStop))
                .style(move |_, _| button::Style {
                    background: Some((if playing { ORANGE } else { CARD2 }).into()),
                    border: Border { radius: 7.0.into(), ..Border::default() },
                    ..Default::default()
                }),
            column![
                row![
                    label(name, 13, if playing { INK } else { FAINT }).width(Length::Fill),
                    numbers(if playing { format!("{} / {}", clock_text(position), clock_text(length)) } else { String::new() }, 12, DIM),
                ]
                .spacing(12),
                meter(if playing && length > 0.0 { position / length } else { 0.0 }, ORANGE),
            ]
            .spacing(6)
            .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(iced::Center);
        let controls = row![
            label("Стоп всё", 12, DIM),
            self.bind_button(super::SOUND_STOP_BIND, self.sound_stop_key, self.focus == STOP_BIND, false, 130.0),
            Space::new().width(Length::Fill),
            icon(glyph::VOLUME, 13, DIM),
            container(frame(
                tacho(0.0..=200.0, self.sound_volume * 100.0, Msg::SoundpadVolume, clock).default(100.0).segments(18).compact().phase(0.0),
                self.ring(self.focus == VOLUME),
            ))
            .width(250),
            frame(switch(self.sound_normalize, Msg::SoundpadNormalize, true), self.focus == NORMALIZE),
            label("выравнивать", 12, DIM),
            frame(switch(self.sound_monitor, Msg::SoundpadHear, true), self.focus == HEAR),
            label("в наушниках", 12, DIM),
        ]
        .spacing(8)
        .align_y(iced::Center);
        let mut footer = column![now, container(Space::new().height(1)).width(Length::Fill).style(|_| container::Style { background: Some(LINE.into()), ..Default::default() }), controls].spacing(10);
        let hint = self.monitor_hint();
        if !hint.is_empty() {
            footer = footer.push(label(hint, 11, if self.sound_monitor && self.monitor == 3 { RED } else { FAINT }));
        }
        card(footer).padding([12, 14]).into()
    }
    /// Left column: "all", automatic prefix groups, custom sections, and the editor of the
    /// selected custom section. Custom entries are drop targets while a clip is dragged.
    fn section_sidebar(&self, custom: Option<usize>) -> Element<'_, Msg> {
        use focus::soundpad::*;
        let items = self.section_items();
        let mut list = column![].spacing(2);
        for (i, item) in items.iter().enumerate() {
            let selected = item.selection == self.section;
            let target = match item.selection {
                super::Selection::Custom(section) => Some(section),
                _ => None,
            };
            let hot = self.dragging.is_some() && target.is_some() && self.drag_over == target;
            let focused = self.focus == SECTION_BASE + i;
            let entry = button(focus_target(
                row![
                    label(&item.label, 13, if selected { INK } else { DIM }).width(Length::Fill),
                    numbers(item.count.to_string(), 12, if selected { DIM } else { FAINT }),
                ]
                .spacing(6)
                .align_y(iced::Center),
                focused,
            ))
            .width(Length::Fill)
            .padding([6, 10])
            .on_press(Msg::SectionSelect(i))
            .style(move |_, status| button::Style {
                background: Some(
                    (if selected {
                        Color::from_rgb8(0x1F, 0x20, 0x23)
                    } else if hot || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                        Color::from_rgb8(0x1B, 0x1C, 0x1F)
                    } else {
                        Color::TRANSPARENT
                    })
                    .into(),
                ),
                text_color: INK,
                border: Border {
                    color: if hot || focused { ORANGE } else { Color::TRANSPARENT },
                    width: if hot || focused { 2.0 } else { 0.0 },
                    radius: 7.0.into(),
                },
                ..Default::default()
            });
            // Drop targets report the cursor; the global mouse release finishes the drop. Every
            // entry is wrapped so the tree (and the sidebar scroll offset) stays stable.
            list = list.push(mouse_area(entry).on_enter(Msg::DragOver(target)).on_exit(Msg::DragOver(None)));
        }
        let mut sidebar = column![
            label(if self.dragging.is_some() { "Отпустите на разделе" } else { "Разделы" }, 12, if self.dragging.is_some() { ORANGE } else { FAINT }),
            scrollable(
                mouse_area(container(list).padding(iced::Padding { right: 10.0, ..Default::default() }))
                    .on_scroll(|d| Msg::Wheel("sections", smooth::wheel_pixels(d))),
            )
            .id("sections")
            .height(Length::Fill)
            .width(Length::Fill),
            action(label("+ Новый раздел", 12, DIM), Msg::SectionAdd, self.focus == SECTION_ADD, false).width(Length::Fill),
        ]
        .spacing(6)
        .width(180)
        .height(Length::Fill);
        if custom.is_some() {
            sidebar = sidebar.push(
                column![
                    frame(
                        repaint(&self.section_name, text_input(
                            custom.and_then(|i| self.sections.get(i)).map(|s| s.name.as_str()).unwrap_or("Название раздела"),
                            &self.section_name,
                        )
                        .id("section-name")
                        .size(12)
                        .padding([5, 8])
                        .on_input(Msg::SectionName)
                        .on_submit(Msg::SectionRename)
                        .style(input_style)),
                        self.focus == SECTION_NAME,
                    ),
                    action(label("Удалить раздел", 12, RED), Msg::SectionDelete, self.focus == SECTION_DELETE, false).width(Length::Fill),
                    label("Перетащите звук за ≡ из списка; × в строке убирает его из раздела.", 11, FAINT),
                ]
                .spacing(6),
            );
        }
        sidebar.into()
    }
    /// Header plus the virtualised clip list in its own scrollable (id "body" so keyboard
    /// focus reveal keeps working here).
    fn sound_list(&self, visible: &[usize], custom: Option<usize>, playing_id: u32, position: f32, length: f32, clock: Clock) -> Element<'_, Msg> {
        use focus::soundpad::*;
        let header = row![
            Space::new().width(46),
            label("Звук", 12, FAINT).width(Length::Fill),
            label("Громкость", 12, FAINT).width(170),
            label("Клавиша", 12, FAINT).width(if custom.is_some() { 164 } else { 130 }),
        ]
        .spacing(10);
        if self.sounds.is_empty() {
            return column![header, label("В папке пока нет mp3, wav, ogg или m4a.", 13, DIM)].spacing(8).into();
        }
        if visible.is_empty() {
            return column![
                header,
                label(if custom.is_some() { "Раздел пуст: перетащите сюда звуки из «Все звуки» за ≡." } else { "Ничего не найдено." }, 13, DIM)
            ]
            .spacing(8)
            .into();
        }
        // Build only the viewport and one keyboard target. Fixed row pitch lets spacers
        // preserve all skipped distances, including the gap to an off-screen focus target.
        // Keys keep row state (shaped text) attached to the same clip as the window slides.
        const PITCH: f32 = ROW_HEIGHT + ROW_SPACING;
        let (scroll, viewport) = self.sound_scroll;
        let focused = self.focus.checked_sub(ROW_BASE).and_then(|f| visible.iter().position(|&i| i == f / 3));
        let mounted = sound_rows(visible.len(), scroll, viewport, PITCH, focused);
        let mut rows: widget::keyed::Column<'_, usize, Msg> = widget::keyed::Column::new().spacing(ROW_SPACING);
        let mut next = 0;
        for at in mounted {
            if at > next {
                rows = rows.push(usize::MAX - next, Space::new().height((at - next) as f32 * PITCH - ROW_SPACING));
            }
            next = at + 1;
            let i = visible[at];
            let sound = &self.sounds[i];
            let lit = playing_id == super::App::sound_id(i);
            let dragged = self.dragging == Some(i);
            let stem = sound.name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&sound.name);
            let status = match &sound.state {
                _ if lit => format!("{} / {}", clock_text(position), clock_text(length)),
                SoundState::Loaded(seconds) => clock_text(*seconds),
                SoundState::Loading => "загрузка…".into(),
                SoundState::Failed(e) => e.clone(),
                SoundState::Unloaded => String::new(),
            };
            let failed = matches!(sound.state, SoundState::Failed(_));
            let play_focused = self.focus == ROW_BASE + 3 * i;
            let volume_focused = self.focus == ROW_BASE + 3 * i + 1;
            let volume_active = self.sound_hover == Some(i) || volume_focused || sound.volume != 100;
            let grip = mouse_area(container(label("≡", 14, if dragged { ORANGE } else { FAINT })).width(14).height(ROW_HEIGHT).center_y(ROW_HEIGHT))
                .on_press(Msg::DragStart(i))
                .interaction(iced::mouse::Interaction::Grab);
            let play = button(focus_target(
                row![
                    container(icon(if lit { glyph::STOP } else { glyph::PLAY }, 10, if lit { ORANGE_DARK } else { DIM }))
                        .width(26)
                        .height(26)
                        .center(26)
                        .style(move |_| container::Style {
                            background: Some((if lit { ORANGE } else { Color::from_rgb8(0x1D, 0x1E, 0x21) }).into()),
                            border: Border { color: if lit { ORANGE } else { Color::from_rgb8(0x34, 0x35, 0x3A) }, width: 1.0, radius: 13.0.into() },
                            ..Default::default()
                        }),
                    label(stem, 13, if lit { Color::from_rgb8(0xFF, 0xB2, 0x7A) } else { INK }).width(Length::Fill),
                    numbers(status, 12, if failed { RED } else if lit { ORANGE } else { FAINT }),
                ]
                .spacing(10)
                .align_y(iced::Center),
                play_focused,
            ))
            .width(Length::Fill)
            .height(ROW_HEIGHT)
            .padding([0, 6])
            .on_press(Msg::SoundPlay(i))
            .style(move |_, status| button::Style {
                background: Some(
                    (if lit {
                        Color { a: 0.08, ..ORANGE }
                    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                        CARD
                    } else {
                        Color::TRANSPARENT
                    })
                    .into(),
                ),
                text_color: INK,
                border: Border { color: if play_focused { ORANGE } else { Color::TRANSPARENT }, width: 2.0, radius: 8.0.into() },
                ..Default::default()
            });
            let volume: Element<'_, Msg> = if volume_active {
                frame(
                    // No wheel: the list scrolls under the cursor, and a passing slider must not
                    // catch the wheel and change a volume.
                    tacho(0.0..=200.0, sound.volume as f32, move |v| Msg::SoundVolume(i, v), Clock { animate: false, ..clock })
                        .wheel(false)
                        .default(100.0)
                        .segments(12)
                        .compact(),
                    self.ring(volume_focused),
                )
            } else {
                Space::new().into()
            };
            let bind = self.bind_button(super::SOUND_BIND_BASE + i, sound.key, self.focus == ROW_BASE + 3 * i + 2, false, 130.0);
            let mut line = row![grip, play, container(volume).width(170), bind].spacing(10).height(ROW_HEIGHT).align_y(iced::Center);
            if custom.is_some() {
                line = line.push(
                    button(container(icon(glyph::CLOSE, 9, RED)).center(Length::Fill))
                        .width(24)
                        .height(24)
                        .padding(0)
                        .on_press(Msg::SoundUnassign(i))
                        .style(|_, status| button::Style {
                            background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| HOVER.into()),
                            text_color: RED,
                            border: Border { radius: 6.0.into(), ..Default::default() },
                            ..Default::default()
                        }),
                );
            }
            // A row-sized clip layer lets tiny-skia invalidate the moving row as a whole.
            // Without it, scattered text/slider damage fragments repaint the same list repeatedly.
            rows = rows.push(
                i,
                widget::stack![
                    Space::new().width(Length::Fill).height(ROW_HEIGHT),
                    mouse_area(line).on_enter(Msg::SoundHover(i, true)).on_exit(Msg::SoundHover(i, false)),
                ]
                .clip(true),
            );
        }
        if next < visible.len() {
            rows = rows.push(usize::MAX - next, Space::new().height((visible.len() - next) as f32 * PITCH - ROW_SPACING));
        }
        column![
            header,
            scrollable(
                mouse_area(container(rows).padding(iced::Padding { right: 10.0, ..Default::default() }))
                    .on_scroll(|d| Msg::Wheel("body", smooth::wheel_pixels(d))),
            )
            .id("body")
            .on_scroll(|v| Msg::SoundpadScroll(v.absolute_offset().y, v.bounds().height))
            .width(Length::Fill)
            .height(Length::Fill),
        ]
        .spacing(6)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
    /// What the headphone monitor is doing for the soundpad, with the level it really renders.
    fn monitor_hint(&self) -> String {
        if !self.sound_monitor {
            return String::new();
        }
        match self.monitor {
            2 if self.monitor_all => "Сейчас слышен весь голос, звуки в нём.".into(),
            2 => {
                let level = if self.monitor_peak > 0.0005 {
                    format!("{:.0} dBFS", 20.0 * self.monitor_peak.log10())
                } else {
                    "тишина".into()
                };
                format!("Наушники: {} · {level}", self.monitor_message)
            }
            1 => "Подключение наушников…".into(),
            3 => "Ошибка прослушивания — см. сообщение сверху.".into(),
            _ if self.running() => "Прослушивание не запущено.".into(),
            _ => "Включится вместе с обработкой микрофона.".into(),
        }
    }
    fn settings_view(&self) -> Element<'_, Msg> {
        use focus::settings::*;
        let locked = self.running() || self.busy;
        let route: Element<'_, Msg> = if locked {
            column![
                row![label("Микрофон", 12, FAINT).width(150), label(self.input.as_ref().map(|d| d.name.clone()).unwrap_or_default(), 13, INK)],
                row![label("Передать голос в", 12, FAINT).width(150), label(self.output.as_ref().map(|d| d.name.clone()).unwrap_or_default(), 13, INK)],
                row![label("Модель", 12, FAINT).width(150), label(format!("Denoiser v{}  ·  буфер {} мс", self.version, self.buffer), 13, INK)],
                label("Выход и модель меняются, пока обработка остановлена.", 12, FAINT),
            ]
            .spacing(8)
            .into()
        } else {
            column![
                row![
                    label("Микрофон", 12, FAINT).width(150),
                    frame(repaint(self.input.as_ref().map(ToString::to_string), pick_list(self.inputs.as_slice(), self.input.as_ref(), Msg::Input).placeholder("Микрофон отключён / не выбран").width(Length::Fill).text_size(13).style(device_style)), self.focus == INPUT),
                ]
                .align_y(iced::Center),
                row![
                    label("Передать голос в", 12, FAINT).width(150),
                    frame(repaint(self.output.as_ref().map(ToString::to_string), pick_list(self.outputs.as_slice(), self.output.as_ref(), Msg::Output).placeholder("Выберите выход").width(Length::Fill).text_size(13).style(device_style)), self.focus == OUTPUT),
                ]
                .align_y(iced::Center),
                row![
                    label("Модель", 12, FAINT).width(150),
                    frame(repaint(self.version, pick_list([1, 2], Some(self.version), Msg::Version).width(90).style(device_style)), self.focus == VERSION),
                    label("v2 экспериментальная", 12, FAINT),
                    Space::new().width(Length::Fill),
                    label("Буфер, мс", 12, FAINT),
                    frame(repaint(self.buffer, pick_list([10, 20, 30, 40, 60, 80], Some(self.buffer), Msg::Buffer).width(80).style(device_style)), self.focus == BUFFER),
                ]
                .spacing(10)
                .align_y(iced::Center),
            ]
            .spacing(8)
            .into()
        };
        let startup = column![
            row![frame(switch(self.app_autostart, Msg::AppAutostart, true), self.focus == APP_AUTOSTART), label("Запускать Mic Noize вместе с Windows (в трее)", 13, INK)].spacing(8).align_y(iced::Center),
            row![frame(switch(self.autostart, Msg::Autostart, true), self.focus == AUTOSTART), label("Держать виртуальный микрофон доступным после входа в Windows", 13, INK)].spacing(8).align_y(iced::Center),
        ]
        .spacing(6);
        // Only while the virtual microphone is missing: a button that can do nothing is noise.
        let mut device = column![
            row![
                label("Устройство Mic Noize", 13, DIM),
                label(self.device_state.label(), 13, match self.device_state {
                    engine::DeviceState::Ready => GREEN,
                    engine::DeviceState::UserAction => RED,
                    _ => DIM,
                }),
            ]
            .spacing(8),
            label(&self.device_detail, 12, FAINT),
        ]
        .spacing(8);
        if !self.driver_ready {
            device = device.push(label("Виртуальный микрофон не установлен: Windows запросит права администратора.", 12, DIM)).push(
                action(label(if self.driver_installing { "Устанавливаем…" } else { "Установить виртуальный микрофон" }, 13, INK), Msg::InstallDriver, self.focus == DRIVER, false)
                    .on_press_maybe((!self.driver_installing && !self.core_installing).then_some(Msg::InstallDriver)),
            );
            if !self.driver_error.is_empty() {
                device = device.push(label(&self.driver_error, 12, RED));
            }
        }
        let device = if self.repair_confirm {
            device.push(
                column![
                    bold("Восстановление устройства", 15, INK),
                    label("Обработка микрофона и наушников будет остановлена на время проверки. Текущая линия сохранится, если перенос не требуется.", 12, DIM),
                    row![frame(switch(self.repair_lines, Msg::RepairLines, true), self.focus == REPAIR_LINES), label("Освободить место для наушников и перенести старые линии Mic Noize", 13, INK)].spacing(8).align_y(iced::Center),
                    label("Стандартный вход TAG Microphone будет освобождён, в том числе после перезагрузок. При переносе старой линии Mic Noize её потребуется снова выбрать в Discord и других программах. Физический микрофон не меняется.", 12, DIM),
                    row![frame(switch(self.repair_reinstall, Msg::RepairReinstall, true), self.focus == REPAIR_REINSTALL), label("Разрешить переустановку драйвера, если проверка и перезапуск не помогут", 13, INK)].spacing(8).align_y(iced::Center),
                    label("При переустановке Windows запросит права администратора. Устройство может получить новый идентификатор — тогда его нужно снова выбрать в Discord и других программах.", 12, DIM),
                    row![
                        action(label("Восстановить", 13, ORANGE_DARK), Msg::RepairConfirm, self.focus == REPAIR_CONFIRM, true),
                        action(label("Отмена", 13, INK), Msg::RepairCancel, self.focus == REPAIR_CANCEL, false),
                    ]
                    .spacing(8),
                ]
                .spacing(8),
            )
        } else {
            device.push(
                row![
                    action(label(if self.repair_resume.is_some() { "Восстанавливаем…" } else { "Восстановить устройство" }, 13, INK), Msg::Repair, self.focus == REPAIR, false)
                        .on_press_maybe((!self.driver_installing && !self.core_installing && !self.quitting && !self.apply_pending).then_some(Msg::Repair)),
                    action(label("Обновить устройства", 13, INK), Msg::Refresh, self.focus == REFRESH, false),
                ]
                .spacing(8),
            )
        };
        // The version stands in the title bar; a ready update restarts from the rail's card, so
        // this block only says where the check stands.
        let updates = column![
            label(&self.update_status, 12, if self.update_ready { GREEN } else { FAINT }),
            action(label(if self.update_checking { "Проверка…" } else { "Проверить обновления" }, 13, if self.update_checking { FAINT } else { INK }), Msg::UpdateCheck, self.focus == UPDATE, false)
                .on_press_maybe((!self.update_checking).then_some(Msg::UpdateCheck)),
        ]
        .spacing(8);
        let visuals = column![
            row![frame(switch(self.pixel_shift, Msg::PixelShift, true), self.focus == PIXEL_SHIFT), label("Пиксельный переход между разделами", 13, INK)].spacing(8).align_y(iced::Center),
            row![frame(switch(self.slider_idle, Msg::SliderIdle, true), self.focus == SLIDER_IDLE), label("Волна и прогрев ползунков", 13, INK)].spacing(8).align_y(iced::Center),
        ]
        .spacing(6);
        let diagnostics = column![
            numbers(format!("NVIDIA {:.2} мс   очередь {:.1} мс", self.snapshot.process_ms, self.snapshot.queue_ms), 13, DIM),
            numbers(
                format!(
                    "пропуски {} / {}   pitch {:.1} мс (макс {:.2} мс)",
                    self.snapshot.underruns, self.snapshot.drops, self.snapshot.pitch_delay_ms, self.snapshot.pitch_max_ms
                ),
                13,
                DIM,
            ),
            label("Буфер — запас от обрывов, не полная задержка. Pitch добавляет задержку только при удержании.", 12, FAINT),
            row![
                action(row![icon(glyph::LOGS, 12, INK), label("Логи и отчёт", 13, INK)].spacing(8).align_y(iced::Center), Msg::Page(5), self.focus == LOGS, false),
                Space::new().width(Length::Fill),
                action(label("Выход из Mic Noize", 13, INK), Msg::Quit, self.focus == QUIT, false),
            ]
            .spacing(8)
            .align_y(iced::Center),
            // «Анимация обновления» plays «Перезапустить» → update window → restart for real,
            // without installing; «Карточка обновления» shows the ready card's celebration.
            row![
                action(label("Анимация обновления", 13, INK), Msg::RehearseUpdate, self.focus == REHEARSE, false),
                action(label("Карточка обновления", 13, INK), Msg::ReadyPreview, self.focus == READY_PREVIEW, false),
            ]
            .spacing(8),
        ]
        .spacing(8);
        // Two columns under the device card, so the page fits the default window unscrolled.
        column![
            title("Настройки"),
            card(column![heading_row(glyph::MIC, "Микрофон и выход", Space::new().into()), route].spacing(10)),
            row![
                column![
                    card(column![heading_row(glyph::REFRESH, "Запуск", Space::new().into()), startup].spacing(10)),
                    card(column![heading_row(glyph::SAVE, "Обновления", Space::new().into()), updates].spacing(10)),
                    card(column![heading_row(glyph::PALETTE, "Визуальные эффекты", Space::new().into()), visuals].spacing(10)),
                ]
                .spacing(14)
                .width(Length::FillPortion(1)),
                column![
                    card(column![heading_row(glyph::OUTPUT, "Виртуальный микрофон", Space::new().into()), device].spacing(10)),
                    card(column![heading_row(glyph::CHIP, "Диагностика", Space::new().into()), diagnostics].spacing(10)),
                ]
                .spacing(14)
                .width(Length::FillPortion(1)),
            ]
            .spacing(14),
        ]
        .spacing(14)
        .into()
    }
    /// A hotkey shown as keycaps that captures its key in place: click, press the key, done.
    /// A clash stays on the button in red and keeps waiting; a second click or Esc cancels; the
    /// small cross clears an existing binding while capturing.
    fn bind_button(&self, target: usize, key: u32, focused: bool, lit: bool, width: f32) -> Element<'_, Msg> {
        if self.binding != Some(target) {
            let content: Element<'_, Msg> = if key == 0 {
                label("+ клавиша", 12, FAINT).into()
            } else {
                let name = key_name(key);
                widget::Row::with_children(name.split(" + ").map(|part| {
                    let vk = match part { "Ctrl" => 0x11, "Alt" => 0x12, "Shift" => 0x10, _ => key & 255 };
                    tacho::keycap(part, lit || self.key_held(vk))
                }))
                .spacing(3)
                .align_y(iced::Center)
                .into()
            };
            return button(focus_target(content, focused))
                .padding([4, 6])
                .on_press(Msg::Bind(target))
                .style(move |_, status| {
                    let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
                    button::Style {
                        background: hover.then(|| HOVER.into()),
                        text_color: INK,
                        border: Border {
                            color: if focused { ORANGE } else if key == 0 { Color::from_rgb8(0x3A, 0x3B, 0x41) } else { Color::TRANSPARENT },
                            width: if focused { 2.0 } else { 1.0 },
                            radius: 7.0.into(),
                        },
                        ..Default::default()
                    }
                })
                .into();
        }
        let (text, color) = match self.bind_conflict {
            Some(taken) => (format!("Занято: {}", key_name(taken)), RED),
            None => ("Нажмите…".into(), ORANGE),
        };
        let capture = button(focus_target(label(text, 12, color), focused))
            .padding([6, 10])
            .width(Length::Fill)
            .on_press(Msg::Bind(target))
            .style(|_, _| button::Style {
                background: Some(BG.into()),
                text_color: ORANGE,
                border: Border { color: ORANGE, width: 2.0, radius: 8.0.into() },
                ..Default::default()
            });
        if key == 0 {
            return container(capture).width(width).into();
        }
        row![
            capture,
            button(container(icon(glyph::CLOSE, 9, RED)).center(Length::Fill))
                .width(24)
                .height(28)
                .padding(0)
                .on_press(Msg::ClearBind)
                .style(|_, status| button::Style {
                    background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| HOVER.into()),
                    text_color: RED,
                    border: Border { radius: 6.0.into(), ..Default::default() },
                    ..Default::default()
                }),
        ]
        .spacing(4)
        .align_y(iced::Center)
        .width(width)
        .into()
    }
}

const ROW_HEIGHT: f32 = 34.0;
const ROW_SPACING: f32 = 2.0;
/// m:ss for clip lengths and playback position.
fn clock_text(seconds: f32) -> String {
    let whole = seconds.max(0.0).round() as u32;
    format!("{}:{:02}", whole / 60, whole % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_times_round_up() {
        assert_eq!(eta_text(0.2), "≈ 5 с");
        assert_eq!(eta_text(41.0), "≈ 45 с");
        assert_eq!(eta_text(121.0), "≈ 2 мин 10 с");
        assert_eq!(eta_text(175.0), "≈ 3 мин");
        assert_eq!(eta_text(900.5), "≈ 16 мин");
        assert_eq!(speed_text(8.4 * MB as f64), "8,4 МБ/с");
    }
    #[test]
    fn soundpad_scroll_timing() {
        use iced::advanced::{Renderer as _, Layout, graphics::{damage, Viewport}};
        let folder = std::env::var("MNR_SCROLL_BENCH_FOLDER").ok();
        let settings = folder.as_ref().map(|f| format!("[soundpad]\nfolder={f}")).unwrap_or_default();
        let (mut app, _) = App::from_settings(Settings::for_test(&settings))
            .unwrap().unwrap();
        if folder.is_none() {
            app.sound_folder = Some(PathBuf::from("test sounds"));
            app.sounds = (0..100).map(|i| Sound {
                name: format!("Section {} - Sound {i:03}.wav", i / 5), path: PathBuf::new(),
                key: 0, volume: 100, played: 0, modified: 0, state: SoundState::Unloaded,
            }).collect();
        }
        let scale: f32 = std::env::var("MNR_SCROLL_BENCH_SCALE").ok()
            .map(|s| s.parse().unwrap()).unwrap_or(1.0);
        let size = Size::new((1100.0 * scale) as u32, (900.0 * scale) as u32);
        app.soundpad_page = true;
        let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
        let mut tree = iced::advanced::widget::Tree::empty();
        let limits = iced::advanced::layout::Limits::new(Size::ZERO, Size::new(1100.0, 900.0));
        for id in ["sections", "body"] {
            let mut samples = Vec::new();
            let mut raster = Vec::new();
            let mut regions = Vec::new();
            let mut previous = Vec::new();
            let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
            let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
            let viewport = Viewport::with_physical_size(size, scale);
            for frame in 0..if folder.is_some() { 90 } else { 6 } {
                let offset = frame as f32 * 7.3;
                app.sound_scroll = (if id == "body" { offset } else { 0.0 }, 600.0);
                let start = Instant::now();
                let mut element = app.view(window::Id::unique());
                tree.diff(element.as_widget());
                let layout = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
                let mut scroll = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
                    Id::new(id), iced::widget::scrollable::AbsoluteOffset { x: None, y: Some(offset) });
                element.as_widget_mut().operate(&mut tree, Layout::new(&layout), &renderer, &mut scroll);
                if frame > 0 { samples.push(start.elapsed().as_secs_f64() * 1000.0); }
                let start = Instant::now();
                renderer.reset(iced::Rectangle::with_size(Size::new(1100.0, 900.0)));
                element.as_widget().draw(&tree, &mut renderer, &Theme::Dark,
                    &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout),
                    iced::mouse::Cursor::Unavailable, &iced::Rectangle::with_size(Size::new(1100.0, 900.0)));
                let changes = damage::group(damage::diff(&previous, renderer.layers(),
                    |layer| vec![layer.bounds], iced_tiny_skia::Layer::damage),
                    iced::Rectangle::with_size(Size::new(1100.0, 900.0)));
                previous = renderer.layers().to_vec();
                renderer.draw(&mut pixels.as_mut(), &mut mask, &viewport, &changes, BG);
                if frame > 0 && !changes.is_empty() {
                    regions.push(changes.len());
                    raster.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            samples.sort_by(f64::total_cmp);
            raster.sort_by(f64::total_cmp);
            eprintln!("{id}, {} clips: view/diff/layout median {:.2} ms, p95 {:.2} ms",
                app.sounds.len(), samples[samples.len()/2], samples[samples.len()*95/100]);
            regions.sort();
            if !regions.is_empty() {
                eprintln!("{id}: draw/damage raster median {:.2} ms, p95 {:.2} ms; damage regions median {}",
                    raster[raster.len()/2], raster[raster.len()*95/100], regions[regions.len()/2]);
            }
            if id == "body" {
                assert!(!regions.is_empty());
                assert!(regions[regions.len()/2] <= 4, "fragmented row damage: {regions:?}");
                if let Ok(path) = std::env::var("MNR_SCROLL_BENCH_IMAGE") {
                    let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), size.width, size.height);
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    encoder.write_header().unwrap().write_image_data(pixels.data()).unwrap();
                }
            }
        }
        // Same damage grouping and rasterizer as the window compositor; excludes OS presentation.
    }
    /// Renders every page headlessly: `MNR_DESIGN_DIR=<dir> cargo test design_snapshots -- --ignored`.
    #[test]
    fn update_points_parse() {
        assert_eq!(crate::update_window::parse_point("960.5, 540"), Some(iced::Point::new(960.5, 540.0)));
        assert_eq!(crate::update_window::parse_point("centered"), None);
        assert_eq!(crate::update_window::parse_point("1,NaN"), None);
    }

    /// Frames of the update shrink and grow; keyed (see-through) pixels are drawn as a checker.
    #[test]
    #[ignore]
    fn update_morph_frames() {
        use iced::advanced::{Renderer as _, Layout, graphics::Viewport};
        let dir = PathBuf::from(std::env::var("MNR_DESIGN_DIR").expect("MNR_DESIGN_DIR"));
        let (w, h) = (1040.0_f32, 740.0_f32);
        let size = Size::new(w as u32, h as u32);
        let full = iced::Rectangle::with_size(Size::new(w, h));
        let save = |app: &App, name: String| {
            let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
            let mut tree = iced::advanced::widget::Tree::empty();
            let mut element = app.view(window::Id::unique());
            tree.diff(element.as_widget());
            let layout = element.as_widget_mut().layout(&mut tree, &renderer, &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(w, h)));
            let start = Instant::now();
            renderer.reset(full);
            element.as_widget().draw(&tree, &mut renderer, &Theme::Dark, &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout), iced::mouse::Cursor::Unavailable, &full);
            let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
            let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
            renderer.draw(&mut pixels.as_mut(), &mut mask, &Viewport::with_physical_size(size, 1.0), &[full], BG);
            eprintln!("{name}: {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
            let mut data = pixels.data().to_vec();
            for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                px.swap(0, 2);
                if px[0] == 1 && px[1] == 0 && px[2] == 1 {
                    let (x, y) = (i % size.width as usize / 16, i / size.width as usize / 16);
                    let v = if (x + y) % 2 == 0 { 0x50 } else { 0x68 };
                    px[0] = v; px[1] = v; px[2] = v + 0x10;
                }
            }
            let mut encoder = png::Encoder::new(std::fs::File::create(dir.join(format!("{name}.png"))).unwrap(), size.width, size.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&data).unwrap();
        };
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window = Some(window::Id::unique());
        let window = Size::new(w, h);
        let card = iced::Rectangle { x: (w - UPDATE_CARD.width) / 2.0, y: (h - UPDATE_CARD.height) / 2.0, width: UPDATE_CARD.width, height: UPDATE_CARD.height };
        let root = app.window_mosaic(window).unwrap();
        let waiting = mosaic_of::<Msg>(update_card(tacho::BarStage::Waiting, "0.2.14", "0.2.15"), UPDATE_CARD).unwrap();
        let done = mosaic_of::<Msg>(update_card(tacho::BarStage::Done, "", "0.2.15"), UPDATE_CARD).unwrap();
        for (kind, from, to, from_rect, to_rect, timeline, times) in [
            ("shrink", root.clone(), waiting, iced::Rectangle::with_size(window), card, tacho::MorphTimeline::SHRINK, [0u64, 150, 320, 450, 700, 900]),
            ("grow", done, root, card, iced::Rectangle::with_size(window), tacho::MorphTimeline::GROW, [0, 150, 300, 450, 700, 900]),
        ] {
            for ms in times {
                let base = match (kind, ms) {
                    ("shrink", ms) if ms < 120 => MorphBase::Root,
                    ("shrink", ms) if ms >= 800 => MorphBase::Card(tacho::BarStage::Waiting),
                    ("grow", ms) if ms < 100 => MorphBase::Card(tacho::BarStage::Done),
                    ("grow", ms) if ms >= 820 => MorphBase::Root,
                    _ => MorphBase::Key,
                };
                app.morph = Some(MorphView {
                    base,
                    from_version: "0.2.14".into(),
                    to_version: "0.2.15".into(),
                    anim: Some(tacho::Morph { from: from.clone(), to: to.clone(), from_rect, to_rect, start: Instant::now() - Duration::from_millis(ms), timeline, events: Vec::new() }),
                    hwnd: None,
                    center: None,
                    shown: None,
                });
                save(&app, format!("morph-{kind}-{ms:03}"));
            }
        }
        app.morph = Some(MorphView { base: MorphBase::Card(tacho::BarStage::Running(Instant::now() - Duration::from_millis(270))), from_version: "0.2.14".into(), to_version: "0.2.15".into(), anim: None, hwnd: None, center: None, shown: None });
        save(&app, "update-window".into());
        app.morph = Some(MorphView { base: MorphBase::Card(tacho::BarStage::Launching), from_version: "0.2.14".into(), to_version: "0.2.15".into(), anim: None, hwnd: None, center: None, shown: None });
        save(&app, "update-launching".into());
    }

    #[test]
    fn page_swap_repaints_whole_window() {
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        let first = app.backdrop();
        let _ = app.update(Msg::Page(0));
        assert_eq!(app.backdrop(), first, "same page keeps region repaints");
        let _ = app.update(Msg::Page(2));
        assert_ne!(app.backdrop(), first, "a page swap forces one full pass");
        assert_eq!(app.backdrop().into_rgba8(), BG.into_rgba8(), "the same pixels");
        let swapped = app.backdrop();
        let _ = app.update(Msg::SoundpadFilter("a".into()));
        assert_ne!(app.backdrop(), swapped, "a rebuilt clip list forces one full pass");
    }
    /// Whole-window cost of each tab switch as the window pays it: update, view/diff/layout on
    /// the persistent tree, then a damaged-region raster. `MNR_TAB_BENCH_FOLDER` = real clips.
    #[test]
    #[ignore]
    fn tab_switch_timing() {
        use iced::advanced::{Renderer as _, Layout, graphics::{damage, Viewport}};
        let folder = std::env::var("MNR_TAB_BENCH_FOLDER").ok();
        let settings = folder.as_ref().map(|f| format!("[soundpad]\nfolder={f}")).unwrap_or_default();
        let (mut app, _) = App::from_settings(Settings::for_test(&settings)).unwrap().unwrap();
        app.window = Some(window::Id::unique());
        let scale: f32 = std::env::var("MNR_TAB_BENCH_SCALE").ok().map(|s| s.parse().unwrap()).unwrap_or(1.0);
        let (w, h) = (1040.0_f32, 740.0_f32);
        let size = Size::new((w * scale) as u32, (h * scale) as u32);
        let full = iced::Rectangle::with_size(Size::new(w, h));
        let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
        let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
        let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
        let viewport = Viewport::with_physical_size(size, scale);
        let mut tree = iced::advanced::widget::Tree::empty();
        let mut previous = Vec::new();
        let mut backdrop = app.backdrop();
        let mut frame = |app: &App| {
            let start = Instant::now();
            let mut element = app.view(window::Id::unique());
            tree.diff(element.as_widget());
            let layout = element.as_widget_mut().layout(&mut tree, &renderer, &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(w, h)));
            let layout_ms = start.elapsed().as_secs_f64() * 1000.0;
            let start = Instant::now();
            renderer.reset(full);
            element.as_widget().draw(&tree, &mut renderer, &Theme::Dark, &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout), iced::mouse::Cursor::Unavailable, &full);
            // As the tiny-skia compositor: a changed background repaints the window in one pass.
            let changes = if app.backdrop() != backdrop { vec![full] } else {
                damage::group(damage::diff(&previous, renderer.layers(), |layer| vec![layer.bounds], iced_tiny_skia::Layer::damage), full)
            };
            backdrop = app.backdrop();
            previous = renderer.layers().to_vec();
            let regions = changes.len();
            renderer.draw(&mut pixels.as_mut(), &mut mask, &viewport, &changes, BG);
            (layout_ms, start.elapsed().as_secs_f64() * 1000.0, regions)
        };
        let _ = frame(&app);
        eprintln!("{} clips, scale {scale}", app.sounds.len());
        for round in 0..2 {
            for (name, page) in [("effects", 6u8), ("soundpad", 4), ("rvc", 1), ("settings", 2), ("logs", 5), ("main", 0)] {
                let start = Instant::now();
                let _ = app.update(Msg::Page(page));
                let update_ms = start.elapsed().as_secs_f64() * 1000.0;
                let start = Instant::now();
                let mosaic = app.page_mosaic();
                let mosaic_ms = start.elapsed().as_secs_f64() * 1000.0;
                app.page_shift = None;
                let (layout_ms, draw_ms, regions) = frame(&app);
                eprintln!("round {round} {name:9}: update {update_ms:5.1} | mosaic {mosaic_ms:5.1} ({}) | new page view+layout {layout_ms:5.1} draw {draw_ms:5.1} ms in {regions} region(s)",
                    mosaic.is_some());
            }
        }
        let _ = app.update(Msg::Page(4));
        let _ = frame(&app);
        let sort = |app: &App| app.sound_sort;
        let mut steps: Vec<(&str, Msg)> = vec![("filter a", Msg::SoundpadFilter("a".into())), ("filter ge", Msg::SoundpadFilter("ge".into())), ("filter clear", Msg::SoundpadFilter(String::new()))];
        steps.push(("sort new", Msg::SoundpadSort(SoundSort::from_code((sort(&app).code() + 3) % 4))));
        steps.push(("sort back", Msg::SoundpadSort(sort(&app))));
        for (name, msg) in steps {
            let _ = app.update(msg);
            let (layout_ms, draw_ms, regions) = frame(&app);
            eprintln!("soundpad {name:12}: view+layout {layout_ms:5.1} draw {draw_ms:5.1} ms in {regions} region(s)");
        }
    }
    /// Frames of the page-switch pixelation, with their draw + raster time.
    #[test]
    #[ignore]
    fn page_shift_frames() {
        use iced::advanced::{Renderer as _, Layout, graphics::Viewport};
        let dir = PathBuf::from(std::env::var("MNR_DESIGN_DIR").expect("MNR_DESIGN_DIR"));
        let (w, h) = (1040.0_f32, 740.0_f32);
        let size = Size::new(w as u32, h as u32);
        let full = iced::Rectangle::with_size(Size::new(w, h));
        let frame = |app: &App| {
            let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
            let mut tree = iced::advanced::widget::Tree::empty();
            let mut element = app.view(window::Id::unique());
            tree.diff(element.as_widget());
            let layout = element.as_widget_mut().layout(&mut tree, &renderer, &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(w, h)));
            let start = Instant::now();
            renderer.reset(full);
            element.as_widget().draw(&tree, &mut renderer, &Theme::Dark, &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout), iced::mouse::Cursor::Unavailable, &full);
            let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
            let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
            renderer.draw(&mut pixels.as_mut(), &mut mask, &Viewport::with_physical_size(size, 1.0), &[full], BG);
            (pixels, start.elapsed().as_secs_f64() * 1000.0)
        };
        // As the window does it: repaint only the regions that changed since the last frame.
        let windowed = |app: &App, tree: &mut iced::advanced::widget::Tree, previous: &mut Vec<iced_tiny_skia::Layer>, renderer: &mut iced::Renderer, pixels: &mut tiny_skia::Pixmap| {
            use iced::advanced::graphics::damage;
            let mut element = app.view(window::Id::unique());
            tree.diff(element.as_widget());
            let layout = element.as_widget_mut().layout(tree, renderer, &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(w, h)));
            let start = Instant::now();
            renderer.reset(full);
            element.as_widget().draw(tree, renderer, &Theme::Dark, &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout), iced::mouse::Cursor::Unavailable, &full);
            let changes = damage::group(damage::diff(previous, renderer.layers(), |layer| vec![layer.bounds], iced_tiny_skia::Layer::damage), full);
            *previous = renderer.layers().to_vec();
            let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
            renderer.draw(&mut pixels.as_mut(), &mut mask, &Viewport::with_physical_size(size, 1.0), &changes, BG);
            (changes.len(), start.elapsed().as_secs_f64() * 1000.0)
        };
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window = Some(window::Id::unique());
        let _ = frame(&app); // lays out the page area
        app.sound_folder = Some(PathBuf::from("test sounds"));
        app.sounds = (0..454).map(|i| Sound {
            name: format!("Section {} - Sound {i:03}.wav", i / 5), path: PathBuf::new(),
            key: 0, volume: 100, played: 0, modified: 0, state: SoundState::Unloaded,
        }).collect();
        for (name, page) in [("main", 0u8), ("effects", 6), ("soundpad", 4), ("rvc", 1), ("settings", 2), ("logs", 5)] {
            let _ = app.update(Msg::Page(page));
            let started = Instant::now();
            let mosaic = app.page_mosaic();
            eprintln!("mosaic {name}: {:.1} ms ({:?})", started.elapsed().as_secs_f64() * 1000.0, mosaic.map(|m| (m.width, m.height)));
        }
        let _ = app.update(Msg::Page(0));
        app.page_shift = None;
        app.effects_page = true;
        let started = Instant::now();
        let to = app.page_mosaic().unwrap();
        eprintln!("new page offscreen: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        {
            let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
            let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
            let mut previous = Vec::new();
            let mut tree = iced::advanced::widget::Tree::empty();
            let begin = Instant::now() - Duration::from_millis(1);
            app.page_shift = Some((to.clone(), begin));
            for step in 0..24u64 {
                app.page_shift = Some((to.clone(), begin - Duration::from_millis(step * 16)));
                let (regions, took) = windowed(&app, &mut tree, &mut previous, &mut renderer, &mut pixels);
                eprintln!("windowed frame {:3} ms: {regions} damage regions, {took:.1} ms", step * 16);
            }
        }
        for ms in [2u64, 14, 26, 38, 50, 62, 74, 86, 98, 110] {
            app.page_shift = Some((to.clone(), Instant::now() - Duration::from_millis(ms)));
            let (pixels, took) = frame(&app);
            eprintln!("frame at {ms} ms: {took:.1} ms");
            let mut data = pixels.data().to_vec();
            for px in data.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
            let mut encoder = png::Encoder::new(std::fs::File::create(dir.join(format!("shift-{ms:03}.png"))).unwrap(), size.width, size.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&data).unwrap();
        }
    }
    /// «by ARKANOID» hovered: the glitch must cost next to nothing between its patterns and
    /// repaint only its pieces when the pattern changes.
    #[test]
    #[ignore]
    fn glitch_frames() {
        use iced::advanced::{Renderer as _, Layout, graphics::{Viewport, damage}};
        let (w, h) = (1040.0_f32, 740.0_f32);
        let size = Size::new(w as u32, h as u32);
        let full = iced::Rectangle::with_size(Size::new(w, h));
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window = Some(window::Id::unique());
        let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
        let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
        let mut tree = iced::advanced::widget::Tree::empty();
        let mut previous: Vec<iced_tiny_skia::Layer> = Vec::new();
        let mut frame = |app: &App| {
            let mut element = app.view(window::Id::unique());
            tree.diff(element.as_widget());
            let layout = element.as_widget_mut().layout(&mut tree, &renderer, &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(w, h)));
            let start = Instant::now();
            renderer.reset(full);
            element.as_widget().draw(&tree, &mut renderer, &Theme::Dark, &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout), iced::mouse::Cursor::Unavailable, &full);
            let changes = damage::group(damage::diff(&previous, renderer.layers(), |layer| vec![layer.bounds], iced_tiny_skia::Layer::damage), full);
            previous = renderer.layers().to_vec();
            let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
            renderer.draw(&mut pixels.as_mut(), &mut mask, &Viewport::with_physical_size(size, 1.0), &changes, BG);
            let area: f32 = changes.iter().map(|r| r.width * r.height).sum();
            (area / (w * h), start.elapsed().as_secs_f64() * 1000.0)
        };
        let _ = frame(&app);
        let _ = frame(&app);
        let started = Instant::now();
        app.glitch_mosaic = app.window_mosaic(App::window_size()).map(|m| (Instant::now(), App::window_size(), m));
        eprintln!("window picture: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        let start = Instant::now() - Duration::from_millis(300);
        app.glitch = Some(start);
        let (area, took) = frame(&app);
        eprintln!("pattern change: {:.0} % of the window, {took:.1} ms", area * 100.0);
        let (same, took) = frame(&app);
        eprintln!("same pattern: {:.0} % of the window, {took:.1} ms", same * 100.0);
        let mut worst = (0.0_f32, 0.0_f64);
        for tick in 1..10 {
            app.glitch = Some(Instant::now() - Duration::from_millis(300 + 70 * tick));
            let (a, t) = frame(&app);
            eprintln!("  pattern {tick}: {:.0} %, {t:.1} ms", a * 100.0);
            worst = (worst.0.max(a), worst.1.max(t));
        }
        eprintln!("next patterns, worst: {:.0} % of the window, {:.1} ms", worst.0 * 100.0, worst.1);
        assert!(same < 0.01, "between patterns nothing repaints");
        assert!(area < 0.8, "a new pattern repaints its pieces, not the whole window");
        app.glitch = None;
        let (gone, _) = frame(&app);
        assert!(gone > 0.0, "leaving repaints the last pattern away");
        // The restart's air: one frame of it after another, 16 ms apart.
        app.update_ready = true;
        let _ = frame(&app);
        let mut worst = (0.0_f32, 0.0_f64, 0.0_f64);
        for i in 0..30u64 {
            let mut restart = tacho::Restart::new(Instant::now() - Duration::from_millis(2500 + 16 * i), tacho::Fall::KnockOut);
            restart.advance(1, Instant::now() - Duration::from_millis(800));
            app.restart = Some(restart);
            let (a, t) = frame(&app);
            worst = (worst.0.max(a), worst.1.max(t), worst.2 + t / 30.0);
        }
        eprintln!("restart air: worst {:.0} % of the window, worst {:.1} ms, mean {:.1} ms", worst.0 * 100.0, worst.1, worst.2);
        // The ready card's confetti over the window, frame after frame.
        (app.restart, app.update_ready) = (None, false);
        app.ready_preview = Some(Instant::now());
        let _ = frame(&app);
        let mut worst = (0.0_f32, 0.0_f64, 0.0_f64);
        let frames = ((tacho::ready::CONFETTI.1 - tacho::ready::CONFETTI.0) / 16.0) as u64;
        for i in 0..frames {
            app.ready_fx = Some(Instant::now() - Duration::from_millis(tacho::ready::CONFETTI.0 as u64 + 16 * i));
            let (a, t) = frame(&app);
            worst = (worst.0.max(a), worst.1.max(t), worst.2 + t / frames as f64);
        }
        eprintln!("confetti: worst {:.0} % of the window, worst {:.1} ms, mean {:.1} ms", worst.0 * 100.0, worst.1, worst.2);
    }
    #[test]
    #[ignore]
    fn design_snapshots() {
        use iced::advanced::{Renderer as _, Layout, graphics::{damage, Viewport}};
        let dir = PathBuf::from(std::env::var("MNR_DESIGN_DIR").expect("MNR_DESIGN_DIR"));
        let window = std::cell::Cell::new((1040.0_f32, 740.0_f32));
        let scale = std::cell::Cell::new(1.0_f32);
        let render = |app: &App, name: &str| {
            let (w, h) = window.get();
            let size = Size::new((w * scale.get()) as u32, (h * scale.get()) as u32);
            let mut renderer = iced::Renderer::new(Font::with_name("Segoe UI"), iced::Pixels(14.0));
            let mut tree = iced::advanced::widget::Tree::empty();
            let limits = iced::advanced::layout::Limits::new(Size::ZERO, Size::new(w, h));
            let mut element = app.view(window::Id::unique());
            tree.diff(element.as_widget());
            let layout = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
            renderer.reset(iced::Rectangle::with_size(Size::new(w, h)));
            element.as_widget().draw(&tree, &mut renderer, &Theme::Dark,
                &iced::advanced::renderer::Style { text_color: INK }, Layout::new(&layout),
                iced::mouse::Cursor::Unavailable, &iced::Rectangle::with_size(Size::new(w, h)));
            let changes = damage::group(damage::diff(&[], renderer.layers(),
                |layer| vec![layer.bounds], iced_tiny_skia::Layer::damage),
                iced::Rectangle::with_size(Size::new(w, h)));
            let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).unwrap();
            let mut mask = tiny_skia::Mask::new(size.width, size.height).unwrap();
            renderer.draw(&mut pixels.as_mut(), &mut mask, &Viewport::with_physical_size(size, scale.get()), &changes, BG);
            // The renderer writes BGRA into the pixmap; PNG wants RGBA.
            let mut data = pixels.data().to_vec();
            for px in data.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
            let mut encoder = png::Encoder::new(std::fs::File::create(dir.join(format!("design-{name}.png"))).unwrap(), size.width, size.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&data).unwrap();
        };
        let (mut app, _) = App::from_settings(Settings::for_test("")).unwrap().unwrap();
        app.window = Some(window::Id::unique());
        app.inputs = vec![Device { id: "mic".into(), name: "Microphone (HyperX QuadCast S)".into() }];
        app.input = app.inputs.first().cloned();
        app.outputs = vec![
            Device { id: "TAG".into(), name: "Mic Noize Microphone".into() },
            Device { id: "dac".into(), name: "Speakers (SMSL USB DAC)".into() },
        ];
        app.output = app.outputs.first().cloned();
        app.headphone_output = app.outputs.get(1).cloned();
        // (modifiers << 8) | virtual key: 1 Ctrl, 2 Alt; 36 Home, 4/6 mouse 3/5, 0x54 T.
        app.keys[..13].copy_from_slice(&[3 << 8 | 0x54, 3 << 8 | 36, 1 << 8 | 36, 2 << 8 | 36, 36, 0, 3 << 8 | 6, 1 << 8 | 6, 2 << 8 | 6, 6, 1 << 8 | 4, 2 << 8 | 4, 4]);
        app.in_peak = 0.2;
        app.peak = 0.08;
        app.controls.intensity = 0.99;
        render(&app, "main");
        app.controls.intensity = 1.35;
        app.route_open = true;
        render(&app, "main-red");
        app.controls.intensity = 0.99;
        app.route_open = false;
        app.headphone_page = true;
        render(&app, "headphones");
        app.headphone_message = "Headphone host rejected request; see results/tag-headphones.log".into();
        render(&app, "headphones-no-line");
        {
            // «by ARKANOID» hovered: the window glitches.
            let page = app.headphone_page;
            app.headphone_page = false;
            app.glitch_mosaic = app.window_mosaic(App::window_size()).map(|m| (Instant::now(), App::window_size(), m));
            app.glitch = Some(Instant::now() - Duration::from_millis(420));
            render(&app, "glitch");
            (app.glitch, app.glitch_mosaic, app.headphone_page) = (None, None, page);
        }
        {
            app.headphone_page = false;
            app.ready_preview = Some(Instant::now());
            app.ready_mosaic = mosaic_of(app.ready_card(), tacho::ready::CARD);
            for ms in [250u64, 560, 950, 1350, 1750, 2150, 2600, 3000, 3400] {
                app.ready_fx = Some(Instant::now() - Duration::from_millis(ms));
                render(&app, &format!("ready-{ms:04}"));
            }
            (app.ready_fx, app.ready_preview, app.ready_mosaic) = (None, None, None);
            // «Перезапустить» clicked: pressed, each fall on its way, then the air blowing in.
            let ready = app.update_ready;
            app.update_ready = true;
            let ago = |ms: u64| Instant::now() - Duration::from_millis(ms);
            app.restart = Some(tacho::Restart::new(ago(90), tacho::Fall::Sink));
            render(&app, "restart-press");
            for fall in tacho::Fall::ALL {
                for ms in [420u64, 700] {
                    app.restart = Some(tacho::Restart::new(ago(ms), fall));
                    render(&app, &format!("restart-{fall:?}-{ms}").to_lowercase());
                }
            }
            for (ms, step) in [(1500u64, 0u8), (3200, 1), (5000, 2)] {
                let mut restart = tacho::Restart::new(ago(ms), tacho::Fall::KnockOut);
                restart.advance(step, ago(600));
                app.restart = Some(restart);
                render(&app, &format!("restart-wind-{ms}"));
            }
            (app.restart, app.update_ready) = (None, ready);
            // «Подбор под микрофон»: idle, the quiet sweep half way, the spoken check, the result.
            app.snapshot.state = 2;
            app.denoiser.0 = 1;
            render(&app, "tune-idle");
            let start = Instant::now() - Duration::from_millis(1600);
            let mut sweep = tune::Tune::new(0.4, start);
            let mut t = start;
            let residual = |s: u8| [-52.0_f32, -58.0, -63.0, -67.0, -71.0, -74.0, -76.0, -78.0, -80.0, -82.0][tune::STEPS.iter().position(|&x| x == s).unwrap()];
            let amp = |db: f32| 10f32.powf(db / 20.0);
            while sweep.phase == tune::Phase::Quiet && t < Instant::now() {
                t += Duration::from_millis(50);
                let r = residual(sweep.strength());
                sweep.feed(t, amp(-50.0), amp(r));
            }
            app.tune = Some(sweep);
            render(&app, "tune-quiet");
            let mut voice = tune::Tune::new(0.4, Instant::now() - Duration::from_secs(10));
            let mut t = Instant::now() - Duration::from_secs(10);
            while voice.phase == tune::Phase::Quiet {
                t += Duration::from_millis(50);
                let r = residual(voice.strength());
                voice.feed(t, amp(-50.0), amp(r));
            }
            voice.phase_started = Instant::now() - Duration::from_millis(1200);
            app.tune = Some(voice);
            render(&app, "tune-voice");
            let mut done = tune::Tune::new(0.4, Instant::now() - Duration::from_secs(10));
            let mut t = Instant::now() - Duration::from_secs(10);
            while done.running() {
                t += Duration::from_millis(50);
                let (i, o) = if done.phase == tune::Phase::Quiet { (amp(-50.0), amp(residual(done.strength()))) } else { (amp(-20.0), amp(-20.5)) };
                done.feed(t, i, o);
            }
            app.tune = Some(done);
            render(&app, "tune-done");
            (app.tune, app.snapshot.state, app.denoiser.0) = (None, 0, 0);
            app.headphone_page = true;
        }
        app.headphone_message.clear();
        app.headphone_page = false;
        {
            use components::{Item, Phase, fake_status};
            // First run on an Ada card: the core downloads, the models wait for it.
            let gpu = std::mem::replace(&mut app.gpu, Ok(("ada".into(), "NVIDIA GeForce RTX 4070".into())));
            let now = Instant::now();
            (app.core_present, app.models_present, app.core_installing, app.driver_ready) = (false, false, true, false);
            (app.snapshot.state, app.denoiser.0) = (0, 0);
            app.transfer = vec![(now - Duration::from_secs(3), 380 * MB), (now, 412 * MB)];
            fake_status(Item::Core, Phase::Download, 412 * MB, 1130 * MB);
            render(&app, "setup-download");
            app.transfer_moved = now - Duration::from_secs(23);
            app.transfer = vec![(now - Duration::from_secs(3), 412 * MB), (now, 412 * MB)];
            render(&app, "setup-stalled");
            app.transfer_moved = now;
            app.transfer = vec![(now - Duration::from_secs(2), 500 * MB), (now, 820 * MB)];
            fake_status(Item::Core, Phase::Unpack, 820 * MB, 1130 * MB);
            render(&app, "setup-unpack");
            (app.core_present, app.transfer) = (true, vec![]);
            fake_status(Item::Models, Phase::Connect, 0, 0);
            render(&app, "setup-models");
            // A PC without NVIDIA whose download broke off, then one whose disk is full.
            app.gpu = Err("NVIDIA GPU не найден".into());
            (app.core_present, app.core_installing) = (false, false);
            app.setup_error = "io: Connection reset by peer (os error 10054)".into();
            render(&app, "setup-failed");
            app.component_root = PathBuf::from(r"C:\Users\a\AppData\Roaming\Mic Noize\Components");
            app.setup_error = "There is not enough space on the disk. (os error 112)".into();
            render(&app, "setup-disk-full");
            // The core is in, the models broke off, the voice runs on the CPU meanwhile.
            app.gpu = Ok(("ada".into(), "NVIDIA GeForce RTX 4070".into()));
            app.core_present = true;
            app.setup_error = "модели NVIDIA для ada: io: Connection reset by peer (os error 10054)".into();
            render(&app, "setup-models-failed");
            app.models_present = true;
            app.setup_error.clear();
            render(&app, "setup-driver");
            window.set((960.0, 680.0));
            render(&app, "setup-driver-smallest-window");
            window.set((1040.0, 740.0));
            app.driver_installing = true;
            render(&app, "setup-driver-installing");
            app.driver_installing = false;
            app.driver_error = "Установка отменена: нужны права администратора. Нажмите кнопку ещё раз и подтвердите запрос Windows.".into();
            render(&app, "setup-driver-refused");
            // Everything installed: the card leaves unless a device choice blocks the voice.
            (app.driver_error, app.driver_ready, app.devices_known) = (String::new(), true, true);
            render(&app, "setup-all-installed");
            let (input, output, inputs) = (app.input.take(), app.output.clone(), app.inputs.clone());
            render(&app, "setup-no-microphone");
            app.inputs.clear();
            render(&app, "setup-no-microphone-found");
            (app.input, app.inputs) = (input, inputs);
            app.output = app.outputs.get(1).cloned();
            render(&app, "setup-wrong-output");
            (app.output, app.devices_known, app.gpu) = (output, false, gpu);
        }
        app.effects_page = true;
        app.clips = (0..6).map(|i| Sound {
            name: format!("Запись 2026-09-25 14-2{i}-0{i} (mix).wav"), path: PathBuf::new(),
            key: 0, volume: 100, played: 0, modified: 0, state: SoundState::Loaded(2.4),
        }).collect();
        render(&app, "effects");
        app.effects_group = 1;
        render(&app, "effects-phrases");
        app.effects_group = 0;
        app.effect_details = Some(5);
        window.set((960.0, 680.0));
        render(&app, "effects-min-echo-details");
        scale.set(2.0);
        render(&app, "effects-min-echo-details-200pct");
        scale.set(1.0);
        window.set((1040.0, 740.0));
        app.effect_details = None;
        app.controls.overload = true;
        let boost = app.controls.boost;
        app.controls.boost = 15.0;
        render(&app, "effects-overload");
        app.controls.boost = boost;
        app.controls.overload = false;
        app.keys_down[0] = 1 << 0x11;
        render(&app, "effects-ctrl-held");
        app.keys_down = [0; 4];
        app.effects_page = false;
        app.soundpad_page = true;
        app.sound_folder = Some(PathBuf::from(r"E:\Dropbox\sounds"));
        app.sounds = ["a chto", "a-a chevo", "aga spasiba", "aleeo", "bogdan - 48chasov", "bruh", "chto proishodit", "davay davay", "gerych - nu davay", "nastya - privet"]
            .iter().map(|n| Sound {
                name: format!("{n}.wav"), path: PathBuf::new(), key: 0, volume: 100, played: 0, modified: 0,
                state: SoundState::Loaded(3.0),
            }).collect();
        app.sound_scroll = (0.0, 420.0);
        render(&app, "soundpad");
        app.soundpad_page = false;
        app.rvc_page = true;
        render(&app, "rvc");
        app.rvc_advanced = true;
        render(&app, "rvc-advanced");
        app.rvc_advanced = false;
        app.rvc_page = false;
        app.details = true;
        render(&app, "settings");
        app.details = false;
        app.logs_page = true;
        render(&app, "logs");
    }
    #[test]
    fn sound_rows_stay_bounded_with_distant_focus() {
        // A selected first clip must not mount the 400 intervening rows while scrolling.
        let rows = sound_rows(1000, 12_800.0, 640.0, 32.0, Some(0));
        assert_eq!(rows, std::iter::once(0).chain(397..423).collect::<Vec<_>>());
        // Keyboard navigation still has a mounted target on either side of the viewport.
        assert_eq!(sound_rows(1000, 0.0, 640.0, 32.0, Some(999)),
            (0..23).chain(std::iter::once(999)).collect::<Vec<_>>());
        assert_eq!(sound_rows(1000, 0.0, 640.0, 32.0, Some(2)), (0..23).collect::<Vec<_>>());
        // Filtering or selecting a short section must mount its clips immediately.
        assert_eq!(sound_rows(3, 12_800.0, 640.0, 32.0, None), vec![0, 1, 2]);
        assert_eq!(sound_rows(3, 12_800.0, 640.0, 32.0, Some(1)), vec![0, 1, 2]);
        assert!(sound_rows(0, 0.0, 640.0, 32.0, Some(0)).is_empty());
    }
    #[test]
    fn studio_roll_mounts_only_visible_notes() {
        let rows = sound_rows(studio::NOTES, studio_offset(studio::ROOT_NOTE, 100, 300.0), 300.0, 28.0, Some(47));
        assert!(rows.len() <= 22, "the roll must not rebuild all 48 note rows");
        assert!(rows.contains(&23), "C4 starts in the viewport");
        assert!(rows.contains(&47), "an off-screen keyboard target stays mounted");
    }
    #[test]
    fn clock_formats_minutes() {
        assert_eq!(clock_text(0.0), "0:00");
        assert_eq!(clock_text(7.4), "0:07");
        assert_eq!(clock_text(125.6), "2:06");
    }
    #[test]
    fn focus_scroll_moves_only_when_outside_viewport() {
        assert_eq!(focus_scroll_delta(120.0, 30.0, 100.0, 200.0), 0.0);
        assert_eq!(focus_scroll_delta(90.0, 30.0, 100.0, 200.0), -18.0);
        assert_eq!(focus_scroll_delta(280.0, 30.0, 100.0, 200.0), 18.0);
    }
}
