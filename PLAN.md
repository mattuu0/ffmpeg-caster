# rust-cast から ffmpeg 画面キャプチャ用ライブラリを抽出する

## Context

`C:\Users\admin\Desktop\rust-cast` は Tauri 製の画面配信アプリで、以下がバラバラの箇所に実装されている:

- `sender/src-tauri/src/*.rs` … 本番実装。Tauri (`AppHandle`)・tokio 非同期ランタイム・独自の設定/セッション管理と密結合。
- `poc/sender_poc/src/*.rs` … 同じロジックの「移植版」。Tauri 依存を排除し `std::process::Command` + `ureq` + 同期コードで書き直されている。**こちらの方が汎用ライブラリの土台として質が良い**(コメントにも `sender/src-tauri` からの独立移植である旨が明記されている)。
- `ffmpeg/example/*` … 各機能の最小 PoC(移植元)。

ユーザーの要望は、この中から

1. ffmpeg のダウンロード(マルチプラットフォーム対応。環境のOS/archを検出し自動取得)
2. ディスプレイ選択(クロスプラットフォーム、`display://` のようなURIスタイルで指定できる形。Parsec VDDによる仮想ディスプレイ作成は対象外)
3. 任意コーデックでのエンコード(モニターのインデックスが変わっても自動再起動し、強制IDR取得もでき、どんな状況でも対象モニターのエンコード結果を取得し続けられる。**1モニターにつき1ffmpegプロセス**が原則)
4. エンコーダ自動判定
5. NAL フレームパース
6. SYSTEM昇格キャプチャ(PAExecの自動ダウンロード込み、**Windowsのみ対応**。`setup`関数で保存先ディレクトリを指定し、未取得のツールのみ自動ダウンロードする。ffmpeg起動時に「SYSTEM昇格するか」をオプションで指定できる)

を「持ち出した」独立ライブラリを rust-cast の外に新規作成すること。目的は、今後別プロジェクトでも screen-capture+ffmpeg 機能を再利用できるようにすること。配布形態は **Rustクレートとしても、DLL(cdylib)としても使える形**にする。さらに、**オフライン環境向けにffmpegバイナリをzip圧縮してDLL/SOに埋め込んだビルドバリアント**も用意する(ネットワーク接続なしでも`setup()`だけでffmpegが使える状態にする)。

## 抽出方針

`poc/sender_poc` 配下の実装を一次ソースとして採用し、Windows 専用部分(ディスプレイ列挙・ddagrab)と自動再起動/IDR監視ロジックは `sender/src-tauri` から補う。**新規スタンドアロン Rust crate**(`C:\Users\admin\Desktop\ffmpeg-caster` を想定、rust-cast とは別のプロジェクト)としてゼロから作成し、コードをコピー→フレームワーク依存除去→整理する形で移植する。rust-cast 自体は変更しない(参照のみ)。

### IDR制御の方針: カスタムffmpegフォーク前提でソケット方式を移植

調査の結果、rust-cast の強制IDR機構は以下の構成になっている:

- ffmpegの `-idr_control_socket <path>` フラグは `mattuu0/ffmpeg-builder` という**カスタムパッチ済みffmpegフォーク**にのみ存在する(パッチ: `patches/idr-control-socket.patch`)。ソケット/named pipeに `force_idr\n` という固定バイト列を書き込むと、次に出力するフレームを強制的にIDRにする。
- Windows版はさらに、PAExecによるSYSTEM昇格キャプチャ(セキュアデスクトップ越え)を前提として、SYSTEM所有のnamed pipeに一般権限プロセスから書き込むための **`ffmpeg_stub`(GUIサブシステム中継バイナリ、TCP→named pipeリレー)** を利用している。

ユーザーの判断により、**この方式(カスタムffmpegフォーク前提の `-idr_control_socket` ソケット制御)をそのまま踏襲する**。さらに追加要望により、**PAExec自動ダウンロード込みのSYSTEM昇格キャプチャも移植対象に含める**。したがって新ライブラリは:

- `downloader` モジュールが取得するffmpegバイナリは `mattuu0/ffmpeg-builder`(または互換のパッチ済みビルド)であることを前提とする。これは元々 rust-cast の `ffmpeg_downloader.rs` がダウンロードしているものと同一なので、追加の前提にはならない。
- `pipeline` モジュールは `-idr_control_socket` フラグを付与する。Unix系では `UnixStream` で直接ソケットに接続して `force_idr\n` を書き込む。Windowsで通常権限起動時は直接named pipeに接続、SYSTEM昇格時はリレー経由で書き込む(下記)。
- **SYSTEM昇格キャプチャ(`elevate.rs`/`paexec_setup.rs`/`ffmpeg_stub`)も移植する**。DXGI Desktop Duplication はUACのセキュアデスクトップ(同意プロンプト画面)を、SYSTEM権限から起動された場合のみキャプチャできるため、この機能により「ロック画面やUACプロンプト中も配信が途切れない」キャプチャが可能になる。
  - `setup()` 相当の初期化関数でffmpegとPAExecを両方自動ダウンロードする(`downloader::ensure_ffmpeg` + 新設する `elevate::ensure_paexec`)。
  - `EncodePipeline` の起動オプションに `elevation: ElevationMode { Normal, PreferSystem }` を追加し、`PreferSystem` 指定時は `spawn_preferring_system` 相当(PAExec経由でSYSTEM起動を試み、失敗したら通常起動にフォールバック)を使う。
  - Windows専用の `ffmpeg_stub` 中継バイナリ(GUIサブシステム、PAExecの`-i`が強制する可視コンソール回避 + SYSTEM所有named pipeへのTCP→パイプ中継)もライブラリに同梱する別バイナリターゲットとして移植する。
- 「モニターのインデックスが変わっても自動再起動」の**監視ループ**(`monitor_session.rs::run_monitor_supervisor`: 500msバックオフ、`find_display_by_name`によるデバイス名ベースの再解決、失敗時は無限リトライ)をロジックとして忠実に移植する。
- 「どんな状況でもエンコード結果を取得できる」という要件は、上記の自動再起動監視ループ + ffmpeg異常終了検知(stdout/TCP読み取りループがEOF/エラーを返したら再起動)+ IDR再送(再起動後の最初のフレームで確実にキーフレームを送る)+ SYSTEM昇格(UACセキュアデスクトップ中も途切れない)の組み合わせで満たす。

## 移植対象と出典(確定)

| # | 機能 | 一次ソース | 理由 |
|---|---|---|---|
| 1 | ffmpeg ダウンローダ | `poc/sender_poc/src/ffmpeg_downloader.rs` (159行) | 同期・`ureq`+`zip`のみ、GitHub Releases API から OS/arch 別バイナリを取得。Tauri進捗イベント無し版が土台として最適。`sender/src-tauri/src/ffmpeg_setup.rs` は同ロジックの非同期+Tauri進捗版なので、進捗コールバックの設計だけ参考にする。 |
| 2 | ディスプレイ列挙・選択 | `sender/src-tauri/src/display_manager.rs` (132行) | DXGI/Direct3D11 で物理ディスプレイを列挙する唯一の実装(`poc`側には無い)。`enumerate_displays()` / `find_display_by_name()` をほぼそのまま移植。 |
| 3 | エンコーダ自動判定 | `poc/sender_poc/src/encoder_autodetect.rs` (201行) | HEVC/H264/VP9 対応、同期・依存クレート無し。`pick_best_encoder()` を核として移植。 |
| 4 | エンコード実行パイプライン(起動・自動再起動・IDR制御・コールバック配信) | `sender/src-tauri/src/streaming.rs`(`encoder_args()`/`build_capture_input_args()`/`run_ffmpeg_pipeline_setup()`/`idr_control_socket_path()`/`send_force_idr()`)+ `monitor_session.rs`(`FanoutRegistry`、`run_monitor_supervisor()`の再起動ループ、`run_ffmpeg_instance()`の異常終了検知) | コーデック引数構築・プロセス起動・IDRソケット制御・監視ループ・複数購読者への配信はこの2ファイルが本実装。**1モニター1ffmpegプロセス**の原則を守り、`FanoutRegistry`の「同じエンコード済みバイト列を複数コールバックへ配る」設計はそのままコア機能として移植する。ここから除くのは **WebSocket配信そのもの**(暗号化・シリアライズ等アプリ層の話)のみ。SYSTEM昇格起動(#6)はこのパイプラインのオプションとして組み込む。 |
| 5 | NAL フレームパース | `poc/sender_poc/src/nal_splitter.rs` (142行) | アクセスユニット単位で正しく分割する改良版(`sender/src-tauri/src/nal.rs` は1 NAL=1メッセージの旧式で既知のバグを持つ、かつ本番コードパスでも実際には未使用の死んだコード)。`NalSplitter` / `FrameChunk` をそのまま移植。 |
| 6 | SYSTEM昇格キャプチャ(PAExec自動取得・ffmpeg中継、Windowsのみ) | `sender/src-tauri/src/elevate.rs`(`spawn_elevated`/`spawn_normal`/`spawn_preferring_system`)+ `paexec_setup.rs`(`resolve_or_download_paexec`)+ `ffmpeg_stub/src/main.rs`(GUIサブシステム中継バイナリ) | PAExec経由のSYSTEM昇格でDXGI Desktop Duplicationがセキュアデスクトップ(UACプロンプト画面)もキャプチャできるようにする仕組み一式。`self_elevate.rs`(GUIアプリ自体をUAC昇格させる部分)は「呼び出し元プロセスが既に管理者権限であること」が`spawn_elevated`の前提条件のため、ライブラリ利用者への注意事項として文書化するに留め、コードとしては移植しない(アプリ全体の自己昇格は利用者側の責務)。非Windowsではこのモジュール自体がno-op(常に通常起動)になる。 |

### オフライン埋め込みビルドの方針

`downloader`モジュールはネットワーク経由でGitHub Releasesから取得する方式が既定だが、これとは別に **Cargo feature `bundled`** を新設し、有効化するとビルド時に対象プラットフォーム用のffmpeg(+Windowsなら`ffmpeg_stub`/PAExecも含めるかは要検討、まずはffmpeg本体を対象とする)のzipをバイナリに埋め込む。

- ビルド前提: 埋め込み対象のzip(`mattuu0/ffmpeg-builder`のリリースアセットをビルド時にダウンロードするか、事前にリポジトリの`assets/`配下に配置しておく)を`build.rs`で用意し、`include_bytes!`でクレートに埋め込む。ターゲットのOS/archごとに埋め込む内容が変わるため、`bundled`feature有効時はビルドしているホスト自身のOS/archのzipのみを埋め込む(クロスコンパイル時は明示的にターゲットを指定する運用にする)。
- 実行時: `downloader::ensure_ffmpeg(dest_dir)`は`bundled`feature有効時、まず埋め込みzipを`dest_dir`に展開し(既存の`extract_zip`ロジックを再利用)、ネットワークアクセスを一切行わない。feature無効時は従来通りGitHub Releasesからダウンロードする。両方のコードパスを持たせ、呼び出し側のAPI(`setup(tools_dir)`)は共通にする。
- 生成物: `cargo build --features bundled --crate-type cdylib` でビルドしたDLL/SOはそれ単体でオフライン環境に配布・展開できる(ffmpeg取得のためのネットワークアクセスが不要になる)。通常のfeature無効ビルドは軽量だがオンライン前提、という2バリアントを用意する。
- サイズに関する注意: ffmpegバイナリ(数十MB規模)をDLLに埋め込むとバイナリサイズが大きくなるため、`bundled`はデフォルト無効のオプトインfeatureとする。

**除外するもの**(rust-cast固有・過剰に密結合、または今回のスコープ外):
- Parsec VDD 仮想ディスプレイ作成 (`virtual_display.rs`, `vdd_setup.rs`) — 「仮想ディスプレイの作成」ではなく「既存ディスプレイの選択」が要望なので対象外。
- 自己UAC昇格 (`self_elevate.rs`) — アプリ全体の起動方法に関わるため、ライブラリではなく利用者アプリ側の責務。
- WebSocket配信、暗号化 (`crypto.rs`)、デバイスID (`identity.rs`)、mDNS (`discovery.rs`)、設定永続化 (`settings.rs`) — 配信プロトコル・セキュリティ・ペアリングはアプリ層の関心事であり、ライブラリはバイト列とコールバックまでを提供する。

## 新規ライブラリの構成

新規ディレクトリ(例: `C:\Users\admin\Desktop\ffmpeg-caster`)に単一 Rust crate として作成:

```
ffmpeg-caster/
  Cargo.toml                  // [lib] crate-type = ["rlib", "cdylib"]。ffmpeg_stubを含むワークスペース
  src/
    lib.rs                    // 公開API再エクスポート + FFI(C ABI)エントリポイント
    downloader.rs              // ffmpeg_downloader.rs 移植: ensure_ffmpeg(dest_dir) — OS/arch別に自動判定・取得
    display.rs                  // display_manager.rs 移植 + display:// URI パース: DisplayInfo, DisplayTarget, enumerate_displays(), find_display_by_name(), parse_display_uri()
    encoder.rs                   // encoder_autodetect.rs 移植: Codec, pick_best_encoder(), is_hardware_encoder()
    pipeline.rs                  // streaming.rs + monitor_session.rs から抽出: 1モニター1ffmpegプロセスの管理 + 自動再起動監視 + IDR制御 + コールバック配信
    elevate.rs                    // elevate.rs + paexec_setup.rs 移植(Windowsのみ有効): ensure_paexec(), spawn_normal(), spawn_elevated(), spawn_preferring_system()
    nal.rs                        // nal_splitter.rs 移植: NalSplitter, FrameChunk, Codec(pipeline::Codecと統合検討)
    ffi.rs                        // cdylib配布用のC ABI関数群(extern "C" fn ffmpeg_caster_*) — 後述
  ffmpeg_stub/                   // ffmpeg_stub/src/main.rs 移植: 独立クレート(Windowsのみビルド対象、理由はsender/src-tauri/Cargo.tomlの
    Cargo.toml                    // コメントと同じく、ビルド成果物のexeを配布物として参照する際の循環ビルド回避のため、ワークスペースメンバーとして分離)
    src/main.rs
  examples/
    capture_and_encode.rs        // 一連の流れを繋いだ最小サンプル(ダウンロード→列挙→自動判定→起動→subscribe_framesでKey/Delta表示)
    system_elevated_capture.rs    // setup()でffmpeg+paexec自動取得→PreferSystemでパイプライン起動するサンプル
    multi_subscriber.rs           // 同一モニターに複数コールバック(frames/raw混在)を登録し、同じエンコード結果が全員に届くことを確認するサンプル
```

### 配布形態: Rustクレート + cdylib(DLL)

`Cargo.toml`の`[lib]`に`crate-type = ["rlib", "cdylib"]`を指定し、Rustプロジェクトからは通常のクレートとして、それ以外の言語からはDLL(Windows: `.dll`、Linux: `.so`、macOS: `.dylib`)としてリンクできるようにする。DLL利用者向けに`src/ffi.rs`で`extern "C"`関数群を用意する:
- コールバックはC関数ポインタ(`extern "C" fn(*const u8, usize, *mut c_void)`)+ `user_data: *mut c_void` の形で受け取る。
- `Vec<u8>`/`String`等のRust型はFFI境界を越えないよう、生ポインタ+長さのペアと不透明ハンドル(`*mut EncodePipelineHandle`)でラップする。
- 内部のRust APIは通常のRust型(`Result`, `String`, トレイトオブジェクトのコールバック等)で設計し、`ffi.rs`はその薄いラッパーに徹する(内部ロジックをFFI制約で歪めない)。

### 依存クレート(`Cargo.toml`)
```
tokio = { version = "1", features = ["rt-multi-thread", "process", "net", "sync", "time", "io-util", "macros"] }
ureq = { version = "2", features = ["tls", "json"] }
serde = { version = "1", features = ["derive"] }
zip = { version = "0.6", default-features = false, features = ["deflate"] }
anyhow (or thiserror) — エラー型統一のため新規導入(元コードは String エラーが多いので整理する)

[target.'cfg(windows)'.dependencies]
windows = { version = "0.62", features = ["Win32_Graphics_Direct3D", "Win32_Graphics_Direct3D11", "Win32_Graphics_Dxgi", "Win32_Graphics_Dxgi_Common"] }
```
tokio が必要になる理由: 自動再起動監視ループ(`run_monitor_supervisor`相当)とffmpeg出力の非ブロッキング読み取りを両立するため。ダウンロード・エンコーダ判定・ディスプレイ列挙・NALパースは同期APIのまま提供する(非同期ランタイムを要求しない)。Tauri・mdns-sd・rsa・aes-gcm 等は不要。

`ffmpeg_stub` はWindows専用のGUIサブシステムバイナリで `windows` クレートの `Win32_System_Threading`/`Win32_UI_WindowsAndMessaging` 等を直接使う(`tokio` 等は不要、依存最小の素の `std::process`/Win32 API実装)。

### 各モジュールの公開API(移植時の変更点含む)

- **`downloader::ensure_ffmpeg(dest_dir: &Path) -> Result<PathBuf>`**
  - `poc/sender_poc/src/ffmpeg_downloader.rs` をほぼそのまま移植。`dest_dir`配下に既にバイナリがあればそれを返してダウンロードをスキップする既存の`find_ffmpeg_binary`ロジックをそのまま活かす。OS/arch検出(`detect_platform`/`detect_arch`)によりWindows/Linux/macOS × amd64/arm64を自動判定してGitHub Releasesから適切なアセットを取得する(=マルチプラットフォーム対応はそのまま踏襲)。エラー型を `String` → 統一エラー型に変更するのみ。取得先が `mattuu0/ffmpeg-builder`(IDR制御ソケットパッチ入りフォーク)であることは変更しない。
  - `bundled` feature有効時は、ネットワークダウンロードの代わりにビルド時に埋め込んだzip(`include_bytes!`)を`extract_zip`相当のロジックで`dest_dir`に展開する(詳細は下記「オフライン埋め込みビルドの方針」)。関数シグネチャ・呼び出し側から見た挙動(`dest_dir`に無ければ用意する)は変わらない。

- **`display::{DisplayInfo, DisplayTarget, enumerate_displays, find_display_by_name, parse_display_uri}`**
  - `sender/src-tauri/src/display_manager.rs` の `enumerate_displays()`/`find_display_by_name()`/`DisplayInfo` をベースに移植。Windowsは既存のDXGI実装をそのまま使う。Linux/macOSは現状スタブしかないため、`pipeline`のキャプチャ入力生成(x11grab/avfoundation)と対になる実列挙を新規実装する(Linux: `xrandr`相当のX11出力列挙、macOS: `AVFoundation`のキャプチャデバイス一覧)。マルチプラットフォーム対応が前提のため、このクロスプラットフォーム実列挙化は必須スコープとする。
  - 新規: `parse_display_uri(uri: &str) -> Result<DisplayTarget>` を追加し、`display://primary`・`display://index/1`・`display://name/<DeviceName>` のようなURIスタイルで指定できるようにする(具体的なスキームは実装時に確定)。`DisplayTarget` は `enumerate_displays()` の結果から解決した実際のディスプレイ情報(`adapter_output_idx`含む)を保持する。

- **`encoder::{Codec, pick_best_encoder, is_hardware_encoder}`**
  - `poc/sender_poc/src/encoder_autodetect.rs` を移植。`probe_encoder_works()` 内のキャプチャ入力文字列組み立ては `pipeline` モジュールの `build_capture_input_args()` 相当と共通化する。

- **`elevate::{ensure_paexec, spawn_normal, spawn_preferring_system, ElevationMode}`**(Windowsのみ有効。非Windowsではこのモジュールは`ElevationMode`が常に`Normal`扱いになるno-op)
  - `ensure_paexec(dest_dir: &Path) -> Result<PathBuf>` — `paexec_setup.rs::resolve_or_download_paexec` を移植。`dest_dir`配下に既にpaexec.exeがあればダウンロードをスキップする既存ロジックをそのまま踏襲。
  - `spawn_normal` / `spawn_elevated` / `spawn_preferring_system` を移植。`spawn_elevated` は `ffmpeg_stub` のパスを解決して PAExec 経由で `-i -s -w <workdir> <stub> <idr_relay_port> <idr_control_path> <ffmpeg> <args...>` を組み立てる。
  - `resolve_ffmpeg_stub_path()` は「メイン実行ファイルと同じディレクトリにある」前提を、ライブラリ利用者の配布物レイアウトに合わせて設定可能にする(例: `EncodeOptions`にstubパスの明示指定を許容するオプションを追加)。
  - ライブラリのpublic docコメントに「`spawn_elevated`はプロセス自身が既に管理者権限で起動していることが前提。呼び出し元アプリが管理者権限でない場合はPAExecのサービスインストールが失敗し、自動的に通常起動へフォールバックする」旨を明記する(元の `elevate.rs` のコメントをそのまま踏襲)。

- **`pipeline::{EncodeOptions, ElevationMode, MonitorPipeline, SubscriptionId, EncodedFrame, FrameKind}`**(「1モニター1ffmpegプロセス」原則のコア実装)

  **ライフサイクルは `new`(生成)→`subscribe_*`(コールバック登録)→`start`(ffmpeg起動)→…→`stop`(終了) の順に分離する**(ユーザー確認済みの利用フロー: `setup` → `enumerate_displays` → ディスプレイオブジェクト取得 → callback設定 → `start` → callbackでデータ取得 → `stop`)。`start()`が生成と起動を同時に行う一体型APIにはしない — コールバックを登録してからffmpegを起動できるようにするため。

  - `MonitorPipeline::new(ffmpeg_path: &Path, display: DisplayTarget, codec: Codec, hw_encoder: Option<HwEncoderKind>, options: EncodeOptions) -> MonitorPipeline` — ffmpegはまだ起動しない。対象ディスプレイ・コーデック・エンコードオプションを保持した「未起動」の状態のオブジェクトを作るだけ。対象ディスプレイにつき常にちょうど1つのffmpegプロセスを起動する原則を守るため、同じディスプレイに対して2個目の`MonitorPipeline`を作ることは許容するが内部で共有はしない設計とする(呼び出し側が1ディスプレイ1インスタンスを守る責務)。
  - コールバックは`start()`より前でも後でも登録可能(内部にコールバックリストを持つだけなので、起動前に登録しておく想定のフローを正式にサポートする)。**フレーム単位**と**生バイト列**の両方を提供する:
    - `EncodedFrame { kind: FrameKind, payload: Vec<u8> }` / `FrameKind::{Key, Delta}` — 1アクセスユニット(1フレーム)分のAnnex-Bバイト列(パラメータセット含む)と、キーフレームかどうかの判定結果をまとめた構造体。`pipeline`内部で`nal::NalSplitter`を常時走らせ、ffmpegの生出力から`FrameChunk{payload, is_key}`が確定するたびに`EncodedFrame`へ変換して配る。
    - `MonitorPipeline::subscribe_frames(&self, callback: impl Fn(&EncodedFrame) + Send + 'static) -> SubscriptionId` — フレーム単位・Key/Delta判定済みでコールバックに渡す(推奨API)。
    - `MonitorPipeline::subscribe_raw(&self, callback: impl Fn(&[u8]) + Send + 'static) -> SubscriptionId` — `monitor_session.rs::FanoutRegistry`相当。ffmpegのstdout/TCPから読み取った生バイト列(NAL分割前、TCP読み取りチャンク境界)をそのまま配る。フレーム境界を自前で扱いたい上級者向け。
    - 両APIは同じ1本のffmpegプロセス・1本の読み取りループを共有する(生バイト列読み取りループの中で`NalSplitter`を通して`subscribe_frames`用のフレームを組み立てつつ、`subscribe_raw`購読者には元のチャンクをそのまま渡す)。購読者数が増えても追加のffmpegプロセスや読み取りループは発生しない。
  - `MonitorPipeline::start(&mut self) -> Result<()>` — `run_ffmpeg_pipeline_setup` 相当。`build_capture_input_args()`(OS別: Windows=ddagrab, Linux=x11grab, macOS=avfoundation)と `encoder_args()`(コーデック×HWベンダー別のCBR/GOP/VBV設定)を移植・統合してffmpeg引数を組み立て、`options.elevation` に応じて `elevate::spawn_normal` または `elevate::spawn_preferring_system` でtokioプロセスをspawnし、読み取り・監視ループを開始する。この時点までに登録済みのコールバックへ配信が始まる。二重`start()`はエラーを返す。
  - `MonitorPipeline::unsubscribe(&self, id: SubscriptionId)` — `subscribe_frames`/`subscribe_raw`どちらの購読IDも受け付ける。
  - `MonitorPipeline`内部で `run_monitor_supervisor` 相当の監視タスクを持ち、ffmpegが異常終了した場合は `find_display_by_name` で `adapter_output_idx` を再解決してから自動再起動する(500msバックオフ、無限リトライ、`stop()`が呼ばれるまで継続)。モニター構成が変化しても全コールバック(フレーム単位・生バイト列とも)への配信は透過的に継続する。
  - `MonitorPipeline::request_idr(&self)` — `idr_control_socket_path`/`send_force_idr` を移植し、実行中のffmpegインスタンスに強制IDRを要求する(再起動をまたいでも常に現在のインスタンスに届くよう、内部で最新のソケットパス・IDRリレー接続を保持する)。SYSTEM昇格時は`ffmpeg_stub`経由のTCPリレー、通常起動時は直接named pipe/Unixソケットへ接続、と`send_force_idr`の分岐をそのまま踏襲する。
  - `MonitorPipeline::get_parameter_sets(&self) -> Option<ParameterSets>` — SPS/PPS(HEVCならVPSも)を単独取得するAPI。`ParameterSets { payload: Vec<u8> }` は直近に観測した非VCL NAL群(AUD/SEIは除きSPS/PPS/VPSのみ)をAnnex-Bのまま連結したバイト列で、WebCodecsの`VideoDecoderConfig.description`やMediaCodecの`csd-0`/`csd-1`にそのまま渡せる形にする。ffmpegの起動直後や再起動直後はまだ1枚もフレームが来ておらずパラメータセットが未確定なため`None`を返し、最初のキーフレームが`NalSplitter`を通過した時点で`Some`になる。以後、SPS/PPS/VPSが変化する度(通常は解像度/コーデック設定が変わらない限り不変)に内部で更新され、常に「直近確定分」を返す。再起動時もこの値は保持される(ffmpeg再起動直後に新しいキーフレームが来るまでは再起動前の値を返し続けるか`None`にリセットするかは実装時に確定。用途上は「再起動直後の一瞬だけ古い値が返る」より「新しいキーフレームまでは古い値を保持」の方が実用的なため後者を既定とする)。
  - `MonitorPipeline::stop(&mut self)` — 監視ループを止め、全購読を解除し、ffmpegプロセスを終了する。`start()`前に呼んでも安全(no-op)。

- **`nal::{NalSplitter, FrameChunk, Codec, ParameterSets}`**
  - `poc/sender_poc/src/nal_splitter.rs` をそのまま移植。`encoder::Codec`(Hevc/H264/Vp9)と `nal::Codec`(H264/Hevc のみ、VP9はAnnex B概念なし)は意味が異なるため型を分けたまま保持する。`pipeline`モジュールが内部で`NalSplitter::push(chunk)`を呼び、`FrameChunk{payload, is_key}`を`pipeline::EncodedFrame{kind, payload}`に変換して`subscribe_frames`購読者へ配信する(利用者が自分で`NalSplitter`を呼ぶ必要はない。ただし`nal`モジュール自体は引き続き`pub`で公開し、`subscribe_raw`で受け取った生バイト列を自前で処理したい利用者も使えるようにする)。
  - 新規: `NalSplitter`の内部状態に非VCL NAL(SPS/PPS/VPS)の直近確定分を保持するフィールドを追加し、`NalSplitter::latest_parameter_sets(&self) -> Option<&ParameterSets>` を公開する。`push()`内でアクセスユニットを確定させる既存ロジック(非VCL NAL到達時にそれまでのVCL群を1フレームとして切り出す箇所)で、切り出す直前に蓄積していた非VCL NAL(SPS/PPS/VPS種別のみ、AUD/SEIは除外)をこのフィールドに保存する。`pipeline::MonitorPipeline::get_parameter_sets()`はこの値をそのまま返す薄いラッパーになる。

- **`setup()` エントリポイント**
  - ユーザー要望の「保存先ディレクトリを指定でき、未取得の場合のみ自動ダウンロードする」を実現するため、`lib.rs`に `pub async fn setup(tools_dir: &Path) -> Result<Toolchain>` を用意する。`tools_dir`は呼び出し側が自由に指定する(アプリのデータディレクトリ配下等)。`Toolchain { ffmpeg_path: PathBuf, paexec_path: Option<PathBuf> }` を返し、内部で `downloader::ensure_ffmpeg(tools_dir)` と(Windowsのみ)`elevate::ensure_paexec(tools_dir)` を呼ぶ — どちらも「既に`tools_dir`にあればダウンロードしない」ロジックを内包しているため、`setup()`を毎回呼んでも冪等に動作する。`paexec_path` はWindows以外では常に`None`。

## 想定利用フロー(確定)

ユーザー確認済みの一連の流れ:

```
setup                      … バイナリの取得・設定
  ↓
enumerate_displays         … ディスプレイオブジェクト(一覧)を取得
  ↓
MonitorPipeline::new        … 選んだディスプレイでパイプラインオブジェクトを生成(まだ起動しない)
  ↓
subscribe_frames / subscribe_raw  … callbackを設定
  ↓
pipeline.start()            … 起動(ffmpeg spawn)
  ↓
callbackからエンコード済みデータを継続的に取得
  ↓
pipeline.stop()             … 配信終了
```

`MonitorPipeline::start()`が生成と起動を同時に行う一体型APIにはせず、`new`(生成)→`subscribe_*`(登録)→`start`(起動)に分離するのはこのフローを成立させるため(コールバックをffmpeg起動前に登録できる必要がある)。

### `examples/capture_and_encode.rs`(想定コード)

まだ実装前のため型・関数名は設計段階の想定だが、上記フローをそのままコードに落とすと以下のようになる。実装時にこのサンプルを土台にする。

```rust
use ffmpeg_caster::{
    setup,
    display::enumerate_displays,
    encoder::pick_best_encoder,
    pipeline::{MonitorPipeline, EncodeOptions, ElevationMode},
};

fn main() -> anyhow::Result<()> {
    // 1. バイナリの取得・設定(既に tools_dir にあればダウンロードはスキップされる)
    let tools_dir = std::path::Path::new("./tools");
    let toolchain = setup(tools_dir)?; // Toolchain { ffmpeg_path, paexec_path }

    // 2. ディスプレイオブジェクト(一覧)を取得
    let displays = enumerate_displays()?;
    let display = displays
        .into_iter()
        .find(|d| d.is_primary)
        .expect("no primary display found");

    // エンコーダの自動判定(HEVC/H264、ハードウェア優先)
    let (codec, hw_encoder) = pick_best_encoder(&toolchain.ffmpeg_path, None)?;

    // 3. パイプラインオブジェクトを生成(まだ起動しない)
    let options = EncodeOptions {
        bitrate_kbps: 8_000,
        elevation: ElevationMode::Normal,
        ..Default::default()
    };
    let mut pipeline = MonitorPipeline::new(
        &toolchain.ffmpeg_path,
        display.into(), // DisplayInfo -> DisplayTarget への変換
        codec,
        hw_encoder,
        options,
    );

    // 4. callbackを設定(起動前に登録しておける)
    pipeline.subscribe_frames(|frame| {
        match frame.kind {
            ffmpeg_caster::pipeline::FrameKind::Key => {
                println!("[frame] KEY   {} bytes", frame.payload.len());
            }
            ffmpeg_caster::pipeline::FrameKind::Delta => {
                println!("[frame] delta {} bytes", frame.payload.len());
            }
        }
        // ここでWebSocket送信・mp4muxへの書き込み等、エンコード済みデータを使った処理を行う
    });

    // 5. 起動
    pipeline.start()?;

    // SPS/PPSが必要なら、最初のキーフレーム到達後にここで取得できる
    // (WebCodecsのVideoDecoderConfig.description等に使う)
    std::thread::sleep(std::time::Duration::from_millis(500));
    if let Some(params) = pipeline.get_parameter_sets() {
        println!("parameter sets: {} bytes", params.payload.len());
    }

    // 6. しばらく配信を継続(callbackが呼ばれ続ける)
    std::thread::sleep(std::time::Duration::from_secs(10));

    // 7. 配信終了
    pipeline.stop();

    Ok(())
}
```

### `examples/multi_subscriber.rs`(想定コード、複数callback)

```rust
use ffmpeg_caster::pipeline::{MonitorPipeline, EncodeOptions, ElevationMode};

fn main() -> anyhow::Result<()> {
    let toolchain = ffmpeg_caster::setup(std::path::Path::new("./tools"))?;
    let display = ffmpeg_caster::display::enumerate_displays()?
        .into_iter()
        .find(|d| d.is_primary)
        .unwrap();
    let (codec, hw) = ffmpeg_caster::encoder::pick_best_encoder(&toolchain.ffmpeg_path, None)?;

    let mut pipeline = MonitorPipeline::new(
        &toolchain.ffmpeg_path,
        display.into(),
        codec,
        hw,
        EncodeOptions::default(),
    );

    // 複数のcallbackを登録しても、ffmpegプロセス・読み取りループは1本のまま。
    // 全callbackに同じエンコード結果が配られる。
    pipeline.subscribe_frames(|frame| {
        // 例: WebSocketクライアントAへ送信
        println!("[subscriber A] {:?} {} bytes", frame.kind, frame.payload.len());
    });
    pipeline.subscribe_frames(|frame| {
        // 例: mp4ファイルへの録画書き込み
        println!("[subscriber B] {:?} {} bytes", frame.kind, frame.payload.len());
    });
    pipeline.subscribe_raw(|chunk| {
        // 例: 生バイト列をそのまま別プロセスへパイプ
        println!("[subscriber C - raw] {} bytes", chunk.len());
    });

    pipeline.start()?;
    std::thread::sleep(std::time::Duration::from_secs(10));
    pipeline.stop();

    Ok(())
}
```

## 実装ステップ

1. 新規ディレクトリ `ffmpeg-caster` に `cargo init --lib` し、上記 `Cargo.toml`(`crate-type = ["rlib", "cdylib"]`、`ffmpeg_stub`をワークスペースメンバーに追加)を作成。
2. `nal.rs` を移植(依存ゼロで最も独立しているため最初に着手、単体テストも追加)。合わせて`latest_parameter_sets()`とその内部状態管理を実装し、SPS/PPS/VPS抽出の単体テストも追加する。
3. `encoder.rs` を移植(依存ゼロ)。
4. `downloader.rs` を移植 — `ensure_ffmpeg(dest_dir)`が既存バイナリを検出したらスキップする挙動を維持。`bundled` featureを追加し、`build.rs`で対象OS/arch用ffmpeg zipを`include_bytes!`する経路を実装。
5. `display.rs` を移植 — Windows(DXGI)はそのまま、Linux/macOSの実列挙を新規実装し、`parse_display_uri`を追加。
6. `elevate.rs`(PAExec自動ダウンロード + spawn系関数、Windowsのみ)と `ffmpeg_stub/src/main.rs`(独立バイナリ、Windowsのみビルド)を移植。
7. `pipeline.rs` を新規に組み立て — `streaming.rs` のエンコーダ引数/キャプチャ入力ロジックと、`monitor_session.rs` の `FanoutRegistry`(コールバック配信)・自動再起動監視ループ・IDR制御を移植し、「1モニター1ffmpegプロセス+複数コールバック購読」の`MonitorPipeline`として再構成する。読み取りループ内で`NalSplitter`を通し、`subscribe_frames`(フレーム単位・Key/Delta判定済み)と`subscribe_raw`(生バイト列)の両APIを実装する。`get_parameter_sets()`を`NalSplitter::latest_parameter_sets()`の薄いラッパーとして実装する。`elevate`モジュールと接続する。
8. `lib.rs` に `setup(tools_dir)` エントリポイントを実装し、各モジュールを `pub mod` 公開。READMEに使用例を記載。
9. `ffi.rs` を実装し、`MonitorPipeline`等の主要APIをC ABI関数としてラップする(cdylibとしてのビルド・エクスポートを確認)。
10. `examples/capture_and_encode.rs`(通常起動)、`examples/system_elevated_capture.rs`(`setup()`→`PreferSystem`起動)、`examples/multi_subscriber.rs`(複数コールバック購読)を書き、Windows実機で動作確認する。

## 検証方法

- `cargo build`(Windows)と `cargo build --target x86_64-unknown-linux-gnu` 等の非Windowsクロスチェックの両方で、クロスプラットフォーム分岐が壊れていないことを確認。
- `cargo build --crate-type cdylib` でDLLとしてビルドできることを確認し、`dumpbin /exports`(Windows)等でFFI関数がエクスポートされていることを確認。
- `cargo build --features bundled --crate-type cdylib` でオフライン埋め込み版DLLをビルドし、ネットワークを切断した状態で`setup()`を呼んでもffmpegが展開・利用可能になることを確認。
- `cargo test`(`nal.rs` の単体テストは `sender/src-tauri/src/nal.rs` の既存テストパターンを参考に追加)。
- `cargo run --example capture_and_encode` を実際にWindows機で実行し、`setup()`が空ディレクトリ指定時にffmpegをダウンロードし、2回目の呼び出しではダウンロードをスキップすることを確認。続けてディスプレイ検出→エンコーダ自動判定→`subscribe_frames`で数秒キャプチャし、フレームごとのKey/Delta判定とpayloadサイズをコンソール出力で確認。最初のキーフレーム到達後に`get_parameter_sets()`が`Some`を返し、そのバイト列がffmpegのSPS/PPS(NALタイプ7/8等)を含むことを確認。
- `cargo run --example system_elevated_capture` を管理者権限のターミナルから実行し、PAExec自動ダウンロード→SYSTEM昇格起動→(可能であればロック画面/UACプロンプトを表示させた状態で)キャプチャが継続することを確認。管理者権限で実行しない場合は自動的に通常起動にフォールバックすることも確認する。
- `cargo run --example multi_subscriber` で同一`MonitorPipeline`に`subscribe_frames`と`subscribe_raw`を混在させて複数登録し、ffmpegプロセスが1つだけ起動されること(タスクマネージャ等で確認)、かつ全コールバックに同一のエンコード結果(フレーム購読者には同一のKey/Delta判定・payload、生バイト列購読者には同一のチャンク)が届くことを確認。
- ディスプレイの抜き差し(モニター構成変更)をシミュレートできない場合は、`pipeline`の自動再起動ロジックはコードレビューベースで `monitor_session.rs` との対応関係を確認する。
