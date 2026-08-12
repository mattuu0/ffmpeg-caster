use ffmpeg_caster::{
    display::enumerate_displays,
    encoder::pick_best_encoder,
    pipeline::{EncodeOptions, MonitorPipeline},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let toolchain = ffmpeg_caster::setup(std::path::Path::new("./tools"))
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let display = enumerate_displays()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .into_iter()
        .find(|d| d.is_primary)
        .unwrap();
    let (codec, hw) = pick_best_encoder(&toolchain.ffmpeg_path, None)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let mut pipeline = MonitorPipeline::new(
        &toolchain.ffmpeg_path,
        display.into(),
        codec,
        Some(hw),
        EncodeOptions::default(),
    );

    // 複数のcallbackを登録しても、ffmpegプロセス・読み取りループは1本のまま。
    // 全callbackに同じエンコード結果が配られる。
    pipeline.subscribe_frames(|frame| {
        println!(
            "[subscriber A] {:?} {} bytes",
            frame.kind,
            frame.payload.len()
        );
    });
    pipeline.subscribe_frames(|frame| {
        println!(
            "[subscriber B] {:?} {} bytes",
            frame.kind,
            frame.payload.len()
        );
    });
    pipeline.subscribe_raw(|chunk| {
        println!("[subscriber C - raw] {} bytes", chunk.len());
    });

    pipeline
        .start()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    pipeline.stop();

    Ok(())
}
