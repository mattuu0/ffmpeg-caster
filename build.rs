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

/// 常時実行(Windowsのみ実質的に意味を持つ): `ffmpeg_stub`(ワークスペース
/// 内の別クレート、`elevate::spawn_elevated`がPAExec経由で起動する
/// コンソール無し中継バイナリ)をこのビルドの一部としてビルドし、その`.exe`を
/// `OUT_DIR/ffmpeg_stub.exe`に配置する。`elevate.rs`の`include_bytes!`から
/// 参照され、実行時に一時ディレクトリへ書き出して使われる(利用者が
/// `ffmpeg_stub.exe`を別途同梱・配置する必要がなくなる)。
///
/// `cargo build -p ffmpeg_stub`をサブプロセスとして呼ぶ。ワークスペース内の
/// 別クレートを一方向にビルドするだけなので、ffmpeg_stub自体がこのビルド
/// スクリプトの完了を待つような逆方向の依存にはならず、循環ビルドにはならない
/// (ffmpeg_stub/src/main.rsのコメント参照: 循環を避けるために別クレートへ
/// 分離したのはこのライブラリと同一パッケージのbinターゲットにしないためで、
/// ここでのサブプロセス呼び出しとは別の話)。
#[cfg(target_os = "windows")]
fn build_and_embed_ffmpeg_stub(out_dir: &Path) {
    use std::process::Command;

    println!("cargo:rerun-if-changed=ffmpeg_stub/src/main.rs");
    println!("cargo:rerun-if-changed=ffmpeg_stub/Cargo.toml");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let workspace_root = Path::new(&manifest_dir);

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let stub_target_dir = out_dir.join("ffmpeg_stub_target");

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(workspace_root)
        .args(["build", "-p", "ffmpeg_stub", "--target-dir"])
        .arg(&stub_target_dir)
        .args(if profile == "release" {
            vec!["--release"]
        } else {
            vec![]
        })
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `cargo build -p ffmpeg_stub`: {e}"));
    if !status.success() {
        panic!("`cargo build -p ffmpeg_stub` failed with status {status:?}");
    }

    let built_exe = stub_target_dir.join(&profile).join("ffmpeg_stub.exe");
    let dest = out_dir.join("ffmpeg_stub.exe");
    fs::copy(&built_exe, &dest).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} to {}: {e}",
            built_exe.display(),
            dest.display()
        )
    });
}

#[cfg(not(target_os = "windows"))]
fn build_and_embed_ffmpeg_stub(_out_dir: &Path) {}

/// `bundled` feature有効時のみ実行される。ビルドホストのOS/archに対応する
/// ffmpeg zipを用意し、OUT_DIR/ffmpeg-bundled.zipへ配置する。
/// downloader.rsのinclude_bytes!から参照される。
///
/// 解決順序:
/// 1. `assets/`配下に事前配置されたzipがあればそれを使う(手動配置・CI等で
///    事前キャッシュしたい場合の経路)。
/// 2. 無ければ`mattuu0/ffmpeg-builder`の最新リリースから自動ダウンロードし、
///    次回以降のビルドで再ダウンロードしないよう`assets/`にキャッシュする。
///
/// クロスコンパイル時は「ビルドを実行しているホスト自身」向けのOS/arch判定を
/// 行う(PLAN.md参照)。別ターゲット向けに埋め込みたい場合は、`assets/`に
/// そのターゲット用のzipを事前配置しておくこと(自動ダウンロードはホスト用の
/// ファイル名しか解決しない)。
fn main() {
    println!("cargo:rerun-if-changed=assets");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_dir = Path::new(&out_dir);
    build_and_embed_ffmpeg_stub(out_dir);

    if env::var("CARGO_FEATURE_BUNDLED").is_err() {
        return;
    }

    let dest = out_dir.join("ffmpeg-bundled.zip");

    let platform = match env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        other => panic!("bundled feature: unsupported OS: {other}"),
    };
    let arch = match env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("bundled feature: unsupported architecture: {other}"),
    };

    let asset_name = format!("ffmpeg-{platform}-{arch}-binary-only.zip");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let assets_dir = Path::new(&manifest_dir).join("assets");
    let cached = assets_dir.join(&asset_name);

    if cached.is_file() {
        println!("cargo:warning=bundled: using cached {}", cached.display());
        fs::copy(&cached, &dest).unwrap_or_else(|e| {
            panic!(
                "failed to copy {} to {}: {e}",
                cached.display(),
                dest.display()
            )
        });
        return;
    }

    println!(
        "cargo:warning=bundled: {} not found, downloading latest {REPO} release from GitHub...",
        cached.display()
    );
    download_and_cache(&asset_name, &assets_dir, &cached).unwrap_or_else(|e| {
        panic!(
            "bundled feature: failed to auto-download {asset_name} from {REPO}: {e}\n\
             (place the zip at {} manually to skip auto-download, e.g. when building offline)",
            cached.display()
        )
    });

    fs::copy(&cached, &dest).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} to {}: {e}",
            cached.display(),
            dest.display()
        )
    });
}

fn download_and_cache(asset_name: &str, assets_dir: &Path, dest: &Path) -> Result<(), String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = ureq::get(&url)
        .set("User-Agent", "ffmpeg-caster-build-rs")
        .call()
        .map_err(|e| format!("GitHub API request failed: {e}"))?;
    let release: Release = response
        .into_json()
        .map_err(|e| format!("failed to parse GitHub API response: {e}"))?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("no asset named \"{asset_name}\" in latest release"))?;

    fs::create_dir_all(assets_dir)
        .map_err(|e| format!("failed to create {}: {e}", assets_dir.display()))?;

    let tmp_dest: PathBuf = dest.with_extension("zip.partial");
    let response = ureq::get(&asset.browser_download_url)
        .set("User-Agent", "ffmpeg-caster-build-rs")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut file = fs::File::create(&tmp_dest)
        .map_err(|e| format!("failed to create {}: {e}", tmp_dest.display()))?;
    io::copy(&mut response.into_reader(), &mut file)
        .map_err(|e| format!("failed to write {}: {e}", tmp_dest.display()))?;
    drop(file);
    fs::rename(&tmp_dest, dest)
        .map_err(|e| format!("failed to finalize {}: {e}", dest.display()))?;

    Ok(())
}
