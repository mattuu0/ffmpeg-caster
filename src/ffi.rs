//! cdylib配布用のC ABI関数群。DLL/SO利用者向けに`extern "C"`関数を提供する。
//!
//! コールバックはC関数ポインタ(`extern "C" fn(*const u8, usize, *mut c_void)`)+
//! `user_data: *mut c_void`の形で受け取る。`Vec<u8>`/`String`等のRust型はFFI
//! 境界を越えないよう、生ポインタ+長さのペアと不透明ハンドル
//! (`*mut MonitorPipeline`)でラップする。内部のRust APIは通常のRust型で
//! 設計し、このモジュールはその薄いラッパーに徹する。

use crate::display::{parse_display_uri, DisplayTarget};
use crate::elevate::ElevationMode;
use crate::encoder::pick_best_encoder;
use crate::pipeline::{EncodeOptions, EncodedFrame, FrameKind, MonitorPipeline};
use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_int;
use std::path::Path;
use std::sync::OnceLock;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for ffmpeg_caster FFI")
    })
}

/// 呼び出し元が渡すフレームコールバック。`kind`: 0=Key, 1=Delta。
pub type FrameCallbackFn =
    extern "C" fn(kind: c_int, data: *const u8, len: usize, user_data: *mut c_void);
pub type RawCallbackFn = extern "C" fn(data: *const u8, len: usize, user_data: *mut c_void);

/// opaqueハンドル。`ffmpeg_caster_pipeline_free`で解放するまで呼び出し元が保持する。
pub struct PipelineHandle {
    inner: MonitorPipeline,
}

/// FFI境界越しに渡される`user_data`ポインタをクロージャに持たせるための
/// ラッパー。呼び出し元がスレッドセーフに扱う責務を負う前提(C ABIの性質上
/// 避けられない)。
struct SendSyncPtr(*mut c_void);
unsafe impl Send for SendSyncPtr {}
unsafe impl Sync for SendSyncPtr {}

/// `path` (UTF-8, NUL終端)に対して`downloader::ensure_ffmpeg`+
/// (Windowsのみ)`elevate::ensure_paexec`を呼ぶ。成功時0、失敗時-1を返し、
/// `out_ffmpeg_path`に解決済みffmpegパスをmallocされたC文字列として書き込む
/// (呼び出し元は`ffmpeg_caster_free_string`で解放すること)。
///
/// # Safety
/// `tools_dir`はNUL終端のUTF-8文字列を指す有効なポインタ、またはnullである
/// こと。`out_ffmpeg_path`はnull、または`*mut c_char`を書き込める有効な
/// ポインタであること。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_setup(
    tools_dir: *const c_char,
    out_ffmpeg_path: *mut *mut c_char,
) -> c_int {
    let Some(dir) = cstr_to_path(tools_dir) else {
        return -1;
    };
    match crate::setup(&dir) {
        Ok(toolchain) => {
            if !out_ffmpeg_path.is_null() {
                unsafe {
                    *out_ffmpeg_path = path_to_cstring(&toolchain.ffmpeg_path);
                }
            }
            0
        }
        Err(_) => -1,
    }
}

/// `ffmpeg_caster_setup`が返したC文字列を解放する。
///
/// # Safety
/// `s`は`ffmpeg_caster_setup`が返したポインタそのもの、またはnullであり、
/// 一度しか解放してはならない(二重解放は未定義動作)。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(s));
    }
}

/// `display_uri`(例: `display://primary`)を解決し、コーデック自動判定込みで
/// 新規パイプラインを生成する。成功時は非nullのハンドルを返す。
///
/// # Safety
/// `ffmpeg_path`・`display_uri`はいずれもNUL終端のUTF-8文字列を指す有効な
/// ポインタであること(nullは許容されない)。返り値のハンドルは
/// `ffmpeg_caster_pipeline_free`で解放するまで有効に保つこと。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_new(
    ffmpeg_path: *const c_char,
    display_uri: *const c_char,
    bitrate_kbps: u32,
    prefer_system_elevation: c_int,
) -> *mut PipelineHandle {
    let Some(ffmpeg_path) = cstr_to_path(ffmpeg_path) else {
        return std::ptr::null_mut();
    };
    let Some(display_uri) = cstr_to_str(display_uri) else {
        return std::ptr::null_mut();
    };

    let display: DisplayTarget = match parse_display_uri(display_uri) {
        Ok(d) => d,
        Err(_) => return std::ptr::null_mut(),
    };

    let (codec, hw_encoder) = match pick_best_encoder(&ffmpeg_path, None) {
        Ok((c, enc)) => (c, Some(enc)),
        Err(_) => return std::ptr::null_mut(),
    };

    let options = EncodeOptions {
        bitrate_kbps,
        elevation: if prefer_system_elevation != 0 {
            ElevationMode::PreferSystem
        } else {
            ElevationMode::Normal
        },
        ..Default::default()
    };

    let pipeline = MonitorPipeline::new(&ffmpeg_path, display, codec, hw_encoder, options);
    Box::into_raw(Box::new(PipelineHandle { inner: pipeline }))
}

/// フレーム単位(Key/Delta判定済み)でコールバックを登録する。
///
/// # Safety
/// `handle`は`ffmpeg_caster_pipeline_new`が返した、まだ解放されていない
/// 有効なハンドルであること。`callback`はハンドルが解放されるまでの間、
/// 別スレッドから任意のタイミングで呼び出される可能性がある。`user_data`は
/// `callback`が呼ばれる間常に有効であること(呼び出し元がスレッドセーフに
/// 扱う責務を負う)。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_subscribe_frames(
    handle: *mut PipelineHandle,
    callback: FrameCallbackFn,
    user_data: *mut c_void,
) {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let user_data = SendSyncPtr(user_data);
    handle.inner.subscribe_frames(move |frame: &EncodedFrame| {
        let user_data = &user_data;
        let kind = match frame.kind {
            FrameKind::Key => 0,
            FrameKind::Delta => 1,
        };
        callback(
            kind,
            frame.payload.as_ptr(),
            frame.payload.len(),
            user_data.0,
        );
    });
}

/// 生バイト列(NAL分割前)でコールバックを登録する。
///
/// # Safety
/// `ffmpeg_caster_pipeline_subscribe_frames`と同じ制約が適用される。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_subscribe_raw(
    handle: *mut PipelineHandle,
    callback: RawCallbackFn,
    user_data: *mut c_void,
) {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let user_data = SendSyncPtr(user_data);
    handle.inner.subscribe_raw(move |chunk: &[u8]| {
        let user_data = &user_data;
        callback(chunk.as_ptr(), chunk.len(), user_data.0);
    });
}

/// ffmpegを起動する。成功時0、失敗時-1。
///
/// # Safety
/// `handle`は`ffmpeg_caster_pipeline_new`が返した、まだ解放されていない
/// 有効なハンドルであること。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_start(handle: *mut PipelineHandle) -> c_int {
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return -1;
    };
    match runtime().block_on(handle.inner.start()) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// 実行中のffmpegインスタンスに強制IDRを要求する。
///
/// # Safety
/// `handle`は`ffmpeg_caster_pipeline_new`が返した、まだ解放されていない
/// 有効なハンドルであること。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_request_idr(handle: *mut PipelineHandle) {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    runtime().block_on(handle.inner.request_idr());
}

/// 監視ループを止め、ffmpegプロセスを終了する。
///
/// # Safety
/// `handle`は`ffmpeg_caster_pipeline_new`が返した、まだ解放されていない
/// 有効なハンドルであること。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_stop(handle: *mut PipelineHandle) {
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return;
    };
    handle.inner.stop();
}

/// パイプラインハンドルを解放する(内部で`stop()`相当も行う)。
///
/// # Safety
/// `handle`は`ffmpeg_caster_pipeline_new`が返したポインタそのもの、または
/// nullであり、一度しか解放してはならない(二重解放は未定義動作)。解放後に
/// このハンドルを他の`ffmpeg_caster_pipeline_*`関数へ渡してはならない。
#[no_mangle]
pub unsafe extern "C" fn ffmpeg_caster_pipeline_free(handle: *mut PipelineHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(handle));
    }
}

fn cstr_to_str<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(s) }.to_str().ok()
}

fn cstr_to_path(s: *const c_char) -> Option<std::path::PathBuf> {
    cstr_to_str(s).map(|s| Path::new(s).to_path_buf())
}

fn path_to_cstring(p: &Path) -> *mut c_char {
    CString::new(p.to_string_lossy().into_owned())
        .map(|c| c.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
