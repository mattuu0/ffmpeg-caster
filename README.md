# ffmpeg-caster

`rust-cast`(Tauri製画面配信アプリ)から抽出した、汎用的な画面キャプチャ+ffmpegエンコードライブラリ。

- ffmpegの自動ダウンロード(OS/arch自動判定、[mattuu0/ffmpeg-builder](https://github.com/mattuu0/ffmpeg-builder) の`-idr_control_socket`パッチ入りビルドを使用)
- `display://` URIスタイルのクロスプラットフォームディスプレイ選択
- 任意コーデックでのエンコード(1モニター1ffmpegプロセス、自動再起動、強制IDR取得)
- エンコーダ自動判定(HEVC/H264/VP9、ハードウェア優先)
- NALフレームパース(アクセスユニット単位、SPS/PPS/VPS抽出)
- SYSTEM昇格キャプチャ(PAExec自動ダウンロード込み、Windowsのみ。UACセキュアデスクトップもキャプチャ可能)

Rustクレート(rlib)としても、DLL/SO(cdylib)としても使える。

## 想定利用フロー

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

`MonitorPipeline::start()`が生成と起動を同時に行う一体型APIにはしていない。コールバックをffmpeg起動前に登録できるようにするため、`new`(生成)→`subscribe_*`(コールバック登録)→`start`(ffmpeg起動)の順に分離している。

## ビルド

### 通常ビルド(ネットワーク経由ダウンロード、既定)

```sh
cargo build --release
```

ffmpegバイナリ自体はビルドには含まれず、実行時に`setup(tools_dir)`を呼んだタイミングで`mattuu0/ffmpeg-builder`の最新リリースからOS/arch判定した上で自動ダウンロードされる。軽量だがオンライン環境が前提。

### cdylib(DLL/SO)としてビルド

`Cargo.toml`の`[lib] crate-type = ["rlib", "cdylib"]`により、`cargo build`だけで両方の成果物(`.rlib`と`.dll`/`.so`/`.dylib`)が生成される。DLL単体が欲しい場合:

```sh
cargo build --release
# Windows: target/release/ffmpeg_caster.dll
# Linux:   target/release/libffmpeg_caster.so
# macOS:   target/release/libffmpeg_caster.dylib
```

エクスポートされているC ABI関数は`src/ffi.rs`にまとまっている。DLLのエクスポート確認(Windows、VS開発者コマンドプロンプトから):

```sh
dumpbin /exports target\release\ffmpeg_caster.dll
```

### SYSTEM昇格キャプチャ用の`ffmpeg_stub`(Windowsのみ、自動的にDLL/rlibへ埋め込まれる)

`ffmpeg_stub.exe`(PAExecが直接起動する、コンソールを持たないGUIサブシステムの中継バイナリ)は、`ffmpeg-caster`をビルドすると`build.rs`が自動的に`cargo build -p ffmpeg_stub`をサブプロセスとして実行し、その`.exe`を`include_bytes!`でライブラリ本体に埋め込む。**利用者が別途`ffmpeg_stub.exe`をビルド・配置する必要はない。**

`ElevationMode::PreferSystem`でパイプラインを`start()`すると、内部で埋め込み済みのstubを`EncodeOptions::tools_dir`(未指定時は一時ディレクトリ)へ自動的に書き出してから使う(既に同一内容が書き出し済みならスキップする)。よってPAExec経由のSYSTEM起動時は常にコンソール無しでffmpegが起動する(PAExecの`-i`が対話的セッションで起動するプロセスのSTARTUPINFO.wShowWindowを強制的にSW_SHOWにする仕様を、コンソールを持たないGUIサブシステムのstubを間に挟むことで回避している)。

`ffmpeg_stub`自体を独自にビルドして差し替えたい場合は`EncodeOptions::ffmpeg_stub_path`に明示パスを渡せる(その場合は埋め込み版が使われない)。

### オフライン埋め込みビルド(`bundled` feature)

`downloader::ensure_ffmpeg`は既定でGitHub Releasesからネットワーク経由でffmpegを取得するが、`bundled` featureを有効にすると、ビルド時に対象OS/arch用のffmpeg zipをバイナリへ`include_bytes!`で埋め込み、**実行時**は一切ネットワークへアクセスせず`setup(tools_dir)`だけでffmpegが使える状態になる。

1. `bundled` feature付きでビルドする。

   ```sh
   cargo build --release --features bundled
   # cdylibとして:
   cargo build --release --features bundled --crate-type cdylib
   ```

2. **ビルド時**(`cargo build`実行時)に、`build.rs`が以下の順序でzipを解決する:

   1. リポジトリルートの`assets/ffmpeg-<platform>-<arch>-binary-only.zip`(`platform`は`windows`/`linux`/`macos`、`arch`は`amd64`(x86_64)/`arm64`(aarch64)、ビルドを実行しているホスト自身のOS/archから自動的に決まる)が既に存在すればそれを使う。
   2. 存在しなければ、`mattuu0/ffmpeg-builder`の[Releases](https://github.com/mattuu0/ffmpeg-builder/releases)から**自動的にダウンロードし**、次回以降のビルドで再ダウンロードしないよう`assets/`にキャッシュする。

   つまり何も用意しなくても`cargo build --features bundled`だけで通る(この**ビルド時ダウンロード**にはネットワークが必要)。オフラインでビルドしたい場合や、CIでのキャッシュ済みビルドを速くしたい場合は、事前に`assets/`へzipを手動配置しておけば手順1の経路が使われ、ビルド時のダウンロードもスキップされる。

   ダウンロードに失敗した場合(ネットワーク不通、GitHub API制限等)は`build.rs`がビルド時にpanicして分かりやすいエラーを出す(失敗を黙って無視して不完全なバイナリを作ることはない)。

3. **クロスコンパイル時の注意**: `build.rs`は「ビルドを実行しているホスト自身」のOS/arch向けのzipを解決する(クロスターゲット向けのOS/arch判定はしない、自動ダウンロードも同様)。別のターゲット向けに埋め込みたい場合は、`assets/`にそのターゲット用のzipを事前配置しておくこと。

4. 生成物(DLL/rlib)はそのまま単体で**実行時オフライン**環境に配布・展開できる。`setup(tools_dir)`を呼ぶと、埋め込まれたzipを`tools_dir`に展開するだけで完了する(ビルド時のダウンロードと実行時のセットアップは別物である点に注意)。

   注意: `bundled`を有効にしても`elevate::ensure_paexec`(PAExec自動ダウンロード、Windows専用)は現時点ではネットワーク経由のみで、埋め込み対象には含まれていない(PLAN.mdの方針: 「まずffmpeg本体を対象とする」)。完全オフラインでSYSTEM昇格キャプチャも使いたい場合は、`paexec.exe`を事前に`tools_dir`へ手動配置しておくこと(`ensure_paexec`は既に存在するファイルを検出したらダウンロードをスキップする)。

## バイナリサイズについて

既定の`[profile.release]`(`Cargo.toml`)は以下を設定しており、素の`cargo build --release`と比べて概ね4割程度サイズが減る(手元計測: 3.1MB → 1.9MB、cdylib本体のみ、`bundled`無効時):

```toml
[profile.release]
strip = true          # デバッグシンボルをバイナリから除去する(pdb/dSYM自体は別途生成される)
lto = true            # リンク時最適化で未使用コードの削除を強化する
codegen-units = 1      # LTOの効果を最大化する(ビルド時間は伸びる)
panic = "abort"        # パニック時のスタック巻き戻しコードを除去する
opt-level = "z"         # 速度よりサイズを優先する
```

さらにサイズを削りたい場合:

- **`bundled` featureとの兼ね合い**: `bundled`を有効にするとffmpeg本体(数十MB規模)がバイナリに埋め込まれるため、`bundled`は既定で無効なオプトインfeatureのままにしておくこと。「小さいDLL+オンラインダウンロード」と「大きいDLL+オフライン即利用」はトレードオフであり、両方を同時に最小化することはできない。
- **`opt-level = "s"`** (`"z"`より若干サイズ最適化の範囲が狭いが、実行速度への影響は緩やか)に変更して速度とサイズのバランスを取ることもできる。
- 現状の主な依存クレートサイズの内訳: `ureq`(HTTPS/TLS、GitHub Releases APIとPAExecダウンロードに必要)、`windows`(DXGI/Direct3D11、Windowsのみ)、`tokio`(非同期ランタイム、プロセス監視・再起動ループに必要)。いずれも機能的に必須のため、これ以上削るには機能自体を削る判断が必要になる。

## Examples

`examples/`配下の3本はいずれも実機(画面キャプチャ・ffmpeg実行が可能な環境)での実行を想定している。CIやheadless環境では`enumerate_displays()`やffmpeg起動が失敗する。

### `capture_and_encode.rs` — 基本フロー(通常起動)

```sh
cargo run --release --example capture_and_encode
```

`setup()`→プライマリディスプレイ選択→エンコーダ自動判定→`subscribe_frames`でKey/Deltaフレームをコンソール出力、という最小構成。`./tools`ディレクトリにffmpegを自動ダウンロードする(2回目以降はキャッシュされたものを使う)。起動後500ms待って`get_parameter_sets()`(SPS/PPS)を取得できることも確認する。10秒間キャプチャして終了する。

### `system_elevated_capture.rs` — SYSTEM昇格キャプチャ(Windowsのみ有効)

```sh
cargo run --release --example system_elevated_capture
```

`ElevationMode::PreferSystem`を指定し、PAExec自動ダウンロード→SYSTEM権限でのffmpeg起動を試みる。`ffmpeg_stub.exe`はライブラリに埋め込まれているため事前配置は不要(「`ffmpeg_stub`」節を参照)。

- 管理者権限のターミナルから実行した場合: PAExec経由のSYSTEM昇格が成功し、UAC同意プロンプト表示中もキャプチャが継続する。
- 管理者権限でない場合: PAExecのサービスインストールが失敗し、自動的に通常起動へフォールバックする(この場合UACセキュアデスクトップはキャプチャできないが、通常のデスクトップキャプチャ自体は機能する)。

15秒間キャプチャして終了する。

### `multi_subscriber.rs` — 複数コールバック購読

```sh
cargo run --release --example multi_subscriber
```

同一の`MonitorPipeline`に対して`subscribe_frames`を2つ、`subscribe_raw`を1つ登録し、全コールバックに同じエンコード結果が配信されることを確認する。タスクマネージャ等でffmpegプロセスが1つだけ起動していることも合わせて確認できる。「1モニターにつき1ffmpegプロセス」の原則(購読者が増えても追加のプロセス・読み取りループは発生しない)を確かめるためのサンプル。10秒間キャプチャして終了する。

## モジュール構成

| モジュール | 内容 |
|---|---|
| `downloader` | ffmpeg自動ダウンロード(`ensure_ffmpeg`)、`bundled` feature対応 |
| `display` | ディスプレイ列挙(`enumerate_displays`)、`display://` URI解析(`parse_display_uri`) |
| `encoder` | エンコーダ自動判定(`pick_best_encoder`) |
| `nal` | NALフレームパース(`NalSplitter`)、パラメータセット抽出 |
| `elevate` | SYSTEM昇格キャプチャ(`ensure_paexec`、`spawn_preferring_system`、Windowsのみ) |
| `pipeline` | `MonitorPipeline`(1モニター1ffmpegプロセスのライフサイクル管理) |
| `ffi` | cdylib配布用のC ABI関数群 |

詳細な設計判断・移植元の対応関係は[PLAN.md](PLAN.md)を参照。
