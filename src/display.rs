//! rust-cast(sender/src-tauri/src/display_manager.rs)から移植したディスプレイ
//! 列挙・選択ロジック。WindowsはDXGI/Direct3D11で物理ディスプレイを列挙する。
//! Linux/macOSはこのcrateで新規実装したクロスプラットフォーム対応。
//!
//! `display://`スタイルのURIで対象ディスプレイを指定できる`parse_display_uri`
//! も提供する。

use crate::error::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
    /// false for virtual/software displays that don't support capture
    pub can_capture: bool,
    /// このディスプレイが属するアダプター内でのEnumOutputsインデックス(0-based)。
    /// ffmpegのddagrabフィルタは常にプライマリアダプター(EnumAdapters1(0))固定で
    /// output_idxにこの値をそのまま渡す設計のため、indexとは別に保持する。
    /// Linux/macOSではキャプチャ対象を区別するためのインデックスとして流用する。
    pub adapter_output_idx: u32,
}

/// `MonitorPipeline::new`に渡す、解決済みのキャプチャ対象。
/// `enumerate_displays()`の結果から`DisplayInfo`をそのまま変換して作る想定。
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayTarget {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub adapter_output_idx: u32,
}

impl From<DisplayInfo> for DisplayTarget {
    fn from(d: DisplayInfo) -> Self {
        DisplayTarget {
            name: d.name,
            width: d.width,
            height: d.height,
            adapter_output_idx: d.adapter_output_idx,
        }
    }
}

#[cfg(target_os = "windows")]
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    };

    let mut displays = Vec::new();
    let mut index = 0usize;

    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| e.to_string())?;

        let mut adapter_idx = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            adapter_idx += 1;

            let mut device: Option<ID3D11Device> = None;
            let _ = D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            );

            let mut output_idx = 0u32;
            loop {
                let output: IDXGIOutput = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let this_output_idx = output_idx;
                output_idx += 1;

                let desc = output.GetDesc().map_err(|e| e.to_string())?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }

                let name = String::from_utf16_lossy(
                    &desc.DeviceName[..desc
                        .DeviceName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.DeviceName.len())],
                );

                let rect = desc.DesktopCoordinates;
                let width = (rect.right - rect.left) as u32;
                let height = (rect.bottom - rect.top) as u32;
                let is_primary = rect.left == 0 && rect.top == 0;

                let can_capture = if let Some(ref dev) = device {
                    let output1: std::result::Result<IDXGIOutput1, _> = output.cast();
                    match output1 {
                        Ok(o1) => o1.DuplicateOutput(dev).is_ok(),
                        Err(_) => false,
                    }
                } else {
                    false
                };

                displays.push(DisplayInfo {
                    index,
                    name,
                    width,
                    height,
                    is_primary,
                    can_capture,
                    adapter_output_idx: this_output_idx,
                });
                index += 1;
            }
        }
    }

    Ok(displays)
}

/// DXGIの`DeviceName`(例: `\\.\DISPLAY3`)からそのディスプレイの現在の
/// adapter_output_idxを再解決する。他の仮想/実モニターが増減すると
/// EnumOutputsの列挙順(=adapter_output_idx)がズレるため、ffmpeg起動時に
/// 固定したインデックスをそのまま使い続けると誤ったモニターを掴んだり
/// キャプチャが止まったりする(実機確認済み)。モニター構成の変化を検知した
/// 際、pipeline::MonitorPipelineの監視ループがこの関数で現在の正しい
/// インデックスを取り直してからffmpegを再起動する。
#[cfg(target_os = "windows")]
pub fn find_display_by_name(name: &str) -> Option<DisplayInfo> {
    enumerate_displays()
        .ok()?
        .into_iter()
        .find(|d| d.name == name)
}

/// X11の`xrandr --current`出力を解析してディスプレイ一覧を得る。
/// Waylandは非対応(x11grabで直接キャプチャできる出力のみを対象とする)。
#[cfg(target_os = "linux")]
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    let output = std::process::Command::new("xrandr")
        .arg("--current")
        .output()
        .map_err(|e| format!("failed to run xrandr: {e}"))?;
    if !output.status.success() {
        return Err("xrandr exited with a non-zero status".into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut displays = Vec::new();
    let mut index = 0usize;
    for line in stdout.lines() {
        // 例: "HDMI-1 connected primary 1920x1080+0+0 (normal left inverted...) 527mm x 296mm"
        if !line.contains(" connected") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let rest: Vec<&str> = parts.collect();
        let is_primary = rest.iter().any(|p| *p == "primary");

        let geometry = rest.iter().find(|p| {
            p.chars().next().is_some_and(|c| c.is_ascii_digit())
                && p.contains('x')
                && p.contains('+')
        });
        let Some(geometry) = geometry else { continue };
        // "1920x1080+0+0" -> width=1920, height=1080
        let Some((size, _offsets)) = geometry.split_once('+') else {
            continue;
        };
        let Some((w, h)) = size.split_once('x') else {
            continue;
        };
        let (Ok(width), Ok(height)) = (w.parse::<u32>(), h.parse::<u32>()) else {
            continue;
        };

        displays.push(DisplayInfo {
            index,
            name: name.to_string(),
            width,
            height,
            is_primary,
            can_capture: true,
            adapter_output_idx: index as u32,
        });
        index += 1;
    }

    Ok(displays)
}

#[cfg(target_os = "linux")]
pub fn find_display_by_name(name: &str) -> Option<DisplayInfo> {
    enumerate_displays()
        .ok()?
        .into_iter()
        .find(|d| d.name == name)
}

/// AVFoundationのキャプチャデバイス一覧(`ffmpeg -f avfoundation -list_devices
/// true -i ""`)から画面キャプチャデバイスのみを抜き出す。AVFoundationの
/// デバイス一覧はビデオ入力(カメラ等)と画面キャプチャが混在するインデックス
/// 空間を共有するため、"Capture screen"を含む行のみを対象にする。
///
/// AVFoundationのデバイス一覧自体には解像度情報が含まれないため、
/// `system_profiler SPDisplaysDataType`から別途取得した解像度をディスプレイ
/// 順序で対応付ける。両者の順序が完全に一致する保証はないが、実用上
/// 「Capture screen 0」がメインディスプレイに対応するのがほぼ確実であり、
/// 対応する解像度が見つからない場合は`0x0`ではなくメインディスプレイの
/// 解像度をフォールバックとして使う(`-video_size 0x0`という無効な引数を
/// ffmpegに渡してキャプチャが失敗する不具合を避けるため)。
#[cfg(target_os = "macos")]
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    let ffmpeg =
        which_ffmpeg().ok_or("ffmpeg not found on PATH; cannot enumerate AVFoundation devices")?;
    let output = std::process::Command::new(ffmpeg)
        .args(["-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .output()
        .map_err(|e| format!("failed to run ffmpeg for device listing: {e}"))?;
    // ffmpegはデバイス一覧をstderrに出力し、-iに空文字を渡すため常に非0で終了する。
    let stderr = String::from_utf8_lossy(&output.stderr);

    let resolutions = query_display_resolutions();

    let mut displays = Vec::new();
    let mut in_video_section = false;
    for line in stderr.lines() {
        if line.contains("AVFoundation video devices") {
            in_video_section = true;
            continue;
        }
        if line.contains("AVFoundation audio devices") {
            in_video_section = false;
            continue;
        }
        if !in_video_section || !line.contains("Capture screen") {
            continue;
        }
        // 例: "[AVFoundation indev @ 0x...] [1] Capture screen 0"
        let Some(bracket_start) = line.rfind('[') else {
            continue;
        };
        let Some(bracket_end) = line[bracket_start..].find(']') else {
            continue;
        };
        let Ok(idx) = line[bracket_start + 1..bracket_start + bracket_end].parse::<u32>() else {
            continue;
        };

        let (width, height) = resolutions
            .get(displays.len())
            .or_else(|| resolutions.first())
            .copied()
            .unwrap_or((1920, 1080));

        displays.push(DisplayInfo {
            index: idx as usize,
            name: format!("Capture screen {idx}"),
            width,
            height,
            is_primary: idx == 0,
            can_capture: true,
            adapter_output_idx: idx,
        });
    }

    Ok(displays)
}

/// `system_profiler SPDisplaysDataType`のプレーンテキスト出力から
/// "Resolution: 1920 x 1080"のような行を、出現順(ディスプレイの列挙順)で
/// 取り出す。`system_profiler`が使えない、または解析に失敗した場合は
/// 空Vecを返す(呼び出し元がフォールバック解像度を使う)。
#[cfg(target_os = "macos")]
fn query_display_resolutions() -> Vec<(u32, u32)> {
    let output = match std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut resolutions = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("Resolution:") else {
            continue;
        };
        // 例: "Resolution: 1920 x 1080" (Retinaの場合 "3024 x 1964 Retina" 等の
        // 表記が付くこともあるため、先頭の2つの数値だけを見る)。
        let mut nums = rest
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty());
        let Some(Ok(width)) = nums.next().map(str::parse::<u32>) else {
            continue;
        };
        let Some(Ok(height)) = nums.next().map(str::parse::<u32>) else {
            continue;
        };
        resolutions.push((width, height));
    }
    resolutions
}

#[cfg(target_os = "macos")]
fn which_ffmpeg() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("ffmpeg"))
        .find(|p| p.is_file())
}

#[cfg(target_os = "macos")]
pub fn find_display_by_name(name: &str) -> Option<DisplayInfo> {
    enumerate_displays()
        .ok()?
        .into_iter()
        .find(|d| d.name == name)
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    Err("screen capture is not supported on this platform".into())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn find_display_by_name(_name: &str) -> Option<DisplayInfo> {
    None
}

/// `display://`スタイルのURIから対象ディスプレイを解決する。対応スキーム:
/// - `display://primary` — プライマリディスプレイ
/// - `display://index/<N>` — `enumerate_displays()`が返す配列のN番目(0-based)
/// - `display://name/<DeviceName>` — ディスプレイ名(Windowsなら`\\.\DISPLAY3`等)で指定
pub fn parse_display_uri(uri: &str) -> Result<DisplayTarget> {
    let rest = uri
        .strip_prefix("display://")
        .ok_or_else(|| format!("not a display:// URI: {uri}"))?;

    let displays = enumerate_displays()?;

    if rest == "primary" {
        return displays
            .into_iter()
            .find(|d| d.is_primary)
            .map(DisplayTarget::from)
            .ok_or_else(|| "no primary display found".into());
    }

    if let Some(idx_str) = rest.strip_prefix("index/") {
        let idx: usize = idx_str
            .parse()
            .map_err(|_| format!("invalid display index in URI: {idx_str}"))?;
        return displays
            .into_iter()
            .find(|d| d.index == idx)
            .map(DisplayTarget::from)
            .ok_or_else(|| format!("no display with index {idx}").into());
    }

    if let Some(name) = rest.strip_prefix("name/") {
        return displays
            .into_iter()
            .find(|d| d.name == name)
            .map(DisplayTarget::from)
            .ok_or_else(|| format!("no display named {name}").into());
    }

    Err(format!("unrecognized display:// URI form: {uri}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_uri_rejects_unknown_scheme() {
        assert!(parse_display_uri("foo://bar").is_err());
    }

    #[test]
    fn parse_display_uri_rejects_unknown_form() {
        // enumerate_displays() may itself fail in a headless CI environment,
        // but an unrecognized scheme suffix should be rejected before that
        // matters on platforms where enumeration succeeds trivially.
        let result = parse_display_uri("display://bogus");
        assert!(result.is_err());
    }
}
