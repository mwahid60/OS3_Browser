use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui::{Terminal, prelude::*};
use ratatui_image::picker::{Picker, ProtocolType};
use tokio::sync::mpsc;

use crate::config::{self, Config, Connection};
use crate::s3::{S3Client, human_size};
use crate::ui;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct S3Item {
    pub name: String,
    pub key: String,
    pub is_dir: bool,
    pub size: Option<i64>,
    pub modified: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub conn_idx: usize,
    pub bucket: String,
    pub prefix: String, // "" or ends with "/"
}

pub enum AppEvent {
    BucketsLoaded {
        conn_idx: usize,
        req_id: u64,
        result: Result<Vec<String>, String>,
        is_mount: bool,
    },
    ConnTestResult {
        test_seq: u64,
        ok: bool,
        message: String,
    },
    ListingLoaded {
        id: u64,
        result: Result<Vec<S3Item>, String>,
    },
    PreviewLoaded {
        id: u64,
        result: Result<PreviewData, String>,
    },
    DeleteInfoReady {
        id: u64,
        targets: Vec<DeleteTarget>,
    },
    /// Progress download (string siap tampil di status bar).
    DownloadProgress {
        text: String,
    },
    /// Progress upload (string siap tampil di status bar).
    ///
    /// Sengaja dipisah dari `DownloadProgress` agar upload tidak menimpa/
    /// memakai state progress milik download.
    UploadProgress {
        text: String,
    },
    OpDone {
        message: String,
        is_error: bool,
        refresh: bool,
        restore: Option<Clipboard>,
    },
}

pub enum PreviewData {
    Text(String),
    Image(Vec<u8>),
    Meta(Vec<(String, String)>),
    Listing(Vec<S3Item>),
}

pub enum PreviewState {
    Empty,
    Loading,
    Text(String),
    Image(ratatui_image::protocol::StatefulProtocol),
    Meta(Vec<(String, String)>),
    Listing(Vec<S3Item>),
    Message(String),
}

pub struct ConnNode {
    pub conn: Connection,
    pub expanded: bool,
    pub mounted: bool,
    pub buckets: Option<Vec<String>>,
    pub loading: bool,
    pub req_id: u64, // invalidasi response BucketsLoaded yang stale
}

#[derive(PartialEq, Clone, Copy)]
pub enum Focus {
    Sidebar,
    Main,
    Preview,
}

#[derive(Clone)]
pub struct Clipboard {
    pub items: Vec<S3Item>,
    pub is_move: bool,
    pub src: Location,
}

/// Satu entri pada popup Download.
#[derive(Clone, Debug)]
pub struct DownloadItem {
    /// Nama tampilan (folder memakai akhiran '/').
    pub name: String,
    /// S3 key, atau prefix untuk folder (diakhiri '/').
    pub key: String,
    pub is_dir: bool,
    pub size: Option<i64>,
}

#[derive(Clone)]
pub struct DeleteTarget {
    pub item: S3Item,
    pub keys: Vec<String>,
    pub loading: bool,
    pub error: Option<String>,
}

pub struct TextInput {
    pub value: String,
    pub cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        TextInput {
            value: String::new(),
            cursor: 0,
        }
    }
    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.value[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.value.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }
    pub fn handle(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = self.value[..self.cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            KeyCode::Right => {
                if self.cursor < self.value.len() {
                    self.cursor += self.value[self.cursor..]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(0);
                }
            }
            _ => {}
        }
    }
}

pub struct FormState {
    pub editing: Option<usize>, // Some(conn_idx) bila edit, None bila tambah baru
    pub fields: Vec<(String, TextInput, bool)>, // (label, input, is_secret)
    pub active: usize,
    pub testing: bool,
    pub test_seq: u64, // invalidasi ConnTestResult dari form yang sudah diganti
    pub test_result: Option<(bool, String)>,
}

pub struct LocalEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

/// Mutasi teks di posisi kursor (byte index) - dipakai FindState & filter picker.
fn text_insert(s: &mut String, cursor: &mut usize, c: char) {
    *cursor = (*cursor).min(s.len());
    s.insert(*cursor, c);
    *cursor += c.len_utf8();
}

fn text_backspace(s: &mut String, cursor: &mut usize) {
    *cursor = (*cursor).min(s.len());
    if *cursor == 0 {
        return;
    }
    let prev = s[..*cursor]
        .char_indices()
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    s.replace_range(prev..*cursor, "");
    *cursor = prev;
}

fn text_left(s: &str, cursor: &mut usize) {
    *cursor = (*cursor).min(s.len());
    if *cursor > 0 {
        *cursor = s[..*cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
}

fn text_right(s: &str, cursor: &mut usize) {
    *cursor = (*cursor).min(s.len());
    if *cursor < s.len() {
        *cursor += s[*cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
    }
}

/// Pane pemilik sebuah state find/filter.
///
/// Tiap pane punya slot query sendiri sehingga filter satu pane tidak pernah
/// mempengaruhi pane lain.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FindScope {
    #[default]
    Sidebar,
    Main,
    Preview,
}

impl FindScope {
    /// Scope yang sesuai dengan pane yang sedang fokus.
    pub fn for_focus(focus: Focus) -> Self {
        match focus {
            Focus::Sidebar => FindScope::Sidebar,
            Focus::Main => FindScope::Main,
            Focus::Preview => FindScope::Preview,
        }
    }
}

/// State fitur find/filter (`/`) di mode Normal.
#[derive(Default)]
#[derive(Clone)]
pub struct FindState {
    /// Input field sedang aktif (mengetik).
    pub active: bool,
    /// Isi filter (tetap tersimpan walau `active == false`).
    pub query: String,
    /// Posisi kursor (byte index).
    pub cursor: usize,
    /// Filter ini milik pane mana (lihat [`FindScope`]).
    pub scope: FindScope,
}

impl FindState {
    /// Filter ini sedang menyaring sidebar?
    pub fn applies_to_sidebar(&self) -> bool {
        self.scope == FindScope::Sidebar && !self.query.is_empty()
    }

    /// Filter ini sedang menyaring main pane?
    pub fn applies_to_main(&self) -> bool {
        self.scope == FindScope::Main && !self.query.is_empty()
    }

    /// Filter ini sedang menyaring preview pane?
    #[allow(dead_code)] // dipakai ui.rs untuk highlight preview
    pub fn applies_to_preview(&self) -> bool {
        self.scope == FindScope::Preview && !self.query.is_empty()
    }

    /// `true` bila query kosong ATAU `s` mengandung query (case-insensitive).
    pub fn matches(&self, s: &str) -> bool {
        if self.query.is_empty() {
            return true;
        }
        s.to_lowercase().contains(&self.query.to_lowercase())
    }

    pub fn insert(&mut self, c: char) {
        text_insert(&mut self.query, &mut self.cursor, c);
    }

    pub fn backspace(&mut self) {
        text_backspace(&mut self.query, &mut self.cursor);
    }

    pub fn move_left(&mut self) {
        text_left(&self.query, &mut self.cursor);
    }

    pub fn move_right(&mut self) {
        text_right(&self.query, &mut self.cursor);
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.cursor = 0;
    }
}

pub struct FilePickerState {
    pub dir: PathBuf,
    pub entries: Vec<LocalEntry>,
    pub list_state: ListState,
    /// Path yang ditandai, disimpan sebagai Vec agar urutan penandaan stabil
    /// dan tetap tersimpan saat berpindah folder.
    pub marked: Vec<PathBuf>,
    /// `true` = boleh pilih banyak file (upload), `false` = pilih satu folder.
    pub multi: bool,
    /// Isi filter nama entry (case-insensitive contains).
    pub filter: String,
    /// Input filter sedang aktif (mengetik).
    pub filter_active: bool,
    /// Posisi kursor filter (byte index).
    pub filter_cursor: usize,
}

impl FilePickerState {
    pub fn new(dir: PathBuf) -> Self {
        Self::new_with_mode(dir, true)
    }

    pub fn new_with_mode(dir: PathBuf, multi: bool) -> Self {
        let mut s = FilePickerState {
            dir,
            entries: vec![],
            list_state: ListState::default(),
            marked: Vec::new(),
            multi,
            filter: String::new(),
            filter_active: false,
            filter_cursor: 0,
        };
        s.reload();
        s
    }

    pub fn reload(&mut self) {
        self.entries = load_local_entries(&self.dir);
        self.filter_cursor = self.filter_cursor.min(self.filter.len());
        let n = self.visible_indices().len();
        self.list_state.select(if n == 0 { None } else { Some(0) });
    }

    /// Hapus teks filter dan kembalikan daftar penuh (seleksi tetap valid).
    /// Dipakai untuk Esc berlapis: Esc#1 nonaktifkan input, Esc#2 hapus
    /// teks filter (popup tetap terbuka), Esc#3 baru menutup popup.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_cursor = 0;
        let n = self.visible_len();
        let cur = self.list_state.selected().unwrap_or(0);
        self.list_state.select(if n == 0 { None } else { Some(cur.min(n - 1)) });
    }

    /// Seleksi entry dengan path persis `path` — dipakai setelah kembali ke
    /// direktori parent agar highlight berada di direktori yang barusan
    /// ditinggalkan (konsisten dengan perilaku `restore_highlight` main pane).
    /// Bila entry tidak terlihat (mis. tersembunyi oleh filter aktif),
    /// seleksi dibiarkan pada posisi default dari `reload()`.
    pub fn select_path(&mut self, path: &Path) {
        let pos = self
            .visible_indices()
            .iter()
            .position(|&i| self.entries[i].path == path);
        if let Some(p) = pos {
            self.list_state.select(Some(p));
        }
    }

    /// Index ke `self.entries` yang cocok dengan `filter` (semua bila kosong).
    pub fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let q = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn visible_len(&self) -> usize {
        self.visible_indices().len()
    }

    /// Entry yang sedang di-highlight (lewat daftar terfilter).
    ///
    /// Catatan: `list_state.selected()` adalah index ke dalam
    /// `visible_indices()`, bukan ke `entries`.
    pub fn selected(&self) -> Option<&LocalEntry> {
        let vis = self.visible_indices();
        self.list_state
            .selected()
            .and_then(|i| vis.get(i))
            .and_then(|idx| self.entries.get(*idx))
    }

    pub fn filter_insert(&mut self, c: char) {
        text_insert(&mut self.filter, &mut self.filter_cursor, c);
        self.clamp_selection();
    }

    pub fn filter_backspace(&mut self) {
        text_backspace(&mut self.filter, &mut self.filter_cursor);
        self.clamp_selection();
    }

    pub fn filter_move_left(&mut self) {
        text_left(&self.filter, &mut self.filter_cursor);
    }

    pub fn filter_move_right(&mut self) {
        text_right(&self.filter, &mut self.filter_cursor);
    }


    /// Jaga `list_state.selected()` selalu di dalam rentang hasil filter.
    pub fn clamp_selection(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            self.list_state.select(None);
        } else {
            let cur = self.list_state.selected().unwrap_or(0);
            self.list_state.select(Some(cur.min(n - 1)));
        }
    }

    /// Tandai/lepas tanda file (folder dilewati).
    pub fn toggle_mark(&mut self) {
        let Some(e) = self.selected() else { return };
        if e.is_dir {
            return;
        }
        let path = e.path.clone();
        if let Some(pos) = self.marked.iter().position(|p| *p == path) {
            self.marked.remove(pos);
        } else if self.multi {
            self.marked.push(path);
        } else {
            self.marked = vec![path];
        }
    }

    /// File yang akan diunggah: semua yang ditandai; bila tidak ada yang
    /// ditandai, pakai file yang sedang di-highlight (bila memang file).
    pub fn upload_targets(&self) -> Vec<PathBuf> {
        if !self.marked.is_empty() {
            return self.marked.clone();
        }
        self.selected()
            .filter(|e| !e.is_dir)
            .map(|e| vec![e.path.clone()])
            .unwrap_or_default()
    }
}

pub fn load_local_entries(dir: &Path) -> Vec<LocalEntry> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let md = e.metadata().ok();
            let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = md.map(|m| m.len()).unwrap_or(0);
            v.push(LocalEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                path: e.path(),
                is_dir,
                size,
            });
        }
    }
    v.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
    v
}

pub enum Mode {
    Normal,
    AddMenu {
        selected: usize,
    },
    NewFolder(TextInput),
    FilePicker(FilePickerState),
    DeleteConfirm {
        targets: Vec<DeleteTarget>,
        selected: usize,
    },
    ConnForm(FormState),
    ConnMenu {
        conn_idx: usize,
        selected: usize,
    },
    DeleteConnConfirm {
        conn_idx: usize,
    },
    DownloadConfirm {
        items: Vec<DownloadItem>,
        dest_dir: PathBuf,
        /// Fokus di field path (bukan editor teks - fokus saja).
        field_active: bool,
        /// 0 = tombol Download, 1 = Cancel.
        selected: usize,
        listing_state: ListState,
        /// File picker untuk memilih folder tujuan.
        picker: Option<FilePickerState>,
        error: Option<String>,
    },
    Help,
}

#[derive(Clone, Copy)]
pub enum SidebarRow {
    Conn(usize),
    Bucket(usize, usize),
    Loading,
    Empty,
    // sudah tidak dipakai sebagai baris list (New Connection kini footer statis);
    // variant dipertahankan karena ui.rs masih me-referensinya.
    #[allow(dead_code)]
    NewConnection,
}

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "json", "toml", "yaml", "yml", "rs", "py", "js", "ts", "jsx", "tsx", "html",
    "css", "xml", "csv", "log", "sh", "bash", "zsh", "sql", "go", "java", "c", "h", "cpp", "hpp",
    "ini", "cfg", "conf", "env", "lock", "dockerfile", "gitignore", "tf", "proto",
];
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
const MAX_TEXT_PREVIEW: u64 = 64 * 1024; // 64KB
const MAX_IMAGE_PREVIEW: i64 = 25 * 1024 * 1024; // 25MB

/// Batas waktu satu permintaan listing (bucket / objects). Tanpa ini, server
/// yang tidak merespons akan membuat flag `loading` nyangkut selamanya.
const LISTING_TIMEOUT: Duration = Duration::from_secs(20);

/// Label operasi berat yang sedang berjalan (lihat [`App::busy`]).
const BUSY_COPY_MOVE: &str = "Copy/Move";
const BUSY_DELETE: &str = "Delete";
const BUSY_UPLOAD: &str = "Upload";
const BUSY_DOWNLOAD: &str = "Download";

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub should_quit: bool,
    pub config: Config,
    pub config_path: PathBuf,
    pub nodes: Vec<ConnNode>,
    pub sidebar_state: ListState,
    pub focus: Focus,
    pub location: Option<Location>,
    pub items: Vec<S3Item>,
    pub main_state: ListState,
    /// Fitur find/filter (`/`): state milik pane yang sedang fokus.
    pub find: FindState,
    /// Filter khusus sidebar (terpisah dari filter main/preview pane).
    pub find_sidebar: FindState,
    /// Filter khusus main pane.
    pub find_main: FindState,
    /// Filter khusus preview pane.
    pub find_preview: FindState,
    /// Key item yang harus di-highlight setelah listing berikutnya dimuat
    /// (dipakai `go_back()` agar highlight kembali ke folder yang barusan dibuka).
    pub restore_highlight: Option<String>,
    pub loading: bool,
    pub marked: HashSet<String>,
    pub visual: bool,
    pub clipboard: Option<Clipboard>,
    pub preview: PreviewState,
    pub preview_scroll: u16,
    pub preview_list_state: ListState,
    pub zoom: Option<Focus>,
    pub needs_clear: bool,
    pub icon_style: crate::theme::IconStyle,
    pub mode: Mode,
    pub status: String,
    pub status_error: bool,
    /// Progress download untuk status bar, mis. "Downloading 3/12 a.png (2.4 MB)".
    pub download_status: Option<String>,
    pub download_active: bool,
    /// Label operasi berat yang sedang berjalan (`None` = idle).
    /// Dipakai [`App::ensure_idle`] untuk mencegah aksi S3 bertumpuk, dan
    /// direset di handler `AppEvent::OpDone`.
    pub busy: Option<&'static str>,
    clients: HashMap<usize, S3Client>,
    tx: mpsc::UnboundedSender<AppEvent>,
    rx: mpsc::UnboundedReceiver<AppEvent>,
    req_counter: u64,
    current_listing_req: u64,
    current_preview_req: u64,
    current_delete_req: u64,
    preview_due: Option<Instant>,
    pub picker: Picker,
    pub sidebar_area: Rect,
    pub main_area: Rect,
    pub preview_area: Rect,
    pub newconn_area: Rect, // footer statis sidebar, diset oleh ui.rs saat draw
}

impl App {
    pub fn new() -> Result<Self> {
        let (config, config_path) = config::load()?;
        let nodes = config
            .connections
            .iter()
            .cloned()
            .map(|conn| ConnNode {
                conn,
                expanded: false,
                mounted: false,
                buckets: None,
                loading: false,
                req_id: 0,
            })
            .collect();
        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(0));
        let picker = make_picker();
        let icon_style = config.icon_style;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(App {
            should_quit: false,
            config,
            config_path,
            nodes,
            sidebar_state,
            focus: Focus::Sidebar,
            location: None,
            items: vec![],
            main_state: ListState::default(),
            // fokus awal = Sidebar, jadi state find aktif mengikuti sidebar
            find: FindState {
                scope: FindScope::Sidebar,
                ..FindState::default()
            },
            find_sidebar: FindState {
                scope: FindScope::Sidebar,
                ..FindState::default()
            },
            find_main: FindState {
                scope: FindScope::Main,
                ..FindState::default()
            },
            find_preview: FindState {
                scope: FindScope::Preview,
                ..FindState::default()
            },
            restore_highlight: None,
            loading: false,
            marked: HashSet::new(),
            visual: false,
            clipboard: None,
            preview: PreviewState::Empty,
            preview_scroll: 0,
            preview_list_state: ListState::default(),
            zoom: None,
            needs_clear: false,
            icon_style,
            mode: Mode::Normal,
            status: String::new(),
            status_error: false,
            download_status: None,
            download_active: false,
            busy: None,
            clients: HashMap::new(),
            tx,
            rx,
            req_counter: 0,
            current_listing_req: 0,
            current_preview_req: 0,
            current_delete_req: 0,
            preview_due: None,
            picker,
            sidebar_area: Rect::default(),
            main_area: Rect::default(),
            preview_area: Rect::default(),
            newconn_area: Rect::default(),
        })
    }

    fn client(&mut self, conn_idx: usize) -> Option<S3Client> {
        if let Some(c) = self.clients.get(&conn_idx) {
            return Some(c.clone());
        }
        let conn = self.nodes.get(conn_idx)?.conn.clone();
        let c = S3Client::new(&conn);
        self.clients.insert(conn_idx, c.clone());
        Some(c)
    }

    /// Ganti gaya icon/border (nerd -> unicode -> ascii), lalu simpan ke config.
    pub fn cycle_icon_style(&mut self) {
        self.icon_style = self.icon_style.next();
        self.config.icon_style = self.icon_style;
        let msg = match crate::config::save(&self.config, &self.config_path) {
            Ok(_) => format!("Tampilan: {} (tersimpan)", self.icon_style.label()),
            Err(e) => format!("Tampilan: {} (gagal simpan: {e})", self.icon_style.label()),
        };
        self.needs_clear = true;
        self.set_status(msg);
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_error = false;
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_error = true;
    }

    /// `true` bila tidak ada operasi berat yang sedang berjalan.
    ///
    /// Operasi S3 (copy/move, delete, upload, download) berjalan di background
    /// dan memakai state bersama (clipboard, marked, listing, status), jadi
    /// aksi baru saat masih ada operasi berjalan harus ditolak. Saat menolak,
    /// pesan error langsung dipasang agar user tahu alasannya.
    pub fn ensure_idle(&mut self) -> bool {
        if let Some(op) = self.busy {
            self.set_error(format!("{op} sedang berjalan, tunggu sampai selesai"));
            return false;
        }
        true
    }

    fn next_req(&mut self) -> u64 {
        self.req_counter += 1;
        self.req_counter
    }

    // -----------------------------------------------------------------------
    // Main loop
    // -----------------------------------------------------------------------

    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(80));
        while !self.should_quit {
            if self.needs_clear {
                self.needs_clear = false;
                let _ = terminal.clear();
            }
            terminal.draw(|f| ui::draw(f, self))?;
            tokio::select! {
                maybe = events.next() => {
                    if let Some(Ok(ev)) = maybe {
                        self.handle_event(ev).await;
                    }
                }
                Some(msg) = self.rx.recv() => self.handle_app_event(msg).await,
                _ = tick.tick() => self.on_tick().await,
            }
        }
        Ok(())
    }

    async fn on_tick(&mut self) {
        if let Some(due) = self.preview_due {
            if Instant::now() >= due {
                self.preview_due = None;
                self.request_preview();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Input dispatch
    // -----------------------------------------------------------------------

    async fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    self.should_quit = true;
                    return;
                }
                // Ctrl+Z: zoom/unzoom pane yang sedang fokus (hanya di mode normal)
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
                    if matches!(self.mode, Mode::Normal) {
                        self.zoom = if self.zoom == Some(self.focus) {
                            None
                        } else {
                            Some(self.focus)
                        };
                        // layout berubah drastis -> paksa redraw penuh
                        self.needs_clear = true;
                    }
                    return;
                }
                if matches!(self.mode, Mode::Normal) {
                    self.handle_normal_key(key).await;
                } else {
                    self.handle_popup_key(key).await;
                }
            }
            Event::Resize(_, _) => {
                // Paksa redraw penuh: sisa frame lama bisa tertinggal bila lebar glyph
                // dihitung berbeda oleh terminal (lihat catatan di ui.rs).
                self.needs_clear = true;
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.handle_mouse(m.column, m.row).await;
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    self.handle_right_click(m.column, m.row).await;
                }
                MouseEventKind::ScrollUp => self.handle_wheel(m.column, m.row, -3),
                MouseEventKind::ScrollDown => self.handle_wheel(m.column, m.row, 3),
                _ => {}
            },
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Normal-mode keys
    // -----------------------------------------------------------------------

    async fn handle_normal_key(&mut self, key: KeyEvent) {
        // Mode input find menelan hampir semua key (kecuali Up/Down untuk navigasi).
        if self.find.active && self.handle_find_key(key) {
            return;
        }
        let was_esc = key.code == KeyCode::Esc;
        match key.code {
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_icon_style()
            }
            KeyCode::Char('q') => {
                // jangan keluar di tengah operasi background: state S3 masih dipakai
                if self.busy.is_some() {
                    self.set_error("Operasi sedang berjalan…");
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/')
                if matches!(self.focus, Focus::Sidebar | Focus::Main | Focus::Preview) =>
            {
                // Aktifkan input find untuk PANE YANG SEDANG FOKUS saja.
                self.sync_find_scope();
                self.find.active = true;
                self.find.cursor = self.find.query.len();
            }
            KeyCode::Tab => {
                self.store_find_scope();
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Main,
                    Focus::Main => Focus::Preview,
                    Focus::Preview => Focus::Sidebar,
                };
                self.sync_find_scope();
            }
            KeyCode::BackTab => {
                self.store_find_scope();
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Preview,
                    Focus::Preview => Focus::Main,
                    Focus::Main => Focus::Sidebar,
                };
                self.sync_find_scope();
            }
            KeyCode::Char('n') | KeyCode::Char('N') => self.open_conn_form(None),
            _ => match self.focus {
                Focus::Sidebar => self.handle_sidebar_key(key).await,
                Focus::Main => self.handle_main_key(key).await,
                Focus::Preview => self.handle_preview_key(key),
            },
        }
        // Fallback terakhir untuk Esc: reset filter pane yang sedang fokus saja.
        // (Esc pertama menonaktifkan input; Esc berikutnya menghapus query.)
        if was_esc && !self.find.active && !self.find.query.is_empty() {
            self.clear_find();
        }
    }

    /// Tangani key saat input find aktif. `true` = sudah dikonsumsi.
    ///
    /// Up/Down sengaja diteruskan ke navigasi pane, dan Tab/BackTab diteruskan
    /// agar tetap berpindah pane (beserta scope find) walau sedang mengetik.
    fn handle_find_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up | KeyCode::Down => false,
            // Tab/BackTab tidak ditelan: biar handler normal memindah pane
            // sekaligus store+sync scope find.
            KeyCode::Tab | KeyCode::BackTab => false,
            KeyCode::Esc | KeyCode::Enter => {
                self.find.active = false;
                self.store_find_scope();
                true
            }
            KeyCode::Backspace => {
                self.find.backspace();
                self.store_find_scope();
                self.on_find_changed();
                true
            }
            KeyCode::Left => {
                self.find.move_left();
                true
            }
            KeyCode::Right => {
                self.find.move_right();
                true
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.find.insert(c);
                self.store_find_scope();
                self.on_find_changed();
                true
            }
            _ => true,
        }
    }

    /// Salin state `find` ke slot milik pane yang sedang fokus (atau sebaliknya)
    /// agar filter tiap pane tetap terpisah.
    ///
    /// Bila focus berpindah pane, state pane lama disimpan ke slotnya lalu slot
    /// pane baru dimuat. Status `active` dipertahankan agar mengetik langsung
    /// berlanjut ke query milik pane baru (mis. `/` lalu Tab).
    fn sync_find_scope(&mut self) {
        let target = FindScope::for_focus(self.focus);
        if self.find.scope != target {
            // Simpan state pane lama ke slotnya, lalu ambil slot pane baru.
            self.store_find_scope();
            let keep_active = self.find.active;
            let prev = self.find.clone();
            self.find = match target {
                FindScope::Sidebar => self.find_sidebar.clone(),
                FindScope::Main => self.find_main.clone(),
                FindScope::Preview => self.find_preview.clone(),
            };
            self.find.scope = target;
            self.find.active = keep_active;
            // Bawa query pane sebelumnya HANYA bila pane tujuan belum punya query
            // sendiri. Bila pane tujuan sudah punya query, gunakan miliknya
            // (tiap pane tetap menyimpan salinannya masing-masing).
            if keep_active && prev.active && self.find.query.is_empty() {
                self.find.query = prev.query.clone();
            }
            self.find.cursor = self.find.query.len();
        }
    }

    /// Simpan state `find` kembali ke slot milik pane-nya.
    fn store_find_scope(&mut self) {
        match self.find.scope {
            FindScope::Sidebar => self.find_sidebar = self.find.clone(),
            FindScope::Main => self.find_main = self.find.clone(),
            FindScope::Preview => self.find_preview = self.find.clone(),
        }
    }

    /// Query yang sedang berlaku untuk pane yang fokus (dipakai search bar global).
    #[allow(dead_code)] // dipakai ui.rs untuk search bar global
    pub fn active_query(&self) -> &str {
        &self.find.query
    }

    /// `true` bila input search sedang aktif.
    #[allow(dead_code)] // dipakai ui.rs untuk search bar global
    pub fn search_active(&self) -> bool {
        self.find.active
    }

    /// Query untuk highlight di preview pane (milik scope Preview).
    #[allow(dead_code)] // dipakai ui.rs untuk highlight preview
    pub fn preview_query(&self) -> &str {
        &self.find_preview.query
    }

    /// Reset filter DAN kembalikan highlight ke rentang yang valid.
    /// Hapus filter di SEMUA pane sekaligus (sidebar/main/preview), lalu
    /// reset seleksi agar tetap valid.
    fn clear_find(&mut self) {
        self.find.clear();
        self.find_sidebar.clear();
        self.find_main.clear();
        self.find_preview.clear();
        self.store_find_scope();
        self.on_find_changed();
    }

    /// Dipanggil setiap query filter berubah: jaga seleksi tetap valid.
    fn on_find_changed(&mut self) {
        let rows = self.sidebar_rows().len();
        let sel = self.sidebar_state.selected().unwrap_or(0);
        if rows == 0 {
            self.sidebar_state.select(None);
        } else {
            self.sidebar_state.select(Some(sel.min(rows - 1)));
        }
        let n = self.visible_items().len();
        if n == 0 {
            self.main_state.select(None);
        } else {
            let cur = self.main_state.selected().unwrap_or(0);
            self.main_state.select(Some(cur.min(n - 1)));
        }
        self.schedule_preview();
    }

    async fn handle_sidebar_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.sidebar_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.sidebar_move(1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                self.sidebar_activate().await;
            }
            KeyCode::Left | KeyCode::Char('h') => self.sidebar_collapse(),
            KeyCode::Char(' ') => {
                if let Some(SidebarRow::Conn(ci)) = self.selected_sidebar_row() {
                    self.mode = Mode::ConnMenu { conn_idx: ci, selected: 0 };
                }
            }
            KeyCode::Char('r') => {
                if let Some(SidebarRow::Conn(ci)) = self.selected_sidebar_row() {
                    if self.nodes.get(ci).map(|n| n.mounted).unwrap_or(false) {
                        self.fetch_buckets(ci, true);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_preview_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.preview_scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.preview_scroll_by(1),
            KeyCode::PageUp => self.preview_scroll_by(-10),
            KeyCode::PageDown => self.preview_scroll_by(10),
            _ => {}
        }
    }

    fn preview_scroll_by(&mut self, delta: isize) {
        match &mut self.preview {
            PreviewState::Listing(items) => {
                if items.is_empty() {
                    return;
                }
                let cur = self.preview_list_state.selected().unwrap_or(0) as isize;
                let next = (cur + delta).rem_euclid(items.len() as isize) as usize;
                self.preview_list_state.select(Some(next));
            }
            _ => {
                let cur = self.preview_scroll as isize;
                // clamp ke rentang u16 supaya cast tidak wrap/truncate
                self.preview_scroll = (cur + delta).clamp(0, u16::MAX as isize) as u16;
            }
        }
    }

    async fn handle_main_key(&mut self, key: KeyEvent) {
        if self.location.is_none() {
            self.set_status("Pilih bucket dulu dari sidebar (Enter/klik)");
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.main_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.main_move(1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.open_selected_dir(),
            KeyCode::Left | KeyCode::Char('h') => self.go_back(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('v') => self.toggle_visual(),
            KeyCode::Char('V') => {
                self.marked.clear();
                self.visual = false;
                self.set_status("Semua penanda dihapus");
            }
            KeyCode::Char('c') => self.start_copy_move(false),
            KeyCode::Char('x') => self.start_copy_move(true),
            KeyCode::Char('p') => self.paste(),
            KeyCode::Char('a') => self.mode = Mode::AddMenu { selected: 0 },
            KeyCode::Char('d') => self.start_download(),
            KeyCode::Char('D') => self.start_delete(),
            KeyCode::Char('r') => self.request_listing(),
            KeyCode::Esc => {
                self.visual = false;
                if self.clipboard.is_some() {
                    self.clipboard = None;
                    self.set_status("Clipboard dibatalkan");
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar logic
    // -----------------------------------------------------------------------

    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let mut rows = vec![];
        for (ci, node) in self.nodes.iter().enumerate() {
            rows.push(SidebarRow::Conn(ci));
            if node.mounted && node.expanded {
                if node.loading {
                    rows.push(SidebarRow::Loading);
                } else if let Some(b) = &node.buckets {
                    // baris koneksi selalu tampil; bucket disaring oleh query find
                    let hits: Vec<usize> = (0..b.len())
                        .filter(|bi| self.bucket_matches(&b[*bi]))
                        .collect();
                    if hits.is_empty() {
                        rows.push(SidebarRow::Empty);
                    } else {
                        for bi in hits {
                            rows.push(SidebarRow::Bucket(ci, bi));
                        }
                    }
                }
            }
        }
        rows
    }

    /// Apakah nama bucket lolos filter find yang aktif.
    pub fn bucket_matches(&self, name: &str) -> bool {
        // Hanya filter yang memang milik sidebar yang boleh menyaring bucket.
        if !self.find_sidebar.applies_to_sidebar() {
            return true;
        }
        self.find_sidebar.matches(name)
    }

    fn selected_sidebar_row(&self) -> Option<SidebarRow> {
        let rows = self.sidebar_rows();
        self.sidebar_state.selected().and_then(|i| rows.get(i).copied())
    }

    fn sidebar_move(&mut self, delta: isize) {
        let rows = self.sidebar_rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.sidebar_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(rows.len() as isize) as usize;
        self.sidebar_state.select(Some(next));
    }

    async fn sidebar_activate(&mut self) {
        match self.selected_sidebar_row() {
            Some(SidebarRow::Conn(ci)) => {
                if !self.nodes.get(ci).map(|n| n.mounted).unwrap_or(false) {
                    // belum ter-mount -> mount (sekaligus expand bila sukses)
                    self.mount_conn(ci);
                } else {
                    self.toggle_expand(ci);
                }
            }
            Some(SidebarRow::Bucket(ci, bi)) => {
                let bucket = match self.nodes.get(ci).and_then(|n| n.buckets.as_ref()).and_then(|b| b.get(bi)) {
                    Some(b) => b.clone(),
                    None => return,
                };
                self.open_bucket(ci, bucket);
            }
            _ => {}
        }
    }

    fn sidebar_collapse(&mut self) {
        match self.selected_sidebar_row() {
            Some(SidebarRow::Conn(ci)) => {
                if let Some(n) = self.nodes.get_mut(ci) {
                    n.expanded = false;
                }
            }
            Some(SidebarRow::Bucket(ci, _)) => {
                if let Some(n) = self.nodes.get_mut(ci) {
                    n.expanded = false;
                }
                // move selection onto the parent connection row
                let rows = self.sidebar_rows();
                if let Some(pos) = rows.iter().position(|r| matches!(r, SidebarRow::Conn(c) if *c == ci)) {
                    self.sidebar_state.select(Some(pos));
                }
            }
            _ => {}
        }
    }

    fn toggle_expand(&mut self, ci: usize) {
        let needs_fetch = {
            let node = match self.nodes.get_mut(ci) {
                Some(n) => n,
                None => return,
            };
            if !node.mounted {
                return;
            }
            node.expanded = !node.expanded;
            node.expanded && node.buckets.is_none() && !node.loading
        };
        if needs_fetch {
            self.fetch_buckets(ci, false);
        }
    }

    /// Mount = bangun client + fetch bucket. Sukses -> mounted+expanded.
    fn mount_conn(&mut self, ci: usize) {
        if self.nodes.get(ci).map(|n| n.loading).unwrap_or(true) {
            return;
        }
        // client dibangun DULU: bila gagal, flag `loading` tidak boleh nyangkut
        let Some(client) = self.client(ci) else {
            self.set_error("Client S3 tidak tersedia");
            return;
        };
        let req_id = self.next_req();
        if let Some(node) = self.nodes.get_mut(ci) {
            node.loading = true;
            node.expanded = true;
            node.req_id = req_id;
        }
        let tx = self.tx.clone();
        let name = self.nodes.get(ci).map(|n| n.conn.name.clone()).unwrap_or_default();
        self.set_status(format!("Mounting {name}..."));
        tokio::spawn(async move {
            let result = match tokio::time::timeout(LISTING_TIMEOUT, client.list_buckets()).await {
                Ok(r) => r,
                Err(_) => Err("timeout".to_string()),
            };
            let _ = tx.send(AppEvent::BucketsLoaded {
                conn_idx: ci,
                req_id,
                result,
                is_mount: true,
            });
        });
    }

    fn unmount_conn(&mut self, ci: usize) {
        if let Some(node) = self.nodes.get_mut(ci) {
            node.mounted = false;
            node.expanded = false;
            node.buckets = None;
            node.loading = false;
        }
        self.clients.remove(&ci);
        // invalidasi semua request async yang masih berjalan
        self.current_listing_req = self.next_req();
        self.current_preview_req = self.next_req();
        self.current_delete_req = self.next_req();
        self.preview_due = None;
        self.marked.clear();
        // kalau sedang browsing koneksi ini, kembali ke keadaan awal
        if self.location.as_ref().map(|l| l.conn_idx) == Some(ci) {
            self.location = None;
            self.items = vec![];
            self.main_state.select(None);
            self.preview = PreviewState::Empty;
            self.clipboard = None;
        }
        // jaga seleksi sidebar tetap valid setelah rows menyusut
        let rows = self.sidebar_rows();
        let sel = self.sidebar_state.selected().unwrap_or(0);
        self.sidebar_state.select(Some(sel.min(rows.len().saturating_sub(1))));
        let name = self.nodes.get(ci).map(|n| n.conn.name.clone()).unwrap_or_default();
        self.set_status(format!("{name}: unmounted"));
    }

    fn fetch_buckets(&mut self, ci: usize, force: bool) {
        if let Some(node) = self.nodes.get(ci) {
            if node.loading || (!force && node.buckets.is_some()) {
                return;
            }
        } else {
            return;
        }
        // client dibangun DULU: bila gagal, flag `loading` tidak boleh nyangkut
        let Some(client) = self.client(ci) else {
            self.set_error("Client S3 tidak tersedia");
            return;
        };
        let req_id = self.next_req();
        if let Some(node) = self.nodes.get_mut(ci) {
            node.loading = true;
            node.buckets = None;
            node.req_id = req_id;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(LISTING_TIMEOUT, client.list_buckets()).await {
                Ok(r) => r,
                Err(_) => Err("timeout".to_string()),
            };
            let _ = tx.send(AppEvent::BucketsLoaded {
                conn_idx: ci,
                req_id,
                result,
                is_mount: false,
            });
        });
    }

    fn open_bucket(&mut self, conn_idx: usize, bucket: String) {
        self.location = Some(Location {
            conn_idx,
            bucket,
            prefix: String::new(),
        });
        self.restore_highlight = None;
        self.store_find_scope();
        self.focus = Focus::Main;
        self.sync_find_scope();
        self.marked.clear();
        self.visual = false;
        self.request_listing();
    }

    // -----------------------------------------------------------------------
    // Main pane logic
    // -----------------------------------------------------------------------

    pub fn request_listing(&mut self) {
        let Some(loc) = self.location.clone() else { return };
        let Some(client) = self.client(loc.conn_idx) else { return };
        self.loading = true;
        // jangan bawa highlight lama ke listing baru; pemanggil yang butuh
        // restore (mis. go_back) men-set-nya setelah fungsi ini
        self.restore_highlight = None;
        // kosongkan item agar aksi (d/c/x/space/klik) tidak mengenai data stale
        // dari direktori sebelumnya saat loading berlangsung
        self.items = vec![];
        self.main_state.select(None);
        self.preview = PreviewState::Empty;
        self.preview_due = None;
        // invalidasi response preview lama dari direktori sebelumnya
        self.current_preview_req = self.next_req();
        let id = self.next_req();
        self.current_listing_req = id;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(
                LISTING_TIMEOUT,
                client.list_objects(&loc.bucket, &loc.prefix, None),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err("timeout".to_string()),
            };
            let _ = tx.send(AppEvent::ListingLoaded { id, result });
        });
    }

    fn main_move(&mut self, delta: isize) {
        // navigasi memakai daftar hasil filter, bukan seluruh `items`
        let len = self.visible_items().len();
        if len == 0 {
            self.main_state.select(None);
            return;
        }
        let cur = self.main_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len as isize) as usize;
        self.main_state.select(Some(next));
        if self.visual {
            self.mark_current();
        }
        self.schedule_preview();
    }

    /// Index ke `self.items` yang lolos filter find (semua bila query kosong).
    pub fn visible_items(&self) -> Vec<usize> {
        // Hanya filter milik main pane yang boleh menyaring daftar file/folder.
        if !self.find_main.applies_to_main() {
            return (0..self.items.len()).collect();
        }
        let q = self.find_main.query.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    /// Item yang sedang di-highlight. `main_state.selected()` adalah index ke
    /// `visible_items()`, bukan langsung ke `self.items`.
    pub fn selected_item(&self) -> Option<&S3Item> {
        let vis = self.visible_items();
        self.main_state
            .selected()
            .and_then(|i| vis.get(i))
            .and_then(|idx| self.items.get(*idx))
    }

    fn open_selected_dir(&mut self) {
        let Some(item) = self.selected_item().cloned() else { return };
        if !item.is_dir {
            return;
        }
        self.enter_prefix(item.key);
    }

    fn enter_prefix(&mut self, prefix: String) {
        if let Some(loc) = &mut self.location {
            loc.prefix = prefix;
        }
        self.marked.clear();
        self.visual = false;
        self.restore_highlight = None;
        self.request_listing();
    }

    fn go_back(&mut self) {
        let Some(loc) = self.location.as_ref() else { return };
        if loc.prefix.is_empty() {
            // tetap di main pane - pindah pane hanya via Tab/klik
            self.set_status("Sudah di root bucket");
            return;
        }
        let cur = loc.prefix.trim_end_matches('/');
        let seg = cur.rsplit('/').next().unwrap_or(cur).to_string();
        let parent = match cur.rfind('/') {
            Some(i) => cur[..=i].to_string(),
            None => String::new(),
        };
        // setelah mundur, highlight harus berada pada folder yang barusan dibuka
        let restore = format!("{parent}{seg}/");
        if let Some(loc) = &mut self.location {
            loc.prefix = parent;
        }
        self.marked.clear();
        self.visual = false;
        self.request_listing();
        // set SETELAH request_listing (fungsi itu mereset restore_highlight)
        self.restore_highlight = Some(restore);
    }

    // -----------------------------------------------------------------------
    // Marking / clipboard
    // -----------------------------------------------------------------------

    fn toggle_mark(&mut self) {
        let Some(item) = self.selected_item().cloned() else { return };
        if !self.marked.remove(&item.key) {
            self.marked.insert(item.key);
        }
    }

    fn mark_current(&mut self) {
        if let Some(item) = self.selected_item() {
            self.marked.insert(item.key.clone());
        }
    }

    fn toggle_visual(&mut self) {
        if self.visual {
            self.visual = false;
            self.set_status(format!("{} item ditandai", self.marked.len()));
        } else {
            self.visual = true;
            self.mark_current();
            self.set_status("VISUAL: gerakkan Up/Down untuk menandai, v selesai, V batalkan semua");
        }
    }

    fn target_items(&self) -> Vec<S3Item> {
        if !self.marked.is_empty() {
            // hanya item yang MASIH ada di listing dan bertanda
            let marked: Vec<S3Item> = self
                .items
                .iter()
                .filter(|i| self.marked.contains(&i.key))
                .cloned()
                .collect();
            // bila semua tanda menunjuk item yang sudah tidak ada (mis. setelah
            // pindah folder), jatuh ke item yang sedang di-highlight
            if !marked.is_empty() {
                return marked;
            }
        }
        self.selected_item().cloned().into_iter().collect()
    }

    fn start_copy_move(&mut self, is_move: bool) {
        let verb = if is_move { "MOVE" } else { "COPY" };
        if let Some(cb) = &self.clipboard {
            if cb.is_move == is_move {
                self.clipboard = None;
                self.set_status(format!("Mode {verb} dibatalkan"));
                return;
            }
        }
        // jangan mulai copy/move saat operasi lain masih jalan
        if !self.ensure_idle() {
            return;
        }
        let items = self.target_items();
        if items.is_empty() {
            self.set_status("Tidak ada item yang dipilih");
            return;
        }
        let Some(src) = self.location.clone() else {
            self.set_error("Tidak ada bucket aktif");
            return;
        };
        let n = items.len();
        self.clipboard = Some(Clipboard { items, is_move, src });
        self.set_status(format!(
            "{verb}: {n} item - navigasi ke folder tujuan lalu tekan p, {key} lagi untuk batal",
            key = if is_move { "x" } else { "c" }
        ));
    }

    fn paste(&mut self) {
        if !self.ensure_idle() {
            return;
        }
        let Some(cb) = self.clipboard.clone() else {
            self.set_status("Clipboard kosong (tekan c/x dulu)");
            return;
        };
        let Some(loc) = self.location.clone() else {
            self.set_error("Tidak ada lokasi tujuan");
            return;
        };
        if loc.conn_idx != cb.src.conn_idx {
            self.set_error("Copy/move antar koneksi belum didukung");
            return;
        }
        let mut dest_prefix = loc.prefix.clone();
        if let Some(item) = self.selected_item() {
            if item.is_dir {
                dest_prefix = item.key.clone();
            }
        }
        if dest_prefix == cb.src.prefix && loc.bucket == cb.src.bucket {
            self.set_status("Sumber = tujuan, tidak ada yang dilakukan");
            return;
        }
        for it in &cb.items {
            // guard self-nesting hanya relevan dalam bucket yang sama
            if loc.bucket == cb.src.bucket && it.is_dir && dest_prefix.starts_with(&it.key) {
                self.set_error(format!("Tidak bisa menyalin folder ke dalam dirinya sendiri: {}", it.name));
                return;
            }
        }
        // clipboard di-keep di App; task membawa clone. Bila paste batal karena
        // kolisi/error cek, clipboard dikembalikan via OpDone.restore.
        self.clipboard = None;
        let Some(client) = self.client(loc.conn_idx) else { return };
        let dst_bucket = loc.bucket.clone();
        let src_bucket = cb.src.bucket.clone();
        let items = cb.items.clone();
        let is_move = cb.is_move;
        let tx = self.tx.clone();
        let verb = if is_move { "Moved" } else { "Copied" };
        self.busy = Some(BUSY_COPY_MOVE);
        self.set_status(format!("Memproses {} item...", items.len()));
        tokio::spawn(async move {
            let mut folder_keys: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            // cek tabrakan per-key di tujuan - jangan timpa diam-diam
            for item in &items {
                let dest_base = format!("{}{}", dest_prefix, item.name);
                if item.is_dir {
                    let src_keys = match client.list_all_keys(&src_bucket, &item.key).await {
                        Ok(k) => k,
                        Err(e) => {
                            let _ = tx.send(AppEvent::OpDone {
                                message: format!("Gagal list sumber {}: {e}", item.name),
                                is_error: true,
                                refresh: false,
                                restore: Some(cb.clone()),
                            });
                            return;
                        }
                    };
                    let dst_keys: std::collections::HashSet<String> =
                        match client.list_all_keys(&dst_bucket, &dest_base).await {
                            Ok(k) => k.into_iter().collect(),
                            Err(e) => {
                                let _ = tx.send(AppEvent::OpDone {
                                    message: format!("Gagal cek tujuan: {e}"),
                                    is_error: true,
                                    refresh: false,
                                    restore: Some(cb.clone()),
                                });
                                return;
                            }
                        };
                    // bandingkan per key relatif: src photos/1.jpg -> dest backup/photos/1.jpg
                    // strip_prefix (bukan slicing) agar key tak terduga tidak panic
                    let rel_keys: Vec<String> = match src_keys
                        .iter()
                        .map(|k| {
                            k.strip_prefix(item.key.as_str())
                                .map(|rel| format!("{dest_base}{rel}"))
                                .ok_or_else(|| format!("key tak terduga: {k}"))
                        })
                        .collect::<Result<Vec<String>, String>>()
                    {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx.send(AppEvent::OpDone {
                                message: e,
                                is_error: true,
                                refresh: false,
                                restore: Some(cb.clone()),
                            });
                            return;
                        }
                    };
                    let overlap: Vec<String> =
                        rel_keys.into_iter().filter(|dk| dst_keys.contains(dk)).collect();
                    if !overlap.is_empty() {
                        let _ = tx.send(AppEvent::OpDone {
                            message: format!(
                                "Batal: {} key sudah ada di tujuan, mis. {}",
                                overlap.len(),
                                overlap[0]
                            ),
                            is_error: true,
                            refresh: false,
                            restore: Some(cb.clone()),
                        });
                        return;
                    }
                    folder_keys.insert(item.key.clone(), src_keys);
                } else {
                    match client.exists(&dst_bucket, &dest_base).await {
                        Ok(true) => {
                            let _ = tx.send(AppEvent::OpDone {
                                message: format!("Batal: tujuan sudah ada: {dest_base}"),
                                is_error: true,
                                refresh: false,
                                restore: Some(cb.clone()),
                            });
                            return;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            let _ = tx.send(AppEvent::OpDone {
                                message: format!("Gagal cek tujuan: {e}"),
                                is_error: true,
                                refresh: false,
                                restore: Some(cb.clone()),
                            });
                            return;
                        }
                    }
                }
            }
            let mut total = 0usize;
            for item in &items {
                let dest_base = format!("{}{}", dest_prefix, item.name);
                let r: Result<(), String> = async {
                    if item.is_dir {
                        let src_keys = folder_keys
                            .get(&item.key)
                            .cloned()
                            .unwrap_or_default();
                        for k in &src_keys {
                            let rel = k
                                .strip_prefix(item.key.as_str())
                                .ok_or_else(|| format!("key tak terduga: {k}"))?
                                .to_string();
                            client
                                .copy_object(&dst_bucket, &src_bucket, k, &format!("{dest_base}{rel}"))
                                .await?;
                            total += 1;
                        }
                        if is_move {
                            client.delete_keys(&src_bucket, src_keys).await?;
                        }
                    } else {
                        client
                            .copy_object(&dst_bucket, &src_bucket, &item.key, &dest_base)
                            .await?;
                        total += 1;
                        if is_move {
                            client.delete_keys(&src_bucket, vec![item.key.clone()]).await?;
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(e) = r {
                    let _ = tx.send(AppEvent::OpDone {
                        message: format!("Gagal: {e}"),
                        is_error: true,
                        refresh: true,
                        restore: None,
                    });
                    return;
                }
            }
            let _ = tx.send(AppEvent::OpDone {
                message: format!("{verb} {total} object(s)"),
                is_error: false,
                refresh: true,
                restore: None,
            });
        });
    }

    // -----------------------------------------------------------------------
    // Download
    // -----------------------------------------------------------------------

    /// Kumpulkan target (marked bila ada, kalau tidak item ter-highlight) lalu
    /// buka popup konfirmasi download.
    fn start_download(&mut self) {
        let mut items: Vec<DownloadItem> = self
            .target_items()
            .into_iter()
            .map(|it| DownloadItem {
                name: it.name,
                key: it.key,
                is_dir: it.is_dir,
                size: it.size,
            })
            .collect();
        if items.is_empty() {
            self.set_status("Tidak ada item yang dipilih");
            return;
        }
        // dedup: file yang berada di dalam folder yang ikut terpilih dibuang,
        // karena folder sudah mewakili isinya.
        let dir_keys: Vec<String> = items
            .iter()
            .filter(|i| i.is_dir)
            .map(|i| i.key.clone())
            .collect();
        items.retain(|it| {
            !dir_keys
                .iter()
                .any(|k| *k != it.key && it.key.starts_with(k.as_str()))
        });
        if items.is_empty() {
            self.set_status("Tidak ada item yang dipilih");
            return;
        }
        let dest_dir = download_dir_default();
        let _ = std::fs::create_dir_all(&dest_dir);
        let mut listing_state = ListState::default();
        listing_state.select(Some(0));
        self.mode = Mode::DownloadConfirm {
            items,
            dest_dir,
            field_active: true,
            selected: 0,
            listing_state,
            picker: None,
            error: None,
        };
    }

    /// Eksekusi download di background lalu tutup popup.
    fn start_download_exec(&mut self, items: Vec<DownloadItem>, dest_dir: PathBuf) {
        if !self.ensure_idle() {
            return;
        }
        if items.is_empty() {
            self.set_status("Tidak ada item yang dipilih");
            return;
        }
        if !dest_dir.is_dir() {
            let mut listing_state = ListState::default();
            listing_state.select(Some(0));
            self.mode = Mode::DownloadConfirm {
                items,
                dest_dir,
                field_active: false,
                selected: 0,
                listing_state,
                picker: None,
                error: Some("Folder tujuan tidak ada".into()),
            };
            return;
        }
        let _ = std::fs::create_dir_all(&dest_dir);
        let Some(loc) = self.location.clone() else {
            self.set_error("Tidak ada bucket aktif");
            return;
        };
        let Some(client) = self.client(loc.conn_idx) else {
            self.set_error("Client S3 tidak tersedia");
            return;
        };
        let bucket = loc.bucket.clone();
        let tx = self.tx.clone();
        self.mode = Mode::Normal;
        self.download_active = true;
        self.busy = Some(BUSY_DOWNLOAD);

        if items.len() == 1 && !items[0].is_dir {
            // satu file -> unduh langsung (streaming)
            let it = items[0].clone();
            let path = crate::s3::unique_path(&dest_dir, &it.name);
            self.download_status = Some(format!("Mengunduh {}", it.name));
            tokio::spawn(async move {
                let label = format!("Mengunduh {}", it.name);
                let progress_tx = tx.clone();
                let mut last = Instant::now();
                let on_progress = move |done: u64| {
                    if last.elapsed() < Duration::from_millis(200) {
                        return;
                    }
                    last = Instant::now();
                    let _ = progress_tx.send(AppEvent::DownloadProgress {
                        text: format!("{label} ({})", human_size(Some(done as i64))),
                    });
                };
                let sink = match std::fs::File::create(&path) {
                    Ok(f) => BlockingFile(f),
                    Err(e) => {
                        let _ = tx.send(AppEvent::OpDone {
                            message: format!("Download gagal: {e}"),
                            is_error: true,
                            refresh: false,
                            restore: None,
                        });
                        return;
                    }
                };
                match client
                    .download_object_stream(&bucket, &it.key, sink, on_progress)
                    .await
                {
                    Ok(_) => {
                        let _ = tx.send(AppEvent::OpDone {
                            message: format!("Download selesai: {}", path.display()),
                            is_error: false,
                            refresh: false,
                            restore: None,
                        });
                    }
                    Err(e) => {
                        // jangan tinggalkan file parsial
                        let _ = std::fs::remove_file(&path);
                        let _ = tx.send(AppEvent::OpDone {
                            message: format!("Download gagal: {e}"),
                            is_error: true,
                            refresh: false,
                            restore: None,
                        });
                    }
                }
            });
        } else {
            // beberapa item / folder -> bungkus jadi satu zip
            let count = items.len();
            let zip_name = if count == 1 {
                format!("{}.zip", items[0].name.trim_end_matches('/'))
            } else {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("s3-download-{secs}.zip")
            };
            let path = crate::s3::unique_path(&dest_dir, &zip_name);
            let pairs: Vec<(String, String)> = items
                .iter()
                .map(|it| (it.key.clone(), it.name.trim_end_matches('/').to_string()))
                .collect();
            self.download_status = Some(format!("Membuat zip {count} item..."));
            tokio::spawn(async move {
                match crate::s3::zip_objects(&client, &bucket, &pairs, &path).await {
                    Ok(_) => {
                        let _ = tx.send(AppEvent::OpDone {
                            message: format!("Download selesai: {}", path.display()),
                            is_error: false,
                            refresh: false,
                            restore: None,
                        });
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&path);
                        let _ = tx.send(AppEvent::OpDone {
                            message: format!("Download gagal: {e}"),
                            is_error: true,
                            refresh: false,
                            restore: None,
                        });
                    }
                }
            });
        }
    }

    // -----------------------------------------------------------------------
    // Add / delete
    // -----------------------------------------------------------------------

    fn start_delete(&mut self) {
        if !self.ensure_idle() {
            return;
        }
        let items = self.target_items();
        if items.is_empty() {
            self.set_status("Tidak ada item yang dipilih");
            return;
        }
        let targets: Vec<DeleteTarget> = items
            .into_iter()
            .map(|item| DeleteTarget {
                loading: item.is_dir,
                keys: if item.is_dir {
                    vec![]
                } else {
                    vec![item.key.clone()]
                },
                error: None,
                item,
            })
            .collect();
        let has_dirs = targets.iter().any(|t| t.loading);
        // default fokus ke Cancel (lebih aman)
        self.mode = Mode::DeleteConfirm {
            targets: targets.clone(),
            selected: 1,
        };
        // selalu bump id: dialog ini men-invalidate response delete sebelumnya
        let id = self.next_req();
        self.current_delete_req = id;
        // kumpulkan isi folder di background
        if has_dirs {
            let Some(loc) = self.location.clone() else { return };
            let Some(client) = self.client(loc.conn_idx) else { return };
            let bucket = loc.bucket.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let mut out = Vec::new();
                for mut t in targets {
                    if t.item.is_dir {
                        match client.list_all_keys(&bucket, &t.item.key).await {
                            Ok(keys) => {
                                // folder "kosong" (hanya berisi marker) -> hapus
                                // marker-nya, kalau tidak tidak ada yang terhapus
                                t.keys = if keys.is_empty() {
                                    vec![t.item.key.clone()]
                                } else {
                                    keys
                                };
                                t.loading = false;
                            }
                            Err(e) => {
                                t.error = Some(e);
                                t.loading = false;
                            }
                        }
                    }
                    out.push(t);
                }
                let _ = tx.send(AppEvent::DeleteInfoReady { id, targets: out });
            });
        }
    }

    /// Jalankan konfirmasi delete bila semua target siap (tidak loading/error).
    /// Popup tetap terbuka (dengan fokus tombol yang sama) bila belum siap.
    fn try_confirm_delete(&mut self, targets: Vec<DeleteTarget>, selected: usize) {
        if targets.iter().any(|t| t.loading) {
            self.set_status("Menunggu perhitungan isi folder...");
            self.mode = Mode::DeleteConfirm { targets, selected };
        } else if targets.iter().any(|t| t.error.is_some()) {
            self.set_error("Ada folder gagal di-list; batal dengan Esc");
            self.mode = Mode::DeleteConfirm { targets, selected };
        } else {
            self.confirm_delete(targets);
        }
    }

    fn confirm_delete(&mut self, targets: Vec<DeleteTarget>) {
        let Some(loc) = self.location.clone() else { return };
        let Some(client) = self.client(loc.conn_idx) else { return };
        let mut all_keys: Vec<String> = vec![];
        for t in &targets {
            all_keys.extend(t.keys.iter().cloned());
        }
        if all_keys.is_empty() {
            self.set_status("Tidak ada yang dihapus");
            return;
        }
        let bucket = loc.bucket.clone();
        let tx = self.tx.clone();
        self.marked.clear();
        self.visual = false;
        // operasi hapus yang sebenarnya baru dimulai di sini (butuh OpDone
        // untuk melepas guard)
        self.busy = Some(BUSY_DELETE);
        tokio::spawn(async move {
            match client.delete_keys(&bucket, all_keys).await {
                Ok(n) => {
                    let _ = tx.send(AppEvent::OpDone {
                        message: format!("Terhapus {n} object(s)"),
                        is_error: false,
                        refresh: true,
                        restore: None,
                    });
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::OpDone {
                        message: format!("Gagal hapus: {e}"),
                        is_error: true,
                        refresh: true,
                        restore: None,
                    });
                }
            }
        });
        self.set_status("Menghapus...");
    }

    fn create_folder(&mut self, name: String) {
        let raw = name;
        let name = raw.trim().trim_matches('/').to_string();
        // validasi: nama dengan segmen kosong (a//b), "."/"..", atau backslash
        // akan menghasilkan key yang membingungkan/tidak valid
        let has_bad_segment = name
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
        if name.is_empty() || has_bad_segment || name.contains('\\') {
            let msg = if name.is_empty() {
                "Nama folder kosong".to_string()
            } else {
                format!("Nama folder tidak valid: {name}")
            };
            self.set_error(msg);
            // kembali ke input folder dengan isi tetap agar bisa diperbaiki
            let mut input = TextInput::new();
            input.value = raw.trim().to_string();
            input.cursor = input.value.len();
            self.mode = Mode::NewFolder(input);
            return;
        }
        let Some(loc) = self.location.clone() else {
            self.set_error("Tidak ada bucket aktif");
            return;
        };
        let Some(client) = self.client(loc.conn_idx) else {
            self.set_error("Client S3 tidak tersedia");
            return;
        };
        let key = format!("{}{}/", loc.prefix, name);
        let bucket = loc.bucket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.put_folder(&bucket, &key).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::OpDone {
                        message: format!("Folder dibuat: {key}"),
                        is_error: false,
                        refresh: true,
                        restore: None,
                    });
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::OpDone {
                        message: format!("Gagal buat folder: {e}"),
                        is_error: true,
                        refresh: false,
                        restore: None,
                    });
                }
            }
        });
    }

    fn upload_files(&mut self, paths: Vec<PathBuf>) {
        if !self.ensure_idle() {
            return;
        }
        let Some(loc) = self.location.clone() else {
            self.set_error("Pilih bucket dulu sebelum upload");
            return;
        };
        let Some(client) = self.client(loc.conn_idx) else {
            self.set_error("Client S3 tidak tersedia");
            return;
        };
        let bucket = loc.bucket.clone();
        let prefix = loc.prefix.clone();
        let tx = self.tx.clone();
        let n = paths.len();
        self.busy = Some(BUSY_UPLOAD);
        self.set_status(format!("Mengupload {n} file..."));
        tokio::spawn(async move {
            let mut done = 0usize;
            let mut failed: Vec<String> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            let mut conflicts: Vec<String> = Vec::new();
            for p in &paths {
                let Some(name) = p.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                    skipped.push(p.display().to_string());
                    continue;
                };
                // Pastikan filenya masih ada & terbaca sebelum mencoba upload.
                if let Err(e) = std::fs::metadata(p) {
                    skipped.push(format!("{name} ({e})"));
                    continue;
                }
                let key = format!("{prefix}{name}");
                // jangan timpa object yang sudah ada di tujuan
                match client.exists(&bucket, &key).await {
                    Ok(true) => {
                        conflicts.push(name.clone());
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        failed.push(format!("{name}: gagal cek tujuan: {e}"));
                        continue;
                    }
                }
                if let Err(e) = client.put_file(&bucket, &key, p).await {
                    failed.push(format!("{name}: {e}"));
                    continue; // lanjut ke file berikutnya
                }
                done += 1;
                let _ = tx.send(AppEvent::UploadProgress {
                    text: format!("Upload {done}/{n}: {name}"),
                });
            }
            let mut msg = format!("Terupload {done}/{n} file");
            let mut is_error = false;
            if !failed.is_empty() {
                is_error = true;
                msg.push_str(&format!(" | gagal: {}", failed.join("; ")));
            }
            if !skipped.is_empty() {
                msg.push_str(&format!(" | dilewati: {}", skipped.join("; ")));
            }
            if !conflicts.is_empty() {
                msg.push_str(&format!(
                    " | {} file dilewati (sudah ada): {}",
                    conflicts.len(),
                    conflicts.join(", ")
                ));
            }
            let _ = tx.send(AppEvent::OpDone {
                message: msg,
                is_error,
                refresh: true,
                restore: None,
            });
        });
    }

    // -----------------------------------------------------------------------
    // Add connection form
    // -----------------------------------------------------------------------

    fn open_conn_form(&mut self, editing: Option<usize>) {
        let mk = |v: &str| {
            let mut input = TextInput::new();
            input.value = v.to_string();
            input.cursor = v.len();
            input
        };
        let fields = match editing.and_then(|ci| self.nodes.get(ci)) {
            Some(node) => vec![
                ("Name".into(), mk(&node.conn.name), false),
                ("Endpoint URL".into(), mk(&node.conn.endpoint), false),
                ("Access Key".into(), mk(&node.conn.access_key), false),
                ("Secret Key".into(), mk(&node.conn.secret_key), true),
                ("Region (opsional)".into(), mk(node.conn.region.as_deref().unwrap_or("")), false),
                (
                    "Payload signing off? (y/n)".into(),
                    mk(if node.conn.disable_payload_signing { "y" } else { "n" }),
                    false,
                ),
            ],
            None => vec![
                ("Name".into(), TextInput::new(), false),
                ("Endpoint URL".into(), TextInput::new(), false),
                ("Access Key".into(), TextInput::new(), false),
                ("Secret Key".into(), TextInput::new(), true),
                ("Region (opsional)".into(), TextInput::new(), false),
                ("Payload signing off? (y/n)".into(), TextInput::new(), false),
            ],
        };
        self.mode = Mode::ConnForm(FormState {
            editing,
            fields,
            active: 0,
            testing: false,
            test_seq: 0,
            test_result: None,
        });
    }

    fn form_connection(&self, form: &FormState) -> Result<Connection, String> {
        let get = |i: usize| form.fields[i].1.value.trim().to_string();
        let name = get(0);
        let endpoint = get(1);
        let access_key = get(2);
        let secret_key = get(3);
        let region = get(4);
        if name.is_empty() || endpoint.is_empty() || access_key.is_empty() || secret_key.is_empty() {
            return Err("Name/Endpoint/Access Key/Secret Key wajib diisi".into());
        }
        let dup = self
            .nodes
            .iter()
            .enumerate()
            .any(|(i, n)| n.conn.name == name && form.editing != Some(i));
        if dup {
            return Err(format!("Koneksi '{name}' sudah ada"));
        }
        Ok(Connection {
            name,
            endpoint,
            access_key,
            secret_key,
            region: if region.is_empty() { None } else { Some(region) },
            disable_payload_signing: form.fields.get(5).map(|f| {
                let v = f.1.value.trim().to_ascii_lowercase();
                v == "y" || v == "yes" || v == "true"
            }).unwrap_or(false),
        })
    }

    fn test_connection(&mut self, form: &mut FormState) {
        match self.form_connection(form) {
            Ok(conn) => {
                form.testing = true;
                form.test_result = None;
                form.test_seq += 1;
                let test_seq = form.test_seq;
                let client = S3Client::new(&conn);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    // Beberapa server S3-compatible (Dell ECS/ViPR) menolak
                    // payload signing. Bila percobaan pertama gagal karena itu,
                    // ulangi sekali dengan payload signing dimatikan.
                    let mut note = String::new();
                    let mut result = client.list_buckets().await;
                    if let Err(e) = &result {
                        if e.contains("SignatureDoesNotMatch")
                            || e.contains("XAmzContentSHA256Mismatch")
                        {
                            let mut alt = conn.clone();
                            alt.disable_payload_signing = true;
                            let client2 = S3Client::new(&alt);
                            if let Ok(b) = client2.list_buckets().await {
                                note = format!(
                                    " | payload signing dimatikan otomatis (server menolak payload signing) - simpan dengan opsi ini: {}",
                                    b.len()
                                );
                                result = Ok(b);
                                // buang hasil pertama yang error
                            }
                        }
                    }
                    match result {
                        Ok(b) => {
                            let _ = tx.send(AppEvent::ConnTestResult {
                                test_seq,
                                ok: true,
                                message: format!(
                                    "Terhubung - {} bucket ditemukan{}",
                                    b.len(),
                                    note
                                ),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(AppEvent::ConnTestResult {
                                test_seq,
                                ok: false,
                                message: e,
                            });
                        }
                    }
                });
            }
            Err(e) => {
                form.test_result = Some((false, e));
            }
        }
    }

    fn submit_conn_form(&mut self, form: FormState) {
        let conn = match self.form_connection(&form) {
            Ok(c) => c,
            Err(e) => {
                // kembali ke form dengan pesan error
                self.mode = Mode::ConnForm(FormState {
                    test_result: Some((false, e)),
                    ..form
                });
                return;
            }
        };
        let name = conn.name.clone();
        // bangun config baru dulu; commit ke state hanya setelah save berhasil
        let mut new_cfg = self.config.clone();
        match form.editing {
            Some(ci) if ci < self.nodes.len() => {
                let old_name = self.nodes[ci].conn.name.clone();
                match new_cfg.connections.iter_mut().find(|c| c.name == old_name) {
                    Some(slot) => *slot = conn.clone(),
                    None => new_cfg.connections.push(conn.clone()),
                }
            }
            Some(_) => return,
            None => new_cfg.connections.push(conn.clone()),
        }
        if let Err(e) = config::save(&new_cfg, &self.config_path) {
            self.set_error(format!("Gagal simpan config: {e}"));
            return;
        }
        self.config = new_cfg;
        let ci = match form.editing {
            Some(ci) => {
                if let Some(node) = self.nodes.get_mut(ci) {
                    node.conn = conn;
                    node.mounted = false;
                    node.expanded = false;
                    node.buckets = None;
                    node.loading = false;
                }
                self.clients.remove(&ci);
                ci
            }
            None => {
                self.nodes.push(ConnNode {
                    conn,
                    expanded: false,
                    mounted: false,
                    buckets: None,
                    loading: false,
                    req_id: 0,
                });
                self.nodes.len() - 1
            }
        };
        self.set_status(format!("Koneksi '{name}' tersimpan - mounting..."));
        self.mount_conn(ci);
    }

    fn delete_connection(&mut self, ci: usize) {
        if ci >= self.nodes.len() {
            return;
        }
        let name = self.nodes[ci].conn.name.clone();
        let mut new_cfg = self.config.clone();
        new_cfg.connections.retain(|c| c.name != name);
        if let Err(e) = config::save(&new_cfg, &self.config_path) {
            self.set_error(format!("Gagal simpan config: {e}"));
            return;
        }
        self.config = new_cfg;
        self.nodes.remove(ci);
        // index client bergeser -> rebuild lazy
        self.clients.clear();
        // clipboard/marked mengacu koneksi by-index -> tidak valid lagi
        self.clipboard = None;
        self.marked.clear();
        self.visual = false;
        // invalidasi semua request async yang masih berjalan
        self.current_listing_req = self.next_req();
        self.current_preview_req = self.next_req();
        self.current_delete_req = self.next_req();
        self.preview_due = None;
        // sesuaikan location yang mengacu index koneksi
        let mut clear_loc = false;
        if let Some(loc) = &mut self.location {
            if loc.conn_idx == ci {
                clear_loc = true;
            } else if loc.conn_idx > ci {
                loc.conn_idx -= 1;
            }
        }
        if clear_loc {
            self.location = None;
            self.items = vec![];
            self.main_state.select(None);
            self.preview = PreviewState::Empty;
        }
        // jaga seleksi sidebar tetap valid
        let rows = self.sidebar_rows();
        let sel = self.sidebar_state.selected().unwrap_or(0);
        self.sidebar_state.select(Some(sel.min(rows.len().saturating_sub(1))));
        self.set_status(format!("Koneksi '{name}' dihapus"));
    }

    // -----------------------------------------------------------------------
    // Preview
    // -----------------------------------------------------------------------

    fn schedule_preview(&mut self) {
        // invalidasi response preview yang masih in-flight
        self.current_preview_req = self.next_req();
        self.preview_due = Some(Instant::now() + Duration::from_millis(250));
    }

    fn request_preview(&mut self) {
        let Some(loc) = self.location.clone() else {
            self.preview = PreviewState::Empty;
            return;
        };
        let Some(item) = self.selected_item().cloned() else {
            self.preview = PreviewState::Empty;
            return;
        };
        let Some(client) = self.client(loc.conn_idx) else { return };
        self.preview = PreviewState::Loading;
        let id = self.next_req();
        self.current_preview_req = id;
        let tx = self.tx.clone();
        let bucket = loc.bucket.clone();
        tokio::spawn(async move {
            let result = preview_item(&client, &bucket, &item).await;
            let _ = tx.send(AppEvent::PreviewLoaded { id, result });
        });
    }

    // -----------------------------------------------------------------------
    // Popup keys
    // -----------------------------------------------------------------------

    async fn handle_popup_key(&mut self, key: KeyEvent) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Help => {
                // any key closes
            }
            Mode::AddMenu { selected } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Down => {
                    self.mode = Mode::AddMenu {
                        selected: 1 - selected,
                    }
                }
                KeyCode::Enter => {
                    if selected == 0 {
                        self.mode = Mode::NewFolder(TextInput::new());
                    } else {
                        let start = std::env::current_dir().unwrap_or_else(|_| {
                            dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
                        });
                        self.mode = Mode::FilePicker(FilePickerState::new(start));
                    }
                }
                _ => self.mode = Mode::AddMenu { selected },
            },
            Mode::NewFolder(mut input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.create_folder(input.value.clone()),
                _ => {
                    input.handle(&key);
                    self.mode = Mode::NewFolder(input);
                }
            },
            Mode::FilePicker(mut fp) => {
                if picker_filter_key(&mut fp, &key) {
                    self.mode = Mode::FilePicker(fp);
                } else {
                    match key.code {
                        KeyCode::Char('/') => {
                            fp.filter_active = true;
                            fp.filter_cursor = fp.filter.len();
                            self.mode = Mode::FilePicker(fp);
                        }
                        KeyCode::Esc => {
                            // Esc berlapis: bila masih ada teks filter, hapus
                            // dulu dan popup TETAP terbuka; bila sudah kosong
                            // barulah popup tertutup (mode tidak dikembalikan).
                            if !fp.filter.is_empty() {
                                fp.clear_filter();
                                self.mode = Mode::FilePicker(fp);
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let n = fp.visible_len();
                            move_list(&mut fp.list_state, n, -1);
                            self.mode = Mode::FilePicker(fp);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let n = fp.visible_len();
                            move_list(&mut fp.list_state, n, 1);
                            self.mode = Mode::FilePicker(fp);
                        }
                        KeyCode::Char(' ') => {
                            fp.toggle_mark();
                            self.mode = Mode::FilePicker(fp);
                        }
                        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                            if let Some(parent) = fp.dir.parent().map(|p| p.to_path_buf()) {
                                let came_from = fp.dir.clone();
                                fp.dir = parent;
                                fp.reload();
                                fp.select_path(&came_from);
                            }
                            self.mode = Mode::FilePicker(fp);
                        }
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                            let action = match fp.selected() {
                                Some(e) if e.is_dir => Some(Ok(e.path.clone())),
                                Some(e) => Some(Err(e.path.clone())),
                                None => None,
                            };
                            match action {
                                Some(Ok(dir)) => {
                                    fp.dir = dir;
                                    fp.reload();
                                    self.mode = Mode::FilePicker(fp);
                                }
                                Some(Err(file)) => {
                                    let _ = file;
                                    // Enter pada file = unggah SEMUA file yang ditandai
                                    // (atau file ini bila belum ada yang ditandai).
                                    let paths = fp.upload_targets();
                                    if paths.is_empty() {
                                        self.mode = Mode::FilePicker(fp);
                                    } else {
                                        self.upload_files(paths);
                                    }
                                }
                                None => self.mode = Mode::FilePicker(fp),
                            }
                        }
                        _ => self.mode = Mode::FilePicker(fp),
                    }
                }
            }
            Mode::DeleteConfirm { targets, selected } => {
                let selected = selected.min(1);
                match key.code {
                    // pindah fokus antar tombol
                    KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Char('h')
                    | KeyCode::Char('l') => {
                        self.mode = Mode::DeleteConfirm {
                            targets,
                            selected: 1 - selected,
                        };
                    }
                    // Enter pada tombol Delete, atau Shift+Y = langsung hapus
                    KeyCode::Enter if selected == 0 => self.try_confirm_delete(targets, selected),
                    KeyCode::Char('Y') => self.try_confirm_delete(targets, selected),
                    // Enter pada tombol Cancel, n, atau Esc = batal
                    KeyCode::Enter | KeyCode::Char('n') | KeyCode::Esc => {
                        // dibatalkan: invalidasi response DeleteInfoReady yang masih jalan
                        self.current_delete_req = self.next_req();
                    }
                    // tombol lain tidak melakukan apa-apa (popup tetap terbuka)
                    _ => {
                        self.mode = Mode::DeleteConfirm { targets, selected };
                    }
                }
            }
            Mode::ConnForm(mut form) => {
                let last = form.fields.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {}
                    // Shift+Enter = Test Connection (butuh keyboard enhancement / kitty protocol;
                    // di terminal tanpa dukungan ini jadi Enter biasa = Mount)
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.test_connection(&mut form);
                        self.mode = Mode::ConnForm(form);
                    }
                    // Enter polos = Mount/simpan
                    KeyCode::Enter => self.submit_conn_form(form),
                    KeyCode::Up | KeyCode::BackTab => {
                        form.active = form.active.saturating_sub(1);
                        self.mode = Mode::ConnForm(form);
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        form.active = (form.active + 1).min(last);
                        self.mode = Mode::ConnForm(form);
                    }
                    _ => {
                        if form.active < form.fields.len() {
                            if let Some((_, input, _)) = form.fields.get_mut(form.active) {
                                input.handle(&key);
                            }
                            form.test_result = None; // invalidasi hasil test saat field berubah
                            // batalkan juga hasil test yang masih in-flight, agar
                            // status `testing` tidak nyangkut menunggu jawaban basi
                            form.testing = false;
                            form.test_seq += 1;
                        }
                        self.mode = Mode::ConnForm(form);
                    }
                }
            },
            Mode::ConnMenu { conn_idx, selected } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    self.mode = Mode::ConnMenu {
                        conn_idx,
                        selected: selected.saturating_sub(1),
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.mode = Mode::ConnMenu {
                        conn_idx,
                        selected: (selected + 1).min(2),
                    };
                }
                KeyCode::Enter => match selected {
                    0 => {
                        if self.nodes.get(conn_idx).map(|n| n.mounted).unwrap_or(false) {
                            self.unmount_conn(conn_idx);
                        } else {
                            self.mount_conn(conn_idx);
                        }
                    }
                    1 => self.open_conn_form(Some(conn_idx)),
                    _ => self.mode = Mode::DeleteConnConfirm { conn_idx },
                },
                _ => self.mode = Mode::ConnMenu { conn_idx, selected },
            },
            Mode::DeleteConnConfirm { conn_idx } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.delete_connection(conn_idx),
                _ => {}
            },
            Mode::DownloadConfirm {
                items,
                mut dest_dir,
                mut field_active,
                mut selected,
                mut listing_state,
                mut picker,
                error,
            } => {
                let mut close = false;
                let mut exec = false;
                if let Some(mut fp) = picker.take() {
                    // --- mode file picker (memilih folder tujuan) ---
                    if picker_filter_key(&mut fp, &key) {
                        picker = Some(fp);
                    } else {
                        match key.code {
                            KeyCode::Char('/') => {
                                fp.filter_active = true;
                                fp.filter_cursor = fp.filter.len();
                                picker = Some(fp);
                            }
                            KeyCode::Esc => {
                                if !fp.filter.is_empty() {
                                    // hapus teks filter dulu, picker tetap terbuka
                                    fp.clear_filter();
                                    picker = Some(fp);
                                } else {
                                    // tutup picker, dest_dir tidak berubah
                                    self.set_status(format!("Folder tujuan: {}", dest_dir.display()));
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                let n = fp.visible_len();
                                move_list(&mut fp.list_state, n, -1);
                                picker = Some(fp);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                let n = fp.visible_len();
                                move_list(&mut fp.list_state, n, 1);
                                picker = Some(fp);
                            }
                            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                                if let Some(parent) = fp.dir.parent().map(|p| p.to_path_buf()) {
                                    let came_from = fp.dir.clone();
                                    fp.dir = parent;
                                    fp.reload();
                                    fp.select_path(&came_from);
                                }
                                picker = Some(fp);
                            }
                            KeyCode::Right | KeyCode::Char('l') => {
                                if let Some(dir) = picker_selected_dir(&fp) {
                                    fp.dir = dir;
                                    fp.reload();
                                }
                                picker = Some(fp);
                            }
                            KeyCode::Enter => {
                                if let Some(dir) = picker_selected_dir(&fp) {
                                    dest_dir = dir;
                                    self.set_status(format!(
                                        "Folder tujuan: {}",
                                        dest_dir.display()
                                    ));
                                } else {
                                    // file: abaikan, picker tetap terbuka
                                    picker = Some(fp);
                                }
                            }
                            KeyCode::Char('s') => {
                                if let Some(dir) = picker_selected_dir(&fp) {
                                    dest_dir = dir;
                                    self.set_status(format!(
                                        "Folder tujuan: {}",
                                        dest_dir.display()
                                    ));
                                } else {
                                    picker = Some(fp);
                                }
                            }
                            _ => picker = Some(fp),
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => close = true,
                        KeyCode::Tab | KeyCode::BackTab => field_active = !field_active,
                        KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Char('h')
                        | KeyCode::Char('l') => selected = 1 - selected.min(1),
                        KeyCode::Up | KeyCode::Char('k') => {
                            move_list(&mut listing_state, items.len(), -1)
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            move_list(&mut listing_state, items.len(), 1)
                        }
                        KeyCode::Char('p') => {
                            picker = Some(FilePickerState::new_with_mode(dest_dir.clone(), false));
                            self.set_status("Pilih folder tujuan, Enter untuk memilih");
                        }
                        KeyCode::Enter => {
                            if selected.min(1) == 0 {
                                exec = true;
                            } else {
                                close = true;
                            }
                        }
                        KeyCode::Char('y') | KeyCode::Char('Y') => exec = true,
                        // tombol lain: popup tetap terbuka
                        _ => {}
                    }
                }
                if exec {
                    self.start_download_exec(items, dest_dir);
                } else if !close {
                    self.mode = Mode::DownloadConfirm {
                        items,
                        dest_dir,
                        field_active,
                        selected,
                        listing_state,
                        picker,
                        error,
                    };
                }
            }
            Mode::Normal => {}
        }
    }

    // -----------------------------------------------------------------------
    // Mouse
    // -----------------------------------------------------------------------

    async fn handle_mouse(&mut self, col: u16, row: u16) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        // footer statis "+ New Connection" di bawah sidebar
        if inside(col, row, self.newconn_area) {
            self.open_conn_form(None);
            return;
        }
        if inside(col, row, self.sidebar_area) {
            self.store_find_scope();
            self.focus = Focus::Sidebar;
            self.sync_find_scope();
            let inner_y = self.sidebar_area.y + 1;
            let inner_h = self.sidebar_area.height.saturating_sub(2);
            if row < inner_y || row >= inner_y + inner_h {
                return; // klik di border
            }
            let idx = (row - inner_y) as usize + self.sidebar_state.offset();
            let rows = self.sidebar_rows();
            if idx >= rows.len() {
                return;
            }
            self.sidebar_state.select(Some(idx));
            match rows[idx] {
                SidebarRow::Conn(ci) => {
                    if !self.nodes.get(ci).map(|n| n.mounted).unwrap_or(false) {
                        self.mount_conn(ci);
                    } else {
                        self.toggle_expand(ci);
                    }
                }
                SidebarRow::Bucket(ci, bi) => {
                    let bucket = match self
                        .nodes
                        .get(ci)
                        .and_then(|n| n.buckets.as_ref())
                        .and_then(|b| b.get(bi))
                    {
                        Some(b) => b.clone(),
                        None => return,
                    };
                    self.open_bucket(ci, bucket);
                }
                _ => {}
            }
        } else if inside(col, row, self.preview_area) {
            self.store_find_scope();
            self.focus = Focus::Preview;
            self.sync_find_scope();
        } else if inside(col, row, self.main_area) {
            self.store_find_scope();
            self.focus = Focus::Main;
            self.sync_find_scope();
            if self.loading || self.location.is_none() {
                return;
            }
            // baris yang terlihat = hasil filter, bukan seluruh `items`
            let vis = self.visible_items();
            if vis.is_empty() {
                return;
            }
            let inner_y = self.main_area.y + 1;
            let inner_h = self.main_area.height.saturating_sub(2);
            if row < inner_y || row >= inner_y + inner_h {
                return; // klik di border
            }
            let idx = (row - inner_y) as usize + self.main_state.offset();
            if idx >= vis.len() {
                return;
            }
            let was_selected = self.main_state.selected() == Some(idx);
            self.main_state.select(Some(idx));
            if was_selected {
                self.open_selected_dir();
            }
            self.schedule_preview();
        }
    }

    /// Klik kanan pada row koneksi di sidebar -> menu konteks.
    async fn handle_right_click(&mut self, col: u16, row: u16) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        if !inside(col, row, self.sidebar_area) {
            return;
        }
        self.store_find_scope();
        self.focus = Focus::Sidebar;
        self.sync_find_scope();
        let inner_y = self.sidebar_area.y + 1;
        // list hanya setinggi inner dikurangi footer statis (2 baris)
        let inner_h = self
            .sidebar_area
            .height
            .saturating_sub(2)
            .saturating_sub(2);
        if row < inner_y || row >= inner_y + inner_h {
            return;
        }
        let idx = (row - inner_y) as usize + self.sidebar_state.offset();
        let rows = self.sidebar_rows();
        if let Some(SidebarRow::Conn(ci)) = rows.get(idx).copied() {
            self.sidebar_state.select(Some(idx));
            self.mode = Mode::ConnMenu { conn_idx: ci, selected: 0 };
        }
    }

    /// Mouse wheel scroll per pane.
    fn handle_wheel(&mut self, col: u16, row: u16, delta: isize) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        if inside(col, row, self.sidebar_area) {
            self.sidebar_move(delta);
        } else if inside(col, row, self.main_area) {
            if self.location.is_some() {
                self.main_move(delta);
            }
        } else if inside(col, row, self.preview_area) {
            self.preview_scroll_by(delta);
        }
    }

    // -----------------------------------------------------------------------
    // Async results
    // -----------------------------------------------------------------------

    async fn handle_app_event(&mut self, msg: AppEvent) {
        match msg {
            AppEvent::BucketsLoaded {
                conn_idx,
                req_id,
                result,
                is_mount,
            } => {
                // abaikan response stale (unmount/delete/re-mount saat request jalan)
                let valid = self
                    .nodes
                    .get(conn_idx)
                    .map(|n| n.req_id == req_id && n.loading)
                    .unwrap_or(false);
                if !valid {
                    return;
                }
                let name = self
                    .nodes
                    .get(conn_idx)
                    .map(|n| n.conn.name.clone())
                    .unwrap_or_default();
                if let Some(node) = self.nodes.get_mut(conn_idx) {
                    node.loading = false;
                    match &result {
                        Ok(b) => {
                            node.buckets = Some(b.clone());
                            node.mounted = true;
                            node.expanded = true;
                        }
                        Err(_) => {
                            node.buckets = None;
                            if is_mount {
                                node.mounted = false;
                                node.expanded = false;
                            }
                        }
                    }
                }
                match result {
                    Ok(b) => self.set_status(format!("{name}: terhubung, {} bucket", b.len())),
                    Err(e) => {
                        let prefix = if is_mount { "Mount gagal" } else { "gagal list bucket" };
                        self.set_error(format!("{name}: {prefix}: {e}"));
                    }
                }
            }
            AppEvent::ConnTestResult { test_seq, ok, message } => {
                if let Mode::ConnForm(form) = &mut self.mode {
                    if form.testing && form.test_seq == test_seq {
                        form.testing = false;
                        form.test_result = Some((ok, message));
                    }
                }
            }
            AppEvent::ListingLoaded { id, result } => {
                if id != self.current_listing_req {
                    return; // stale
                }
                self.loading = false;
                match result {
                    Ok(items) => {
                        self.items = items;
                        // sinkronkan tanda dengan listing baru: key yang sudah tidak
                        // ada (terhapus / pindah folder) tidak boleh tetap bertanda
                        let items_ref = &self.items;
                        self.marked.retain(|k| items_ref.iter().any(|it| &it.key == k));
                        let restore = self.restore_highlight.take();
                        let vis = self.visible_items();
                        let sel = if vis.is_empty() {
                            None
                        } else {
                            // highlight kembali ke item yang barusan ditinggalkan (bila ada)
                            let pos = restore.and_then(|key| {
                                vis.iter().position(|i| {
                                    self.items.get(*i).map(|it| it.key == key).unwrap_or(false)
                                })
                            });
                            Some(pos.unwrap_or(0))
                        };
                        self.main_state.select(sel);
                        self.schedule_preview();
                    }
                    Err(e) => {
                        self.restore_highlight = None;
                        self.items = vec![];
                        self.main_state.select(None);
                        self.set_error(format!("Gagal list objects: {e}"));
                    }
                }
            }
            AppEvent::PreviewLoaded { id, result } => {
                if id != self.current_preview_req {
                    return; // stale
                }
                // reset state scroll untuk konten baru
                self.preview_scroll = 0;
                self.preview_list_state.select(Some(0));
                match result {
                    Ok(PreviewData::Text(t)) => self.preview = PreviewState::Text(t),
                    Ok(PreviewData::Listing(items)) => {
                        if items.is_empty() {
                            self.preview = PreviewState::Message("(folder kosong)".into());
                        } else {
                            self.preview = PreviewState::Listing(items);
                        }
                    }
                    Ok(PreviewData::Image(bytes)) => {
                        match image::load_from_memory(&bytes) {
                            Ok(dyn_img) => {
                                let proto = self.picker.new_resize_protocol(dyn_img);
                                self.preview = PreviewState::Image(proto);
                            }
                            Err(e) => {
                                self.preview = PreviewState::Message(format!("Gagal decode gambar: {e}"));
                            }
                        }
                    }
                    Ok(PreviewData::Meta(m)) => self.preview = PreviewState::Meta(m),
                    Err(e) => self.preview = PreviewState::Message(format!("Preview error: {e}")),
                }
            }
            AppEvent::DeleteInfoReady { id, targets } => {
                // abaikan hasil dari aksi delete yang sudah dibatalkan/diganti,
                // dan pertahankan fokus tombol yang sedang dipilih
                if let Mode::DeleteConfirm { selected, .. } = &self.mode {
                    let selected = *selected;
                    if id == self.current_delete_req {
                        self.mode = Mode::DeleteConfirm { targets, selected };
                    }
                }
            }
            AppEvent::DownloadProgress { text } => {
                // hanya update bila download masih berjalan (progress bisa datang
                // setelah OpDone bila task selesai lebih dulu)
                if self.download_active {
                    self.download_status = Some(text);
                }
            }
            AppEvent::UploadProgress { text } => {
                // status upload berdiri sendiri: tidak menyentuh state download
                self.set_status(text);
            }
            AppEvent::OpDone {
                message,
                is_error,
                refresh,
                restore,
            } => {
                // semua operasi background berakhir di sini -> lepas guard
                let op = self.busy.take();
                // download selesai (sukses/gagal) -> bersihkan progress.
                // OpDone milik upload TIDAK boleh mereset state download.
                if self.download_active && op != Some(BUSY_UPLOAD) {
                    self.download_active = false;
                    self.download_status = None;
                }
                if let Some(cb) = restore {
                    // jangan timpa clipboard baru yang dibuat user selama paste berjalan
                    if self.clipboard.is_none() {
                        self.clipboard = Some(cb);
                    }
                }
                if is_error {
                    self.set_error(message);
                } else {
                    self.set_status(message);
                }
                if refresh {
                    self.request_listing();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers for UI
    // -----------------------------------------------------------------------

    pub fn breadcrumb(&self) -> String {
        match &self.location {
            Some(loc) => {
                let conn = self
                    .nodes
                    .get(loc.conn_idx)
                    .map(|n| n.conn.name.as_str())
                    .unwrap_or("?");
                format!("{conn} / {} / {}", loc.bucket, loc.prefix)
            }
            None => "Pilih bucket dari sidebar".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Deteksi protokol gambar dari env. Sengaja TIDAK pakai
/// `Picker::from_query_stdio`: query itu membaca stdin dan bila terminal
/// tidak merespons (ssh lambat / pty), orphan thread-nya terus memakan
/// keystroke user.
fn make_picker() -> Picker {
    let mut picker = Picker::from_fontsize((10, 20));
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if std::env::var("KITTY_WINDOW_ID").is_ok()
        || term.contains("kitty")
        || term_program == "WezTerm"
        || std::env::var("WEZTERM_EXECUTABLE").is_ok()
    {
        picker.set_protocol_type(ProtocolType::Kitty);
    } else if term_program == "iTerm.app" || std::env::var("ITERM_SESSION_ID").is_ok() {
        picker.set_protocol_type(ProtocolType::Iterm2);
    } else if term.contains("sixel") {
        picker.set_protocol_type(ProtocolType::Sixel);
    }
    // selain itu: Halfblocks (universal)
    picker
}

fn move_list(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    state.select(Some((cur + delta).rem_euclid(len as isize) as usize));
}

/// Tangani key saat input filter file picker aktif.
/// `true` = key sudah dikonsumsi. Up/Down tetap untuk navigasi, dan
/// Enter/Space tetap bekerja pada entry yang ter-highlight.
fn picker_filter_key(fp: &mut FilePickerState, key: &KeyEvent) -> bool {
    if !fp.filter_active {
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up | KeyCode::Down => return false,
        KeyCode::Enter => return false,
        KeyCode::Char(' ') => return false,
        KeyCode::Esc => fp.filter_active = false,
        KeyCode::Backspace => fp.filter_backspace(),
        KeyCode::Left => fp.filter_move_left(),
        KeyCode::Right => fp.filter_move_right(),
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            fp.filter_insert(c)
        }
        _ => {}
    }
    true
}

/// Folder yang sedang disorot di file picker (None bila yang disorot file).
fn picker_selected_dir(fp: &FilePickerState) -> Option<PathBuf> {
    fp.selected().filter(|e| e.is_dir).map(|e| e.path.clone())
}

fn inside(col: u16, row: u16, area: Rect) -> bool {
    col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
}

/// Folder tujuan default: ~/Downloads, fallback home, fallback cwd.
fn download_dir_default() -> PathBuf {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Adapter `std::fs::File` -> `tokio::io::AsyncWrite`.
/// Dipakai agar tidak bergantung pada fitur `fs` milik tokio (blocking write
/// terjadi di task background, bukan di loop UI).
struct BlockingFile(std::fs::File);

impl tokio::io::AsyncWrite for BlockingFile {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::task::Poll::Ready(io::Write::write(&mut self.0, buf))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(io::Write::flush(&mut self.0))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(io::Write::flush(&mut self.0))
    }
}

impl LocalEntry {
}

async fn preview_item(client: &S3Client, bucket: &str, item: &S3Item) -> Result<PreviewData, String> {
    if item.is_dir {
        // tampilkan isi folder sebagai listing seperti main pane
        let items = client.list_objects(bucket, &item.key, Some(200)).await?;
        return Ok(PreviewData::Listing(items));
    }

    let ext = item
        .name
        .rsplit('.')
        .next()
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    let has_ext = item.name.contains('.');
    let size = item.size.unwrap_or(0);

    if has_ext && IMAGE_EXTS.contains(&ext.as_str()) && size <= MAX_IMAGE_PREVIEW {
        let bytes = client.get_bytes(bucket, &item.key, None).await?;
        return Ok(PreviewData::Image(bytes));
    }

    let textish = (has_ext && TEXT_EXTS.contains(&ext.as_str())) || size <= MAX_TEXT_PREVIEW as i64;
    if textish {
        let bytes = client
            .get_bytes(bucket, &item.key, Some(&format!("bytes=0-{}", MAX_TEXT_PREVIEW - 1)))
            .await?;
        let binary = bytes.iter().take(8192).any(|&b| b == 0);
        if !binary {
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if size > MAX_TEXT_PREVIEW as i64 {
                text.push_str(&format!(
                    "\n\n... (dipotong, total {})",
                    human_size(item.size)
                ));
            }
            return Ok(PreviewData::Text(text));
        }
    }
    // fallback: metadata
    let meta = client.head(bucket, &item.key).await?;
    Ok(PreviewData::Meta(meta))
}
