use ffmpeg_caster::{
    display::enumerate_displays,
    elevate::ElevationMode,
    encoder::pick_best_encoder,
    pipeline::{EncodeOptions, MonitorPipeline},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tools_dir = std::path::Path::new("./tools");
    let toolchain = ffmpeg_caster::setup(tools_dir).map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let displays = enumerate_displays().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let display = displays
        .into_iter()
        .find(|d| d.is_primary)
        .expect("no primary display found");

    let (codec, hw_encoder) = pick_best_encoder(&toolchain.ffmpeg_path, None)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // SYSTEM昇格キャプチャ(Windowsのみ有効)。呼び出し元プロセス自身が既に
    // 管理者権限で起動していることが前提 -- そうでない場合はPAExecのサービス
    // インストールが失敗し、自動的に通常起動へフォールバックする。
    let options = EncodeOptions {
        bitrate_kbps: 8_000,
        elevation: ElevationMode::PreferSystem,
        tools_dir: Some(tools_dir.to_path_buf()),
        ..Default::default()
    };
    let mut pipeline = MonitorPipeline::new(
        &toolchain.ffmpeg_path,
        display.into(),
        codec,
        Some(hw_encoder),
        options,
    );

    pipeline.subscribe_frames(|frame| {
        println!("[frame] {:?} {} bytes", frame.kind, frame.payload.len());
    });

    pipeline
        .start()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    println!("capturing (including UAC secure desktop, if elevation succeeded)...");
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;

    pipeline.stop();
    Ok(())
}
