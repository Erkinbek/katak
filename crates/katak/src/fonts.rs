//! System fonts as fallbacks for what egui's built-in fonts lack: CJK,
//! Hangul, Indic scripts, arrows and other symbols. The files are mapped,
//! not copied, and loaded on a background thread after the first frame,
//! so startup time and private memory stay the same.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

/// Candidate files in fallback order (missing ones are skipped).
fn candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        let dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        [
            "seguisym.ttf", // symbols and arrows
            "msyh.ttc",     // CJK (Microsoft YaHei)
            "malgun.ttf",   // Hangul
            "Nirmala.ttc",  // Indic scripts
            "segoeui.ttf",  // Arabic, Hebrew, more Latin
        ]
        .iter()
        .map(|f| dir.join(f))
        .collect()
    } else if cfg!(target_os = "macos") {
        [
            "/System/Library/Fonts/Apple Symbols.ttf",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    } else {
        [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// Maps a font file read-only.
fn map(path: &PathBuf) -> Option<FontData> {
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: read-only map of a system font file; fonts are not modified
    // while in use (installers replace them only across reboots).
    let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    Some(FontData::from_blob(Arc::new(mmap), 0))
}

/// Adds the fallbacks found on this system, in the background.
pub fn load_fallbacks(ctx: egui::Context) {
    let _ = std::thread::Builder::new()
        .name("katak-fonts".into())
        .spawn(move || {
            let mut defs = FontDefinitions::default();
            let mut added = 0;
            for path in candidates() {
                let Some(data) = map(&path) else { continue };
                let name = format!("system:{}", path.display());
                defs.font_data.insert(name.clone(), Arc::new(data));
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    defs.families.entry(family).or_default().push(name.clone());
                }
                added += 1;
            }
            if added > 0 {
                ctx.set_fonts(defs);
                ctx.request_repaint();
            }
        });
}
