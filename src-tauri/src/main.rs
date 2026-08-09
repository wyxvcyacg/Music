// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `music --tracker [addr]` 进入纯 Tracker 服务模式（不开窗口）。
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--tracker") {
        // 可选地在 --tracker 后指定监听地址，否则用默认。
        let addr = args
            .iter()
            .position(|a| a == "--tracker")
            .and_then(|i| args.get(i + 1))
            .filter(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| music_lib::TRACKER_ADDR.to_string());

        // 账号持久化目录：--data-dir <path>，默认当前目录。
        let data_dir = args
            .iter()
            .position(|a| a == "--data-dir")
            .and_then(|i| args.get(i + 1))
            .filter(|a| !a.starts_with("--"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        if let Err(e) = music_lib::run_tracker_with_data_dir(&addr, Some(data_dir)) {
            eprintln!("[tracker] fatal: {e}");
            std::process::exit(1);
        }
        return;
    }

    music_lib::run()
}
