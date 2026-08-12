//! rust-cast(poc/sender_poc/src/ffmpeg_downloader.rs)から移植。
//! mattuu0/ffmpeg-builder(-idr_control_socketパッチ入りフォーク)の
//! 最新リリースからOS/アーキテクチャに応じたビルドをダウンロードし、
//! `dest_dir`配下に展開する。既に展開済みならダウンロードをスキップする。
//!
//! `bundled` feature有効時は、ネットワークダウンロードの代わりにビルド時に
//! 埋め込んだzip(`include_bytes!`、`build.rs`で用意)を展開する。

use crate::error::Result;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const REPO: &str = "mattuu0/ffmpeg-builder";

#[derive(serde::Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn detect_platform() -> Result<&'static str> {
    match env::consts::OS {
        "windows" => Ok("windows"),
        "macos" => Ok("macos"),
        "linux" => Ok("linux"),
        other => Err(format!("unsupported OS: {other}").into()),
    }
}

fn detect_arch() -> Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => Err(format!("unsupported architecture: {other}").into()),
    }
}

fn ffmpeg_binary_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn find_ffmpeg_binary(dest_dir: &Path) -> Option<PathBuf> {
    let binary_name = ffmpeg_binary_name();

    let direct = dest_dir.join("bin").join(binary_name);
    if direct.exists() {
        return Some(direct);
    }

    if let Ok(entries) = fs::read_dir(dest_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("bin").join(binary_name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn fetch_latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = ureq::get(&url)
        .set("User-Agent", "ffmpeg-caster")
        .call()
        .map_err(|e| format!("GitHub API request failed: {e}"))?;
    response
        .into_json()
        .map_err(|e| format!("failed to parse GitHub API response: {e}").into())
}

fn pick_asset<'a>(release: &'a Release, platform: &str, arch: &str) -> Result<&'a Asset> {
    let wanted = format!("ffmpeg-{platform}-{arch}-binary-only.zip");
    release
        .assets
        .iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| format!("no asset named \"{wanted}\" in latest release").into())
}

fn download_to_file(url: &str, dest: &Path) -> Result<()> {
    let response = ureq::get(url)
        .set("User-Agent", "ffmpeg-caster")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut file =
        fs::File::create(dest).map_err(|e| format!("failed to create {}: {e}", dest.display()))?;
    io::copy(&mut response.into_reader(), &mut file)
        .map_err(|e| format!("failed to write {}: {e}", dest.display()))?;
    Ok(())
}

fn extract_zip_bytes(zip_bytes: &[u8], dest_dir: &Path) -> Result<()> {
    let reader = io::Cursor::new(zip_bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("failed to read zip: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("failed to read zip entry {i}: {e}"))?;
        let out_path = match entry.enclosed_name() {
            Some(p) => dest_dir.join(p),
            None => continue,
        };

        if entry.name().ends_with('/') {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("failed to create dir {}: {e}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create dir {}: {e}", parent.display()))?;
        }
        let mut out_file = fs::File::create(&out_path)
            .map_err(|e| format!("failed to create {}: {e}", out_path.display()))?;
        io::copy(&mut entry, &mut out_file)
            .map_err(|e| format!("failed to write {}: {e}", out_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn extract_zip_file(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    let bytes =
        fs::read(zip_path).map_err(|e| format!("failed to open {}: {e}", zip_path.display()))?;
    extract_zip_bytes(&bytes, dest_dir)
}

#[cfg(feature = "bundled")]
fn bundled_zip_bytes() -> Result<&'static [u8]> {
    // build.rsが対象OS/arch用のffmpeg zipをOUT_DIR/ffmpeg-bundled.zipに用意し、
    // ここでinclude_bytes!する。クロスコンパイル時はビルドしているホスト自身の
    // OS/archのzipを埋め込む前提(クロスターゲットの明示指定が必要)。
    Ok(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/ffmpeg-bundled.zip"
    )))
}

/// `dest_dir`にffmpegが既に展開されていればそれを返す。無ければ、`bundled`
/// feature有効時はビルド時に埋め込んだzipを展開し、無効時は最新リリースを
/// ネットワークからダウンロード・展開してから返す。
pub fn ensure_ffmpeg(dest_dir: &Path) -> Result<PathBuf> {
    if let Some(existing) = find_ffmpeg_binary(dest_dir) {
        return Ok(existing);
    }

    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("failed to create {}: {e}", dest_dir.display()))?;

    #[cfg(feature = "bundled")]
    {
        let zip_bytes = bundled_zip_bytes()?;
        extract_zip_bytes(zip_bytes, dest_dir)?;
    }

    #[cfg(not(feature = "bundled"))]
    {
        let platform = detect_platform()?;
        let arch = detect_arch()?;

        let release = fetch_latest_release()?;
        let asset = pick_asset(&release, platform, arch)?;

        let tmp_zip = env::temp_dir().join(&asset.name);
        download_to_file(&asset.browser_download_url, &tmp_zip)?;
        extract_zip_file(&tmp_zip, dest_dir)?;
        let _ = fs::remove_file(&tmp_zip);
    }

    find_ffmpeg_binary(dest_dir).ok_or_else(|| {
        format!(
            "extraction completed but no bin/{} was found under {}",
            ffmpeg_binary_name(),
            dest_dir.display()
        )
        .into()
    })
}
