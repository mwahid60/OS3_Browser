//! Set glyph/tampilan UI yang bisa dipilih pengguna.
//!
//! Latar belakang: ratatui menghitung lebar glyph dengan `unicode-width`
//! (`width()`), sedangkan terminal menghitung sendiri. Untuk glyph
//! "East Asian Ambiguous" (●, ▶, …, ─, ·) dan Private Use Area (icon Nerd
//! Font), kedua perhitungan bisa berbeda 1 vs 2 sel. Bila berbeda, baris
//! bergeser dan sisa frame lama tertinggal (tampilan rusak).
//!
//! Karena itu tersedia tiga tingkat:
//! - `Nerd`    : icon Nerd Font + border Unicode (paling cantik, butuh terminal
//!               yang menghitung glyph PUA/Ambiguous sebagai 1 sel, mis. Ghostty
//!               dengan `grapheme-width-method = legacy`)
//! - `Unicode` : border Unicode + icon ASCII (kompromi)
//! - `Ascii`   : semuanya ASCII (paling aman di semua terminal)

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    Nerd,
    Unicode,
    #[default]
    Ascii,
}

impl<'de> Deserialize<'de> for IconStyle {
    /// Nilai yang tidak dikenal (mis. typo di config) tidak menggagalkan load:
    /// fallback ke `Ascii` dengan peringatan di stderr.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.to_ascii_lowercase().as_str() {
            "nerd" => IconStyle::Nerd,
            "unicode" => IconStyle::Unicode,
            "ascii" => IconStyle::Ascii,
            unknown => {
                eprintln!(
                    "peringatan: icon_style '{unknown}' tidak dikenal, memakai 'ascii'"
                );
                IconStyle::Ascii
            }
        })
    }
}

/// Kategori file untuk pemilihan icon. Satu tabel ekstensi dipakai bersama oleh
/// style Nerd dan Ascii, sehingga keduanya tidak mungkin lagi berbeda kategori.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Dir,
    Image,
    Video,
    Audio,
    Archive,
    Pdf,
    Doc,
    Sheet,
    Data,
    Code,
    Text,
    Other,
}

impl FileKind {
    fn from_ext(ext: &str) -> Self {
        match ext {
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" => FileKind::Image,
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "flv" | "m4v" => FileKind::Video,
            "mp3" | "wav" | "ogg" | "flac" | "m4a" | "aac" => FileKind::Audio,
            "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" => FileKind::Archive,
            "pdf" => FileKind::Pdf,
            "doc" | "docx" => FileKind::Doc,
            "xls" | "xlsx" => FileKind::Sheet,
            "csv" | "json" | "toml" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "conf"
            | "env" => FileKind::Data,
            "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "go" | "java" | "c" | "cpp" | "h"
            | "hpp" | "sh" | "rb" | "php" | "html" | "css" | "sql" => FileKind::Code,
            "md" | "txt" | "log" => FileKind::Text,
            _ => FileKind::Other,
        }
    }

    fn nerd(self) -> &'static str {
        match self {
            FileKind::Dir => "\u{f07b} ",
            FileKind::Image => "\u{f03e} ",
            FileKind::Video => "\u{f03d} ",
            FileKind::Audio => "\u{f001} ",
            FileKind::Archive => "\u{f410} ",
            FileKind::Pdf => "\u{f1c1} ",
            FileKind::Doc => "\u{f1c2} ",
            FileKind::Sheet => "\u{f1c3} ",
            FileKind::Data => "\u{e615} ",
            FileKind::Code => "\u{f121} ",
            FileKind::Text => "\u{f15c} ",
            FileKind::Other => "\u{f016} ",
        }
    }

    fn ascii(self) -> &'static str {
        match self {
            FileKind::Dir => "[D] ",
            FileKind::Image => "[i] ",
            FileKind::Video => "[v] ",
            FileKind::Audio => "[a] ",
            FileKind::Archive => "[z] ",
            FileKind::Pdf => "[p] ",
            FileKind::Doc => "[w] ",
            FileKind::Sheet => "[x] ",
            FileKind::Data => "[c] ",
            FileKind::Code => "[k] ",
            FileKind::Text => "[t] ",
            FileKind::Other => "[-] ",
        }
    }
}

impl IconStyle {
    pub fn next(self) -> Self {
        match self {
            IconStyle::Nerd => IconStyle::Unicode,
            IconStyle::Unicode => IconStyle::Ascii,
            IconStyle::Ascii => IconStyle::Nerd,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IconStyle::Nerd => "nerd",
            IconStyle::Unicode => "unicode",
            IconStyle::Ascii => "ascii",
        }
    }

    /// Icon dengan lebar 2 sel (icon + spasi) untuk semua style.
    pub fn icon(self, name: &str, is_dir: bool) -> &'static str {
        match self {
            IconStyle::Nerd => nerd_icon(name, is_dir),
            IconStyle::Unicode | IconStyle::Ascii => ascii_icon(name, is_dir),
        }
    }

    /// Symbol seleksi (lebar 2 sel).
    pub fn highlight(self) -> &'static str {
        match self {
            IconStyle::Nerd => "\u{2502} ",
            _ => "> ",
        }
    }

    pub fn border(self) -> ratatui::symbols::border::Set {
        match self {
            IconStyle::Nerd | IconStyle::Unicode => ratatui::symbols::border::PLAIN,
            IconStyle::Ascii => ratatui::symbols::border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            },
        }
    }

    /// Indikator mounted (2 sel).
    pub fn dot(self) -> &'static str {
        match self {
            IconStyle::Nerd => "\u{25cf} ",
            _ => "* ",
        }
    }

    /// Ellipsis: satu karakter (U+2026) hanya pada style nerd.
    pub fn ellipsis(self) -> &'static str {
        match self {
            IconStyle::Nerd => "\u{2026}",
            _ => "...",
        }
    }

    /// Icon bucket sidebar. Nerd Font: glyph database. Unicode/Ascii: string kosong
    /// (tanpa icon sama sekali, sesuai permintaan user).
    pub fn bucket_icon(self) -> &'static str {
        match self {
            IconStyle::Nerd => "\u{f1c0} ", // nerd font database glyph, 2 sel
            _ => "",
        }
    }
}

fn classify(name: &str, is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Dir;
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    FileKind::from_ext(&ext)
}

fn nerd_icon(name: &str, is_dir: bool) -> &'static str {
    classify(name, is_dir).nerd()
}

fn ascii_icon(name: &str, is_dir: bool) -> &'static str {
    classify(name, is_dir).ascii()
}

#[cfg(test)]
mod tests {
    use super::IconStyle;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn icon_widths_are_consistent_per_style() {
        // Setiap style punya lebar kolom icon yang tetap: perbedaan lebar antar
        // baris dalam satu style-lah yang menyebabkan salah tata letak.
        let names = [
            "a/", "a.png", "a.mp4", "a.mp3", "a.zip", "a.pdf", "a.json", "a.rs", "a.md",
            "a.xyz",
        ];
        for style in [IconStyle::Ascii, IconStyle::Unicode, IconStyle::Nerd] {
            let widths: Vec<usize> = names
                .iter()
                .map(|n| style.icon(n, n.ends_with('/')).width())
                .collect();
            let first = widths[0];
            assert!(
                widths.iter().all(|w| *w == first),
                "style {:?} punya lebar icon tidak seragam: {:?} untuk {:?}",
                style,
                widths,
                names
            );
            // file tanpa ekstensi dikenal harus punya lebar sama dengan folder
            assert_eq!(style.icon("a.xyz", false).width(), first);
            // nerd: semua glyph 1 sel + spasi = 2; ascii: badge 3 char + spasi = 4
            let expected = match style {
                IconStyle::Nerd => 2,
                _ => 4,
            };
            assert_eq!(first, expected, "lebar dasar style {:?}", style);
            // trailing space selalu ada supaya kolom nama sejajar
            assert!(style.icon("a.md", false).ends_with(' '));
        }
    }

    #[test]
    fn bucket_icon_width_is_fixed_per_style() {
        // Nerd: glyph PUA + spasi = 2 sel. Style lain: tidak ada icon sama sekali,
        // jadi nama bucket tampil langsung tanpa prefix.
        assert_eq!(IconStyle::Nerd.bucket_icon().width(), 2);
        assert_eq!(IconStyle::Unicode.bucket_icon().width(), 0);
        assert_eq!(IconStyle::Ascii.bucket_icon().width(), 0);
    }

    fn parse_style(value: &str) -> IconStyle {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            icon_style: IconStyle,
        }
        let raw = format!("icon_style = \"{value}\"");
        toml::from_str::<Wrapper>(&raw)
            .expect("style tidak dikenal tidak boleh menggagalkan load")
            .icon_style
    }

    #[test]
    fn icon_style_serializes_lowercase() {
        #[derive(serde::Serialize)]
        struct Wrapper {
            icon_style: IconStyle,
        }
        for (style, expected) in [
            (IconStyle::Ascii, "ascii"),
            (IconStyle::Unicode, "unicode"),
            (IconStyle::Nerd, "nerd"),
        ] {
            let raw = toml::to_string(&Wrapper { icon_style: style }).unwrap();
            assert!(raw.contains(expected), "{raw}");
        }
    }

    #[test]
    fn unknown_icon_style_falls_back_to_ascii() {
        assert_eq!(parse_style("tidak-ada"), IconStyle::Ascii);
        // nilai valid tetap dihormati (termasuk case-insensitive)
        assert_eq!(parse_style("Nerd"), IconStyle::Nerd);
        assert_eq!(parse_style("unicode"), IconStyle::Unicode);
    }

    #[test]
    fn selection_symbols_are_two_cells() {
        for style in [IconStyle::Ascii, IconStyle::Unicode, IconStyle::Nerd] {
            assert_eq!(style.highlight().width(), 2);
            assert_eq!(style.dot().width(), 2);
        }
    }
}
