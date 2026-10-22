use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Connection {
    pub name: String,
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub region: Option<String>,
    /// Matikan payload signing (`x-amz-content-sha256` = UNSIGNED-PAYLOAD).
    ///
    /// Diperlukan untuk sebagian server S3-compatible (mis. Dell ECS / ViPR)
    /// yang menghitung ulang signature dengan hash body kosong; bila payload
    /// ditandatangani, server membalas `SignatureDoesNotMatch` /
    /// `XAmzContentSHA256Mismatch` sehingga yang tampil hanya "service error".
    #[serde(default)]
    pub disable_payload_signing: bool,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("access_key", &self.access_key)
            .field("secret_key", &"***")
            .field("region", &self.region)
            .field("disable_payload_signing", &self.disable_payload_signing)
            .finish()
    }
}

impl Connection {
    pub fn region_or_default(&self) -> String {
        self.region.clone().unwrap_or_else(|| "us-east-1".to_string())
    }

    /// Validasi endpoint dan nama koneksi.
    ///
    /// Endpoint harus URL `http://` atau `https://` dengan host tidak kosong,
    /// port (bila ada) berupa angka 1..=65535, dan tanpa userinfo
    /// (`user:pass@host`) karena kredensial dikelola di field terpisah.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("nama koneksi kosong");
        }
        validate_endpoint(&self.endpoint)
            .map_err(|e| anyhow!("koneksi '{}': endpoint '{}' tidak valid: {e}", self.name, self.endpoint))
    }
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    let rest = if let Some(r) = endpoint.strip_prefix("http://") {
        r
    } else if let Some(r) = endpoint.strip_prefix("https://") {
        r
    } else {
        bail!("harus diawali http:// atau https://");
    };

    // authority = bagian sebelum path/query/fragment
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        bail!("host kosong");
    }
    if authority.contains('@') {
        bail!("userinfo (user:pass@) tidak diizinkan");
    }

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        // IPv6 literal: [::1]:9000
        let end = after_bracket
            .find(']')
            .ok_or_else(|| anyhow!("IPv6 literal tidak ditutup ']'"))?;
        let host = &after_bracket[..end];
        let after = &after_bracket[end + 1..];
        let port = if after.is_empty() {
            None
        } else {
            Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| anyhow!("format authority tidak valid"))?,
            )
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            bail!("IPv6 literal harus diapit '[' dan ']'");
        }
        (host, Some(port))
    } else {
        (authority, None)
    };

    if host.is_empty() {
        bail!("host kosong");
    }
    if host.chars().any(|c| c.is_whitespace()) {
        bail!("host mengandung spasi");
    }
    if let Some(port) = port {
        let port: u16 = port
            .parse()
            .map_err(|_| anyhow!("port '{port}' tidak valid"))?;
        if port == 0 {
            bail!("port 0 tidak valid");
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default)]
    pub icon_style: crate::theme::IconStyle,
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("os3")
        .join("config.toml")
}

pub fn load() -> Result<(Config, PathBuf)> {
    let path = config_path();
    let cfg = load_from(&path)?;
    Ok((cfg, path))
}

fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        let cfg = Config {
            connections: vec![Connection {
                name: "local-minio".to_string(),
                endpoint: "http://localhost:9000".to_string(),
                // isi via form atau env
                access_key: String::new(),
                // isi via form atau env
                secret_key: String::new(),
                region: None,
                disable_payload_signing: false,
            }],
            icon_style: crate::theme::IconStyle::default(),
        };
        save(&cfg, path)?;
        return Ok(cfg);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Config berisi secret key: pastikan hanya owner yang bisa membacanya.
    #[cfg(unix)]
    tighten_permissions(path)?;

    // `message()` hanya berisi pesan tanpa snippet baris/posisi, sehingga secret
    // yang mungkin ikut terserialisasi di config tidak bocor ke output error.
    let cfg: Config = toml::from_str(&raw)
        .map_err(|e| anyhow!("config {} tidak valid: {}", path.display(), e.message()))?;

    let mut seen = HashSet::new();
    for conn in &cfg.connections {
        if !seen.insert(conn.name.clone()) {
            bail!("nama koneksi duplikat: {}", conn.name);
        }
    }
    for conn in &cfg.connections {
        conn.validate()
            .with_context(|| format!("config {} tidak valid", path.display()))?;
    }

    Ok(cfg)
}

#[cfg(unix)]
fn tighten_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = fs::metadata(path)?;
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("gagal set permission 0600 pada {}", path.display()))?;
    }
    Ok(())
}

pub fn save(cfg: &Config, path: &Path) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let raw = toml::to_string_pretty(cfg)?;

    // Tulis atomik + permission 0600: file tmp dibuat baru (create_new) supaya
    // tidak pernah menulis ke file yang sudah ada dengan permission longgar.
    let tmp = path.with_extension("toml.tmp");
    let write_result = (|| -> Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = match opts.open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // sisa proses sebelumnya yang gagal; buang lalu coba sekali lagi
                fs::remove_file(&tmp)?;
                opts.open(&tmp)?
            }
            Err(e) => return Err(e.into()),
        };
        f.write_all(raw.as_bytes())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::rename(&tmp, path)?;

    // fsync direktori agar entry rename benar-benar persisten.
    #[cfg(unix)]
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(endpoint: &str) -> Connection {
        Connection {
            name: "test".to_string(),
            endpoint: endpoint.to_string(),
            access_key: "a".to_string(),
            secret_key: "s".to_string(),
            region: None,
            disable_payload_signing: false,
        }
    }

    #[test]
    fn valid_endpoints_accepted() {
        for ep in [
            "http://localhost:9000",
            "https://s3.example.com",
            "http://127.0.0.1:9000/",
            "https://[::1]:9443",
        ] {
            assert!(conn(ep).validate().is_ok(), "harus valid: {ep}");
        }
    }

    #[test]
    fn invalid_endpoints_rejected() {
        for ep in [
            "localhost:9000",
            "ftp://localhost:9000",
            "http://",
            "http://user:pass@host:9000",
            "http://host:0",
            "http://host:99999",
            "http://host:abc",
        ] {
            assert!(conn(ep).validate().is_err(), "harus invalid: {ep}");
        }
    }

    #[test]
    fn debug_redacts_secret() {
        let dbg = format!("{:?}", conn("http://localhost:9000"));
        assert!(dbg.contains("secret_key: \"***\""));
        assert!(!dbg.contains("secret_key: \"s\""));
    }

    fn test_path(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("config-tests")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn write(path: &Path, raw: &str) {
        fs::write(path, raw).unwrap();
    }

    #[test]
    fn duplicate_names_rejected() {
        let path = test_path("dup");
        write(
            &path,
            r#"
[[connections]]
name = "sama"
endpoint = "http://localhost:9000"
access_key = "a"
secret_key = "s"

[[connections]]
name = "sama"
endpoint = "http://localhost:9001"
access_key = "a"
secret_key = "s"
"#,
        );
        let err = load_from(&path).unwrap_err().to_string();
        assert!(err.contains("nama koneksi duplikat: sama"), "{err}");
    }

    #[test]
    fn invalid_endpoint_in_config_is_clear_error() {
        let path = test_path("bad-endpoint");
        write(
            &path,
            r#"
[[connections]]
name = "bad"
endpoint = "localhost:9000"
access_key = "a"
secret_key = "s"
"#,
        );
        let err = load_from(&path).unwrap_err().to_string();
        assert!(err.contains("tidak valid"), "{err}");
        assert!(err.contains("endpoint"), "{err}");
    }

    #[test]
    fn toml_error_has_no_line_snippet() {
        let path = test_path("toml-error");
        write(&path, "icon_style = \"ascii\"\nsecret_key = \"leak-me\"\nbukan toml\n");
        let err = load_from(&path).unwrap_err().to_string();
        assert!(err.contains("tidak valid"), "{err}");
        assert!(!err.contains("leak-me"), "snippet bocor: {err}");
        assert!(!err.contains("bukan toml"), "snippet bocor: {err}");
    }

    #[test]
    fn unknown_icon_style_does_not_fail_load() {
        let path = test_path("icon-style");
        write(&path, "icon_style = \"tidak-ada\"\n");
        let cfg = load_from(&path).expect("harus fallback, bukan error");
        assert_eq!(cfg.icon_style, crate::theme::IconStyle::Ascii);
    }

    #[cfg(unix)]
    #[test]
    fn load_tightens_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = test_path("perm");
        write(&path, "icon_style = \"ascii\"\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        load_from(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode aktual: {mode:o}");
    }

    #[test]
    fn save_is_atomic_0600_and_roundtrips() {
        let path = test_path("save");
        let cfg = Config {
            connections: vec![conn("http://localhost:9000")],
            icon_style: crate::theme::IconStyle::Ascii,
        };
        save(&cfg, &path).unwrap();
        assert!(!path.with_extension("toml.tmp").exists(), "tmp harus dibersihkan");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode aktual: {mode:o}");
        }
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].endpoint, "http://localhost:9000");

        // save ulang (menimpa) tetap berhasil dan tetap 0600
        save(&loaded, &path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode aktual: {mode:o}");
        }
    }

    #[test]
    fn missing_config_uses_empty_credentials() {
        let path = test_path("default").with_file_name("belum-ada.toml");
        let cfg = load_from(&path).unwrap();
        assert!(path.exists());
        assert_eq!(cfg.connections.len(), 1);
        assert_eq!(cfg.connections[0].name, "local-minio");
        assert!(cfg.connections[0].access_key.is_empty());
        assert!(cfg.connections[0].secret_key.is_empty());
    }
}
