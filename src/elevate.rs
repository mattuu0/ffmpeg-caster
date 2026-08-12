//! rust-cast(sender/src-tauri/src/elevate.rs + paexec_setup.rs)から移植した
//! SYSTEM昇格キャプチャ(Windowsのみ)。DXGI Desktop DuplicationはUACの
//! セキュアデスクトップ(同意プロンプト画面)を、SYSTEM権限から起動された
//! 場合のみキャプチャできる。
//!
//! 非Windowsではこのモジュールはno-op(`ElevationMode`が常に`Normal`扱いになる)。
//!
//! 重要な前提: `spawn_elevated`/`spawn_preferring_system`は、呼び出し元プロセス
//! 自身が既に管理者権限で起動していることを前提とする。PAExecはローカルの
//! サービス制御マネージャに一時サービスをインストールしてSYSTEM権限の
//! プロセスを生成する仕組みのため、サービスのインストール自体に管理者権限が
//! 必要になる。呼び出し元アプリが管理者権限でない場合、PAExecのサービス
//! インストールが失敗し、自動的に通常起動へフォールバックする(この場合UAC
//! セキュアデスクトップはキャプチャできないが、通常のデスクトップキャプチャ
//! 自体は機能する)。アプリ全体の自己UAC昇格(起動方法そのもの)はこの
//! ライブラリの責務ではなく、呼び出し側アプリが行うこと。

use crate::error::Result;

/// パイプライン起動時のSYSTEM昇格方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ElevationMode {
    /// 通常権限(現在のプロセスと同じ権限)で起動する。
    #[default]
    Normal,
    /// PAExec経由のSYSTEM起動を優先する。失敗したら通常起動にフォールバックする。
    PreferSystem,
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::process::{Child, Command};

    const PAEXEC_DOWNLOAD_URL: &str = "https://www.poweradmin.com/paexec/paexec.exe";

    /// アプリのデータディレクトリ配下、tools/ に保存するpaexec.exeのパス。
    pub fn bundled_paexec_path(dest_dir: &Path) -> PathBuf {
        dest_dir.join("paexec.exe")
    }

    /// paexec.exeがダウンロード済みかどうか。
    pub fn is_paexec_installed(dest_dir: &Path) -> bool {
        bundled_paexec_path(dest_dir).is_file()
    }

    /// 既にダウンロード済みならそのパスを返し、未取得なら公式配布元
    /// (poweradmin.com、PsExecの再実装で再配布自由なOSSツール)から
    /// ダウンロードしてから返す。
    pub fn ensure_paexec(dest_dir: &Path) -> Result<PathBuf> {
        let path = bundled_paexec_path(dest_dir);
        if path.is_file() {
            return Ok(path);
        }
        std::fs::create_dir_all(dest_dir)
            .map_err(|e| format!("failed to create {}: {e}", dest_dir.display()))?;
        download_paexec_to(&path)?;
        Ok(path)
    }

    fn download_paexec_to(path: &Path) -> Result<()> {
        let response = ureq::get(PAEXEC_DOWNLOAD_URL)
            .set("User-Agent", "ffmpeg-caster")
            .call()
            .map_err(|e| format!("paexec.exe download failed: {e}"))?;
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        std::io::copy(&mut response.into_reader(), &mut file)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(())
    }

    /// `build.rs`がこのライブラリのビルドの一部として`ffmpeg_stub`
    /// (ワークスペース内の別クレート)をビルドし、その`.exe`をここへ埋め込む。
    /// 利用者側はffmpeg_stub.exeを別途同梱・配置する必要がない。
    const EMBEDDED_FFMPEG_STUB: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg_stub.exe"));

    /// ffmpeg_stub.exeを解決する。`explicit`が指定されていればそれを使う
    /// (呼び出し側が独自にビルド・配置したstubを使いたい場合の抜け道)。
    /// 指定が無ければ、埋め込み済みバイナリを`tools_dir/ffmpeg_stub.exe`へ
    /// 書き出して使う(既に同一内容が書き出し済みならスキップする)。
    pub fn ensure_ffmpeg_stub(tools_dir: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            if p.is_file() {
                return Ok(p.to_path_buf());
            }
            return Err(format!("ffmpeg_stub not found at explicit path {}", p.display()).into());
        }

        std::fs::create_dir_all(tools_dir)
            .map_err(|e| format!("failed to create {}: {e}", tools_dir.display()))?;
        let stub_path = tools_dir.join("ffmpeg_stub.exe");

        let needs_write = match std::fs::read(&stub_path) {
            Ok(existing) => existing != EMBEDDED_FFMPEG_STUB,
            Err(_) => true,
        };
        if needs_write {
            std::fs::write(&stub_path, EMBEDDED_FFMPEG_STUB).map_err(|e| {
                format!(
                    "failed to write embedded ffmpeg_stub to {}: {e}",
                    stub_path.display()
                )
            })?;
        }
        Ok(stub_path)
    }

    fn new_hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x00008000;
        let mut cmd = Command::new(program);
        cmd.creation_flags(CREATE_NO_WINDOW | ABOVE_NORMAL_PRIORITY_CLASS);
        cmd
    }

    /// SYSTEM権限で任意のプログラム(主にffmpeg)を起動する。PAExecが直接起動
    /// する対象は`program_path`(ffmpeg.exe)ではなく、同梱のffmpeg_stub.exe
    /// (GUIサブシステムの中継プロセス)。PAExecの`-i`(対話的セッション)指定
    /// 時はターゲットプロセスのSTARTUPINFO.wShowWindowを常にSW_SHOWにし
    /// CREATE_NEW_CONSOLEでコンソールを割り当てる仕様のため、コンソールを
    /// 持たないGUIサブシステムのスタブを代わりに起動する。
    pub fn spawn_elevated(
        paexec_path: &Path,
        stub_path: &Path,
        program_path: &Path,
        args: &[String],
        idr_relay_port: u16,
        idr_control_path: &str,
    ) -> Result<Child> {
        let working_dir = program_path.parent().ok_or_else(|| {
            format!(
                "program_path has no parent directory: {}",
                program_path.display()
            )
        })?;

        new_hidden_command(paexec_path)
            .arg("-i")
            .arg("-s")
            .arg("-w")
            .arg(working_dir)
            .arg(stub_path)
            .arg(idr_relay_port.to_string())
            .arg(idr_control_path)
            .arg(program_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn PAExec: {e}").into())
    }

    /// 通常権限(現在のプロセスと同じ権限)でプログラムを起動する。
    pub fn spawn_normal(program_path: &Path, args: &[String]) -> Result<Child> {
        new_hidden_command(program_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn program normally: {e}").into())
    }

    /// PAExec経由(SYSTEM権限)を優先し、失敗したら通常起動にフォールバックして
    /// プログラム(主にffmpeg)を起動する。
    pub async fn spawn_preferring_system(
        tools_dir: &Path,
        stub_path_override: Option<&Path>,
        program_path: &Path,
        args: &[String],
        idr_relay_port: u16,
        idr_control_path: &str,
    ) -> Result<Child> {
        match try_spawn_elevated(
            tools_dir,
            stub_path_override,
            program_path,
            args,
            idr_relay_port,
            idr_control_path,
        ) {
            Ok(child) => return Ok(child),
            Err(_e) => {
                // フォールスルーして通常起動を試す。
            }
        }

        spawn_normal(program_path, args).map_err(|e| {
            format!("failed to spawn program (both elevated and normal attempts failed): {e}")
                .into()
        })
    }

    fn try_spawn_elevated(
        tools_dir: &Path,
        stub_path_override: Option<&Path>,
        program_path: &Path,
        args: &[String],
        idr_relay_port: u16,
        idr_control_path: &str,
    ) -> Result<Child> {
        let stub_path = ensure_ffmpeg_stub(tools_dir, stub_path_override)?;
        let paexec_path = ensure_paexec(tools_dir)?;
        spawn_elevated(
            &paexec_path,
            &stub_path,
            program_path,
            args,
            idr_relay_port,
            idr_control_path,
        )
    }

    #[allow(dead_code)]
    const PAEXEC_SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
}

#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(not(target_os = "windows"))]
mod noop_impl {
    use super::*;
    use std::path::{Path, PathBuf};
    use tokio::process::{Child, Command};

    pub fn ensure_paexec(_dest_dir: &Path) -> Result<PathBuf> {
        Err("paexec/SYSTEM elevation is only supported on Windows".into())
    }

    pub fn spawn_normal(program_path: &Path, args: &[String]) -> Result<Child> {
        Command::new(program_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn program: {e}").into())
    }

    /// 非Windowsでは常に通常起動する(ElevationModeがPreferSystemでもno-op)。
    pub async fn spawn_preferring_system(
        _tools_dir: &Path,
        _stub_path_override: Option<&Path>,
        program_path: &Path,
        args: &[String],
        _idr_relay_port: u16,
        _idr_control_path: &str,
    ) -> Result<Child> {
        spawn_normal(program_path, args)
    }
}

#[cfg(not(target_os = "windows"))]
pub use noop_impl::*;
