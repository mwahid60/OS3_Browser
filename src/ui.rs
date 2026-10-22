use std::borrow::Cow;

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui_image::StatefulImage;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, FindScope, Focus, Mode, PreviewState, SidebarRow};

use crate::s3::human_size;
use crate::theme::IconStyle;

/// Warna latar untuk baris yang sedang ter-highlight.
/// Sengaja bukan DarkGray: teks abu-abu menjadi tidak terbaca di atasnya.
const HL_BG: Color = Color::Rgb(53, 68, 105);
// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let show_search = search_bar_visible(app);
    let vchunks = Layout::vertical([
        Constraint::Length(1),                            // title bar
        Constraint::Length(if show_search { 1 } else { 0 }), // search bar global
        Constraint::Min(0),                               // body
        Constraint::Length(1),                            // status bar
    ])
    .split(area);

    draw_title(f, app, vchunks[0]);
    if show_search {
        draw_search_bar(f, app, vchunks[1]);
    }
    draw_body(f, app, vchunks[2]);
    draw_status(f, app, vchunks[3]);
    draw_popup(f, app, area);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn focused_block(title: &str, focused: bool, style: IconStyle) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_set(style.border())
        .border_style(border_style)
        .title(Span::styled(format!(" {title} "), border_style))
}

fn popup_block(title: &str, style: IconStyle) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(style.border())
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Yellow).bold(),
        ))
        .style(Style::default().bg(Color::Black))
}

/// Hitung rect terpusat tanpa risiko underflow.
///
/// Persentase di-clamp ke `1..=100`; hasil selalu minimal 1 kolom/baris
/// (kecuali area memang kosong) dan tidak pernah melebihi area aslinya.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let px = u32::from(percent_x.clamp(1, 100));
    let py = u32::from(percent_y.clamp(1, 100));
    let rw = u32::from(r.width);
    let rh = u32::from(r.height);

    let w = (rw * px / 100).max(1).min(rw);
    let h = (rh * py / 100).max(1).min(rh);

    Rect {
        x: r.x + ((rw - w) / 2) as u16,
        y: r.y + ((rh - h) / 2) as u16,
        width: w as u16,
        height: h as u16,
    }
}

/// Potong teks agar muat dalam `max` kolom.
///
/// Mengembalikan `Cow::Borrowed` bila tidak perlu memotong, sehingga baris
/// yang sudah muat tidak mengalokasikan `String` baru.
fn truncate_to_width<'a>(s: &'a str, max: usize, style: IconStyle) -> Cow<'a, str> {
    if max == 0 {
        return Cow::Borrowed("");
    }
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    let ell = style.ellipsis();
    let ell_w = ell.width();
    if max <= ell_w {
        return Cow::Owned(ell.chars().take(max).collect());
    }
    let budget = max.saturating_sub(ell_w);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push_str(ell);
    Cow::Owned(out)
}

/// Indeks byte yang aman untuk memotong `value` pada posisi `cursor`.
///
/// Cursor bisa saja menunjuk tengah karakter multi-byte (mis. setelah
/// backspace/operasi teks tertentu); turunkan hingga batas karakter terdekat.
fn safe_cursor(value: &str, cursor: usize) -> usize {
    let mut c = cursor.min(value.len());
    while !value.is_char_boundary(c) {
        c -= 1;
    }
    c
}

/// Petunjuk singkat saat input find/filter sedang aktif.

/// Baris input FIND/filter: `/` + teks, dengan indikator cursor bila aktif.
/// `cursor` adalah byte index (sama seperti `FindState::cursor`); `None` = di akhir teks.
fn find_input_line(text: &str, cursor: Option<usize>, active: bool) -> Line<'static> {
    let style = if active {
        Style::default().fg(Color::Black).bg(Color::Yellow).bold()
    } else {
        Style::default().fg(Color::Yellow)
    };
    let mut spans: Vec<Span> = vec![Span::styled("/", style)];
    if !active {
        spans.push(Span::styled(text.to_string(), style));
        return Line::from(spans);
    }
    let mut byte = cursor.unwrap_or(text.len()).min(text.len());
    while !text.is_char_boundary(byte) {
        byte = byte.saturating_sub(1);
    }
    if byte > 0 {
        spans.push(Span::styled(text[..byte].to_string(), style));
    }
    spans.push(Span::styled(
        "|".to_string(),
        Style::default()
            .fg(Color::White)
            .bg(Color::Yellow)
            .bold(),
    ));
    if byte < text.len() {
        spans.push(Span::styled(text[byte..].to_string(), style));
    }
    Line::from(spans)
}

/// Label scope filter yang sedang aktif pada search bar global.
fn scope_label(scope: FindScope) -> &'static str {
    match scope {
        FindScope::Sidebar => "[sidebar]",
        FindScope::Main => "[main]",
        FindScope::Preview => "[preview]",
    }
}

/// `true` bila search bar global perlu ditampilkan: input sedang aktif, atau
/// masih ada query yang tersimpan pada pane mana pun.
fn search_bar_visible(app: &App) -> bool {
    app.search_active()
        || !app.active_query().is_empty()
        || !app.find_sidebar.query.is_empty()
        || !app.find_main.query.is_empty()
        || !app.find_preview.query.is_empty()
}

/// Query + scope yang ditampilkan pada search bar global.
///
/// Saat input aktif (atau pane fokus punya query), pakai state pane fokus;
/// kalau tidak, tampilkan query pane lain yang masih menyaring.
fn search_display(app: &App) -> (&str, FindScope, Option<usize>) {
    let query = app.active_query();
    if app.search_active() || !query.is_empty() {
        return (query, app.find.scope, Some(app.find.cursor));
    }
    // Query dari pane yang tidak fokus hanya ditampilkan sebagai teks;
    // tidak ada cursor karena input pane tersebut tidak sedang aktif.
    if !app.find_sidebar.query.is_empty() {
        return (&app.find_sidebar.query, FindScope::Sidebar, None);
    }
    if !app.find_main.query.is_empty() {
        return (&app.find_main.query, FindScope::Main, None);
    }
    (
        &app.find_preview.query,
        FindScope::Preview,
        None,
    )
}

/// Search bar global di bawah title bar: `/` + query pane fokus, label scope
/// dan hint di sisi kanan.
fn draw_search_bar(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let active = app.search_active();
    let base_style = if active {
        Style::default().fg(Color::Black).bg(Color::Yellow).bold()
    } else {
        Style::default().fg(Color::Yellow)
    };
    let (query, scope, cursor) = search_display(app);
    let line = find_input_line(query, cursor, active);

    let scope = scope_label(scope);
    let scope_w = scope.width();
    // Sisakan minimal 8 kolom untuk input sebelum menampilkan bagian kanan.
    let avail = (area.width as usize).saturating_sub(8);
    let right: Option<String> = if avail >= scope_w {
        Some(scope.to_string())
    } else {
        None
    };

    match right {
        Some(right) => {
            let right_w = right.width() as u16;
            let cols =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).split(area);
            f.render_widget(Paragraph::new(line).style(base_style), cols[0]);
            f.render_widget(
                Paragraph::new(Span::styled(right, Style::default().fg(Color::DarkGray)))
                    .style(base_style),
                cols[1],
            );
        }
        None => f.render_widget(Paragraph::new(line).style(base_style), area),
    }
}

/// Gaya sorotan kemunculan keyword di preview pane.
fn keyword_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Yellow).bold()
}

/// Pecah satu baris teks menjadi beberapa `Span`, dengan setiap kemunculan
/// `needle` diberi gaya sorotan.
///
/// `needle` harus sudah lowercase (pemanggil men-hoist `to_lowercase()` sekali
/// per frame, bukan sekali per baris).
fn highlight_line(text: &str, needle: &str) -> Line<'static> {
    if needle.is_empty() || text.is_empty() {
        return Line::from(text.to_string());
    }

    // Peta byte versi lowercase -> rentang byte pada teks asli, supaya
    // pencarian case-insensitive tetap menghasilkan indeks yang valid.
    let mut lower = String::with_capacity(text.len());
    let mut map: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    for (i, ch) in text.char_indices() {
        let end = i + ch.len_utf8();
        for lc in ch.to_lowercase() {
            for _ in 0..lc.len_utf8() {
                map.push((i, end));
            }
            lower.push(lc);
        }
    }

    let hay = lower.as_bytes();
    let needle = needle.as_bytes();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut orig = 0usize; // byte teks asli yang sudah dikeluarkan
    let mut pos = 0usize; // posisi awal pencarian pada `hay`
    while pos + needle.len() <= hay.len() {
        let Some(off) = hay[pos..]
            .windows(needle.len())
            .position(|w| w == needle)
        else {
            break;
        };
        let start = pos + off;
        let end = start + needle.len();
        let start_o = map[start].0.max(orig);
        let end_o = map[end - 1].1.max(start_o);
        if end_o > start_o {
            if start_o > orig {
                spans.push(Span::styled(text[orig..start_o].to_string(), Style::default()));
            }
            spans.push(Span::styled(text[start_o..end_o].to_string(), keyword_style()));
            orig = end_o;
        }
        pos = end;
    }
    if orig < text.len() {
        spans.push(Span::styled(text[orig..].to_string(), Style::default()));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), Style::default()));
    }
    Line::from(spans)
}

/// `true` bila baris input find perlu ditampilkan (aktif atau sudah ada query).
///
/// Search bar global memakai `search_bar_visible`; helper ini dipakai filter
/// pada popup file picker.
pub(crate) fn find_visible(query: &str, active: bool) -> bool {
    active || !query.is_empty()
}

/// Karakter pemisah footer sidebar. U+2500 tergolong "East Asian Ambiguous",
/// jadi hanya dipakai pada style yang memang menghendaki glyph Unicode.
fn footer_sep_chars(style: IconStyle, width: usize) -> String {
    let ch = match style {
        IconStyle::Ascii => '-',
        IconStyle::Nerd | IconStyle::Unicode => '\u{2500}',
    };
    ch.to_string().repeat(width)
}

/// Word-wrap sederhana berbasis lebar sel. Mengembalikan baris-baris yang
/// sudah siap dirender tanpa `.wrap()` agar jumlah baris (dan clamp scroll)
/// bisa dihitung dengan tepat.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        let mut last_space: Option<usize> = None;
        for ch in logical.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width {
                if let Some(idx) = last_space.take() {
                    let rest = cur[idx..].trim_start_matches(' ').to_string();
                    let kept = cur[..idx].to_string();
                    out.push(kept);
                    cur = rest;
                    cur_w = cur.width();
                } else if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
            }
            let idx = cur.len();
            cur.push(ch);
            cur_w += cw;
            if ch == ' ' {
                last_space = Some(idx);
            }
        }
        out.push(cur);
    }
    out
}

fn s3_row(
    it: &crate::app::S3Item,
    list_width: usize,
    show_mark: bool,
    marked: bool,
    style: IconStyle,
) -> ListItem<'static> {
    let icon = style.icon(&it.name, it.is_dir);
    let icon_w = icon.width();
    let name_style = if it.is_dir {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::White)
    };
    let size = if it.is_dir {
        "-".to_string()
    } else {
        human_size(it.size)
    };
    let date: String = it
        .modified
        .as_deref()
        .map(|m| m.chars().take(10).collect())
        .unwrap_or_default();
    let right = format!("{size:>10}  {date:>10}");
    let mark_w = if show_mark { 2 } else { 0 };
    let name_avail = list_width
        .saturating_sub(mark_w + icon_w + right.width())
        .max(8);
    let name_disp = truncate_to_width(&it.name, name_avail, style).into_owned();
    let pad = name_avail.saturating_sub(name_disp.width());

    let mut spans: Vec<Span> = Vec::new();
    if show_mark {
        if marked {
            spans.push(Span::styled(
                "* ",
                Style::default().fg(Color::Magenta).bold(),
            ));
        } else {
            spans.push(Span::raw("  "));
        }
    }
    spans.push(Span::styled(
        icon,
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(name_disp, name_style));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::default().fg(Color::DarkGray)));

    ListItem::new(Line::from(spans))
}

// ---------------------------------------------------------------------------
// Title bar
// ---------------------------------------------------------------------------

fn zoom_active(app: &App) -> bool {
    app.zoom == Some(app.focus)
}

fn draw_title(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        " OS3 Browser ",
        Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
    )];
    if zoom_active(app) {
        spans.push(Span::styled(
            " [ZOOM]",
            Style::default().fg(Color::Yellow).bold(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------------
// Body / layout + zoom
// ---------------------------------------------------------------------------

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    app.sidebar_area = Rect::default();
    app.main_area = Rect::default();
    app.preview_area = Rect::default();
    app.newconn_area = Rect::default();

    if let Some(zoom) = app.zoom.filter(|z| *z == app.focus) {
        match zoom {
            Focus::Sidebar => {
                app.sidebar_area = area;
                draw_sidebar(f, app, area);
            }
            Focus::Main => {
                app.main_area = area;
                draw_main(f, app, area);
            }
            Focus::Preview => {
                app.preview_area = area;
                draw_preview(f, app, area);
            }
        }
        return;
    }

    let hchunks = Layout::horizontal([Constraint::Length(28), Constraint::Min(0)]).split(area);
    let rchunks = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(hchunks[1]);

    app.sidebar_area = hchunks[0];
    app.main_area = rchunks[0];
    app.preview_area = rchunks[1];

    draw_sidebar(f, app, hchunks[0]);
    draw_main(f, app, rchunks[0]);
    draw_preview(f, app, rchunks[1]);
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.sidebar_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .filter_map(|row| match *row {
            SidebarRow::Conn(ci) => {
                // Indeks dari sidebar_rows bisa saja sudah tidak valid
                // (mis. koneksi dihapus); lewati baris itu alih-alih panic.
                let node = app.nodes.get(ci)?;
                let (dot_style, expand, name_style) = if node.mounted {
                    (
                        Style::default().fg(Color::Green),
                        if node.expanded { "- " } else { "+ " },
                        Style::default().fg(Color::White).bold(),
                    )
                } else {
                    (
                        Style::default().fg(Color::DarkGray),
                        "  ",
                        Style::default().fg(Color::DarkGray),
                    )
                };
                Some(ListItem::new(Line::from(vec![
                    Span::styled(app.icon_style.dot(), dot_style),
                    Span::styled(expand, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(node.conn.name.clone(), name_style),
                ])))
            }
            SidebarRow::Bucket(ci, bi) => {
                let name = app
                    .nodes
                    .get(ci)
                    .and_then(|n| n.buckets.as_ref())
                    .and_then(|b| b.get(bi))
                    .cloned()
                    .unwrap_or_default();
                // Nama bucket selalu mulai di kolom 6 (satu level di bawah nama
                // koneksi); style tanpa icon memakai spasi sebagai gantinya.
                let icon = app.icon_style.bucket_icon();
                let pad = 6usize.saturating_sub(icon.width());
                let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(pad))];
                if !icon.is_empty() {
                    spans.push(Span::styled(icon, Style::default().fg(Color::DarkGray)));
                }
                spans.push(Span::styled(name, Style::default().fg(Color::White)));
                Some(ListItem::new(Line::from(spans)))
            }
            SidebarRow::Loading => Some(ListItem::new(Line::from(Span::styled(
                "    loading...",
                Style::default().fg(Color::DarkGray).italic(),
            )))),
            SidebarRow::Empty => {
                let (text, indent) = if app.find_sidebar.query.is_empty() {
                    ("(no buckets)", "    ")
                } else {
                    ("(tidak ada hasil)", "      ")
                };
                Some(ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(text, Style::default().fg(Color::DarkGray).italic()),
                ])))
            }
            // Footer statis: row ini tidak lagi diproduksi oleh app.sidebar_rows(),
            // tapi arm-nya dipertahankan agar match tetap exhaustive.
            SidebarRow::NewConnection => Some(ListItem::new(Line::from(""))),
        })
        .collect();

    let block = focused_block("Connections", app.focus == Focus::Sidebar, app.icon_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // [ list scrollable ][ 2 baris footer statis ]
    let (list_area, footer_area) = if inner.height >= 3 {
        let c = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(inner);
        (c[0], c[1])
    } else {
        (inner, Rect::default())
    };

    let list = List::new(items)
        .highlight_style(Style::default().bg(HL_BG))
        .highlight_symbol(app.icon_style.highlight())
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, list_area, &mut app.sidebar_state);

    // Simpan rect footer (2 baris) untuk hit-testing klik mouse.
    app.newconn_area = footer_area;
    if footer_area.height >= 2 {
        let width = footer_area.width as usize;
        let sep_area = Rect {
            height: 1,
            ..footer_area
        };
        let btn_area = Rect {
            y: footer_area.y.saturating_add(1),
            height: 1,
            ..footer_area
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                footer_sep_chars(app.icon_style, width),
                Style::default().fg(Color::DarkGray),
            )),
            sep_area,
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "[+] New Connection",
                    Style::default().fg(Color::Yellow).bold(),
                ),
            ])),
            btn_area,
        );
    }
}

// ---------------------------------------------------------------------------
// Main pane
// ---------------------------------------------------------------------------

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let title = app.breadcrumb();
    let block = focused_block(&title, app.focus == Focus::Main, app.icon_style);

    if app.location.is_none() {
        let p = Paragraph::new("Pilih koneksi -> bucket di sidebar\n(Enter / klik untuk membuka)")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    }
    if app.loading {
        let p = Paragraph::new("Loading...")
            .style(Style::default().fg(Color::Yellow))
            .block(block);
        f.render_widget(p, area);
        return;
    }
    if app.items.is_empty() && app.find_main.query.is_empty() {
        let p = Paragraph::new("(kosong)")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    }

    let inner = block.inner(area);
    f.render_widget(block, area);

    let list_area = inner;

    let visible = app.visible_items();
    if visible.is_empty() {
        f.render_widget(
            Paragraph::new("(tidak ada hasil)").style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    // kolom highlight symbol (2); border sudah dikurangi lewat `inner`
    let list_width = (list_area.width as usize).saturating_sub(2);
    let items: Vec<ListItem> = visible
        .iter()
        .filter_map(|i| app.items.get(*i))
        .map(|it| {
            s3_row(
                it,
                list_width,
                true,
                app.marked.contains(&it.key),
                app.icon_style,
            )
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().bg(HL_BG))
        .highlight_symbol(app.icon_style.highlight())
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, list_area, &mut app.main_state);
}

// ---------------------------------------------------------------------------
// Preview pane
// ---------------------------------------------------------------------------

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Preview;
    let style = app.icon_style;
    // Hoist lowercase sekali per frame: highlight_line tidak perlu
    // mengalokasikan ulang untuk setiap baris preview.
    let needle = app.preview_query().to_lowercase();
    match &mut app.preview {
        PreviewState::Empty => {
            let block = focused_block("Preview", focused, style);
            f.render_widget(
                Paragraph::new("(tidak ada preview)")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(block),
                area,
            );
        }
        PreviewState::Loading => {
            let block = focused_block("Preview", focused, style);
            f.render_widget(
                Paragraph::new("Loading...")
                    .style(Style::default().fg(Color::Yellow))
                    .block(block),
                area,
            );
        }
        PreviewState::Message(m) => {
            let block = focused_block("Preview", focused, style);
            f.render_widget(
                Paragraph::new(m.clone())
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: false })
                    .block(block),
                area,
            );
        }
        PreviewState::Text(t) => {
            let block = focused_block("Preview", focused, style);
            let inner = block.inner(area);
            let lines = wrap_text(t.as_str(), inner.width as usize);
            let total = lines.len();
            let max_scroll = total.saturating_sub(inner.height as usize);
            if app.preview_scroll as usize > max_scroll {
                app.preview_scroll = max_scroll.min(u16::MAX as usize) as u16;
            }
            // Highlight keyword setelah wrap agar posisi konsisten dengan scroll.
            let rendered: Vec<Line> = lines
                .into_iter()
                .map(|l| highlight_line(&l, &needle))
                .collect();
            let p = Paragraph::new(rendered)
                .style(Style::default().fg(Color::White))
                .scroll((app.preview_scroll, 0))
                .block(block);
            f.render_widget(p, area);
        }
        PreviewState::Meta(meta) => {
            let block = focused_block("Preview", focused, style);
            let inner = block.inner(area);
            let max_scroll = meta.len().saturating_sub(inner.height as usize);
            if app.preview_scroll as usize > max_scroll {
                app.preview_scroll = max_scroll.min(u16::MAX as usize) as u16;
            }
            let lines: Vec<Line> = meta
                .iter()
                .map(|(k, v)| {
                    Line::from(vec![
                        Span::styled(format!("{k:<16} "), Style::default().fg(Color::Yellow)),
                        Span::styled(v.clone(), Style::default().fg(Color::White)),
                    ])
                })
                .collect();
            let p = Paragraph::new(lines)
                .scroll((app.preview_scroll, 0))
                .block(block);
            f.render_widget(p, area);
        }
        PreviewState::Listing(items) => {
            let block = focused_block("Preview", focused, style);
            let list_width = (area.width.saturating_sub(2) as usize).saturating_sub(2);
            let rows: Vec<ListItem> = items
                .iter()
                .map(|it| s3_row(it, list_width, false, false, style))
                .collect();
            let list = List::new(rows)
                .block(block)
                .highlight_style(Style::default().bg(HL_BG))
                .highlight_symbol(style.highlight())
                .highlight_spacing(HighlightSpacing::Always);
            f.render_stateful_widget(list, area, &mut app.preview_list_state);
        }
        PreviewState::Image(proto) => {
            let block = focused_block("Preview", focused, style);
            let inner = block.inner(area);
            f.render_widget(block, area);
            // Bersihkan sisa glyph/karakter dari frame sebelumnya supaya
            // gambar tidak tercampur konten lama di area yang sama.
            f.render_widget(Clear, inner);
            f.render_stateful_widget(StatefulImage::default(), inner, proto);
        }
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    if app.download_active {
        if let Some(status) = &app.download_status {
            spans.push(Span::styled(
                format!("[DL] {status} "),
                Style::default().fg(Color::Cyan).bold(),
            ));
        }
    }
    if app.visual {
        spans.push(Span::styled(
            "[VISUAL] ",
            Style::default().fg(Color::Magenta).bold(),
        ));
    }
    if let Some(cb) = &app.clipboard {
        spans.push(Span::styled(
            format!(
                "[{} {} item] ",
                if cb.is_move { "MOVE" } else { "COPY" },
                cb.items.len()
            ),
            Style::default().fg(Color::Cyan).bold(),
        ));
    }
    if !app.marked.is_empty() {
        spans.push(Span::styled(
            format!("[{} marked] ", app.marked.len()),
            Style::default().fg(Color::Magenta),
        ));
    }
    let status_style = if app.status_error {
        Style::default().fg(Color::Red).bold()
    } else {
        Style::default().fg(Color::Green)
    };
    spans.push(Span::styled(app.status.clone(), status_style));

    if area.width < 30 {
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(11)]).split(area);
    f.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled("[?] help", Style::default().fg(Color::DarkGray)))
            .alignment(Alignment::Right),
        cols[1],
    );
}

// ---------------------------------------------------------------------------
// Popups
// ---------------------------------------------------------------------------

fn draw_popup(f: &mut Frame, app: &mut App, area: Rect) {
    let style = app.icon_style;
    match &mut app.mode {
        Mode::Normal => {}
        Mode::Help => draw_help(f, area, style),
        Mode::AddMenu { selected } => {
            let rect = centered_rect(40, 24, area);
            f.render_widget(Clear, rect);
            let block = popup_block("Add", style);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let body = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

            let options = ["New Folder", "Upload File"];
            let items: Vec<ListItem> = options
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    let style = if i == *selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    ListItem::new(format!(" {o} ")).style(style)
                })
                .collect();
            f.render_widget(List::new(items), body[0]);
            f.render_widget(
                Paragraph::new(" Enter pilih   Esc batal")
                    .style(Style::default().fg(Color::DarkGray)),
                body[1],
            );
        }
        Mode::NewFolder(input) => {
            let rect = centered_rect(54, 20, area);
            f.render_widget(Clear, rect);
            let block = popup_block("New Folder", style);
            let inner = block.inner(rect);
            f.render_widget(block, rect);

            let lines = vec![
                Line::from(Span::styled(
                    "Nama folder:",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    input.value.clone(),
                    Style::default().fg(Color::White).underlined(),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "Enter buat   Esc batal",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            f.render_widget(Paragraph::new(lines), inner);

            let col = input.value[..safe_cursor(&input.value, input.cursor)].width() as u16;
            let x = inner
                .x
                .saturating_add(col)
                .min(inner.x + inner.width.saturating_sub(1));
            let y = (inner.y + 1).min(inner.y + inner.height.saturating_sub(1));
            f.set_cursor_position(Position::new(x, y));
        }
        Mode::FilePicker(fp) => {
            let rect = centered_rect(72, 70, area);
            f.render_widget(Clear, rect);
            let dir_str = fp.dir.display().to_string();
            let title = if fp.multi {
                "Upload File"
            } else {
                "Pilih Folder Tujuan"
            };
            let block = popup_block(title, style).title(Span::styled(
                format!(" {dir_str} "),
                Style::default().fg(Color::DarkGray),
            ));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            // [filter (opsional)][daftar][ringkasan][hint]
            let visible = fp.visible_indices();
            let show_filter = find_visible(&fp.filter, fp.filter_active);
            let (filter_area, list_area, summary_area, hint_area) =
                if show_filter && inner.height >= 3 {
                    let body = Layout::vertical([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                        Constraint::Length(1),
                    ])
                    .split(inner);
                    (Some(body[0]), body[1], body[2], body[3])
                } else {
                    let body = Layout::vertical([
                        Constraint::Min(0),
                        Constraint::Length(1),
                        Constraint::Length(1),
                    ])
                    .split(inner);
                    (None, body[0], body[1], body[2])
                };
            if let Some(fa) = filter_area {
                f.render_widget(
                    Paragraph::new(find_input_line(
                        &fp.filter,
                        Some(fp.filter_cursor),
                        fp.filter_active,
                    )),
                    fa,
                );
            }

            let items: Vec<ListItem> = visible
                .iter()
                .filter_map(|i| fp.entries.get(*i))
                .map(|e| {
                    let icon = style.icon(&e.name, e.is_dir);
                    let name_style = if e.is_dir {
                        Style::default().fg(Color::Cyan).bold()
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let marked = fp.marked.iter().any(|p| *p == e.path);
                    let mark = if marked {
                        Span::styled("* ", Style::default().fg(Color::Magenta).bold())
                    } else {
                        Span::raw("  ")
                    };
                    let size = if e.is_dir {
                        String::new()
                    } else {
                        format!("  {}", human_size(Some(e.size as i64)))
                    };
                    // nama yang sudah ditandai dibuat tebal agar mudah dilihat
                    let name_style = if marked {
                        name_style.add_modifier(Modifier::BOLD)
                    } else {
                        name_style
                    };
                    ListItem::new(Line::from(vec![
                        mark,
                        Span::styled(icon, Style::default().fg(Color::DarkGray)),
                        Span::styled(e.name.clone(), name_style),
                        Span::styled(size, Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect();
            if items.is_empty() {
                f.render_widget(
                    Paragraph::new("   (tidak ada hasil)")
                        .style(Style::default().fg(Color::DarkGray)),
                    list_area,
                );
            } else {
                let list = List::new(items)
                    .highlight_style(Style::default().bg(HL_BG))
                    .highlight_symbol(style.highlight())
                    .highlight_spacing(HighlightSpacing::Always);
                f.render_stateful_widget(list, list_area, &mut fp.list_state);
            }

            let total = visible.len();
            let sel = fp.list_state.selected().map(|s| s + 1).unwrap_or(0);
            let summary = if fp.multi {
                format!(
                    " {} dari {} item   |   {} file ditandai",
                    sel,
                    total,
                    fp.marked.len()
                )
            } else {
                format!(" {} dari {} item", sel, total)
            };
            f.render_widget(
                Paragraph::new(summary).style(Style::default().fg(Color::Yellow)),
                summary_area,
            );
            let hint = if fp.multi {
                " Up/Down pilih   Space tandai/lepas   Enter unggah (semua yang ditandai)   <- parent   Esc batal"
            } else {
                " Up/Down pilih   Enter pilih folder   -> masuk folder   <- parent   Esc batal"
            };
            let hint_line = Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)));
            f.render_widget(Paragraph::new(hint_line), hint_area);
        }
        Mode::DeleteConfirm { targets, selected } => {
            let selected = (*selected).min(1);
            let rect = centered_rect(66, 62, area);
            f.render_widget(Clear, rect);
            let block = popup_block("Confirm Delete", style);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let body = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

            let any_loading = targets.iter().any(|t| t.loading);
            let any_error = targets.iter().any(|t| t.error.is_some());
            let total_keys: usize = targets.iter().map(|t| t.keys.len()).sum();

            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    "{} target, {} object akan dihapus:",
                    targets.len(),
                    total_keys
                ),
                Style::default().fg(Color::Red).bold(),
            )));
            lines.push(Line::raw(""));
            for t in targets.iter() {
                let icon = style.icon(&t.item.name, t.item.is_dir);
                lines.push(Line::from(Span::styled(
                    format!("{icon} {}", t.item.key),
                    Style::default().fg(Color::White).bold(),
                )));
                if t.loading {
                    lines.push(Line::from(Span::styled(
                        "    menghitung isi...",
                        Style::default().fg(Color::DarkGray).italic(),
                    )));
                } else if let Some(e) = &t.error {
                    lines.push(Line::from(Span::styled(
                        format!("    <gagal list: {e}>"),
                        Style::default().fg(Color::Red),
                    )));
                } else if t.item.is_dir {
                    let show = t.keys.len().min(8);
                    for k in t.keys.iter().take(show) {
                        lines.push(Line::from(Span::styled(
                            format!("    - {k}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    if t.keys.len() > show {
                        lines.push(Line::from(Span::styled(
                            format!("    ... dan {} lagi", t.keys.len() - show),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            }
            f.render_widget(Paragraph::new(lines), body[0]);

            // baris tombol: fokus = hitam di atas kuning
            let focused = Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .bold();
            let delete_style = if selected == 0 {
                focused
            } else {
                Style::default().fg(Color::Red).bold()
            };
            let cancel_style = if selected == 1 {
                focused
            } else {
                Style::default().fg(Color::Yellow)
            };
            let buttons = Line::from(vec![
                Span::raw("  ["),
                Span::styled(" [Y] Delete ", delete_style),
                Span::raw("]    ["),
                Span::styled(" [n] Cancel ", cancel_style),
                Span::raw("]"),
            ]);
            f.render_widget(Paragraph::new(buttons), body[1]);

            let (hint_text, hint_style) = if any_loading {
                (
                    " menghitung isi folder...",
                    Style::default().fg(Color::Yellow).bold(),
                )
            } else if any_error {
                (
                    " ada folder gagal di-list - n/Esc batal",
                    Style::default().fg(Color::Red).bold(),
                )
            } else {
                (
                    " <-/-> pilih   Enter konfirmasi   Shift+Y delete   n/Esc batal",
                    Style::default().fg(Color::DarkGray),
                )
            };
            f.render_widget(Paragraph::new(hint_text).style(hint_style), body[2]);
        }
        Mode::DownloadConfirm {
            items,
            dest_dir,
            field_active,
            selected,
            listing_state,
            picker,
            error,
        } => {
            let rect = centered_rect(70, 72, area);
            f.render_widget(Clear, rect);

            if let Some(fp) = picker {
                // --- Sub-mode: file picker folder tujuan -------------------
                let block = popup_block("Pilih Folder Tujuan", style);
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                let (path_area, list_area, filter_area, hint_area) =
                    if find_visible(&fp.filter, fp.filter_active) && inner.height >= 4 {
                        let body = Layout::vertical([
                            Constraint::Length(1), // path folder aktif
                            Constraint::Length(1), // input filter
                            Constraint::Min(0),    // entries
                            Constraint::Length(1), // hint
                        ])
                        .split(inner);
                        (body[0], body[2], Some(body[1]), body[3])
                    } else {
                        let body = Layout::vertical([
                            Constraint::Length(1), // path folder aktif
                            Constraint::Min(0),    // entries
                            Constraint::Length(1), // hint
                        ])
                        .split(inner);
                        (body[0], body[1], None, body[2])
                    };

                f.render_widget(
                    Paragraph::new(Span::styled(
                        truncate_to_width(
                            &fp.dir.display().to_string(),
                            path_area.width as usize,
                            style,
                        )
                        .into_owned(),
                        Style::default().fg(Color::Cyan),
                    )),
                    path_area,
                );
                if let Some(fa) = filter_area {
                    f.render_widget(
                        Paragraph::new(find_input_line(
                            &fp.filter,
                            Some(fp.filter_cursor),
                            fp.filter_active,
                        )),
                        fa,
                    );
                }

                // borders sudah di luar body, hanya highlight symbol yang menyita 2 kolom
                let list_width = (list_area.width as usize).saturating_sub(2);
                let rows: Vec<ListItem> = fp
                    .visible_indices()
                    .iter()
                    .filter_map(|i| fp.entries.get(*i))
                    .map(|e| {
                        let icon = style.icon(&e.name, e.is_dir);
                        let name_style = if e.is_dir {
                            Style::default().fg(Color::Cyan).bold()
                        } else {
                            Style::default().fg(Color::White)
                        };
                        let size = if e.is_dir {
                            String::new()
                        } else {
                            format!("  ({})", human_size(Some(e.size as i64)))
                        };
                        let name_avail = list_width
                            .saturating_sub(icon.width() + 1 + size.width())
                            .max(8);
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                format!("{icon} "),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                truncate_to_width(&e.name, name_avail, style).into_owned(),
                                name_style,
                            ),
                            Span::styled(size, Style::default().fg(Color::DarkGray)),
                        ]))
                    })
                    .collect();
                let list = List::new(rows)
                    .highlight_style(Style::default().bg(HL_BG))
                    .highlight_symbol(style.highlight())
                    .highlight_spacing(HighlightSpacing::Always);
                f.render_stateful_widget(list, list_area, &mut fp.list_state);
                let hint_text = if fp.filter_active {
                    format!(
                        " Enter pilih folder \u{00b7} Up/Down navigasi \u{00b7} Esc batal"
                    )
                } else {
                    " Enter pilih folder \u{00b7} Up/Down navigasi \u{00b7} Left/Backspace parent \u{00b7} Esc batal".to_string()
                };
                f.render_widget(
                    Paragraph::new(Span::styled(
                        hint_text,
                        Style::default().fg(Color::DarkGray),
                    )),
                    hint_area,
                );
            } else {
                // --- Konfirmasi download -----------------------------------
                let block = popup_block("Download", style);
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                let body = Layout::vertical([
                    Constraint::Length(1), // header
                    Constraint::Min(0),    // daftar item
                    Constraint::Length(3), // folder tujuan + error
                    Constraint::Length(2), // tombol + hint
                ])
                .split(inner);

                // 1. header
                let n = items.len();
                let has_dir = items.iter().any(|it| it.is_dir);
                let mut header = vec![Span::styled(
                    format!("{n} item akan diunduh"),
                    Style::default().fg(Color::Cyan).bold(),
                )];
                if n > 1 || has_dir {
                    header.push(Span::styled(
                        " (dikemas jadi ZIP)",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                f.render_widget(Paragraph::new(Line::from(header)), body[0]);

                // 2. daftar item
                let list_width = (body[1].width as usize).saturating_sub(2);
                let rows: Vec<ListItem> = items
                    .iter()
                    .map(|it| {
                        let icon = style.icon(&it.name, it.is_dir);
                        let name_style = if it.is_dir {
                            Style::default().fg(Color::Cyan).bold()
                        } else {
                            Style::default().fg(Color::White)
                        };
                        let size = if it.is_dir {
                            String::new()
                        } else {
                            format!("  ({})", human_size(it.size))
                        };
                        let name_avail = list_width
                            .saturating_sub(icon.width() + 1 + size.width())
                            .max(8);
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                format!("{icon} "),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                truncate_to_width(&it.name, name_avail, style).into_owned(),
                                name_style,
                            ),
                            Span::styled(size, Style::default().fg(Color::DarkGray)),
                        ]))
                    })
                    .collect();
                let list = List::new(rows)
                    .highlight_style(Style::default().bg(HL_BG))
                    .highlight_symbol(style.highlight())
                    .highlight_spacing(HighlightSpacing::Always);
                f.render_stateful_widget(list, body[1], listing_state);

                // 3. field folder tujuan
                let path_str = dest_dir.display().to_string();
                let path_style = if *field_active {
                    Style::default().fg(Color::Black).bg(Color::Yellow).bold()
                } else {
                    Style::default().fg(Color::White).bg(Color::DarkGray)
                };
                let path_text = format!(
                    " {}",
                    truncate_to_width(&path_str, (body[2].width as usize).saturating_sub(1), style)
                );
                let field_rows = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(body[2]);
                f.render_widget(
                    Paragraph::new(Span::styled(
                        "Folder tujuan:",
                        Style::default().fg(Color::DarkGray),
                    )),
                    field_rows[0],
                );
                f.render_widget(Paragraph::new(Span::raw(path_text)).style(path_style), field_rows[1]);
                if let Some(err) = error.as_ref() {
                    f.render_widget(
                        Paragraph::new(Span::styled(
                            format!(" {err}"),
                            Style::default().fg(Color::Red).bold(),
                        )),
                        field_rows[2],
                    );
                }

                // 4. tombol + hint
                let focused = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .bold();
                let dl_style = if *selected == 0 {
                    focused
                } else {
                    Style::default().fg(Color::Green).bold()
                };
                let cancel_style = if *selected == 1 {
                    focused
                } else {
                    Style::default().fg(Color::Yellow)
                };
                let buttons = Line::from(vec![
                    Span::raw("  ["),
                    Span::styled(" Download ", dl_style),
                    Span::raw("]    ["),
                    Span::styled(" Cancel ", cancel_style),
                    Span::raw("]"),
                ]);
                let btn_rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
                    .split(body[3]);
                f.render_widget(Paragraph::new(buttons), btn_rows[0]);
                f.render_widget(
                    Paragraph::new(Span::styled(
                        " Tab fokus \u{00b7} Left/Right tombol \u{00b7} Up/Down scroll \u{00b7} p pilih folder \u{00b7} Enter jalankan \u{00b7} Esc batal",
                        Style::default().fg(Color::DarkGray),
                    )),
                    btn_rows[1],
                );
            }
        }
        Mode::ConnForm(form) => {
            let title = if form.editing.is_some() {
                "Edit Connection"
            } else {
                "New Connection"
            };
            let rect = centered_rect(64, 55, area);
            f.render_widget(Clear, rect);
            let block = popup_block(title, style);
            let inner = block.inner(rect);
            f.render_widget(block, rect);

            let mut lines: Vec<Line> = Vec::new();
            let mut cursor_pos: Option<Position> = None;

            for (i, (label, input, secret)) in form.fields.iter().enumerate() {
                let active = i == form.active;
                let shown = if *secret {
                    "*".repeat(input.value.chars().count())
                } else {
                    input.value.clone()
                };
                let label_style = if active {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let val_style = if active {
                    Style::default().fg(Color::White).underlined()
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<18}"), label_style),
                    Span::styled(shown, val_style),
                ]));
                if active {
                    let col = input.value[..safe_cursor(&input.value, input.cursor)].width() as u16;
                    let x = inner
                        .x
                        .saturating_add(18)
                        .saturating_add(col)
                        .min(inner.x + inner.width.saturating_sub(1));
                    let y = (inner.y + i as u16).min(inner.y + inner.height.saturating_sub(1));
                    cursor_pos = Some(Position::new(x, y));
                }
            }

            lines.push(Line::raw(""));
            // Tombol statis (tidak fokusable): Enter/Shift+Enter ditangani app.rs.
            let btn = |label: &str| {
                Span::styled(format!("[ {label} ]"), Style::default().fg(Color::Yellow))
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                btn("Test Connection"),
                Span::raw("    "),
                btn("Mount"),
            ]));

            let test_line = if form.testing {
                Line::from(Span::styled(
                    "  Testing...",
                    Style::default().fg(Color::Yellow).italic(),
                ))
            } else {
                match &form.test_result {
                    Some((true, msg)) => Line::from(Span::styled(
                        format!("  OK {msg}"),
                        Style::default().fg(Color::Green),
                    )),
                    Some((false, msg)) => Line::from(Span::styled(
                        format!("  X {msg}"),
                        Style::default().fg(Color::Red),
                    )),
                    None => Line::from(""),
                }
            };
            lines.push(test_line);
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Up/Down/Tab pindah field   Enter: mount   Shift+Enter: test   Esc: batal",
                Style::default().fg(Color::DarkGray),
            )));

            f.render_widget(Paragraph::new(lines), inner);
            if let Some(pos) = cursor_pos {
                f.set_cursor_position(pos);
            }
        }
        Mode::ConnMenu { conn_idx, selected } => {
            let (name, mounted) = app
                .nodes
                .get(*conn_idx)
                .map(|n| (n.conn.name.clone(), n.mounted))
                .unwrap_or_else(|| ("?".to_string(), false));
            let rect = centered_rect(36, 26, area);
            f.render_widget(Clear, rect);
            let block = popup_block(&name, style);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let body = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

            let options: Vec<&str> =
                vec![if mounted { "Unmount" } else { "Mount" }, "Edit", "Delete"];
            let items: Vec<ListItem> = options
                .iter()
                .map(|o| ListItem::new(format!("  {o}")))
                .collect();
            let mut state = ListState::default();
            state.select(Some((*selected).min(options.len() - 1)));
            let list = List::new(items)
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                .highlight_symbol(style.highlight())
                .highlight_spacing(HighlightSpacing::Always);
            f.render_stateful_widget(list, body[0], &mut state);
            f.render_widget(
                Paragraph::new(" Enter pilih   Esc batal")
                    .style(Style::default().fg(Color::DarkGray)),
                body[1],
            );
        }
        Mode::DeleteConnConfirm { conn_idx } => {
            let name = app
                .nodes
                .get(*conn_idx)
                .map(|n| n.conn.name.clone())
                .unwrap_or_default();
            let rect = centered_rect(50, 24, area);
            f.render_widget(Clear, rect);
            let block = popup_block("Delete Connection", style);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let body = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

            let lines = vec![
                Line::from(vec![
                    Span::styled("Hapus koneksi '", Style::default().fg(Color::White)),
                    Span::styled(name, Style::default().fg(Color::Yellow).bold()),
                    Span::styled("'?", Style::default().fg(Color::White)),
                ]),
                Line::raw(""),
                Line::from(Span::styled(
                    "Config di-update permanen.",
                    Style::default().fg(Color::Red),
                )),
            ];
            f.render_widget(Paragraph::new(lines), body[0]);
            f.render_widget(
                Paragraph::new(" y/Enter hapus   tombol lain batal")
                    .style(Style::default().fg(Color::Yellow).bold()),
                body[1],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

fn draw_help(f: &mut Frame, area: Rect, style: IconStyle) {
    let rect = centered_rect(68, 82, area);
    f.render_widget(Clear, rect);
    let block = popup_block("Help", style);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let help: [(&str, &str); 29] = [
        ("Navigasi", ""),
        ("  Up/Down / j/k", "pilih item"),
        ("  -> / Enter / l", "buka folder / bucket"),
        ("  <- / h", "mundur ke folder induk"),
        ("  Tab / BackTab", "pindah pane sidebar->main->preview"),
        ("  Ctrl+Z", "zoom / unzoom pane"),
        ("  mouse", "klik/wheel   klik kanan koneksi = menu"),
        ("  /", "cari/filter live (hanya pane/popup yang aktif)"),
        ("  Esc", "nonaktifkan input; Esc lagi = hapus filter"),
        ("Seleksi", ""),
        ("  Space", "tandai di main / menu koneksi di sidebar"),
        ("  v", "mode visual: tandai sambil navigasi Up/Down"),
        ("  V", "reset semua tanda"),
        ("Operasi", ""),
        ("  a", "add: new folder / insert new file (upload)"),
        ("  D (shift+d)", "hapus item tertandai / ter-highlight"),
        (
            "  d",
            "download item tertandai / ter-highlight (popup) -> ZIP bila >1 atau folder",
        ),
        ("  c / x", "copy mode / move mode"),
        ("  p", "paste ke folder saat ini / ter-highlight"),
        ("  r", "refresh"),
        ("Koneksi", ""),
        ("  N", "tambah koneksi S3 baru"),
        (
            "  space / klik kanan",
            "mount / unmount / edit / delete koneksi",
        ),
        ("Lainnya", ""),
        ("  ?", "help ini"),
        (
            "  Ctrl+G",
            "ganti gaya icon/border (nerd/unicode/ascii)",
        ),
        ("  q / Ctrl+C", "keluar"),
        ("  Esc", "tutup popup / batal mode"),
        ("  catatan", "mount diperlukan sebelum browsing bucket"),
    ];

    let rows = help.len();
    let avail_h = inner.height as usize;
    let avail_w = inner.width as usize;
    if avail_h == 0 || avail_w == 0 {
        return;
    }

    // Kasus normal: seluruh entri muat dalam satu kolom.
    if rows <= avail_h {
        let key_w = 22usize.min(avail_w);
        let val_w = avail_w.saturating_sub(key_w);
        f.render_widget(
            Paragraph::new(help_lines(&help, key_w, val_w, style)),
            inner,
        );
        return;
    }

    // Konten lebih panjang dari tinggi popup. Tidak ada state scroll di ui.rs,
    // jadi bila lebar memadai konten dipecah ke beberapa kolom agar semua
    // entri tetap terbaca; kalau tidak, konten dipangkas dengan penanda jelas
    // bahwa masih ada entri lain (lihat README).
    let cols = rows.div_ceil(avail_h);
    let col_w = avail_w / cols;
    if avail_w > 70 && col_w >= 24 {
        let per_col = rows.div_ceil(cols);
        let key_w = 22usize.min(col_w.saturating_sub(6));
        let val_w = col_w.saturating_sub(key_w);
        let mut col_lines: Vec<Vec<Line>> = Vec::with_capacity(cols);
        for c in 0..cols {
            let start = (c * per_col).min(rows);
            let end = (start + per_col).min(rows);
            if start >= end {
                break;
            }
            col_lines.push(help_lines(&help[start..end], key_w, val_w, style));
        }
        if let Some(last) = col_lines.last_mut() {
            if last.len() < avail_h {
                last.push(Line::from(Span::styled(
                    " ... lihat README",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        let areas = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols]).split(inner);
        for (i, lines) in col_lines.into_iter().enumerate() {
            if let Some(a) = areas.get(i) {
                f.render_widget(Paragraph::new(lines), *a);
            }
        }
        return;
    }

    let key_w = 22usize.min(avail_w);
    let val_w = avail_w.saturating_sub(key_w);
    let mut lines: Vec<Line> = if avail_h > 1 {
        help_lines(&help[..avail_h - 1], key_w, val_w, style)
    } else {
        Vec::new()
    };
    lines.push(Line::from(Span::styled(
        "  ... entri lain lihat README",
        Style::default().fg(Color::Yellow).bold(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

/// Bangun baris-baris help dengan kolom key selebar `key_w` dan value
/// dipotong agar muat `val_w` kolom.
fn help_lines(
    entries: &[(&str, &str)],
    key_w: usize,
    val_w: usize,
    style: IconStyle,
) -> Vec<Line<'static>> {
    entries
        .iter()
        .map(|(k, v)| {
            if v.is_empty() && !k.is_empty() {
                Line::from(Span::styled(
                    k.to_string(),
                    Style::default().fg(Color::Cyan).bold(),
                ))
            } else {
                let value_style = if *k == "  catatan" {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::White)
                };
                let padded = format!("{k:<key_w$}");
                let key = truncate_to_width(&padded, key_w, style).into_owned();
                let val = truncate_to_width(v, val_w, style).into_owned();
                Line::from(vec![
                    Span::styled(key, Style::default().fg(Color::Yellow)),
                    Span::styled(val, value_style),
                ])
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn hits(line: &Line) -> Vec<String> {
        line.spans
            .iter()
            .filter(|s| s.style == keyword_style())
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn highlight_without_query_is_plain() {
        let line = highlight_line("hello world", "");
        assert_eq!(joined(&line), "hello world");
        assert!(hits(&line).is_empty());
    }

    #[test]
    fn highlight_finds_every_occurrence_case_insensitive() {
        let line = highlight_line("Foo bar foo FOO", "foo");
        assert_eq!(joined(&line), "Foo bar foo FOO");
        assert_eq!(hits(&line), vec!["Foo", "foo", "FOO"]);
    }

    #[test]
    fn highlight_multi_word_query() {
        let line = highlight_line("a big bad wolf and a BIG BAD wolf", "big bad");
        assert_eq!(hits(&line), vec!["big bad", "BIG BAD"]);
        assert_eq!(joined(&line), "a big bad wolf and a BIG BAD wolf");
    }

    #[test]
    fn highlight_handles_non_ascii() {
        let line = highlight_line("Laporan BULANAN selesai", "bulanan");
        assert_eq!(joined(&line), "Laporan BULANAN selesai");
        assert_eq!(hits(&line), vec!["BULANAN"]);

        // case folding yang mengubah panjang byte tetap menghasilkan span valid
        let line = highlight_line("\u{130}stanbul istanbul", "istanbul");
        assert_eq!(joined(&line), "\u{130}stanbul istanbul");
        assert!(!hits(&line).is_empty());
    }

    #[test]
    fn find_visible_only_for_active_or_query() {
        assert!(!find_visible("", false));
        assert!(find_visible("", true));
        assert!(find_visible("abc", false));
    }

    #[test]
    fn centered_rect_clamps_percent_and_stays_inside() {
        let area = Rect::new(2, 3, 80, 24);
        for px in [0u16, 1, 50, 100, 150, u16::MAX] {
            for py in [0u16, 1, 50, 100, 150, u16::MAX] {
                let r = centered_rect(px, py, area);
                assert!(r.width >= 1 && r.height >= 1);
                assert!(r.x >= area.x && r.y >= area.y);
                assert!(r.right() <= area.right());
                assert!(r.bottom() <= area.bottom());
            }
        }
        // area kosong tidak boleh underflow / panik
        let empty = centered_rect(50, 50, Rect::new(0, 0, 0, 0));
        assert_eq!((empty.width, empty.height), (0, 0));
    }

    #[test]
    fn truncate_borrows_when_it_fits() {
        let s = "hello";
        let out = truncate_to_width(s, 5, IconStyle::Ascii);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "hello");

        let out = truncate_to_width(s, 3, IconStyle::Ascii);
        assert!(matches!(out, Cow::Owned(_)));
        assert!(out.width() <= 3);
        assert_eq!(truncate_to_width(s, 0, IconStyle::Ascii), "");
    }

    #[test]
    fn safe_cursor_snaps_to_char_boundary() {
        let s = "a\u{00e9}b";
        assert_eq!(safe_cursor(s, 2), 1);
        assert_eq!(safe_cursor(s, 1), 1);
        assert_eq!(safe_cursor(s, 99), s.len());
        assert_eq!(safe_cursor("", 7), 0);
    }

    fn render_to_string(width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| draw_help(f, f.area(), IconStyle::Ascii))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn help_popup_fits_small_and_large_terminals() {
        // muat satu kolom, pangkas dengan penanda, dan multi-kolom
        for (w, h) in [(40u16, 12u16), (100, 20), (120, 20), (120, 40)] {
            let text = render_to_string(w, h);
            assert!(!text.is_empty(), "help kosong pada {w}x{h}");
        }
        // pada terminal tinggi, semua entri (termasuk yang terakhir) terlihat
        let tall = render_to_string(120, 40);
        assert!(tall.contains("catatan"));
        assert!(tall.contains("keluar"));
        // pada terminal pendek tapi lebar, entri terakhir tetap ada berkat kolom
        let wide = render_to_string(120, 20);
        assert!(wide.contains("catatan"));
        // pada terminal sempit, ada indikasi bahwa konten dipangkas
        let narrow = render_to_string(60, 12);
        assert!(narrow.contains("README"));
    }
}
