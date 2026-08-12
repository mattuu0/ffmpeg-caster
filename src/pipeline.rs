//! rust-cast(sender/src-tauri/src/streaming.rs + monitor_session.rs)から抽出・
//! 再構成した、「1モニターにつき1ffmpegプロセス」を原則とするエンコード
//! パイプライン。
//!
//! ライフサイクルは `new`(生成)→`subscribe_*`(コールバック登録)→
//! `start`(ffmpeg起動)→…→`stop`(終了) の順に分離する。`start()`が生成と
//! 起動を同時に行う一体型APIにはしない — コールバックをffmpeg起動前に
//! 登録できるようにするため。
//!
//! フレーム単位(`subscribe_frames`)と生バイト列(`subscribe_raw`)の両方を
//! 提供する。両APIは同じ1本のffmpegプロセス・1本の読み取りループを共有する。
//! 購読者数が増えても追加のffmpegプロセスや読み取りループは発生しない
//! (`monitor_session.rs::FanoutRegistry`相当)。
//!
//! ffmpegが異常終了した場合は自動再起動する(`monitor_session.rs::
//! run_monitor_supervisor`相当、500msバックオフ、`find_display_by_name`に
//! よるデバイス名ベースの再解決、無限リトライ)。

use crate::display::DisplayTarget;
use crate::elevate::{self, ElevationMode};
use crate::encoder::Codec;
use crate::error::Result;
use crate::nal::{FrameChunk, NalSplitter, ParameterSets};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// フレームレート上限をユーザー設定として持たず、ビットレート上限だけで
/// 帯域・エンコード負荷を制御する方針(rust-cast streaming.rsを踏襲)。
/// GOP長・VBV bufsizeの算出や Linux/macOSの-framerate指定には基準値として
/// この定数を使う(この値自体は実際のフレームレートを制限しない)。
const ENCODER_BASELINE_FPS: u32 = 60;

/// Windows(ddagrab)専用: フィルタ自体の取得レート上限。既定値は30fpsだが、
/// 高リフレッシュレートモニターに対応するため明示的にこの値まで許可する。
#[cfg(target_os = "windows")]
const DDAGRAB_MAX_FPS: u32 = 75;

/// ffmpegが異常終了した際に自動再起動するまでの待機時間。直後に再起動を
/// 繰り返して無限ループでCPU/GPUを消費し続けないようにする。
const FFMPEG_RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Key,
    Delta,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

/// 使用するハードウェアエンコーダの種類(ベンダー)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwEncoderKind {
    Nvenc,
    Amf,
    Qsv,
}

impl HwEncoderKind {
    fn from_encoder_name(name: &str) -> Option<Self> {
        if name.ends_with("_nvenc") {
            Some(HwEncoderKind::Nvenc)
        } else if name.ends_with("_amf") {
            Some(HwEncoderKind::Amf)
        } else if name.ends_with("_qsv") {
            Some(HwEncoderKind::Qsv)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodeOptions {
    pub bitrate_kbps: u32,
    pub elevation: ElevationMode,
    /// ffmpeg_stubのパスを明示指定したい場合。既定(None)ではライブラリに
    /// 埋め込まれたffmpeg_stub.exeを`tools_dir`へ書き出して使うため、通常は
    /// 指定不要。
    pub ffmpeg_stub_path: Option<PathBuf>,
    /// PAExec/paexec.exe・埋め込みffmpeg_stub.exeの書き出し先ディレクトリ
    /// (ElevationMode::PreferSystem時のみ使用)。
    pub tools_dir: Option<PathBuf>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            bitrate_kbps: 8_000,
            elevation: ElevationMode::Normal,
            ffmpeg_stub_path: None,
            tools_dir: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubscriptionId(u64);

type FrameCallback = Arc<dyn Fn(&EncodedFrame) + Send + Sync + 'static>;
type RawCallback = Arc<dyn Fn(&[u8]) + Send + Sync + 'static>;

enum Subscriber {
    Frames(FrameCallback),
    Raw(RawCallback),
}

/// `monitor_session.rs::FanoutRegistry`相当。フレーム単位・生バイト列の両方の
/// 購読者を1つのレジストリで管理する。
#[derive(Default)]
struct FanoutRegistry {
    subscribers: Mutex<HashMap<u64, Subscriber>>,
    next_id: AtomicU64,
}

impl FanoutRegistry {
    fn subscribe_frames(&self, callback: FrameCallback) -> SubscriptionId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers
            .lock()
            .unwrap()
            .insert(id, Subscriber::Frames(callback));
        SubscriptionId(id)
    }

    fn subscribe_raw(&self, callback: RawCallback) -> SubscriptionId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers
            .lock()
            .unwrap()
            .insert(id, Subscriber::Raw(callback));
        SubscriptionId(id)
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        self.subscribers.lock().unwrap().remove(&id.0);
    }

    fn broadcast_raw(&self, chunk: &[u8]) {
        let subscribers = self.subscribers.lock().unwrap();
        for sub in subscribers.values() {
            if let Subscriber::Raw(cb) = sub {
                cb(chunk);
            }
        }
    }

    fn broadcast_frame(&self, frame: &EncodedFrame) {
        let subscribers = self.subscribers.lock().unwrap();
        for sub in subscribers.values() {
            if let Subscriber::Frames(cb) = sub {
                cb(frame);
            }
        }
    }
}

/// idr_control_socket関連のミュータブルな状態。ffmpeg再起動のたびに
/// idr_control_path/idr_relay_streamも作り直されるため、監視ループがここを
/// 都度上書きする。
struct IdrControlHandle {
    idr_control_path: String,
    idr_relay_stream: Option<tokio::net::TcpStream>,
}

struct RunningInstance {
    supervisor_handle: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
}

/// 「1モニターにつき1ffmpegプロセス」を原則とするパイプライン。
///
/// 対象ディスプレイ・コーデック・エンコードオプションを保持した「未起動」の
/// 状態のオブジェクトを`new`で作り、`subscribe_frames`/`subscribe_raw`で
/// コールバックを登録してから`start`でffmpegを起動する。
pub struct MonitorPipeline {
    ffmpeg_path: PathBuf,
    display: DisplayTarget,
    codec: Codec,
    hw_encoder: Option<HwEncoderKind>,
    options: EncodeOptions,

    fanout: Arc<FanoutRegistry>,
    nal_splitter: Arc<Mutex<NalSplitter>>,
    idr_control: Arc<AsyncMutex<Option<IdrControlHandle>>>,
    running: Option<RunningInstance>,
}

impl MonitorPipeline {
    pub fn new(
        ffmpeg_path: &Path,
        display: DisplayTarget,
        codec: Codec,
        hw_encoder: Option<String>,
        options: EncodeOptions,
    ) -> MonitorPipeline {
        let hw_encoder = hw_encoder.and_then(|name| HwEncoderKind::from_encoder_name(&name));
        let codec_is_hevc = codec == Codec::Hevc;
        MonitorPipeline {
            ffmpeg_path: ffmpeg_path.to_path_buf(),
            display,
            codec,
            hw_encoder,
            options,
            fanout: Arc::new(FanoutRegistry::default()),
            nal_splitter: Arc::new(Mutex::new(NalSplitter::new(codec_is_hevc))),
            idr_control: Arc::new(AsyncMutex::new(None)),
            running: None,
        }
    }

    /// フレーム単位・Key/Delta判定済みでコールバックに渡す(推奨API)。
    pub fn subscribe_frames(
        &self,
        callback: impl Fn(&EncodedFrame) + Send + Sync + 'static,
    ) -> SubscriptionId {
        self.fanout.subscribe_frames(Arc::new(callback))
    }

    /// ffmpegのstdout/TCPから読み取った生バイト列(NAL分割前、TCP読み取り
    /// チャンク境界)をそのまま配る。フレーム境界を自前で扱いたい上級者向け。
    pub fn subscribe_raw(
        &self,
        callback: impl Fn(&[u8]) + Send + Sync + 'static,
    ) -> SubscriptionId {
        self.fanout.subscribe_raw(Arc::new(callback))
    }

    /// `subscribe_frames`/`subscribe_raw`どちらの購読IDも受け付ける。
    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.fanout.unsubscribe(id);
    }

    /// 直近確定分のパラメータセット(SPS/PPS/VPS)。最初のキーフレームに到達
    /// するまではNone。
    pub fn get_parameter_sets(&self) -> Option<ParameterSets> {
        self.nal_splitter
            .lock()
            .unwrap()
            .latest_parameter_sets()
            .cloned()
    }

    /// 実行中のffmpegインスタンスに強制IDRを要求する。
    pub async fn request_idr(&self) {
        let mut guard = self.idr_control.lock().await;
        if let Some(handle) = guard.as_mut() {
            send_force_idr(&handle.idr_control_path, handle.idr_relay_stream.as_mut()).await;
        }
    }

    /// ffmpegを起動し、監視ループを開始する。この時点までに登録済みの
    /// コールバックへ配信が始まる。二重`start()`はエラーを返す。
    pub async fn start(&mut self) -> Result<()> {
        if self.running.is_some() {
            return Err("pipeline already started".into());
        }

        let stop_flag = Arc::new(AtomicBool::new(false));

        let setup = run_ffmpeg_pipeline_setup(
            &self.ffmpeg_path,
            &self.display,
            self.codec,
            self.hw_encoder,
            &self.options,
        )
        .await?;

        {
            let mut guard = self.idr_control.lock().await;
            *guard = Some(IdrControlHandle {
                idr_control_path: setup.idr_control_path.clone(),
                idr_relay_stream: setup.idr_relay_stream,
            });
        }

        let supervisor_handle = tokio::spawn(run_monitor_supervisor(
            self.display.clone(),
            self.codec,
            self.hw_encoder,
            self.ffmpeg_path.clone(),
            self.options.clone(),
            stop_flag.clone(),
            self.fanout.clone(),
            self.nal_splitter.clone(),
            self.idr_control.clone(),
            setup.ffmpeg_process,
            setup.ffmpeg_stdout,
        ));

        self.running = Some(RunningInstance {
            supervisor_handle,
            stop_flag,
        });

        Ok(())
    }

    /// 監視ループを止め、ffmpegプロセスを終了する。`start()`前に呼んでも
    /// 安全(no-op)。購読は解除しない(呼び出し元が明示的に`unsubscribe`する)。
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop_flag.store(true, Ordering::Relaxed);
            running.supervisor_handle.abort();
        }
    }
}

impl Drop for MonitorPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn new_hidden_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut cmd = tokio::process::Command::new(program);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        tokio::process::Command::new(program)
    }
}

/// キャプチャ対象ディスプレイに応じたffmpeg入力引数を構築する
/// (streaming.rs::build_capture_input_args相当)。
fn build_capture_input_args(
    display: &DisplayTarget,
    hw_encoder: Option<HwEncoderKind>,
) -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        let ddagrab = format!(
            "ddagrab=output_idx={}:framerate={DDAGRAB_MAX_FPS}:video_size={}x{}:dup_frames=false",
            display.adapter_output_idx, display.width, display.height
        );
        let filter_complex = match hw_encoder {
            Some(_) => format!("{ddagrab}[vout]"),
            None => format!("{ddagrab},hwdownload,format=bgra,format=yuv420p[vout]"),
        };
        vec![
            "-filter_complex".into(),
            filter_complex,
            "-map".into(),
            "[vout]".into(),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        let _ = hw_encoder;
        vec![
            "-f".into(),
            "x11grab".into(),
            "-video_size".into(),
            format!("{}x{}", display.width, display.height),
            "-framerate".into(),
            ENCODER_BASELINE_FPS.to_string(),
            "-i".into(),
            std::env::var("DISPLAY").unwrap_or_else(|_| ":0.0".into()),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        let _ = hw_encoder;
        vec![
            "-f".into(),
            "avfoundation".into(),
            "-video_size".into(),
            format!("{}x{}", display.width, display.height),
            "-framerate".into(),
            ENCODER_BASELINE_FPS.to_string(),
            "-capture_cursor".into(),
            "1".into(),
            "-i".into(),
            format!("{}:none", display.adapter_output_idx),
        ]
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = (display, hw_encoder);
        panic!("screen capture is not supported on this platform")
    }
}

/// エンコーダ種別ごとのffmpeg引数(-c:v以降、出力直前まで)を組み立てる
/// (streaming.rs::encoder_args相当)。
fn encoder_args(codec: Codec, hw: Option<HwEncoderKind>, bitrate_kbps: u32) -> Vec<String> {
    let gop = (ENCODER_BASELINE_FPS as u64 * 60 * 60 * 24).to_string();
    let bufsize_kbps = (bitrate_kbps / ENCODER_BASELINE_FPS.max(1)).max(1);
    let rate_control_kbps_args = |rc_flag: &str| -> Vec<String> {
        vec![
            rc_flag.into(),
            "cbr".into(),
            "-b:v".into(),
            format!("{bitrate_kbps}k"),
            "-maxrate".into(),
            format!("{bitrate_kbps}k"),
            "-bufsize".into(),
            format!("{bufsize_kbps}k"),
        ]
    };

    match (codec, hw) {
        (Codec::Hevc, Some(HwEncoderKind::Nvenc)) => {
            let mut args = vec![
                "-c:v".into(),
                "hevc_nvenc".into(),
                "-preset".into(),
                "p1".into(),
                "-tune".into(),
                "ull".into(),
                "-profile:v".into(),
                "main".into(),
                "-g".into(),
                gop.clone(),
                "-bf".into(),
                "0".into(),
                "-forced-idr".into(),
                "1".into(),
                "-rc-lookahead".into(),
                "0".into(),
                "-delay".into(),
                "0".into(),
                "-zerolatency".into(),
                "1".into(),
            ];
            args.extend(rate_control_kbps_args("-rc"));
            args.extend([
                "-intra-refresh".into(),
                "1".into(),
                "-spatial-aq".into(),
                "1".into(),
                "-aq-strength".into(),
                "8".into(),
            ]);
            args
        }
        (Codec::H264, Some(HwEncoderKind::Nvenc)) => {
            let mut args = vec![
                "-c:v".into(),
                "h264_nvenc".into(),
                "-preset".into(),
                "p1".into(),
                "-tune".into(),
                "ull".into(),
                "-profile:v".into(),
                "baseline".into(),
                "-g".into(),
                gop.clone(),
                "-bf".into(),
                "0".into(),
                "-forced-idr".into(),
                "1".into(),
                "-rc-lookahead".into(),
                "0".into(),
                "-delay".into(),
                "0".into(),
                "-zerolatency".into(),
                "1".into(),
            ];
            args.extend(rate_control_kbps_args("-rc"));
            args.extend([
                "-intra-refresh".into(),
                "1".into(),
                "-spatial-aq".into(),
                "1".into(),
                "-aq-strength".into(),
                "8".into(),
            ]);
            args
        }
        (Codec::Hevc, Some(HwEncoderKind::Amf)) => {
            let mut args = vec![
                "-c:v".into(),
                "hevc_amf".into(),
                "-usage".into(),
                "ultralowlatency".into(),
                "-profile:v".into(),
                "main".into(),
                "-g".into(),
                gop,
                "-bf".into(),
                "0".into(),
            ];
            args.extend(rate_control_kbps_args("-rc"));
            args
        }
        (Codec::H264, Some(HwEncoderKind::Amf)) => {
            let mut args = vec![
                "-c:v".into(),
                "h264_amf".into(),
                "-usage".into(),
                "ultralowlatency".into(),
                "-profile:v".into(),
                "baseline".into(),
                "-g".into(),
                gop,
                "-bf".into(),
                "0".into(),
            ];
            args.extend(rate_control_kbps_args("-rc"));
            args
        }
        (Codec::Hevc, Some(HwEncoderKind::Qsv)) => {
            let mut args = vec![
                "-c:v".into(),
                "hevc_qsv".into(),
                "-preset".into(),
                "veryfast".into(),
                "-profile:v".into(),
                "main".into(),
                "-g".into(),
                gop,
                "-bf".into(),
                "0".into(),
                "-low_delay_brc".into(),
                "1".into(),
            ];
            args.extend(rate_control_kbps_args("-look_ahead"));
            args
        }
        (Codec::H264, Some(HwEncoderKind::Qsv)) => {
            let mut args = vec![
                "-c:v".into(),
                "h264_qsv".into(),
                "-preset".into(),
                "veryfast".into(),
                "-profile:v".into(),
                "baseline".into(),
                "-g".into(),
                gop,
                "-bf".into(),
                "0".into(),
                "-low_delay_brc".into(),
                "1".into(),
            ];
            args.extend(rate_control_kbps_args("-look_ahead"));
            args
        }
        (Codec::H264, None) => vec![
            "-c:v".into(),
            "libopenh264".into(),
            "-profile:v".into(),
            "main".into(),
            "-g".into(),
            gop,
            "-bf".into(),
            "0".into(),
            "-rc_mode".into(),
            "bitrate".into(),
            "-allow_skip_frames".into(),
            "1".into(),
            "-b:v".into(),
            format!("{bitrate_kbps}k"),
            "-maxrate".into(),
            format!("{bitrate_kbps}k"),
        ],
        (Codec::Hevc, None) => unreachable!("HEVC has no CPU-only fallback"),
        (Codec::Vp9, hw) => {
            let _ = hw;
            vec![
                "-c:v".into(),
                "libvpx-vp9".into(),
                "-b:v".into(),
                format!("{bitrate_kbps}k"),
            ]
        }
    }
}

/// ffmpeg-builderが同梱するpatches/idr-control-socket.patchで追加された
/// `-idr_control_socket <path>`用のパス。
fn idr_control_socket_path(output_tcp_port: u16) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\ffmpeg-caster-idr-{output_tcp_port}")
    } else {
        std::env::temp_dir()
            .join(format!("ffmpeg-caster-idr-{output_tcp_port}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// "force_idr\n"を1回送信する。
async fn send_force_idr(
    idr_control_path: &str,
    idr_relay_stream: Option<&mut tokio::net::TcpStream>,
) {
    use tokio::io::AsyncWriteExt;

    #[cfg(windows)]
    if let Some(stream) = idr_relay_stream {
        let _ = stream.write_all(b"force_idr\n").await;
        return;
    }
    #[cfg(not(windows))]
    let _ = idr_relay_stream;

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        if let Ok(mut pipe) = ClientOptions::new().open(idr_control_path) {
            let _ = pipe.write_all(b"force_idr\n").await;
        }
    }
    #[cfg(unix)]
    {
        if let Ok(mut sock) = tokio::net::UnixStream::connect(idr_control_path).await {
            let _ = sock.write_all(b"force_idr\n").await;
        }
    }
}

struct FfmpegPipelineSetup {
    ffmpeg_process: tokio::process::Child,
    ffmpeg_stdout: tokio::net::TcpStream,
    idr_control_path: String,
    idr_relay_stream: Option<tokio::net::TcpStream>,
}

async fn run_ffmpeg_pipeline_setup(
    ffmpeg_path: &Path,
    display: &DisplayTarget,
    codec: Codec,
    hw_encoder: Option<HwEncoderKind>,
    options: &EncodeOptions,
) -> Result<FfmpegPipelineSetup> {
    let output_tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("failed to bind local TCP listener for ffmpeg output: {e}"))?;
    let output_tcp_port = output_tcp_listener
        .local_addr()
        .map_err(|e| format!("failed to read output TCP listener address: {e}"))?
        .port();

    let mut ffmpeg_args = build_capture_input_args(display, hw_encoder);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        ffmpeg_args.extend(["-vf".into(), "format=yuv420p".into()]);
        ffmpeg_args.extend(["-r".into(), ENCODER_BASELINE_FPS.to_string()]);
    }

    ffmpeg_args.extend(encoder_args(codec, hw_encoder, options.bitrate_kbps));

    let idr_control_path = idr_control_socket_path(output_tcp_port);
    ffmpeg_args.extend(["-idr_control_socket".into(), idr_control_path.clone()]);

    #[cfg(windows)]
    let idr_relay_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("failed to bind local TCP listener for idr relay: {e}"))?;
    #[cfg(windows)]
    let idr_relay_port = idr_relay_listener
        .local_addr()
        .map_err(|e| format!("failed to read idr relay listener address: {e}"))?
        .port();
    #[cfg(not(windows))]
    let idr_relay_port: u16 = 0;

    let output_format = codec.output_format();
    ffmpeg_args.extend([
        "-f".into(),
        output_format.into(),
        format!("tcp://127.0.0.1:{output_tcp_port}?tcp_nodelay=1&send_buffer_size=65536"),
    ]);

    let mut ffmpeg_process = match options.elevation {
        ElevationMode::PreferSystem => {
            let tools_dir = options
                .tools_dir
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("ffmpeg-caster-tools"));
            elevate::spawn_preferring_system(
                &tools_dir,
                options.ffmpeg_stub_path.as_deref(),
                ffmpeg_path,
                &ffmpeg_args,
                idr_relay_port,
                &idr_control_path,
            )
            .await?
        }
        ElevationMode::Normal => {
            let mut cmd = new_hidden_command(ffmpeg_path);
            cmd.args(&ffmpeg_args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped());
            cmd.spawn()
                .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?
        }
    };

    if let Some(stderr) = ffmpeg_process.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(_line)) = lines.next_line().await {
                // ログ出力先は呼び出し側に委ねる(このライブラリはログを持たない)。
            }
        });
    }

    let (ffmpeg_stdout, _peer_addr) = tokio::select! {
        accept_result = output_tcp_listener.accept() => {
            accept_result.map_err(|e| format!("failed to accept ffmpeg's output TCP connection: {e}"))?
        }
        wait_result = ffmpeg_process.wait() => {
            let status = wait_result.map_err(|e| format!("failed to wait for ffmpeg process: {e}"))?;
            return Err(format!("ffmpeg exited before connecting to the output TCP listener (status={status:?})").into());
        }
    };

    #[cfg(windows)]
    let idr_relay_stream: Option<tokio::net::TcpStream> = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        idr_relay_listener.accept(),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(|(s, _)| s);
    #[cfg(not(windows))]
    let idr_relay_stream: Option<tokio::net::TcpStream> = None;
    let mut idr_relay_stream = idr_relay_stream;

    send_force_idr(&idr_control_path, idr_relay_stream.as_mut()).await;

    Ok(FfmpegPipelineSetup {
        ffmpeg_process,
        ffmpeg_stdout,
        idr_control_path,
        idr_relay_stream,
    })
}

/// ffmpegプロセス1つの生存期間中、TCP出力を読み取ってfanoutへ配り続け、
/// NalSplitterを通してフレーム単位の配信も行う。stop_flagが立って正常終了
/// した場合はtrue、ffmpeg側の都合(TCP切断・読み取りエラー)で終了した場合は
/// falseを返す(supervisor側で再起動するかどうかの判定に使う)。
async fn run_ffmpeg_instance(
    stop_flag: &Arc<AtomicBool>,
    fanout: &Arc<FanoutRegistry>,
    nal_splitter: &Arc<Mutex<NalSplitter>>,
    mut ffmpeg_stdout: tokio::net::TcpStream,
    mut ffmpeg_process: tokio::process::Child,
) -> bool {
    let mut read_buf = [0u8; 64 * 1024];
    let stopped_intentionally = loop {
        if stop_flag.load(Ordering::Relaxed) {
            break true;
        }
        match ffmpeg_stdout.read(&mut read_buf).await {
            Ok(0) => break false,
            Ok(n) => {
                let chunk = &read_buf[..n];
                fanout.broadcast_raw(chunk);
                let frames: Vec<FrameChunk> = nal_splitter.lock().unwrap().push(chunk);
                for frame in frames {
                    let kind = if frame.is_key {
                        FrameKind::Key
                    } else {
                        FrameKind::Delta
                    };
                    fanout.broadcast_frame(&EncodedFrame {
                        kind,
                        payload: frame.payload,
                    });
                }
            }
            Err(_) => break false,
        }
    };
    let _ = ffmpeg_process.kill().await;
    stopped_intentionally
}

/// ffmpegの起動〜監視〜(異常終了時の)再起動を担うタスク本体。1回目の起動は
/// `start()`側で既に完了しているため、まずその結果を読み取り、終了したら
/// 「モニター構成の変化に追従してadapter_output_idxを再解決 → ffmpeg再起動」
/// を繰り返す。
#[allow(clippy::too_many_arguments)]
async fn run_monitor_supervisor(
    mut display: DisplayTarget,
    codec: Codec,
    hw_encoder: Option<HwEncoderKind>,
    ffmpeg_path: PathBuf,
    options: EncodeOptions,
    stop_flag: Arc<AtomicBool>,
    fanout: Arc<FanoutRegistry>,
    nal_splitter: Arc<Mutex<NalSplitter>>,
    idr_control: Arc<AsyncMutex<Option<IdrControlHandle>>>,
    mut ffmpeg_process: tokio::process::Child,
    mut ffmpeg_stdout: tokio::net::TcpStream,
) {
    loop {
        let stopped_intentionally = run_ffmpeg_instance(
            &stop_flag,
            &fanout,
            &nal_splitter,
            ffmpeg_stdout,
            ffmpeg_process,
        )
        .await;
        if stopped_intentionally || stop_flag.load(Ordering::Relaxed) {
            break;
        }

        loop {
            tokio::time::sleep(FFMPEG_RESTART_BACKOFF).await;
            if stop_flag.load(Ordering::Relaxed) {
                return;
            }

            // 他の仮想/実モニターの増減でDXGIのEnumOutputsインデックスがズレて
            // いる可能性があるため、モニター名から現在の正しいadapter_output_idx
            // を再解決してから再起動する。
            #[cfg(target_os = "windows")]
            match crate::display::find_display_by_name(&display.name) {
                Some(resolved) => {
                    display.adapter_output_idx = resolved.adapter_output_idx;
                    display.width = resolved.width;
                    display.height = resolved.height;
                }
                None => continue,
            }

            match run_ffmpeg_pipeline_setup(&ffmpeg_path, &display, codec, hw_encoder, &options)
                .await
            {
                Ok(mut new_setup) => {
                    let new_idr_relay_stream = new_setup.idr_relay_stream.take();
                    {
                        let mut guard = idr_control.lock().await;
                        *guard = Some(IdrControlHandle {
                            idr_control_path: new_setup.idr_control_path.clone(),
                            idr_relay_stream: new_idr_relay_stream,
                        });
                    }
                    ffmpeg_process = new_setup.ffmpeg_process;
                    ffmpeg_stdout = new_setup.ffmpeg_stdout;
                    break;
                }
                Err(_e) => {
                    // 失敗してもこの1回のsleepだけ空けて次のイテレーションで再試行する。
                }
            }
        }
    }
}
