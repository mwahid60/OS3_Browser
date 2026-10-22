use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;

use aws_sdk_s3::Client;
use futures::stream::{self, StreamExt};
use tokio::io::AsyncWriteExt;

use crate::app::S3Item;
use crate::config::Connection;

/// Jumlah maksimum key per halaman `ListObjectsV2`.
pub const MAX_LIST_KEYS: i32 = 1000;

#[derive(Clone)]
pub struct S3Client {
    client: Client,
}

fn fmt_dt(dt: &aws_smithy_types::DateTime) -> String {
    dt.fmt(aws_smithy_types::date_time::Format::DateTime)
        .unwrap_or_default()
}

pub fn human_size(size: Option<i64>) -> String {
    let s = size.unwrap_or(0);
    if s < 1024 {
        format!("{} B", s)
    } else if s < 1024 * 1024 {
        format!("{:.1} KB", s as f64 / 1024.0)
    } else if s < 1024 * 1024 * 1024 {
        format!("{:.1} MB", s as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", s as f64 / 1024.0 / 1024.0 / 1024.0)
    }
}

fn encode_copy_source(bucket: &str, key: &str) -> String {
    let enc_key: Vec<String> = key
        .split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect();
    format!("{}/{}", bucket, enc_key.join("/"))
}

/// Teks error yang informatif: `Display` dari aws-sdk sering hanya
/// "service error"/"dispatch failure". Buka detail service error-nya
/// (code + message) supaya user tahu penyebab sebenarnya.
pub fn describe_sdk_error<E, R>(err: aws_sdk_s3::error::SdkError<E, R>) -> String
where
    E: std::error::Error + std::fmt::Debug,
    R: std::fmt::Debug,
{
    if let Some(se) = err.as_service_error() {
        return format!("{se:?}");
    }
    match &err {
        aws_sdk_s3::error::SdkError::DispatchFailure(d) => {
            format!("koneksi gagal: {d:?}")
        }
        aws_sdk_s3::error::SdkError::TimeoutError(_) => "timeout ke endpoint".to_string(),
        aws_sdk_s3::error::SdkError::ResponseError(r) => format!("respons error: {r:?}"),
        aws_sdk_s3::error::SdkError::ConstructionFailure(c) => format!("request tidak valid: {c:?}"),
        other => format!("{other:?}"),
    }
}

/// Content-Type sederhana berdasarkan ekstensi.
fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

impl S3Client {
    pub fn new(conn: &Connection) -> Self {
        let timeout = TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(5))
            .operation_attempt_timeout(Duration::from_secs(30))
            .build();
        let retry = RetryConfig::adaptive().with_max_attempts(5);
        let mut conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(conn.region_or_default()))
            .endpoint_url(conn.endpoint.clone())
            .credentials_provider(Credentials::new(
                conn.access_key.clone(),
                conn.secret_key.clone(),
                None,
                None,
                "os3",
            ))
            .timeout_config(timeout)
            .retry_config(retry)
            .force_path_style(true);
        if conn.disable_payload_signing {
            // Server seperti Dell ECS/ViPR menghitung signature tanpa hash body.
            conf = conf.request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            );
        }
        S3Client {
            client: Client::from_conf(conf.build()),
        }
    }

    pub async fn list_buckets(&self) -> Result<Vec<String>, String> {
        let out = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(describe_sdk_error)?;
        let mut names: Vec<String> = out
            .buckets()
            .iter()
            .filter_map(|b| b.name().map(String::from))
            .collect();
        names.sort();
        Ok(names)
    }

    /// List "directory" contents: folders (common prefixes) then files.
    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        max: Option<usize>,
    ) -> Result<Vec<S3Item>, String> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .delimiter("/")
                .max_keys(MAX_LIST_KEYS);
            if let Some(t) = token.take() {
                req = req.continuation_token(t);
            }
            let out = req.send().await.map_err(describe_sdk_error)?;
            for cp in out.common_prefixes() {
                if let Some(p) = cp.prefix() {
                    let name = p
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or(p)
                        .to_string();
                    items.push(S3Item {
                        name: format!("{name}/"),
                        key: p.to_string(),
                        is_dir: true,
                        size: None,
                        modified: None,
                    });
                }
            }
            for o in out.contents() {
                let key = o.key().unwrap_or_default();
                if key == prefix {
                    continue; // the folder placeholder object itself
                }
                let name = key.rsplit('/').next().unwrap_or(key).to_string();
                items.push(S3Item {
                    name,
                    key: key.to_string(),
                    is_dir: false,
                    size: o.size(),
                    modified: o.last_modified().map(fmt_dt),
                });
            }
            if let Some(m) = max {
                if items.len() >= m {
                    items.truncate(m);
                    return Ok(items);
                }
            }
            if out.is_truncated().unwrap_or(false) {
                token = out.next_continuation_token().map(String::from);
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(items)
    }

    /// All keys under a prefix (recursive, no delimiter). Includes the
    /// prefix placeholder object itself if it exists.
    pub async fn list_all_keys(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, String> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .max_keys(MAX_LIST_KEYS);
            if let Some(t) = token.take() {
                req = req.continuation_token(t);
            }
            let out = req.send().await.map_err(describe_sdk_error)?;
            for o in out.contents() {
                if let Some(k) = o.key() {
                    // Filter sisi klien: jangan pernah mengembalikan key di luar
                    // prefix yang diminta (sebagian server S3-compatible tidak
                    // konsisten menerapkan parameter `prefix`).
                    if prefix.is_empty() || k.starts_with(prefix) {
                        keys.push(k.to_string());
                    }
                }
            }
            if out.is_truncated().unwrap_or(false) {
                token = out.next_continuation_token().map(String::from);
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(keys)
    }

    pub async fn get_bytes(
        &self,
        bucket: &str,
        key: &str,
        range: Option<&str>,
    ) -> Result<Vec<u8>, String> {
        let mut req = self.client.get_object().bucket(bucket).key(key);
        if let Some(r) = range {
            req = req.range(r);
        }
        let out = req.send().await.map_err(describe_sdk_error)?;
        // Kumpulkan chunk langsung ke Vec: `collect().into_bytes()` lalu `.to_vec()`
        // menyalin dua kali, sedangkan cara ini hanya sekali.
        let cap = out.content_length().unwrap_or(0).max(0) as usize;
        let mut buf: Vec<u8> = Vec::with_capacity(cap.min(16 * 1024 * 1024));
        let mut body = out.body;
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|e| format!("gagal membaca body: {e}"))?
        {
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    pub async fn head(&self, bucket: &str, key: &str) -> Result<Vec<(String, String)>, String> {
        let out = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(describe_sdk_error)?;
        let mut meta = vec![
            ("Key".to_string(), key.to_string()),
            (
                "Size".to_string(),
                format!(
                    "{} ({} bytes)",
                    human_size(out.content_length()),
                    out.content_length().unwrap_or(0)
                ),
            ),
            (
                "Content-Type".to_string(),
                out.content_type().unwrap_or("-").to_string(),
            ),
            (
                "Last-Modified".to_string(),
                out.last_modified().map(fmt_dt).unwrap_or_default(),
            ),
            ("ETag".to_string(), out.e_tag().unwrap_or("-").to_string()),
            (
                "Storage-Class".to_string(),
                out.storage_class().map(|s| s.as_str().to_string()).unwrap_or_else(|| "-".into()),
            ),
        ];
        if let Some(m) = out.metadata() {
            for (k, v) in m {
                meta.push((format!("meta:{k}"), v.clone()));
            }
        }
        Ok(meta)
    }

    /// Alias kompatibilitas: metadata object (sama dengan [`Self::head`]).
    pub async fn object_meta(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<(String, String)>, String> {
        self.head(bucket, key).await
    }

    pub async fn put_folder(&self, bucket: &str, key: &str) -> Result<(), String> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(describe_sdk_error)?;
        Ok(())
    }

    pub async fn put_file(&self, bucket: &str, key: &str, path: &Path) -> Result<(), String> {
        let body = ByteStream::from_path(path)
            .await
            .map_err(|e| format!("gagal membaca file: {e}"))?;
        let mut req = self.client.put_object().bucket(bucket).key(key).body(body);
        // MinIO/S3 sebaiknya diberi Content-Type eksplisit; tanpa ini beberapa
        // konfigurasi menolak/mengembalikan error yang tidak informatif.
        req = req.content_type(content_type_for(path));
        req.send().await.map_err(|e| describe_sdk_error(e))?;
        Ok(())
    }

    pub async fn delete_keys(&self, bucket: &str, keys: Vec<String>) -> Result<usize, String> {
        // Hapus satu-per-satu (DeleteObject). Ini paling kompatibel:
        // sebagian server S3-compatible (mis. Dell ECS/ViPR) mewajibkan header
        // `Content-MD5` pada DeleteObjects batch, sedangkan aws-sdk modern
        // mengirim checksum CRC32 -> server menolak dengan InvalidRequest
        // "Missing required header for this request: Content-MD5".
        //
        // Permintaan dijalankan paralel dengan batas 8 supaya folder besar
        // tidak menghabiskan waktu saat dibersihkan.
        let client = self.client.clone();
        let bucket = bucket.to_string();
        let total = keys.len();
        let results: Vec<(String, Result<(), String>)> = stream::iter(keys)
            .map(|key| {
                let client = client.clone();
                let bucket = bucket.clone();
                async move {
                    let res = client
                        .delete_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                        .map(|_| ())
                        .map_err(describe_sdk_error);
                    (key, res)
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;

        let mut deleted = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for (key, res) in results {
            match res {
                Ok(()) => deleted += 1,
                Err(e) => errors.push(format!("{key}: {e}")),
            }
        }
        if !errors.is_empty() {
            let shown = errors.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
            return Err(format!(
                "{deleted}/{total} terhapus; gagal: {shown}{}",
                if errors.len() > 3 {
                    format!(" (+{} lagi)", errors.len() - 3)
                } else {
                    String::new()
                }
            ));
        }
        Ok(deleted)
    }

    /// Copy satu object; mendukung lintas bucket (dst_bucket boleh beda dari src_bucket).
    pub async fn copy_object(
        &self,
        dst_bucket: &str,
        src_bucket: &str,
        src_key: &str,
        dest_key: &str,
    ) -> Result<(), String> {
        self.client
            .copy_object()
            .bucket(dst_bucket)
            .key(dest_key)
            .copy_source(encode_copy_source(src_bucket, src_key))
            .send()
            .await
            .map_err(describe_sdk_error)?;
        Ok(())
    }

    /// Cek apakah key sudah ada di bucket.
    pub async fn exists(&self, bucket: &str, key: &str) -> Result<bool, String> {
        match self.client.head_object().bucket(bucket).key(key).send().await {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.as_service_error()
                    .map(|se| se.is_not_found())
                    .unwrap_or(false)
                {
                    Ok(false)
                } else {
                    Err(describe_sdk_error(e))
                }
            }
        }
    }

    /// Unduh object dan kirim byte-nya ke sink (streaming, tidak menahan seluruh file di memori).
    /// `on_progress` dipanggil setelah setiap chunk; parameter = total byte yang sudah ditulis.
    pub async fn download_object_stream<F>(
        &self,
        bucket: &str,
        key: &str,
        mut sink: impl tokio::io::AsyncWrite + Unpin,
        mut on_progress: F,
    ) -> Result<u64, String>
    where
        F: FnMut(u64),
    {
        let out = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(describe_sdk_error)?;
        let mut body = out.body;
        let mut written: u64 = 0;
        while let Some(bytes) = body
            .try_next()
            .await
            .map_err(|e| format!("gagal membaca stream: {e}"))?
        {
            sink.write_all(&bytes)
                .await
                .map_err(|e| format!("gagal menulis ke disk: {e}"))?;
            written += bytes.len() as u64;
            on_progress(written);
        }
        sink.flush()
            .await
            .map_err(|e| format!("gagal flush ke disk: {e}"))?;
        Ok(written)
    }

    /// Copy a whole "folder" (prefix) recursively. Returns objects copied.
    pub async fn copy_recursive(
        &self,
        dst_bucket: &str,
        src_bucket: &str,
        src_prefix: &str,
        dest_prefix: &str,
    ) -> Result<usize, String> {
        let keys = self.list_all_keys(src_bucket, src_prefix).await?;
        let mut n = 0;
        for k in &keys {
            let rel = k.strip_prefix(src_prefix).ok_or_else(|| {
                format!("key '{k}' tidak berawalan prefix '{src_prefix}'")
            })?;
            let dest = format!("{dest_prefix}{rel}");
            self.copy_object(dst_bucket, src_bucket, k, &dest).await?;
            n += 1;
        }
        Ok(n)
    }
}

/// Pesan dari sisi async ke penulis ZIP (sync, dijalankan di `spawn_blocking`).
enum ZipMsg {
    Start {
        name: String,
        stored: bool,
        large: bool,
    },
    Data(Vec<u8>),
    Finish,
}

/// Media yang sudah terkompresi -> simpan apa adanya (`Stored`) agar tidak buang CPU.
fn is_stored_extension(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "webp"
            | "mp4"
            | "mov"
            | "avi"
            | "mkv"
            | "zip"
            | "gz"
            | "7z"
            | "rar"
    )
}

/// Sanitasi nama entri ZIP: leading `/` diganti `_`, segmen `..`/`.` ditolak,
/// dan `:` (serta karakter kontrol) diganti `_`.
fn sanitize_zip_name(name: &str) -> String {
    let normalized = name.replace('\\', "/");
    let normalized = if normalized.starts_with('/') {
        format!("_{normalized}")
    } else {
        normalized
    };
    let mut parts: Vec<String> = Vec::new();
    for seg in normalized.split('/') {
        if seg.is_empty() {
            continue;
        }
        let mut s: String = seg
            .chars()
            .map(|c| if c == ':' || c.is_control() { '_' } else { c })
            .collect();
        if s == ".." || s == "." {
            s = "_".to_string();
        }
        parts.push(s);
    }
    if parts.is_empty() {
        "_".to_string()
    } else {
        parts.join("/")
    }
}

/// Streaming satu object S3 ke penulis ZIP lewat channel.
async fn zip_download_into(
    client: &S3Client,
    bucket: &str,
    key: &str,
    entry_name: &str,
    tx: &tokio::sync::mpsc::Sender<ZipMsg>,
) -> Result<u64, String> {
    let out = client
        .client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(describe_sdk_error)?;
    let known_len = out.content_length();
    let len = known_len.unwrap_or(0).max(0) as u64;
    tx.send(ZipMsg::Start {
        name: entry_name.to_string(),
        stored: is_stored_extension(entry_name),
        // Zip64 diperlukan bila ukuran tidak diketahui atau >= 4 GiB.
        large: known_len.is_none() || len >= u32::MAX as u64,
    })
    .await
    .map_err(|_| "zip: penulis berhenti".to_string())?;

    let mut body = out.body;
    let mut written = 0u64;
    while let Some(chunk) = body
        .try_next()
        .await
        .map_err(|e| format!("gagal membaca body {key}: {e}"))?
    {
        written += chunk.len() as u64;
        tx.send(ZipMsg::Data(chunk.to_vec()))
            .await
            .map_err(|_| "zip: penulis berhenti".to_string())?;
    }
    Ok(written)
}

/// Bangun ZIP dari beberapa object S3 ke `out_path`.
/// `items`: pasangan (prefix_s3_atau_key_persis, nama_di_dalam_zip).
/// - Bila item adalah "folder" (kunci berakhiran '/'), semua object dengan prefix itu
///   dimasukkan dengan path relatif di bawah nama folder tersebut.
/// - Bila item adalah file, dimasukkan dengan `nama_di_zip` apa adanya.
/// Body di-stream langsung dari S3 ke `ZipWriter` (tidak pernah menahan file utuh
/// di memori); penulisan ZIP berjalan di `spawn_blocking` karena `ZipWriter` sync.
/// Mengembalikan (jumlah file, total byte tidak terkompresi).
pub async fn zip_objects(
    client: &S3Client,
    bucket: &str,
    items: &[(String /*s3 key/prefix*/, String /*nama di zip*/)],
    out_path: &std::path::Path,
) -> Result<(usize, u64), String> {
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::channel::<ZipMsg>(8);
    let path = out_path.to_path_buf();
    let writer = tokio::task::spawn_blocking(move || -> Result<(usize, u64), String> {
        let file = std::fs::File::create(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        let mut zip = zip::ZipWriter::new(BufWriter::new(file));
        let mut count: usize = 0;
        let mut total: u64 = 0;

        while let Some(msg) = rx.blocking_recv() {
            match msg {
                ZipMsg::Start {
                    name,
                    stored,
                    large,
                } => {
                    let method = if stored {
                        zip::CompressionMethod::Stored
                    } else {
                        zip::CompressionMethod::Deflated
                    };
                    let opts = zip::write::SimpleFileOptions::default()
                        .compression_method(method)
                        .large_file(large);
                    zip.start_file(name, opts)
                        .map_err(|e| format!("zip: {e}"))?;
                    count += 1;
                }
                ZipMsg::Data(chunk) => {
                    zip.write_all(&chunk).map_err(|e| format!("zip: {e}"))?;
                    total += chunk.len() as u64;
                }
                ZipMsg::Finish => {
                    let mut w = zip.finish().map_err(|e| format!("zip: {e}"))?;
                    w.flush().map_err(|e| format!("zip flush: {e}"))?;
                    w.get_ref()
                        .sync_all()
                        .map_err(|e| format!("zip sync_all: {e}"))?;
                    return Ok((count, total));
                }
            }
        }
        Err("zip: penulis berhenti sebelum finish".to_string())
    });

    let mut failure: Option<String> = None;
    'items: for (key, zip_name) in items {
        if key.ends_with('/') {
            let keys = match client.list_all_keys(bucket, key).await {
                Ok(k) => k,
                Err(e) => {
                    failure = Some(e);
                    break 'items;
                }
            };
            let base_raw = zip_name.trim_end_matches('/');
            let base = if base_raw.is_empty() {
                String::new()
            } else {
                sanitize_zip_name(base_raw)
            };
            for k in keys {
                // Skip the folder placeholder object itself.
                if k == *key {
                    continue;
                }
                let Some(rel) = k.strip_prefix(key.as_str()) else {
                    continue;
                };
                if rel.is_empty() {
                    continue;
                }
                let raw = if base.is_empty() {
                    rel.to_string()
                } else {
                    format!("{base}/{rel}")
                };
                let name = sanitize_zip_name(&raw);
                if let Err(e) = zip_download_into(client, bucket, &k, &name, &tx).await {
                    failure = Some(e);
                    break 'items;
                }
            }
        } else {
            let name = sanitize_zip_name(zip_name);
            if let Err(e) = zip_download_into(client, bucket, key, &name, &tx).await {
                failure = Some(e);
                break;
            }
        }
    }

    if failure.is_none() {
        if let Err(e) = tx.send(ZipMsg::Finish).await {
            failure = Some(format!("zip: gagal mengirim finish: {e}"));
        }
    }
    drop(tx);

    let joined = writer.await.map_err(|e| format!("zip task: {e}"))?;
    match failure {
        Some(e) => Err(e),
        None => joined,
    }
}

/// Kembalikan path yang belum dipakai: bila `dir/name` sudah ada,
/// coba `name (1)`, `name (2)`, ... (sebelum ekstensi).
pub fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let p = Path::new(name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    let ext = p.extension().and_then(|s| s.to_str());

    let mut n: usize = 1;
    loop {
        let file_name = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({n}).{e}"),
            _ => format!("{stem} ({n})"),
        };
        let cand = dir.join(&file_name);
        if !cand.exists() {
            return cand;
        }
        n += 1;
    }
}

/// End-to-end smoke test terhadap endpoint S3 dari environment.
/// Set `OS3_TEST_ENDPOINT`, `OS3_TEST_AK`, `OS3_TEST_SK` untuk menjalankannya.
pub async fn self_test() -> anyhow::Result<()> {
    let env_nonempty = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (Some(endpoint), Some(access_key), Some(secret_key)) = (
        env_nonempty("OS3_TEST_ENDPOINT"),
        env_nonempty("OS3_TEST_AK"),
        env_nonempty("OS3_TEST_SK"),
    ) else {
        println!("SKIP (env tidak diset)");
        return Ok(());
    };
    let conn = Connection {
        name: "selftest".into(),
        endpoint,
        access_key,
        secret_key,
        region: env_nonempty("OS3_TEST_REGION"),
        disable_payload_signing: false,
    };
    let c = S3Client::new(&conn);

    println!("1. list_buckets ...");
    let buckets = c.list_buckets().await.map_err(anyhow::Error::msg)?;
    println!("   ok -> {buckets:?}");

    let bucket = "os3-selftest";
    println!("2. create bucket {bucket} (ignore if exists) ...");
    match c.client.create_bucket().bucket(bucket).send().await {
        Ok(_) => println!("   created"),
        Err(e) => println!("   skipped ({e})"),
    }

    println!("3. put_folder a/ ...");
    c.put_folder(bucket, "a/").await.map_err(anyhow::Error::msg)?;

    let tmp = std::env::temp_dir().join("os3_selftest.txt");
    std::fs::write(&tmp, "hello from os3 self-test\n")?;
    println!("4. put_file a/hello.txt ...");
    c.put_file(bucket, "a/hello.txt", &tmp)
        .await
        .map_err(anyhow::Error::msg)?;

    println!("5. list_objects prefix a/ ...");
    let items = c.list_objects(bucket, "a/", None).await.map_err(anyhow::Error::msg)?;
    for it in &items {
        println!("   - {} ({:?})", it.name, it.size);
    }
    assert!(items.iter().any(|i| i.key == "a/hello.txt"));

    println!("6. get_bytes ...");
    let bytes = c.get_bytes(bucket, "a/hello.txt", None).await.map_err(anyhow::Error::msg)?;
    assert_eq!(bytes, b"hello from os3 self-test\n");

    println!("7. head ...");
    let meta = c.head(bucket, "a/hello.txt").await.map_err(anyhow::Error::msg)?;
    println!("   {meta:?}");

    println!("8. copy_object a/hello.txt -> a/copy.txt ...");
    c.copy_object(bucket, bucket, "a/hello.txt", "a/copy.txt")
        .await
        .map_err(anyhow::Error::msg)?;

    println!("9. copy_recursive a/ -> b/ ...");
    let n = c.copy_recursive(bucket, bucket, "a/", "b/").await.map_err(anyhow::Error::msg)?;
    println!("   copied {n} objects");

    println!("9b. exists check ...");
    assert!(c.exists(bucket, "a/hello.txt").await.map_err(anyhow::Error::msg)?);
    assert!(!c.exists(bucket, "a/nope.txt").await.map_err(anyhow::Error::msg)?);

    println!("10. delete_keys (cleanup bucket) ...");
    let keys = c.list_all_keys(bucket, "").await.map_err(anyhow::Error::msg)?;
    let d = c.delete_keys(bucket, keys).await.map_err(anyhow::Error::msg)?;
    println!("   deleted {d}");

    let _ = c.client.delete_bucket().bucket(bucket).send().await;
    println!("SELF-TEST OK");
    Ok(())
}

/// Koneksi uji dari environment; `None` bila env tidak lengkap.
/// Tidak ada kredensial yang di-hardcode di test mana pun.
#[cfg(test)]
fn test_conn(disable_payload_signing: bool) -> Option<Connection> {
    let env_nonempty = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (Some(endpoint), Some(access_key), Some(secret_key)) = (
        env_nonempty("OS3_TEST_ENDPOINT"),
        env_nonempty("OS3_TEST_AK"),
        env_nonempty("OS3_TEST_SK"),
    ) else {
        eprintln!("SKIP (env tidak diset)");
        return None;
    };
    Some(Connection {
        name: "test".into(),
        endpoint,
        access_key,
        secret_key,
        region: env_nonempty("OS3_TEST_REGION"),
        disable_payload_signing,
    })
}

#[cfg(test)]
mod tests {
    use super::{is_stored_extension, sanitize_zip_name, unique_path};
    use std::path::PathBuf;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("os3_test_{}", std::process::id()))
    }

    #[test]
    fn unique_path_avoids_existing_names() {
        let dir = test_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("a.zip"), b"x").unwrap();
        std::fs::write(dir.join("a (1).zip"), b"x").unwrap();
        std::fs::write(dir.join("data"), b"x").unwrap();

        let got = unique_path(&dir, "a.zip");
        assert_eq!(
            got.file_name().and_then(|s| s.to_str()),
            Some("a (2).zip")
        );

        let got_no_ext = unique_path(&dir, "data");
        assert_eq!(
            got_no_ext.file_name().and_then(|s| s.to_str()),
            Some("data (1)")
        );

        // A free name is returned unchanged.
        let free = unique_path(&dir, "fresh.zip");
        assert_eq!(free.file_name().and_then(|s| s.to_str()), Some("fresh.zip"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_entry_names_are_sanitized() {
        // segmen '..' ditolak, leading '/' dan ':' diganti '_'
        assert_eq!(sanitize_zip_name("../../etc/passwd"), "_/_/etc/passwd");
        assert_eq!(sanitize_zip_name("/abs/path"), "_/abs/path");
        assert_eq!(sanitize_zip_name("a:b/c.txt"), "a_b/c.txt");
        assert_eq!(sanitize_zip_name("./x"), "_/x");
        assert_eq!(sanitize_zip_name(""), "_");
    }

    #[test]
    fn media_extensions_use_stored_compression() {
        for name in [
            "a.jpg", "a.JPEG", "a.png", "a.gif", "a.webp", "a.mp4", "a.mov", "a.avi",
            "a.mkv", "a.zip", "a.gz", "a.7z", "a.rar",
        ] {
            assert!(is_stored_extension(name), "{name} harus Stored");
        }
        assert!(!is_stored_extension("a.txt"));
        assert!(!is_stored_extension("a.json"));
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;

    fn client() -> Option<S3Client> {
        test_conn(false).map(|conn| S3Client::new(&conn))
    }

    /// Butuh endpoint S3 dari env `OS3_TEST_*`. Di-skip bila tidak tersedia.
    #[tokio::test]
    async fn zip_and_download_roundtrip() {
        let Some(c) = client() else { return; };
        let buckets = match c.list_buckets().await {
            Ok(b) => b,
            Err(_) => {
                eprintln!("SKIP: MinIO tidak tersedia");
                return;
            }
        };
        assert!(!buckets.is_empty());
        let bucket = "images";
        let prefix = "os3test/";

        // Seed data uji (idempoten) supaya test bisa jalan di mesin mana pun.
        for (key, body) in [
            (format!("{prefix}alpha.txt"), b"hello alpha\n".to_vec()),
            (format!("{prefix}proj/a.txt"), b"aaa\n".repeat(100)),
            (format!("{prefix}proj/sub/b.txt"), b"bbb\n".repeat(200)),
            (format!("{prefix}proj/sub/c.txt"), b"ccc\n".repeat(50)),
        ] {
            c.client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(ByteStream::from(body))
                .send()
                .await
                .expect("seed object");
        }

        let dir = std::env::temp_dir().join(format!("os3_dltest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1) ZIP satu folder (proj/) -> harus memuat seluruh isinya
        let zip_path = unique_path(&dir, "proj.zip");
        let items = vec![(
            format!("{prefix}proj/"),
            "proj".to_string(),
        )];
        let (count, total) = zip_objects(&c, bucket, &items, &zip_path).await.unwrap();
        assert_eq!(count, 3, "proj/ berisi a.txt, sub/b.txt, sub/c.txt");
        assert!(total > 0);
        assert!(zip_path.exists());

        // verifikasi isi zip
        let f = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(f).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"proj/a.txt".to_string()), "names={names:?}");
        assert!(names.contains(&"proj/sub/b.txt".to_string()), "names={names:?}");
        assert!(names.contains(&"proj/sub/c.txt".to_string()), "names={names:?}");

        // 2) unduh file tunggal via stream
        let single = unique_path(&dir, "alpha.txt");
        let mut out = Vec::new();
        let mut progress_calls = 0u64;
        let n = c
            .download_object_stream(bucket, &format!("{prefix}alpha.txt"), &mut out, |_w| {
                progress_calls += 1
            })
            .await
            .unwrap();
        assert!(progress_calls > 0, "callback progress harus dipanggil minimal sekali");
        assert!(n > 0);
        std::fs::write(&single, &out).unwrap();
        assert_eq!(std::fs::read_to_string(&single).unwrap(), "hello alpha\n");

        // 3) unique_path: nama bentrok harus dapat suffix
        let again = unique_path(&dir, "alpha.txt");
        assert_ne!(again, single);
        assert!(again.to_string_lossy().contains("(1)"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod upload_repro {
    use super::*;

    fn client() -> Option<S3Client> {
        test_conn(false).map(|conn| S3Client::new(&conn))
    }

    #[tokio::test]
    async fn upload_into_fresh_folder() {
        let Some(c) = client() else { return; };
        if c.list_buckets().await.is_err() {
            eprintln!("SKIP: MinIO tidak tersedia");
            return;
        }
        let bucket = "images";
        let dir = format!("os3uptest-{}/", std::process::id());
        // 1) buat folder (placeholder)
        c.put_folder(bucket, &dir).await.expect("put_folder");
        // 2) upload file json ke dalamnya, persis alur user
        let p = std::env::temp_dir().join("os3_up_repro.json");
        std::fs::write(&p, br#"{"hello":"world"}"#).unwrap();
        let key = format!("{dir}data.json");
        let res = c.put_file(bucket, &key, &p).await;
        match &res {
            Ok(_) => eprintln!("UPLOAD OK: {key}"),
            Err(e) => eprintln!("UPLOAD ERROR: {e}"),
        }
        // verbose error untuk diagnosis
        let body = ByteStream::from_path(&p).await.unwrap();
        if let Err(err) = c
            .client
            .put_object()
            .bucket(bucket)
            .key(&key)
            .body(body)
            .send()
            .await
        {
            eprintln!("RAW DEBUG: {err:?}");
            if let Some(se) = err.as_service_error() {
                eprintln!("SERVICE DETAIL: {se:?}");
            }
        }
        // cleanup
        let keys = c.list_all_keys(bucket, &dir).await.unwrap_or_default();
        let _ = c.delete_keys(bucket, keys).await;
        let _ = std::fs::remove_file(&p);
        res.expect("put_file harus sukses");
    }
}

#[cfg(test)]
mod upload_sizes {
    use super::*;

    fn client() -> Option<S3Client> {
        test_conn(false).map(|conn| S3Client::new(&conn))
    }

    #[tokio::test]
    async fn upload_various_sizes() {
        let Some(c) = client() else { return; };
        if c.list_buckets().await.is_err() {
            eprintln!("SKIP");
            return;
        }
        let bucket = "images";
        let dir = format!("os3sizetest-{}/", std::process::id());
        c.put_folder(bucket, &dir).await.ok();
        for (label, size) in [
            ("empty", 0usize),
            ("tiny", 8),
            ("small", 15 * 1024),
            ("above_16k", 17 * 1024),
            ("1mb", 1024 * 1024),
            ("6mb", 6 * 1024 * 1024),
        ] {
            let p = std::env::temp_dir().join(format!("os3_size_{label}.json"));
            let data: Vec<u8> = (0..size).map(|i| b"{}[],:\"abc\n"[i % 11]).collect();
            std::fs::write(&p, &data).unwrap();
            let key = format!("{dir}{label}.json");
            match c.put_file(bucket, &key, &p).await {
                Ok(_) => eprintln!("OK   {label:<10} {size} bytes"),
                Err(e) => eprintln!("FAIL {label:<10} {size} bytes -> {e}"),
            }
            let _ = std::fs::remove_file(&p);
        }
        let keys = c.list_all_keys(bucket, &dir).await.unwrap_or_default();
        let _ = c.delete_keys(bucket, keys).await;
    }
}

#[cfg(test)]
mod error_message_tests {
    use super::*;

    #[tokio::test]
    async fn error_is_descriptive_not_generic() {
        // Endpoint yang pasti gagal -> pesan harus menjelaskan penyebab,
        // bukan sekadar "service error".
        let bad = S3Client::new(&Connection {
            name: "bad".into(),
            endpoint: "http://127.0.0.1:1".into(),
            access_key: "dummy".into(),
            secret_key: "dummy".into(),
            region: None,
            disable_payload_signing: false,
        });
        let p = std::env::temp_dir().join("os3_err.json");
        std::fs::write(&p, b"{}").unwrap();
        let err = bad.put_file("nope", "a.json", &p).await.unwrap_err();
        let _ = std::fs::remove_file(&p);
        eprintln!("PESAN ERROR: {err}");
        assert!(
            err != "service error" && err.len() > 20,
            "pesan error terlalu umum: {err}"
        );
        assert!(
            !err.starts_with("service error"),
            "masih generik: {err}"
        );
    }
}

#[cfg(test)]
mod big_upload {
    use super::*;

    fn client() -> Option<S3Client> {
        test_conn(false).map(|conn| S3Client::new(&conn))
    }

    #[tokio::test]
    async fn upload_large_json() {
        let Some(c) = client() else { return; };
        if c.list_buckets().await.is_err() {
            eprintln!("SKIP");
            return;
        }
        let bucket = "images";
        let dir = format!("os3big-{}/", std::process::id());
        c.put_folder(bucket, &dir).await.ok();
        // ~20MB json-like
        let p = std::env::temp_dir().join("os3_big.json");
        let mut data = String::from("{");
        for i in 0..120_000 {
            data.push_str(&format!("\"k{i}\":\"value-{i}\","));
        }
        data.push_str("\"end\":1}");
        std::fs::write(&p, &data).unwrap();
        let size = std::fs::metadata(&p).unwrap().len();
        eprintln!("ukuran file: {:.1} MB", size as f64 / 1048576.0);
        let key = format!("{dir}big.json");
        match c.put_file(bucket, &key, &p).await {
            Ok(_) => eprintln!("BIG UPLOAD OK ({size} bytes)"),
            Err(e) => eprintln!("BIG UPLOAD FAIL: {e}"),
        }
        // verifikasi ukuran di server
        let got = c.get_bytes(bucket, &key, None).await.unwrap_or_default();
        eprintln!("terbaca kembali: {} bytes (match={})", got.len(), got.len() as u64 == size);
        let keys = c.list_all_keys(bucket, &dir).await.unwrap_or_default();
        let _ = c.delete_keys(bucket, keys).await;
        let _ = std::fs::remove_file(&p);
    }
}

