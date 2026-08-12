//! rust-cast(poc/sender_poc/src/encoder_autodetect.rs)から移植したエンコーダ
//! 自動判定ロジック。sender本体(streaming.rs select_encoder)と同じ優先順位
//! (HEVC > H264 > VP9、各コーデック内はハードウェア > ソフトウェア)で
//! コーデック横断的に拡張したもの。このffmpegビルドにコンパイルされている
//! エンコーダ一覧を確認し、優先度の高い順に1フレームだけの実エンコードを
//! プローブして、実際にこの環境で動くものを選ぶ。

use crate::error::Result;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
    Vp9,
}

impl Codec {
    /// このコーデックのAnnex B/生ビットストリーム出力に使うffmpegの`-f`値。
    /// VP9はAnnex Bという概念がなく生ビットストリームを直接扱えないため、
    /// 最小限のコンテナ(IVF)を使う。
    pub fn output_format(self) -> &'static str {
        match self {
            Codec::Hevc => "hevc",
            Codec::H264 => "h264",
            Codec::Vp9 => "ivf",
        }
    }

    /// ネゴシエーション用の識別子(WebCodecs/MediaCodec側で使うコーデック名)。
    pub fn wire_name(self) -> &'static str {
        match self {
            Codec::Hevc => "hevc",
            Codec::H264 => "h264",
            Codec::Vp9 => "vp9",
        }
    }

    /// CLI引数`--codec`の値をパースする。
    pub fn parse_cli(value: &str) -> Result<Self> {
        match value {
            "hevc" => Ok(Codec::Hevc),
            "h264" => Ok(Codec::H264),
            "vp9" => Ok(Codec::Vp9),
            other => Err(format!("unknown codec: {other} (expected hevc/h264/vp9)").into()),
        }
    }
}

/// sender本体と同じ優先順位: HEVC(ハードウェア) > H264(ハードウェア) >
/// H264(libopenh264、CPU) > VP9(libvpx-vp9、CPU)。
/// ハードウェアVP9(vp9_nvenc等)はffmpeg-builderの標準ビルドに含まれないため
/// ここでは対象にしない。
fn encoder_priority() -> &'static [(Codec, &'static str)] {
    &[
        (Codec::Hevc, "hevc_nvenc"),
        (Codec::Hevc, "hevc_amf"),
        (Codec::Hevc, "hevc_qsv"),
        (Codec::Hevc, "hevc_vaapi"),
        (Codec::Hevc, "hevc_videotoolbox"),
        (Codec::H264, "h264_nvenc"),
        (Codec::H264, "h264_amf"),
        (Codec::H264, "h264_qsv"),
        (Codec::H264, "h264_vaapi"),
        (Codec::H264, "h264_videotoolbox"),
        (Codec::H264, "libopenh264"),
        (Codec::Vp9, "libvpx-vp9"),
    ]
}

fn list_compiled_encoders(ffmpeg: &Path) -> Vec<String> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .expect("failed to run ffmpeg -encoders");
    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let mut parts = trimmed.splitn(3, char::is_whitespace);
            let flags = parts.next()?;
            let name = parts.next()?;
            if flags.len() == 6 && flags.chars().next().is_some_and(|c| "VAS".contains(c)) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// あるエンコーダがハードウェアエンコーダ(D3D11フレームをそのまま受け取れる)
/// かどうか。ソフトウェアエンコーダはシステムメモリのyuv420pへ変換してから
/// 渡す必要がある(build_capture_input_argsと同じ判断)。
pub fn is_hardware_encoder(encoder: &str) -> bool {
    encoder.ends_with("_nvenc")
        || encoder.ends_with("_amf")
        || encoder.ends_with("_qsv")
        || encoder.ends_with("_vaapi")
        || encoder.ends_with("_videotoolbox")
}

/// エンコーダに対応する出力フォーマット(`-f`)。VP9はAnnex Bという概念が
/// ないためIVFコンテナへ出力する。
fn output_format_for(encoder: &str) -> &'static str {
    if encoder.starts_with("hevc") {
        "hevc"
    } else if encoder.starts_with("h264") || encoder == "libopenh264" {
        "h264"
    } else {
        "ivf"
    }
}

/// ffmpeg-builderの`--disable-everything`ベースの最小構成ビルドでは、`-f lavfi`の
/// colorソースや`null`ムキサー、`hwdownload`フィルタが無効化されている
/// (実機確認済み: `ffmpeg -filters`/`-muxers`に出てこない)。そのため単色
/// フレームでのプローブや`hwdownload`によるCPUフォールバック変換は使えない。
/// 代わりに実際のddagrabキャプチャを1フレームだけNUL(Windowsの捨て先デバイス)
/// へエンコードして、この環境で実際に動くかを確認する。CPUエンコーダ
/// (libopenh264/libvpx-vp9)はD3D11フレームを直接渡せないため、
/// `format=bgra,format=yuv420p`(hwdownloadを介さないシステムメモリへの明示転送)
/// を経由する。
fn probe_encoder_works(ffmpeg: &Path, encoder: &str) -> bool {
    let filter_complex = if is_hardware_encoder(encoder) {
        "ddagrab=framerate=5[vout]".to_string()
    } else {
        "ddagrab=framerate=5,format=bgra,format=yuv420p[vout]".to_string()
    };
    let output_format = output_format_for(encoder);

    let null_sink = if cfg!(windows) { "NUL" } else { "/dev/null" };

    let mut child = match Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-filter_complex", &filter_complex])
        .args(["-map", "[vout]"])
        .args(["-frames:v", "1", "-c:v", encoder])
        .args(["-f", output_format, "-y", null_sink])
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(15) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return false,
        }
    }
}

/// このffmpegビルドで実際に使える最良の(コーデック, エンコーダ名)を
/// HEVC > H264 > VP9優先度でプローブして返す。`preferred_codec`が指定された
/// 場合は、そのコーデック以外の候補を除外する。
pub fn pick_best_encoder(ffmpeg: &Path, preferred_codec: Option<Codec>) -> Result<(Codec, String)> {
    let priority = encoder_priority();
    let compiled = list_compiled_encoders(ffmpeg);

    let candidates: Vec<&(Codec, &str)> = priority
        .iter()
        .filter(|(_, enc)| compiled.iter().any(|c| c == enc))
        .filter(|(codec, _)| match preferred_codec {
            Some(preferred) => *codec == preferred,
            None => true,
        })
        .collect();

    if candidates.is_empty() {
        return Err(format!(
            "no usable encoder compiled into this ffmpeg build (checked: {})",
            priority
                .iter()
                .map(|(_, e)| *e)
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    for (codec, enc) in &candidates {
        if probe_encoder_works(ffmpeg, enc) {
            return Ok((*codec, enc.to_string()));
        }
    }

    Err(format!(
        "no usable encoder found at runtime among compiled candidates: {}",
        candidates
            .iter()
            .map(|(_, e)| *e)
            .collect::<Vec<_>>()
            .join(", ")
    )
    .into())
}
