//! rust-cast(poc/sender_poc/src/nal_splitter.rs)から移植したアクセスユニット
//! 単位のNAL分割ロジック。
//!
//! アクセスユニットの確定条件は「非VCL NAL(AUD/SEI/SPS/PPS等)が来て、かつ
//! 直前までVCLスライスを蓄積済みだったこと」。パラメータセットは除外せず
//! そのままフレームバイト列に含める(インバンド方式)。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

/// SPS/PPS(HEVCならVPSも)を直近確定分だけ連結したAnnex-Bバイト列。
/// WebCodecsの`VideoDecoderConfig.description`やMediaCodecの`csd-0`/`csd-1`に
/// そのまま渡せる形にする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSets {
    pub payload: Vec<u8>,
}

/// Annex-Bストリーム中のスタートコード(`00 00 01`または`00 00 00 01`)の
/// 開始位置一覧を返す。各要素は「スタートコードそのものの開始位置」。
fn find_start_codes(data: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if i > 0 && data[i - 1] == 0 {
                positions.push(i - 1);
            } else {
                positions.push(i);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    positions
}

/// NALユニットタイプを取得する。スタートコード直後の最初のバイトから抽出する
/// (H.264/HEVCでビット位置が異なる)。
fn nal_unit_type(nal_first_byte: u8, codec: Codec) -> u8 {
    match codec {
        Codec::H264 => nal_first_byte & 0x1f,
        Codec::Hevc => (nal_first_byte >> 1) & 0x3f,
    }
}

/// このNALタイプがVCL(スライス、実ピクチャデータ)かどうか。
fn is_vcl_nal(nal_type: u8, codec: Codec) -> bool {
    match codec {
        Codec::H264 => (1..=5).contains(&nal_type),
        Codec::Hevc => nal_type <= 21,
    }
}

/// このNALタイプがIDR/IRAP(キーフレームのVCLスライス)かどうか。
fn is_keyframe_vcl_nal(nal_type: u8, codec: Codec) -> bool {
    match codec {
        Codec::H264 => nal_type == 5,
        Codec::Hevc => (16..=23).contains(&nal_type),
    }
}

/// このNALタイプがパラメータセット(SPS/PPS、HEVCならVPSも)かどうか。
/// AUD/SEIは除外する。
fn is_parameter_set_nal(nal_type: u8, codec: Codec) -> bool {
    match codec {
        // H.264: 7=SPS, 8=PPS
        Codec::H264 => nal_type == 7 || nal_type == 8,
        // HEVC: 32=VPS, 33=SPS, 34=PPS
        Codec::Hevc => (32..=34).contains(&nal_type),
    }
}

/// 1アクセスユニット分のAnnex-Bバイト列にキーフレームのVCLスライスが
/// 含まれるかどうかを走査して判定する。
fn frame_contains_keyframe(frame: &[u8], codec: Codec) -> bool {
    for start in find_start_codes(frame) {
        let nal_body_offset = if frame.get(start + 2) == Some(&1) {
            start + 3
        } else {
            start + 4
        };
        let Some(&first_byte) = frame.get(nal_body_offset) else {
            continue;
        };
        let nal_type = nal_unit_type(first_byte, codec);
        if is_keyframe_vcl_nal(nal_type, codec) {
            return true;
        }
    }
    false
}

/// 1アクセスユニット分のAnnex-Bバイト列から、パラメータセットNAL
/// (SPS/PPS/VPSのみ、AUD/SEIは除外)だけを抜き出して連結する。
fn extract_parameter_sets(frame: &[u8], codec: Codec) -> Vec<u8> {
    let starts = find_start_codes(frame);
    let mut out = Vec::new();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(frame.len());
        let nal_body_offset = if frame.get(start + 2) == Some(&1) {
            start + 3
        } else {
            start + 4
        };
        let Some(&first_byte) = frame.get(nal_body_offset) else {
            continue;
        };
        let nal_type = nal_unit_type(first_byte, codec);
        if is_parameter_set_nal(nal_type, codec) {
            out.extend_from_slice(&frame[start..end]);
        }
    }
    out
}

pub struct FrameChunk {
    pub payload: Vec<u8>,
    pub is_key: bool,
}

/// ffmpeg stdoutから読み取ったバイト列を蓄積し、確定したアクセスユニット
/// (パラメータセットNAL + 1つ以上のVCLスライスNALの組)単位に分割する
/// ストリーミングパーサー。
pub struct NalSplitter {
    codec: Codec,
    stream_buf: Vec<u8>,
    frame_buf: Vec<u8>,
    frame_has_vcl: bool,
    latest_parameter_sets: Option<ParameterSets>,
}

impl NalSplitter {
    pub fn new(codec_is_hevc: bool) -> Self {
        Self {
            codec: if codec_is_hevc {
                Codec::Hevc
            } else {
                Codec::H264
            },
            stream_buf: Vec::new(),
            frame_buf: Vec::new(),
            frame_has_vcl: false,
            latest_parameter_sets: None,
        }
    }

    /// 直近確定分のパラメータセット(SPS/PPS/VPS)。最初のキーフレームが
    /// 通過するまではNone。以後はパラメータセットが変化する度に更新される
    /// (通常は解像度/コーデック設定が変わらない限り不変)。
    pub fn latest_parameter_sets(&self) -> Option<&ParameterSets> {
        self.latest_parameter_sets.as_ref()
    }

    /// 新しく受信したバイト列を追加し、この時点で確定しているアクセス
    /// ユニット(1フレーム分のAnnex-Bバイト列、パラメータセット含む)を返す。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<FrameChunk> {
        self.stream_buf.extend_from_slice(chunk);

        let starts = find_start_codes(&self.stream_buf);
        if starts.len() < 2 {
            return Vec::new();
        }

        let mut frames = Vec::new();
        for pair in starts.windows(2) {
            let (start, next_start) = (pair[0], pair[1]);
            let nal_body_offset = if self.stream_buf[start + 2] == 1 {
                start + 3
            } else {
                start + 4
            };
            let Some(&first_byte) = self.stream_buf.get(nal_body_offset) else {
                continue;
            };
            let nal_type = nal_unit_type(first_byte, self.codec);
            let is_vcl = is_vcl_nal(nal_type, self.codec);

            if !is_vcl && self.frame_has_vcl {
                // 非VCL NAL(次のアクセスユニットのAUD/SEI/SPS/PPS等)が来た =
                // 直前まで蓄積していたスライス群でフレームは確定。
                let is_key = frame_contains_keyframe(&self.frame_buf, self.codec);
                let param_sets = extract_parameter_sets(&self.frame_buf, self.codec);
                if !param_sets.is_empty() {
                    self.latest_parameter_sets = Some(ParameterSets {
                        payload: param_sets,
                    });
                }
                let payload = std::mem::take(&mut self.frame_buf);
                self.frame_has_vcl = false;
                frames.push(FrameChunk { payload, is_key });
            }
            if is_vcl {
                self.frame_has_vcl = true;
            }
            self.frame_buf
                .extend_from_slice(&self.stream_buf[start..next_start]);
        }

        // 最後に見つけたスタートコード以降(未確定の最後のNAL)は次回に持ち越す。
        let last_start = *starts.last().unwrap();
        self.stream_buf.drain(..last_start);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(codec: Codec, nal_type: u8, body: &[u8]) -> Vec<u8> {
        let first_byte = match codec {
            Codec::H264 => nal_type & 0x1f,
            Codec::Hevc => (nal_type & 0x3f) << 1,
        };
        let mut v = vec![0, 0, 0, 1, first_byte];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn splits_h264_access_units() {
        let mut splitter = NalSplitter::new(false);
        let sps = nal(Codec::H264, 7, &[0xaa]);
        let pps = nal(Codec::H264, 8, &[0xbb]);
        let idr = nal(Codec::H264, 5, &[0xcc]);
        let p = nal(Codec::H264, 1, &[0xdd]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&pps);
        stream.extend_from_slice(&idr);
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&pps);
        stream.extend_from_slice(&p);
        // two trailing NALs are needed to flush the second frame: push()
        // only finalizes an access unit once the *next* non-VCL NAL's start
        // code has been observed (the final start code is always carried
        // over to the next push() call, unfinalized).
        stream.extend_from_slice(&nal(Codec::H264, 6, &[]));
        stream.extend_from_slice(&nal(Codec::H264, 6, &[]));

        let frames = splitter.push(&stream);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
        assert!(!frames[1].is_key);
        assert!(splitter.latest_parameter_sets().is_some());
    }

    #[test]
    fn splits_multi_slice_frame_as_one_access_unit() {
        // Two VCL NALs (slices) belonging to the same frame must not be split
        // into two frames (this was the bug in the old 1-NAL=1-message parser).
        let mut splitter = NalSplitter::new(false);
        let idr_slice1 = nal(Codec::H264, 5, &[0x01]);
        let idr_slice2 = nal(Codec::H264, 5, &[0x02]);
        let aud = nal(Codec::H264, 9, &[]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&idr_slice1);
        stream.extend_from_slice(&idr_slice2);
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&nal(Codec::H264, 6, &[]));

        let frames = splitter.push(&stream);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_key);
    }

    #[test]
    fn hevc_parameter_set_detection() {
        let mut splitter = NalSplitter::new(true);
        let vps = nal(Codec::Hevc, 32, &[0x01]);
        let sps = nal(Codec::Hevc, 33, &[0x02]);
        let pps = nal(Codec::Hevc, 34, &[0x03]);
        let idr = nal(Codec::Hevc, 19, &[0x04]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&vps);
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&pps);
        stream.extend_from_slice(&idr);
        stream.extend_from_slice(&nal(Codec::Hevc, 35, &[]));
        stream.extend_from_slice(&nal(Codec::Hevc, 35, &[]));

        let frames = splitter.push(&stream);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_key);
        let params = splitter.latest_parameter_sets().unwrap();
        assert!(!params.payload.is_empty());
    }
}
