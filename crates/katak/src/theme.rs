//! Colors, fonts and the application icon.

use eframe::egui::{self, Color32, FontFamily, FontId, IconData};

/// Colors used by the grid, derived from the current egui visuals.
#[derive(Clone, Copy, Debug)]
pub struct GridColors {
    pub background: Color32,
    pub stripe: Color32,
    pub line: Color32,
    pub header_bg: Color32,
    pub header_text: Color32,
    pub gutter_bg: Color32,
    pub gutter_text: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub selection: Color32,
    pub cursor: Color32,
    pub ragged: Color32,
    pub approximate: Color32,
    pub highlight: Color32,
}

impl GridColors {
    pub fn from_visuals(v: &egui::Visuals) -> Self {
        if v.dark_mode {
            GridColors {
                background: Color32::from_rgb(0x1B, 0x1D, 0x21),
                stripe: Color32::from_rgb(0x21, 0x24, 0x29),
                line: Color32::from_rgb(0x30, 0x34, 0x3A),
                header_bg: Color32::from_rgb(0x2A, 0x2E, 0x35),
                header_text: Color32::from_rgb(0xE6, 0xE8, 0xEB),
                gutter_bg: Color32::from_rgb(0x24, 0x27, 0x2D),
                gutter_text: Color32::from_rgb(0x8A, 0x91, 0x9C),
                text: Color32::from_rgb(0xDD, 0xE1, 0xE6),
                dim: Color32::from_rgb(0x6C, 0x73, 0x7E),
                selection: Color32::from_rgba_unmultiplied(0x3D, 0x8B, 0xFD, 0x40),
                cursor: Color32::from_rgb(0x4D, 0x9B, 0xFF),
                ragged: Color32::from_rgba_unmultiplied(0xE0, 0x8A, 0x1E, 0x38),
                approximate: Color32::from_rgb(0xE0, 0xA4, 0x3A),
                highlight: Color32::from_rgba_unmultiplied(0xC8, 0x9B, 0x0A, 0x90),
            }
        } else {
            GridColors {
                background: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                stripe: Color32::from_rgb(0xF6, 0xF7, 0xF9),
                line: Color32::from_rgb(0xE1, 0xE4, 0xE8),
                header_bg: Color32::from_rgb(0xEC, 0xEE, 0xF1),
                header_text: Color32::from_rgb(0x1F, 0x23, 0x28),
                gutter_bg: Color32::from_rgb(0xF2, 0xF3, 0xF5),
                gutter_text: Color32::from_rgb(0x6A, 0x73, 0x7D),
                text: Color32::from_rgb(0x1F, 0x23, 0x28),
                dim: Color32::from_rgb(0x9A, 0xA1, 0xAA),
                selection: Color32::from_rgba_unmultiplied(0x1F, 0x6F, 0xEB, 0x30),
                cursor: Color32::from_rgb(0x1F, 0x6F, 0xEB),
                ragged: Color32::from_rgba_unmultiplied(0xF0, 0x9A, 0x2B, 0x30),
                approximate: Color32::from_rgb(0xB3, 0x6B, 0x00),
                highlight: Color32::from_rgba_unmultiplied(0xFF, 0xD3, 0x3D, 0xB0),
            }
        }
    }
}

/// Font used for cells.
pub fn cell_font(size: f32, monospace: bool) -> FontId {
    FontId::new(
        size,
        if monospace {
            FontFamily::Monospace
        } else {
            FontFamily::Proportional
        },
    )
}

/// A small procedurally drawn icon: a teal tile with a white table grid.
pub fn app_icon() -> IconData {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            // Rounded corners.
            let (cx, cy) = (x.min(N - 1 - x), y.min(N - 1 - y));
            if cx < 3 && cy < 3 && (3 - cx) * (3 - cx) + (3 - cy) * (3 - cy) > 9 {
                continue;
            }
            let line = (x > 4 && x < N - 5)
                && (y > 4 && y < N - 5)
                && (x == 5 || x == 13 || x == N - 6 || y == 5 || y == 11 || y == 18 || y == N - 6);
            let header = (x > 4 && x < N - 5) && (y > 5 && y < 11);
            let (r, g, b) = if line {
                (255, 255, 255)
            } else if header {
                (0xB8, 0xF0, 0xE6)
            } else {
                (0x0F, 0x8B, 0x7A)
            };
            rgba[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}
