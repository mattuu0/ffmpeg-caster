//! rust-castの画面配信アプリから抽出した、汎用的なffmpeg画面キャプチャ+
//! エンコードライブラリ。
//!
//! 想定利用フロー:
//! ```text
//! setup                      … バイナリの取得・設定
//!   ↓
//! enumerate_displays         … ディスプレイオブジェクト(一覧)を取得
//!   ↓
//! MonitorPipeline::new       … 選んだディスプレイでパイプラインオブジェクトを生成(まだ起動しない)
//!   ↓
//! subscribe_frames / subscribe_raw  … callbackを設定
//!   ↓
//! pipeline.start()           … 起動(ffmpeg spawn)
//!   ↓
//! callbackからエンコード済みデータを継続的に取得
//!   ↓
//! pipeline.stop()            … 配信終了
//! ```

pub mod display;
pub mod downloader;
pub mod elevate;
pub mod encoder;
pub mod error;
pub mod ffi;
pub mod nal;
pub mod pipeline;

use error::Result;
use std::path::{Path, PathBuf};

/// `setup()`が返す、取得済みツール群のパス。`paexec_path`はWindows以外では
/// 常に`None`。
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub ffmpeg_path: PathBuf,
    pub paexec_path: Option<PathBuf>,
}

/// `tools_dir`配下にffmpeg(・Windowsならpaexec)が既にあればダウンロードを
/// スキップし、無ければ自動取得する。冪等に動作するため何度呼んでもよい。
pub fn setup(tools_dir: &Path) -> Result<Toolchain> {
    let ffmpeg_path = downloader::ensure_ffmpeg(tools_dir)?;

    #[cfg(target_os = "windows")]
    let paexec_path = Some(elevate::ensure_paexec(tools_dir)?);
    #[cfg(not(target_os = "windows"))]
    let paexec_path = None;

    Ok(Toolchain {
        ffmpeg_path,
        paexec_path,
    })
}
