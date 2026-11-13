//! Colours, fonts, line icons and the small widgets the window is built from.
//!
//! Two palettes share one layout: a dark one (warm greys, the default) and a
//! light one (ivory and oat). Both use the same clay accent.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin,
    Painter, Pos2, Rect, Response, RichText, Sense, Shadow, Shape, Stroke, StrokeKind, TextStyle,
    Theme, Ui, Visuals, pos2, vec2,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Top bar, right panel.
    pub bg: Color32,
    /// Left sidebar.
    pub side: Color32,
    /// Behind the pages.
    pub canvas: Color32,
    /// The find bar, menus and dialogs.
    pub raised: Color32,
    /// Cards inside the panels.
    pub card: Color32,
    pub hover: Color32,
    /// Pressed tools and the open file in the recent list.
    pub active: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub icon: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub good: Color32,
    pub good_bg: Color32,
    pub warn: Color32,
    pub warn_bg: Color32,
    pub bad: Color32,
    pub bad_bg: Color32,
    /// The one filled button in a group (Save changes).
    pub primary: Color32,
    pub on_primary: Color32,
    pub shadow: Color32,
}

const fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

const fn rgba(c: u32, a: u8) -> Color32 {
    // Stored premultiplied, as egui expects.
    let f = a as u32;
    Color32::from_rgba_premultiplied(
        (((c >> 16) & 255) * f / 255) as u8,
        (((c >> 8) & 255) * f / 255) as u8,
        ((c & 255) * f / 255) as u8,
        a,
    )
}

pub const DARK: Palette = Palette {
    dark: true,
    bg: rgb(0x262624),
    side: rgb(0x1F1E1D),
    canvas: rgb(0x262624),
    raised: rgb(0x30302E),
    card: rgb(0x1F1E1D),
    hover: rgb(0x30302E),
    active: rgb(0x3D3D3A),
    border: rgba(0xDEDCD1, 30),
    text: rgb(0xF5F4ED),
    text2: rgb(0xE8E6DC),
    icon: rgb(0xC2C0B6),
    muted: rgb(0x9C9A92),
    faint: rgb(0x5E5D59),
    accent: rgb(0xD97757),
    good: rgb(0x8DB87A),
    good_bg: rgba(0x8DB87A, 36),
    warn: rgb(0xE3B25B),
    warn_bg: rgba(0xE3B25B, 36),
    bad: rgb(0xEE8572),
    bad_bg: rgba(0xEE8572, 36),
    primary: rgb(0xF5F4ED),
    on_primary: rgb(0x141413),
    shadow: Color32::from_black_alpha(90),
};

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: rgb(0xFAF9F5),
    side: rgb(0xF5F4ED),
    canvas: rgb(0xF0EEE6),
    raised: rgb(0xFFFFFF),
    card: rgb(0xFFFFFF),
    hover: rgb(0xEAE8DF),
    active: rgb(0xE3E1D7),
    border: rgba(0x1F1E1D, 31),
    text: rgb(0x141413),
    text2: rgb(0x30302E),
    icon: rgb(0x3D3D3A),
    muted: rgb(0x5E5D59),
    faint: rgb(0xB0AEA5),
    accent: rgb(0xD97757),
    good: rgb(0x3F6B2E),
    good_bg: rgba(0x788C5D, 36),
    warn: rgb(0x8A5A00),
    warn_bg: rgba(0xC9A82D, 44),
    bad: rgb(0xA32D20),
    bad_bg: rgba(0xDC503C, 32),
    primary: rgb(0x141413),
    on_primary: rgb(0xFAF9F5),
    shadow: Color32::from_black_alpha(34),
};

pub fn palette(dark: bool) -> Palette {
    if dark { DARK } else { LIGHT }
}

/// Fonts and both themes. Call once at start-up.
pub fn install(ctx: &egui::Context, dark: bool) {
    let mut f = FontDefinitions::default();
    f.font_data.insert("onest".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Onest-Regular.ttf"))));
    f.font_data.insert("onest-medium".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Onest-Medium.ttf"))));
    f.font_data.insert("mono".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"))));
    let prop = f.families.entry(FontFamily::Proportional).or_default();
    prop.insert(0, "onest".into());
    let fallbacks: Vec<String> = prop.iter().skip(1).cloned().collect();
    f.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
    let mut medium = vec!["onest-medium".to_string()];
    medium.extend(fallbacks);
    f.families.insert(FontFamily::Name("medium".into()), medium);
    ctx.set_fonts(f);

    ctx.set_style_of(Theme::Dark, style(&DARK));
    ctx.set_style_of(Theme::Light, style(&LIGHT));
    set_dark(ctx, dark);
}

pub fn set_dark(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark { Theme::Dark } else { Theme::Light });
}

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

fn style(p: &Palette) -> egui::Style {
    let mut s = egui::Style::default();
    s.text_styles = [
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Heading, medium(18.0)),
        (TextStyle::Monospace, mono(13.0)),
    ]
    .into();
    s.spacing.item_spacing = vec2(8.0, 6.0);
    s.spacing.button_padding = vec2(10.0, 5.0);
    s.spacing.interact_size.y = 28.0;
    s.spacing.menu_margin = Margin::same(6);
    s.spacing.window_margin = Margin::same(16);

    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.raised;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: p.shadow };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: p.shadow };
    v.extreme_bg_color = if p.dark { rgb(0x1A1918) } else { rgb(0xFFFFFF) };
    v.faint_bg_color = p.side;
    v.code_bg_color = p.side;
    v.hyperlink_color = if p.dark { p.accent } else { rgb(0xA9502F) };
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.bad;
    v.selection.bg_fill = if p.dark { rgba(0xD97757, 92) } else { rgba(0xD97757, 70) };
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v.indent_has_left_vline = false;

    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text2);
    w.noninteractive.corner_radius = r;
    let button = if p.dark { rgb(0x30302E) } else { rgb(0xFFFFFF) };
    w.inactive.bg_fill = button;
    w.inactive.weak_bg_fill = button;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    let hover = if p.dark { rgb(0x3D3D3A) } else { rgb(0xF0EEE6) };
    w.hovered.bg_fill = hover;
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    let press = if p.dark { rgb(0x4D4C48) } else { rgb(0xE8E6DC) };
    w.active.bg_fill = press;
    w.active.weak_bg_fill = press;
    w.active.bg_stroke = Stroke::new(1.0, p.border);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.hovered;
    s.visuals = v;
    s
}

// ---------------------------------------------------------------------------
// Icons: simple line drawings on a 24 x 24 grid, scaled to the rect they get.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Sidebar,
    PanelRight,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    Folder,
    Save,
    Search,
    Printer,
    Highlighter,
    Note,
    Text,
    Sign,
    Rotate,
    Undo,
    Redo,
    Minus,
    Plus,
    Close,
    FitWidth,
    FitPage,
    File,
    Ruler,
    Lock,
    Shield,
    Warn,
    Check,
    Sun,
    Moon,
    Diff,
}

pub fn paint_icon(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()) / 24.0;
    let o = rect.center() - vec2(12.0, 12.0) * s;
    let pt = |x: f32, y: f32| pos2(o.x + x * s, o.y + y * s);
    let stroke = Stroke::new((1.7 * s).max(1.15), color);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), stroke));
    };
    let closed = |pts: &[(f32, f32)]| {
        p.add(Shape::closed_line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), stroke));
    };
    let circle = |x: f32, y: f32, r: f32| {
        p.circle_stroke(pt(x, y), r * s, stroke);
    };
    let rrect = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        p.rect_stroke(Rect::from_min_max(pt(x0, y0), pt(x1, y1)), r * s, stroke, StrokeKind::Middle);
    };
    // Degrees, clockwise on screen, 0 = pointing right.
    let arc = |cx: f32, cy: f32, r: f32, a0: f32, a1: f32| -> Vec<Pos2> {
        let n = ((a1 - a0).abs() / 12.0).ceil().max(2.0) as usize;
        (0..=n)
            .map(|k| {
                let a = (a0 + (a1 - a0) * k as f32 / n as f32).to_radians();
                pt(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    };
    match icon {
        Icon::Sidebar => {
            rrect(3.0, 4.5, 21.0, 19.5, 3.0);
            line(&[(9.0, 4.5), (9.0, 19.5)]);
        }
        Icon::PanelRight => {
            rrect(3.0, 4.5, 21.0, 19.5, 3.0);
            line(&[(15.0, 4.5), (15.0, 19.5)]);
        }
        Icon::ChevronLeft => line(&[(15.0, 18.0), (9.0, 12.0), (15.0, 6.0)]),
        Icon::ChevronRight => line(&[(9.0, 6.0), (15.0, 12.0), (9.0, 18.0)]),
        Icon::ChevronUp => line(&[(6.0, 15.0), (12.0, 9.0), (18.0, 15.0)]),
        Icon::ChevronDown => line(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)]),
        Icon::Folder => closed(&[(3.0, 6.0), (9.0, 6.0), (11.0, 8.0), (21.0, 8.0), (21.0, 19.0), (3.0, 19.0)]),
        Icon::Save => {
            line(&[(12.0, 4.0), (12.0, 15.0)]);
            line(&[(7.5, 10.5), (12.0, 15.0), (16.5, 10.5)]);
            line(&[(4.0, 16.0), (4.0, 20.0), (20.0, 20.0), (20.0, 16.0)]);
        }
        Icon::Search => {
            circle(11.0, 11.0, 6.5);
            line(&[(16.0, 16.0), (20.0, 20.0)]);
        }
        Icon::Printer => {
            line(&[(7.0, 9.0), (7.0, 3.5), (17.0, 3.5), (17.0, 9.0)]);
            rrect(3.0, 9.0, 21.0, 17.0, 2.0);
            closed(&[(7.0, 14.0), (17.0, 14.0), (17.0, 20.5), (7.0, 20.5)]);
        }
        Icon::Highlighter => {
            closed(&[(14.0, 4.5), (19.5, 10.0), (12.0, 17.5), (6.5, 12.0)]);
            line(&[(8.0, 13.5), (4.0, 17.5), (4.0, 20.0), (8.5, 20.0), (10.5, 16.0)]);
        }
        Icon::Note => {
            closed(&[(5.0, 4.0), (19.0, 4.0), (19.0, 14.0), (13.0, 20.0), (5.0, 20.0)]);
            line(&[(13.0, 20.0), (13.0, 14.0), (19.0, 14.0)]);
        }
        Icon::Text => {
            line(&[(5.0, 7.0), (5.0, 4.5), (19.0, 4.5), (19.0, 7.0)]);
            line(&[(12.0, 4.5), (12.0, 19.5)]);
            line(&[(9.0, 19.5), (15.0, 19.5)]);
        }
        Icon::Sign => {
            closed(&[(14.5, 3.5), (18.5, 7.5), (10.5, 15.5), (6.5, 15.5), (6.5, 11.5)]);
            line(&[(3.0, 20.0), (5.5, 18.6), (8.0, 18.0), (10.5, 19.3), (13.0, 20.0), (15.5, 18.8), (18.5, 18.2)]);
        }
        Icon::Rotate => {
            p.add(Shape::line(arc(12.0, 12.0, 7.5, 0.0, 310.0), stroke));
            line(&[(19.8, 4.2), (19.8, 9.2), (14.8, 9.2)]);
        }
        Icon::Undo => {
            line(&[(9.0, 14.0), (4.0, 9.0), (9.0, 4.0)]);
            let mut pts = vec![pt(4.0, 9.0)];
            pts.extend(arc(14.0, 15.0, 6.0, -90.0, 90.0));
            pts.push(pt(11.0, 21.0));
            p.add(Shape::line(pts, stroke));
        }
        Icon::Redo => {
            line(&[(15.0, 14.0), (20.0, 9.0), (15.0, 4.0)]);
            let mut pts = vec![pt(20.0, 9.0)];
            pts.extend(arc(10.0, 15.0, 6.0, 270.0, 90.0));
            pts.push(pt(13.0, 21.0));
            p.add(Shape::line(pts, stroke));
        }
        Icon::Minus => line(&[(5.0, 12.0), (19.0, 12.0)]),
        Icon::Plus => {
            line(&[(5.0, 12.0), (19.0, 12.0)]);
            line(&[(12.0, 5.0), (12.0, 19.0)]);
        }
        Icon::Close => {
            line(&[(6.0, 6.0), (18.0, 18.0)]);
            line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::FitWidth => {
            line(&[(4.0, 5.0), (4.0, 19.0)]);
            line(&[(20.0, 5.0), (20.0, 19.0)]);
            line(&[(8.0, 12.0), (16.0, 12.0)]);
            line(&[(10.5, 9.5), (8.0, 12.0), (10.5, 14.5)]);
            line(&[(13.5, 9.5), (16.0, 12.0), (13.5, 14.5)]);
        }
        Icon::FitPage => {
            rrect(6.0, 3.0, 18.0, 21.0, 2.0);
            line(&[(9.5, 8.0), (14.5, 8.0)]);
            line(&[(9.5, 12.0), (14.5, 12.0)]);
            line(&[(9.5, 16.0), (12.5, 16.0)]);
        }
        Icon::File => {
            closed(&[(5.0, 3.0), (14.0, 3.0), (19.0, 8.0), (19.0, 21.0), (5.0, 21.0)]);
            line(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)]);
        }
        Icon::Ruler => {
            rrect(3.0, 8.0, 21.0, 16.0, 1.5);
            line(&[(7.0, 8.0), (7.0, 11.0)]);
            line(&[(11.0, 8.0), (11.0, 12.0)]);
            line(&[(15.0, 8.0), (15.0, 11.0)]);
        }
        Icon::Lock => {
            rrect(5.0, 11.0, 19.0, 21.0, 2.0);
            let mut pts = vec![pt(8.0, 11.0)];
            pts.extend(arc(12.0, 8.0, 4.0, 180.0, 360.0));
            pts.push(pt(16.0, 11.0));
            p.add(Shape::line(pts, stroke));
        }
        Icon::Shield => {
            closed(&[(12.0, 3.0), (20.0, 6.0), (20.0, 12.0), (18.3, 16.6), (12.0, 21.0), (5.7, 16.6), (4.0, 12.0), (4.0, 6.0)]);
            line(&[(8.5, 12.0), (11.0, 14.5), (15.5, 9.5)]);
        }
        Icon::Warn => {
            closed(&[(12.0, 4.0), (21.0, 19.5), (3.0, 19.5)]);
            line(&[(12.0, 10.0), (12.0, 14.0)]);
            p.circle_filled(pt(12.0, 16.8), 1.1 * s, color);
        }
        Icon::Check => line(&[(5.0, 12.5), (9.5, 17.0), (19.0, 7.0)]),
        Icon::Sun => {
            circle(12.0, 12.0, 4.0);
            for k in 0..8 {
                let a = (k as f32 * 45.0).to_radians();
                line(&[(12.0 + 7.0 * a.cos(), 12.0 + 7.0 * a.sin()), (12.0 + 9.5 * a.cos(), 12.0 + 9.5 * a.sin())]);
            }
        }
        Icon::Moon => {
            // Outer edge of the crescent, then the inner bite back to the start.
            let mut pts = arc(12.0, 12.0, 8.0, -20.0, 250.0);
            let mut bite = arc(15.5, 8.0, 6.2, 145.0, 20.0);
            pts.append(&mut bite);
            p.add(Shape::closed_line(pts, stroke));
        }
        Icon::Diff => {
            rrect(4.0, 4.0, 20.0, 20.0, 3.0);
            line(&[(12.0, 8.0), (12.0, 14.0)]);
            line(&[(9.0, 11.0), (15.0, 11.0)]);
            line(&[(9.0, 16.5), (15.0, 16.5)]);
        }
    }
}

/// Paint an icon in a square of `size` taken from the layout.
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_icon(ui.painter(), rect, icon, color);
    resp
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// A square button with an icon only. `on` marks a tool that is switched on.
pub fn icon_button(ui: &mut Ui, p: &Palette, icon: Icon, size: f32, enabled: bool, on: bool, tip: &str) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), sense);
    if ui.is_rect_visible(rect) {
        let fill = if on {
            p.active
        } else if enabled && resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 8.0, fill);
        let color = if !enabled {
            p.faint
        } else if on || resp.hovered() {
            p.text
        } else {
            p.icon
        };
        let inner = (size * 0.56).round().clamp(14.0, 20.0);
        paint_icon(ui.painter(), Rect::from_center_size(rect.center(), vec2(inner, inner)), icon, color);
    }
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    if tip.is_empty() {
        resp
    } else if enabled {
        resp.on_hover_text(tip)
    } else {
        resp.on_disabled_hover_text(tip).on_hover_text(tip)
    }
}

/// A full-width row: icon, label and (greyed, on the right) a keyboard shortcut.
pub fn nav_row(ui: &mut Ui, p: &Palette, icon: Icon, label: &str, keys: &str, enabled: bool) -> Response {
    let w = ui.available_width();
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), sense);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if enabled && resp.hovered() {
            painter.rect_filled(rect, 8.0, p.hover);
        }
        let ink = if enabled { p.text2 } else { p.faint };
        paint_icon(painter, Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), vec2(18.0, 18.0)), icon, ink);
        painter.text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.0), ink);
        if !keys.is_empty() {
            painter.text(pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, keys, mono(11.0), p.muted);
        }
    }
    resp
}

/// A full-width row of text, cut short with "..." when it does not fit.
pub fn list_row(ui: &mut Ui, p: &Palette, label: &str, selected: bool) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, 8.0, p.active);
        } else if resp.hovered() {
            painter.rect_filled(rect, 8.0, p.hover);
        }
        let color = if selected { p.text } else { p.text2 };
        let galley = ui.painter().layout_job(egui::text::LayoutJob {
            wrap: egui::text::TextWrapping::truncate_at_width(w - 20.0),
            ..egui::text::LayoutJob::simple_singleline(label.to_string(), FontId::proportional(14.0), color)
        });
        let y = rect.center().y - galley.size().y * 0.5;
        painter.galley(pos2(rect.left() + 10.0, y), galley, color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A panel heading, with an optional note on the right (for example "1 valid").
pub fn section(ui: &mut Ui, p: &Palette, title: &str, note: Option<(Icon, &str, Color32)>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(medium(15.0)).color(p.text));
        if let Some((ic, text, color)) = note {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(text).size(13.0).color(color));
                icon(ui, ic, 14.0, color);
            });
        }
    });
}

pub fn divider(ui: &mut Ui, p: &Palette) {
    ui.add_space(12.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.border);
    ui.add_space(12.0);
}

pub fn vdivider(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 20.0), Sense::hover());
    ui.painter().rect_filled(Rect::from_center_size(rect.center(), vec2(1.0, 20.0)), 0.0, p.border);
}

/// A small coloured label with a tick or a warning sign.
pub fn chip(ui: &mut Ui, ic: Icon, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::new().fill(bg).corner_radius(6).inner_margin(Margin { left: 6, right: 8, top: 2, bottom: 2 }).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.horizontal(|ui| {
            icon(ui, ic, 12.0, fg);
            ui.label(RichText::new(text).size(12.0).color(fg));
        });
    });
}

/// The one filled button of a group.
pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str, enabled: bool) -> Response {
    let (fill, ink) = if enabled { (p.primary, p.on_primary) } else { (p.active, p.faint) };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).font(medium(13.5)).color(ink))
            .fill(fill)
            .stroke(Stroke::NONE)
            .corner_radius(9)
            .min_size(vec2(0.0, 34.0)),
    )
}

/// A full-width outlined button with an icon in front of the label.
pub fn wide_button(ui: &mut Ui, p: &Palette, ic: Icon, icon_color: Color32, text: &str, enabled: bool, on: bool) -> Response {
    let w = ui.available_width();
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 38.0), sense);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if on {
            p.active
        } else if enabled && resp.hovered() {
            p.hover
        } else if p.dark {
            p.raised
        } else {
            p.card
        };
        painter.rect(rect, 10.0, fill, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let font = FontId::proportional(14.0);
        let ink = if enabled { p.text } else { p.faint };
        let galley = painter.layout_no_wrap(text.to_string(), font, ink);
        let total = 16.0 + 8.0 + galley.size().x;
        let x0 = rect.center().x - total * 0.5;
        paint_icon(painter, Rect::from_center_size(pos2(x0 + 8.0, rect.center().y), vec2(16.0, 16.0)), ic, if enabled { icon_color } else { p.faint });
        painter.galley(pos2(x0 + 24.0, rect.center().y - galley.size().y * 0.5), galley, ink);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Initials in a circle, for signers.
pub fn avatar(ui: &mut Ui, p: &Palette, name: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    let (bg, fg) = if p.dark { (rgb(0x3D3D3A), rgb(0xE3DACC)) } else { (rgb(0xE3DACC), rgb(0x141413)) };
    ui.painter().circle_filled(rect.center(), 17.0, bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, if initials.is_empty() { "?".into() } else { initials }, medium(12.0), fg);
}

/// Windows: make the title bar match the theme and repaint it. When Windows
/// itself is in light mode the window opens with a light title bar and is
/// turned dark afterwards; Windows then leaves the minimise, maximise and
/// close buttons drawn dark on dark (invisible until the mouse is over them)
/// unless the frame is told to redraw.
#[cfg(windows)]
pub fn sync_title_bar(frame: &eframe::Frame, dark: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowPos,
        WM_NCACTIVATE,
    };
    let Ok(handle) = frame.window_handle() else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
    unsafe {
        let on: i32 = dark as i32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&on as *const i32).cast(), 4);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        // Flipping the active state and back is what makes Windows 10 redraw the buttons.
        let active = GetForegroundWindow() == hwnd;
        SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(!active as usize)), Some(LPARAM(0)));
        SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(active as usize)), Some(LPARAM(0)));
    }
}

#[cfg(not(windows))]
pub fn sync_title_bar(_frame: &eframe::Frame, _dark: bool) {}
