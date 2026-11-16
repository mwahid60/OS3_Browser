mod app;
mod config;
// Catatan: s3.rs/ui.rs di luar scope perbaikan ini dan membawa warning lama
// (import tak terpakai + helper mati). Allow dipasang per-modul agar build bersih
// tanpa menyembunyikan warning crate lain.
#[allow(unused_imports, dead_code)]
mod s3;
mod theme;
#[allow(dead_code)]
mod ui;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

const USAGE: &str = "\
OS3 Browser CLI - interactive S3/MinIO TUI browser

USAGE:
    os3 [OPTIONS]

OPTIONS:
    -h, --help        Tampilkan bantuan ini
    -V, --version     Tampilkan versi
        --self-test   Jalankan self-test koneksi S3 lalu keluar
";

/// Mengembalikan terminal ke kondisi semula saat di-drop, termasuk bila `main`
/// keluar lebih awal karena `?` (mis. Terminal::new gagal).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        // Best-effort: jangan sampai satu kegagalan menghalangi pemulihan lain.
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        let _ = execute!(
            stdout,
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("os3 {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--self-test") {
        return s3::self_test().await;
    }

    enable_raw_mode()?;
    // Setelah raw mode aktif, semua jalur keluar (termasuk `?`) harus memulihkan
    // terminal -> serahkan ke Drop.
    let guard = TerminalGuard;

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Aktifkan keyboard enhancement (kitty protocol) supaya Shift+Enter bisa dibedakan dari
    // Enter. Tidak semua terminal mendukung; abaikan error-nya.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = match app::App::new() {
        Ok(mut app) => app.run(&mut terminal).await,
        Err(e) => Err(e),
    };

    terminal.show_cursor()?;
    drop(guard);

    result
}
