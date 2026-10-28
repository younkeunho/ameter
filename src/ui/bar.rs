//! What the meter shows and how it is laid out.
//!
//! ```text
//! CO  W  94% 6d0h    HF 7,747 11d22h
//! CC  5h 76% 1h13m   W  84%   1d4h
//! ```
//! Each line is a name followed by slots. Slot k starts at the same x on every line and its
//! label, value and countdown columns have shared widths, so the lines read as a table.

use super::gdip::{Canvas, Font};
use windows_sys::Win32::Graphics::GdiPlus::RectF;

pub mod color {
    pub const BACKGROUND: u32 = 0xD8_22_22_22;
    pub const BORDER: u32 = 0x2E_FF_FF_FF;
    pub const CODEX: u32 = 0xFF_FF_FF_FF;
    /// Claude Code logo orange.
    pub const CLAUDE: u32 = 0xFF_D7_77_57;
    pub const HIGGSFIELD: u32 = 0xFF_C8_E6_4C;
    pub const LABEL: u32 = 0xFF_A2_A8_B0;
    pub const RESET: u32 = 0xFF_92_98_A0;
    pub const VALUE: u32 = 0xFF_F5_F5_F5;
    pub const WARN: u32 = 0xFF_FF_D1_66;
    pub const HIGH: u32 = 0xFF_FF_9F_43;
    pub const CRITICAL: u32 = 0xFF_FF_5E_5E;
    pub const MUTED: u32 = 0xFF_8A_90_98;

    pub fn for_severity(severity: u8) -> u32 {
        match severity {
            3 => CRITICAL,
            2 => HIGH,
            1 => WARN,
            _ => VALUE,
        }
    }

    /// Two thirds of the way to the muted grey: values that are not current.
    pub fn dimmed(c: u32) -> u32 {
        let mix = |shift: u32| ((((c >> shift) & 0xFF) + ((MUTED >> shift) & 0xFF) * 2) / 3) << shift;
        0xFF00_0000 | mix(16) | mix(8) | mix(0)
    }
}

/// What stands before a value: nothing, a window label ("W", "5h"), a provider name ("CO", "HF"),
/// or, in the mini style, a dot in the provider's color.
#[derive(Clone, Debug, PartialEq)]
pub enum Label {
    None,
    Plain(String),
    Name(&'static str),
    Dot,
}

impl Label {
    fn width(&self, canvas: &Canvas, fonts: &Fonts) -> f32 {
        match self {
            Label::None => 0.0,
            Label::Plain(text) => canvas.text_width(text, &fonts.regular),
            Label::Name(text) => canvas.text_width(text, &fonts.name),
            Label::Dot => DOT * fonts.scale,
        }
    }

    /// `y` is the top of the text line.
    fn draw(&self, canvas: &Canvas, fonts: &Fonts, argb: u32, x: f32, y: f32) {
        match self {
            Label::None => {}
            Label::Plain(text) => canvas.draw_text(text, &fonts.regular, argb, x, y),
            Label::Name(text) => canvas.draw_text(text, &fonts.name, argb, x, y),
            Label::Dot => {
                let d = DOT * fonts.scale;
                // Centred on the capital letters and digits of the line, not on its box.
                let cy = y + CAP_CENTER * FONT_PX * fonts.scale;
                canvas.fill_ellipse(x, cy - d / 2.0, d, d, argb);
            }
        }
    }
}

/// A slot: label, bold value, countdown ("W 94% 6d0h", "HF 7,747 11d22h"), or a muted status.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub label: Label,
    pub label_color: u32,
    pub value: String,
    pub value_color: u32,
    pub reset: String,
    pub status: Option<String>,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            label: Label::None,
            label_color: color::LABEL,
            value: String::new(),
            value_color: color::VALUE,
            reset: String::new(),
            status: None,
        }
    }
}

impl Cell {
    pub fn status(text: &str) -> Cell {
        Cell { status: Some(text.to_string()), ..Default::default() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// None: the line has no name (Codex hidden but Higgsfield shown).
    pub name: Option<Label>,
    pub name_color: u32,
    pub cells: Vec<Cell>,
}

pub struct Fonts {
    pub regular: Font,
    /// CO / CC / HF.
    pub name: Font,
    /// Percentages and credits.
    pub value: Font,
    pub scale: f32,
}

impl Fonts {
    pub fn new(scale: f32) -> Fonts {
        let px = FONT_PX * scale;
        Fonts {
            regular: Font::new("Segoe UI", px, false),
            name: Font::new("Segoe UI", px, true),
            value: Font::new("Segoe UI Semibold", px, false),
            scale,
        }
    }
}

const FONT_PX: f32 = 12.0;
const PAD_X: f32 = 8.0;
const PAD_Y: f32 = 3.0;
const LINE_HEIGHT: f32 = 15.0;
const NAME_GAP: f32 = 5.0;
const LABEL_GAP: f32 = 4.0;
const RESET_GAP: f32 = 4.0;
const COLUMN_GAP: f32 = 11.0;
const RADIUS: f32 = 5.0;
/// Diameter of a mini-style dot.
const DOT: f32 = 7.0;
/// Segoe UI: (ascent - cap height / 2) / em, the cap-height centre below the top of a line.
const CAP_CENTER: f32 = (2210.0 - 1434.0 / 2.0) / 2048.0;

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub slot_x: Vec<f32>,
    pub label_w: Vec<f32>,
    pub value_w: Vec<f32>,
    pub reset_w: Vec<f32>,
    pub width: i32,
    pub height: i32,
}

pub fn measure(rows: &[Row], canvas: &Canvas, fonts: &Fonts) -> Layout {
    let s = fonts.scale;
    let slots = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    let mut layout = Layout {
        slot_x: vec![0.0; slots],
        label_w: vec![0.0; slots],
        value_w: vec![0.0; slots],
        reset_w: vec![0.0; slots],
        ..Default::default()
    };
    let mut status_w = vec![0.0f32; slots];
    let name_w = rows.iter().filter_map(|r| r.name.as_ref()).map(|n| n.width(canvas, fonts)).fold(0.0, f32::max);

    for row in rows {
        for (k, cell) in row.cells.iter().enumerate() {
            if let Some(status) = &cell.status {
                status_w[k] = status_w[k].max(canvas.text_width(status, &fonts.regular));
                continue;
            }
            layout.label_w[k] = layout.label_w[k].max(cell.label.width(canvas, fonts));
            layout.value_w[k] = layout.value_w[k].max(canvas.text_width(&cell.value, &fonts.value));
            layout.reset_w[k] = layout.reset_w[k].max(canvas.text_width(&cell.reset, &fonts.regular));
        }
    }

    let mut x = PAD_X * s + if name_w > 0.0 { name_w + NAME_GAP * s } else { 0.0 };
    for (k, status_width) in status_w.into_iter().enumerate() {
        if k > 0 {
            x += COLUMN_GAP * s;
        }
        layout.slot_x[k] = x;
        let mut width = layout.label_w[k] + if layout.label_w[k] > 0.0 { LABEL_GAP * s } else { 0.0 } + layout.value_w[k];
        if layout.reset_w[k] > 0.0 {
            width += RESET_GAP * s + layout.reset_w[k];
        }
        x += width.max(status_width);
    }
    layout.width = (x + PAD_X * s).ceil() as i32;
    layout.height = ((PAD_Y * 2.0 + LINE_HEIGHT * rows.len() as f32) * s).ceil() as i32;
    layout
}

pub fn paint(rows: &[Row], layout: &Layout, canvas: &Canvas, fonts: &Fonts) {
    let s = fonts.scale;
    let outline = RectF { X: 0.5, Y: 0.5, Width: layout.width as f32 - 1.0, Height: layout.height as f32 - 1.0 };
    canvas.rounded_rect(outline, RADIUS * s, color::BACKGROUND, color::BORDER);

    let line_height = LINE_HEIGHT * s;
    let text_height = canvas.line_height(&fonts.value);
    for (i, row) in rows.iter().enumerate() {
        let y = (PAD_Y * s + i as f32 * line_height + (line_height - text_height) / 2.0).round();
        if let Some(name) = &row.name {
            name.draw(canvas, fonts, row.name_color, PAD_X * s, y);
        }
        for (k, cell) in row.cells.iter().enumerate() {
            let x = layout.slot_x[k];
            if let Some(status) = &cell.status {
                canvas.draw_text(status, &fonts.regular, color::MUTED, x, y);
                continue;
            }
            // Label, value and countdown each start at the same x on every line.
            cell.label.draw(canvas, fonts, cell.label_color, x, y);
            let value_x = x + layout.label_w[k] + if layout.label_w[k] > 0.0 { LABEL_GAP * s } else { 0.0 };
            canvas.draw_text(&cell.value, &fonts.value, cell.value_color, value_x, y);
            if !cell.reset.is_empty() {
                canvas.draw_text(&cell.reset, &fonts.regular, color::RESET, value_x + layout.value_w[k] + RESET_GAP * s, y);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::color;

    #[test]
    fn dimming_moves_toward_muted_and_stays_opaque() {
        assert_eq!(color::dimmed(color::MUTED), color::MUTED);
        let d = color::dimmed(0xFF_FF_FF_FF);
        assert_eq!(d >> 24, 0xFF);
        assert_eq!((d >> 16) & 0xFF, (0xFF + 0x8A * 2) / 3);
    }
}
