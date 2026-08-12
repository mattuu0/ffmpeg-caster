use ffmpeg_caster::{
    display::enumerate_displays,
    encoder::pick_best_encoder,
    pipeline::{EncodeOptions, MonitorPipeline},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. バイナリの取得・設定(既に tools_dir にあればダウンロードはスキップされる)
    let tools_dir = std::path::Path::new("./tools");
    let toolchain = ffmpeg_caster::setup(tools_dir).map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // 2. ディスプレイオブジェクト(一覧)を取得
    let displays = enumerate_displays().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let display = displays
        .into_iter()
        .find(|d| d.is_primary)
        .expect("no primary display found");

    // エンコーダの自動判定(HEVC/H264、ハードウェア優先)
    let (codec, hw_encoder) = pick_best_encoder(&toolchain.ffmpeg_path, None)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // 3. パイプラインオブジェクトを生成(まだ起動しない)
    let options = EncodeOptions {
        bitrate_kbps: 8_000,
        ..Default::default()
    };
    let mut pipeline = MonitorPipeline::new(
        &toolchain.ffmpeg_path,
        display.into(),
        codec,
        Some(hw_encoder),
        options,
    );

    // 4. callbackを設定(起動前に登録しておける)
    pipeline.subscribe_frames(|frame| match frame.kind {
        ffmpeg_caster::pipeline::FrameKind::Key => {
            println!("[frame] KEY   {} bytes", frame.payload.len());
        }
        ffmpeg_caster::pipeline::FrameKind::Delta => {
            println!("[frame] delta {} bytes", frame.payload.len());
        }
    });

    // 5. 起動
    pipeline
        .start()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // SPS/PPSが必要なら、最初のキーフレーム到達後にここで取得できる
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if let Some(params) = pipeline.get_parameter_sets() {
        println!("parameter sets: {} bytes", params.payload.len());
    }

    // 6. しばらく配信を継続(callbackが呼ばれ続ける)
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;

    // 7. 配信終了
    pipeline.stop();

    Ok(())
}
