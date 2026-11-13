// Оформление окна: цвета светлой и тёмной темы, отступы, шрифты и общие элементы.
// Тема следует за системной.
use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Theme};

pub struct Palette {
    pub background: Color32,
    pub card: Color32,
    pub border: Color32,
    pub text: Color32,
    pub weak: Color32,
    // Акцент: ход работы, ссылки, включённые флаги.
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_text: Color32,
    // Заливка главной кнопки, текст на ней белый.
    pub primary: Color32,
    pub primary_hover: Color32,
    pub good: Color32,
    pub warn: Color32,
    pub bad: Color32,
    // Поля ввода и обычные кнопки.
    field: Color32,
    control: Color32,
    control_hover: Color32,
    control_active: Color32,
    control_border: Color32,
    control_border_hover: Color32,
}

const LIGHT: Palette = Palette {
    background: Color32::from_rgb(244, 245, 247),
    card: Color32::WHITE,
    border: Color32::from_rgb(226, 229, 234),
    text: Color32::from_rgb(31, 35, 40),
    weak: Color32::from_rgb(107, 114, 128),
    accent: Color32::from_rgb(37, 99, 235),
    accent_soft: Color32::from_rgb(221, 233, 255),
    accent_text: Color32::from_rgb(29, 78, 216),
    primary: Color32::from_rgb(37, 99, 235),
    primary_hover: Color32::from_rgb(29, 84, 212),
    good: Color32::from_rgb(22, 128, 61),
    warn: Color32::from_rgb(161, 98, 7),
    bad: Color32::from_rgb(207, 34, 46),
    field: Color32::WHITE,
    control: Color32::WHITE,
    control_hover: Color32::from_rgb(243, 244, 246),
    control_active: Color32::from_rgb(229, 231, 235),
    control_border: Color32::from_rgb(208, 213, 221),
    control_border_hover: Color32::from_rgb(160, 168, 180),
};

const DARK: Palette = Palette {
    background: Color32::from_rgb(17, 19, 23),
    card: Color32::from_rgb(25, 28, 33),
    border: Color32::from_rgb(42, 46, 54),
    text: Color32::from_rgb(229, 231, 235),
    weak: Color32::from_rgb(148, 156, 168),
    accent: Color32::from_rgb(96, 150, 255),
    accent_soft: Color32::from_rgb(32, 54, 97),
    accent_text: Color32::from_rgb(190, 214, 255),
    primary: Color32::from_rgb(47, 106, 228),
    primary_hover: Color32::from_rgb(66, 122, 240),
    good: Color32::from_rgb(74, 190, 110),
    warn: Color32::from_rgb(224, 170, 70),
    bad: Color32::from_rgb(244, 112, 103),
    field: Color32::from_rgb(17, 19, 23),
    control: Color32::from_rgb(36, 40, 47),
    control_hover: Color32::from_rgb(45, 50, 58),
    control_active: Color32::from_rgb(30, 33, 39),
    control_border: Color32::from_rgb(58, 63, 73),
    control_border_hover: Color32::from_rgb(92, 100, 114),
};

pub fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

const SEMIBOLD: &str = "semibold";

pub fn apply(ctx: &egui::Context) {
    fonts(ctx);
    for (theme, p) in [(Theme::Light, &LIGHT), (Theme::Dark, &DARK)] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Heading, FontId::new(17.0, FontFamily::Name(SEMIBOLD.into()))),
                (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            ]
            .into();
            let s = &mut style.spacing;
            s.item_spacing = egui::vec2(8.0, 8.0);
            s.button_padding = egui::vec2(10.0, 5.0);
            s.interact_size.y = 28.0;
            s.icon_width = 16.0;
            s.icon_width_inner = 10.0;

            let v = &mut style.visuals;
            v.panel_fill = p.background;
            v.window_fill = p.card;
            v.window_stroke = Stroke::new(1.0, p.border);
            v.window_corner_radius = CornerRadius::same(12);
            v.menu_corner_radius = CornerRadius::same(8);
            v.extreme_bg_color = p.field;
            v.text_edit_bg_color = Some(p.field);
            v.weak_text_color = Some(p.weak);
            v.hyperlink_color = p.accent;
            v.warn_fg_color = p.warn;
            v.error_fg_color = p.bad;
            v.selection.bg_fill = p.accent_soft;
            v.selection.stroke = Stroke::new(1.0, p.accent_text);
            v.text_cursor.stroke = Stroke::new(2.0, p.accent);

            let w = &mut v.widgets;
            w.noninteractive.bg_fill = p.card;
            w.noninteractive.weak_bg_fill = p.card;
            w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
            w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
            let states = [
                (&mut w.inactive, p.control, p.control_border),
                (&mut w.hovered, p.control_hover, p.control_border_hover),
                (&mut w.active, p.control_active, p.accent),
                (&mut w.open, p.control_hover, p.control_border_hover),
            ];
            for (state, fill, border) in states {
                state.bg_fill = fill;
                state.weak_bg_fill = fill;
                state.bg_stroke = Stroke::new(1.0, border);
                state.fg_stroke = Stroke::new(1.5, p.text);
                state.corner_radius = CornerRadius::same(6);
                state.expansion = 0.0;
            }
        });
    }
}

// Segoe UI и Consolas из Windows: привычный вид и кириллица. Полужирный — отдельное семейство.
fn fonts(ctx: &egui::Context) {
    let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into())).join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    let load = |fonts: &mut egui::FontDefinitions, name: &str, file: &str| -> bool {
        let Ok(bytes) = std::fs::read(dir.join(file)) else { return false };
        fonts.font_data.insert(name.into(), egui::FontData::from_owned(bytes).into());
        true
    };
    if load(&mut fonts, "segoe", "segoeui.ttf") {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    let mut semibold = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if load(&mut fonts, "segoe-semibold", "seguisb.ttf") {
        semibold.insert(0, "segoe-semibold".into());
    }
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    if load(&mut fonts, "consolas", "consola.ttf") {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "consolas".into());
    }
    ctx.set_fonts(fonts);
}

pub fn semibold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).font(FontId::new(size, FontFamily::Name(SEMIBOLD.into())))
}

pub fn weak(ui: &egui::Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(palette(ui).weak)
}

// Карточка: белый блок со скруглением на сером фоне окна.
pub fn card(ui: &egui::Ui) -> egui::Frame {
    let p = palette(ui);
    egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.border)).corner_radius(10).inner_margin(Margin::symmetric(16, 12))
}

// Плашка с сообщением: ошибка, предупреждение.
pub fn banner(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(color));
        });
}

// Главная кнопка экрана: залита акцентом.
pub fn primary_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let p = palette(ui);
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for (state, fill) in [(&mut w.inactive, p.primary), (&mut w.hovered, p.primary_hover), (&mut w.active, p.primary)] {
            state.weak_bg_fill = fill;
            state.bg_fill = fill;
            state.bg_stroke = Stroke::NONE;
            state.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        }
        let button = egui::Button::new(semibold(text, 15.0).color(Color32::WHITE)).min_size(egui::vec2(150.0, 36.0)).corner_radius(8);
        ui.add_enabled(enabled, button)
    })
    .inner
}

// Флаг-переключатель: скруглённая плашка, включённая — акцентного цвета.
pub fn chip(ui: &mut egui::Ui, on: bool, text: &str) -> egui::Response {
    let p = palette(ui);
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(9.0, 2.0);
        ui.spacing_mut().interact_size.y = 24.0;
        let mut button = egui::Button::new(RichText::new(text).size(12.5)).selected(on).corner_radius(12);
        if on {
            button = button.stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.55)));
        }
        ui.add(button)
    })
    .inner
}

// Цветная точка состояния.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}
