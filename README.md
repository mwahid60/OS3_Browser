# OS3 Browser CLI

TUI interaktif untuk browsing S3/MinIO — kombinasi `mc` (koneksi multi-alias) dengan navigasi ala `yazi` (3 pane: sidebar / main / preview).

Ditulis dengan Rust + [ratatui](https://ratatui.rs).

## Fitur

- Multi-koneksi S3 (MinIO, AWS S3, dan server S3-compatible lain seperti Dell ECS/ViPR)
- 3 pane: sidebar (koneksi + bucket), main (listing), preview (isi folder / teks / gambar / metadata)
- Find/filter live dengan satu search bar global
- Upload multi-file, download (file tunggal / ZIP), copy/move antar prefix, delete dengan konfirmasi
- Preview gambar via protokol terminal (kitty / iTerm2 / sixel, fallback halfblocks)
- Dukungan mouse penuh (klik, klik kanan, scroll)
- 3 gaya ikon (`nerd` / `unicode` / `ascii`) — ganti kapan saja dengan `Ctrl+G`

## Install

### Download binary (paling mudah)

Unduh binary sesuai platform dari halaman [Releases](../../releases), lalu:

```sh
chmod +x os3
sudo mv os3 /usr/local/bin/   # atau direktori lain di PATH
```

### Dari source

Butuh [Rust toolchain](https://rustup.rs):

```sh
git clone https://github.com/<username>/os3.git
cd os3
cargo build --release
cp target/release/os3 ~/.local/bin/
```

### Via cargo

```sh
cargo install --git https://github.com/<username>/os3
```

Jalankan:

```sh
os3
```

## Quick Start

1. Jalankan `os3` — file config otomatis dibuat di `~/.config/os3/config.toml`.
2. Tekan `n` untuk menambah koneksi: isi endpoint, access key, secret key.
3. `Enter` pada koneksi untuk mount dan melihat daftar bucket-nya.
4. `Enter` pada bucket untuk mulai browsing.

Config juga bisa diedit manual:

```toml
[[connections]]
name = "my-minio"
endpoint = "http://localhost:9000"
access_key = "minioadmin"
secret_key = "minioadmin"
# region = "us-east-1"   # opsional
```

## Layout

```
┌ Connections ─┐┌ main pane (breadcrumb) ─┐┌ Preview ──────────┐
│ + conn-name  ││ file/folder listing     ││ isi folder / teks │
│   ▪ bucket   ││                          ││ / gambar / meta   │
└──────────────┘└──────────────────────────┘└───────────────────┘
```

- `Enter`/klik pada koneksi → mount + expand, tampilkan bucket-nya.
- `Enter`/klik pada bucket → main pane menampilkan isi bucket.
- Preview pane menampilkan isi folder ter-highlight, isi file teks, render gambar, atau metadata untuk file binary.

## Keybindings

| Key | Aksi |
|---|---|
| `↑/↓` / `j/k` | pindah seleksi |
| `→` / `Enter` / `l` | buka folder / bucket |
| `←` / `h` | mundur ke folder induk (highlight kembali ke folder yang barusan dibuka) |
| `Tab` / `Shift+Tab` | pindah fokus sidebar → main → preview |
| `Ctrl+Z` | zoom/unzoom pane yang sedang fokus |
| klik mouse | mount/expand koneksi, buka bucket, pilih item, fokus pane |
| klik kanan pada koneksi | menu konteks: Mount/Unmount, Edit, Delete |
| scroll wheel | scroll pane di bawah kursor |
| `Space` (di main) | tandai/hapus tanda satu item |
| `v` | mode visual: tandai sambil navigasi ↑/↓ |
| `V` | hapus semua tanda |
| `a` | add: **New Folder** / **Insert New File** (upload, multi-file) |
| `d` | **download** item tertandai / ter-highlight (popup) — ZIP bila >1 item atau folder |
| `D` (Shift+D) | hapus item tertandai / ter-highlight (popup konfirmasi) |
| `c` | copy mode — navigasi ke tujuan lalu `p` untuk paste; `c` lagi = batal |
| `x` | move mode — sama seperti copy; `x` lagi = batal |
| `p` | paste ke folder saat ini (atau folder yang ter-highlight) |
| `r` | refresh listing / bucket |
| `/` | **find/filter** live di pane yang aktif |
| `n` / `N` | tambah koneksi S3 baru |
| `?` | help |
| `q` / `Ctrl+C` | keluar |
| `Esc` | tutup popup / batalkan mode / hapus filter (berlapis) |

### Form koneksi

- `↑/↓` / `Tab` — pindah field
- `Enter` — **Mount** (simpan + hubungkan)
- `Shift+Enter` — **Test Connection** (membutuhkan terminal dengan kitty keyboard protocol; di terminal biasa gunakan klik)

Koneksi yang belum di-mount tampil abu-abu; klik/`Enter` untuk mount. Tombol `[+ New Connection]` selalu tampil di bagian bawah sidebar.

## Find / Filter (`/`)

Tekan `/` untuk memunculkan **satu search bar** di bawah judul (di atas pane) — hasil **langsung terfilter** saat mengetik, tanpa perlu `Enter`. Search bar memfilter **pane yang sedang aktif**:

- **Sidebar** — hanya bucket yang cocok (baris koneksi tetap tampil).
- **Main pane** — hanya file/folder yang cocok; navigasi `↑/↓` tetap jalan di hasil.
- **Preview pane** — bila yang ditampilkan file teks, keyword **di-highlight** di dalam teks.
- **File picker** — hanya entry yang cocok.

**Query terbawa antar-pane saat input masih aktif.** Ketik `re` di main pane lalu tekan `Tab`: search bar tetap menampilkan `re` dan pane baru langsung menerapkan filter itu. Masing-masing pane tetap menyimpan salinan query-nya sendiri.

| Tombol | Aksi |
|---|---|
| `/` | munculkan search bar (query pane aktif tetap ada) |
| `Tab` / `Shift+Tab` | pindah pane (search bar tetap aktif) |
| `Esc` | nonaktifkan input, **query tetap tersimpan** (filter masih berlaku) |
| `Esc` (saat input nonaktif) | hapus **semua** filter di semua pane |

Pencarian tidak membedakan huruf besar/kecil.

## Download

- `d` pada **file tunggal** → diunduh apa adanya dengan namanya.
- `d` pada **folder**, atau saat **>1 item ditandai** → seluruh item dikemas menjadi **satu ZIP** (struktur folder dipertahankan).
- Item yang ditandai di-dedup: file di dalam folder yang juga ditandai tidak dimasukkan dua kali.
- Popup menampilkan daftar item (scroll `↑/↓`), folder tujuan (default `~/Downloads`), dan tombol `[ Download ]` / `[ Cancel ]`.
- `p` di popup membuka file picker untuk memilih folder tujuan lain.
- Nama file yang sudah ada ditambahkan akhiran angka: `alpha (1).txt`, dst.

## Upload (multi-file)

`a` → **Insert New File** membuka file picker:

- `Space` — tandai/lepas tanda file (bisa di beberapa folder; tanda tetap tersimpan saat berpindah folder)
- `Enter` pada file — unggah **semua file yang ditandai** (atau file ter-highlight bila belum ada tanda)
- `Enter`/`→` pada folder — masuk folder · `←`/`Backspace` — kembali ke parent (highlight kembali ke folder asal)
- File yang namanya sudah ada di tujuan **dilewati** dan dilaporkan, tidak ditimpa diam-diam.

File diunggah satu per satu dengan progress; bila salah satu gagal, sisanya tetap dilanjutkan. Content-Type diisi otomatis berdasarkan ekstensi.

### File picker (upload & download)

| Tombol | Aksi |
|---|---|
| `/` | filter entry |
| `Esc` | berlapis: nonaktifkan input → hapus teks filter → tutup picker |

## Tampilan / Icon

Tersedia 3 gaya, ganti kapan saja dengan **`Ctrl+G`** (tersimpan otomatis ke config):

| Style | Icon | Border | Cocok untuk |
|---|---|---|---|
| `nerd` | glyph Nerd Font | Unicode | terminal yang menghitung glyph PUA 1 sel |
| `unicode` | badge `[i] [v] [a]` | Unicode | kompromi |
| `ascii` | badge ASCII | ASCII (`+ - \|`) | **paling aman** di semua terminal |

Atau set di config:

```toml
icon_style = "ascii"   # "ascii" | "unicode" | "nerd"
```

> **Kenapa ada pilihan ini?** ratatui menghitung lebar glyph dengan `unicode-width`, sedangkan terminal menghitung sendiri. Untuk glyph *Ambiguous* dan *Private Use Area* (Nerd Font), hasilnya bisa 1 vs 2 sel — bila berbeda, layout bergeser. `ascii` menghilangkan risiko ini sepenuhnya.

Jika ikon Nerd Font terlihat "mendorong" teks di Ghostty, tambahkan pada `~/.config/ghostty/config`:

```
grapheme-width-method = legacy
```

## Server S3-compatible khusus (ViPR / Dell ECS)

Sebagian server menghitung signature **tanpa** hash body, sehingga payload signing membuat upload gagal dengan `SignatureDoesNotMatch` / `XAmzContentSHA256Mismatch`. Solusinya:

```toml
[[connections]]
name = "my-ecs"
endpoint = "https://s3.example.com:9020"
access_key = "..."
secret_key = "..."
disable_payload_signing = true   # wajib untuk ViPR/ECS
```

Atau saat menambah koneksi lewat UI (`n`), isi field **Payload signing off? (y/n)** dengan `y`.

Selain itu ViPR/ECS mewajibkan header `Content-MD5` pada `DeleteObjects` batch — karena itu os3 menghapus objek **satu per satu** (`DeleteObject`), yang kompatibel di semua server.

## Catatan

- Copy/move hanya dalam satu koneksi (boleh lintas bucket/prefix); paste menolak menimpa key yang sudah ada.
- Penanda (`Space`/`v`) di-reset saat berpindah folder.
- File gambar > 25 MB dan teks > 64 KB menampilkan metadata saja (bukan isi).
- Operasi berjalan di background — `q` ditolak selama operasi berlangsung agar tidak meninggalkan state setengah jadi.

## Development

```sh
cargo build
cargo test
```

Smoke test terhadap server lokal (opsional, butuh env):

```sh
OS3_TEST_ENDPOINT=http://localhost:9000 \
OS3_TEST_AK=minioadmin \
OS3_TEST_SK=minioadmin \
cargo run -- --self-test
```

## License

MIT
