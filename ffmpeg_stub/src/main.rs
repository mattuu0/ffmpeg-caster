// PAExecが`-i`(対話的セッション)でプロセスを起動する際、ターゲットの
// STARTUPINFO.wShowWindowを常にSW_SHOWにし、CREATE_NEW_CONSOLEでコンソールを
// 割り当てる(PAExec自身のソース(Process.cpp)で確認済み: bInteractiveがtrueの
// 場合はSW_HIDE分岐に入らない)。このため、呼び出し側でpaexec.exe自体に
// CREATE_NO_WINDOWを指定しても、PAExecが内部で起動するffmpeg.exeには
// 一切伝播せず、配信中に黒いffmpegコンソールウィンドウがユーザーの
// デスクトップに表示され続けてしまう(実機で確認済み)。
//
// これを解決するため、PAExecにはffmpeg.exeを直接起動させず、この
// GUIサブシステムのスタブexeを起動させる。GUIサブシステムのプロセスは
// CreateProcess時にコンソールを持たないため、PAExecがCREATE_NEW_CONSOLEを
// 指定してもこのスタブ自身にはウィンドウが出ない。スタブは自分の引数
// (先頭がffmpeg.exeへのパス、残りがffmpegの引数)をそのままCREATE_NO_WINDOW付きで
// 子プロセスとして起動し、標準出力/標準エラーを引き継いで自身が終了するまで
// 待つ。CREATE_NO_WINDOWはこの場合スタブ自身が直接の親としてCreateProcessWを
// 呼ぶため、PAExecの`-i`分岐を経由せず正しく効く。
//
// 独立したワークスペースメンバー(このcrate)として分離することで、
// メインライブラリ/アプリのビルドスクリプトがこのexeの成果物を配布物として
// 参照する際の循環ビルドを避ける。
#![windows_subsystem = "windows"]

/// idr_control_socket(名前付きパイプ)はffmpeg自身(SYSTEM権限)が作成するため、
/// 通常権限の呼び出し元プロセスから直接CreateFileしようとするとアクセス拒否になる
/// (デフォルトDACLがSYSTEM/Administratorsのみを許可するため、実機確認済み)。
/// stubはffmpegと同じSYSTEM権限で動いているため、代わりにstubがこのパイプへの
/// 中継役を担う: 呼び出し元(elevate.rs)が事前にbindしたTCPリスナー
/// (idr_relay_port)へこのstubがクライアントとして接続し、受信したバイト列を
/// そのままidr_control_socketへ転送し続ける。接続が切れたら再接続を試みる
/// (呼び出し元のffmpeg再起動ループとタイミングがずれても復旧できるようにする)。
#[cfg(windows)]
fn run_idr_relay(idr_relay_port: u16, idr_control_path: String) {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    loop {
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", idr_relay_port)) else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };

        let mut buf = [0u8; 256];
        loop {
            let n = match stream.read(&mut buf) {
                Ok(0) => break, // 呼び出し元が接続を閉じた(このffmpegプロセス終了時)
                Ok(n) => n,
                Err(_) => break,
            };
            if let Ok(mut pipe) = std::fs::OpenOptions::new()
                .write(true)
                .open(&idr_control_path)
            {
                let _ = pipe.write_all(&buf[..n]);
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, ABOVE_NORMAL_PRIORITY_CLASS,
        CREATE_NO_WINDOW, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    };

    let mut args = std::env::args_os();
    let _self_path = args.next(); // argv[0](このスタブ自身のパス)、使わない

    // 先頭2引数はIDR中継用: <IDR中継用TCPポート> <idr_control_socketのパイプパス>。
    // 呼び出し元(elevate.rs spawn_elevated)がこの順で必ず付与する。
    let idr_relay_port: u16 = args
        .next()
        .and_then(|s| s.to_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    let idr_control_path = args
        .next()
        .and_then(|s| s.to_str().map(|s| s.to_string()))
        .unwrap_or_default();

    if idr_relay_port != 0 && !idr_control_path.is_empty() {
        std::thread::spawn(move || run_idr_relay(idr_relay_port, idr_control_path));
    }

    let target_exe = match args.next() {
        Some(p) => p,
        None => std::process::exit(1),
    };
    let rest_args: Vec<std::ffi::OsString> = args.collect();

    // CreateProcessWのlpCommandLineは1本のコマンドライン文字列。ffmpeg.exe自身の
    // パスも含め、Windowsのコマンドライン引用規則に従って組み立てる。
    let mut command_line = quote_arg(&target_exe);
    for arg in &rest_args {
        command_line.push(' ');
        command_line.push_str(&quote_arg(arg));
    }
    let mut command_line_wide: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // PAExecの-wと同様、隣接DLL(avcodec-*.dll等)のロードを確実にするため
    // 作業ディレクトリをffmpeg.exeの親に明示する。
    let target_exe_path = std::path::Path::new(&target_exe);
    let working_dir_wide: Option<Vec<u16>> = target_exe_path.parent().map(|p| {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    });

    let startup_info = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    // 他プロセスのCPU負荷が急上昇した瞬間、ffmpegのキャプチャ(ddagrabの
    // AcquireNextFrame)・エンコード投入・TCP送出がOSスケジューラに一時的に
    // CPU時間を割り当てられず、その間映像が更新されなくなる(実機確認済み)。
    // 通常優先度(NORMAL_PRIORITY_CLASS)のままだと、他の重いプロセスと同じ
    // 優先度で競合するため、ABOVE_NORMAL_PRIORITY_CLASSに上げてスケジューラの
    // 優先度を確保する。HIGH_PRIORITY_CLASSはシステム全体への影響が大きすぎる
    // ため避ける。
    let success = unsafe {
        CreateProcessW(
            None,
            Some(PWSTR(command_line_wide.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | ABOVE_NORMAL_PRIORITY_CLASS,
            None,
            working_dir_wide
                .as_ref()
                .map(|w| windows::core::PCWSTR(w.as_ptr()))
                .unwrap_or(windows::core::PCWSTR::null()),
            &startup_info,
            &mut process_info,
        )
    };

    if success.is_err() {
        std::process::exit(1);
    }

    unsafe {
        let _ = CloseHandle(process_info.hThread);
    }

    let exit_code = unsafe {
        WaitForSingleObject(process_info.hProcess, INFINITE);
        let mut code: u32 = 0;
        let _ = GetExitCodeProcess(process_info.hProcess, &mut code);
        let _ = CloseHandle(process_info.hProcess);
        code
    };

    std::process::exit(exit_code as i32);
}

#[cfg(windows)]
fn quote_arg(arg: &std::ffi::OsStr) -> String {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains(['"', ' ', '\t']) {
        return s.into_owned();
    }
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                quoted.push('\\');
            }
            '"' => {
                for _ in 0..backslashes {
                    quoted.push('\\');
                }
                quoted.push('\\');
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                quoted.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

#[cfg(not(windows))]
fn main() {}
