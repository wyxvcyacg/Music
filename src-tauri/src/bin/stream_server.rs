//! 集成验证 binary：起一个最小 P2P 节点，监听 stream 协议处理器。
//!
//! 跑法：
//!   1. `music --tracker`               （另一个终端）
//!   2. `stream_server`                 （本程序；默认端口 9100）
//!   3. 看到 "[stream_server] ready"，然后用 curl 打：
//!        curl -v -H "Range: bytes=0-99" http://127.0.0.1:9100/<track_hash>
//!
//! 先用 `--import <file>` 把一首本地音频导入进 ChunkStore 并向 Tracker 宣告；
//! 程序会打印 track_hash，然后用上面的 curl 就能拉分片。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use music_lib::chunk::ChunkStore;
use music_lib::peer::{PeerDiscovery, PeerInfo, RemoteTracker};
use music_lib::stream::StreamCtx;
use music_lib::tracker;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args
        .windows(2)
        .find(|w| w[0] == "--port")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(9100);
    let import_path: Option<PathBuf> = args
        .windows(2)
        .find(|w| w[0] == "--import")
        .map(|w| PathBuf::from(&w[1]));
    // --data-dir <path>：用磁盘持久化缓存（可验证跨重启供片）。
    let data_dir: Option<PathBuf> = args
        .windows(2)
        .find(|w| w[0] == "--data-dir")
        .map(|w| PathBuf::from(&w[1]));

    let peer_id = uuid::Uuid::new_v4().to_string();
    let store = Arc::new(match &data_dir {
        Some(d) => {
            let s = ChunkStore::open(d).expect("open disk chunk store");
            println!(
                "[stream_server] disk cache {} ({} chunks restored)",
                d.display(),
                s.chunk_count()
            );
            s
        }
        None => ChunkStore::new(),
    });

    // 开启分片服务（让别人能从本节点拉分片）
    let chunk_addr = music_lib::transfer::start_chunk_server(Arc::clone(&store))
        .expect("start chunk server");
    // 注册到 Tracker
    let tracker = Arc::new(RemoteTracker::new(tracker::TRACKER_ADDR));
    tracker.register(
        PeerInfo {
            peer_id: peer_id.clone(),
            addr: chunk_addr.clone(),
        },
        &[],
    );
    println!("[stream_server] peer_id={peer_id}");
    println!("[stream_server] chunk_addr={chunk_addr}");

    let registry: Arc<Mutex<HashMap<String, music_lib::stream::StreamEntry>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let ctx = StreamCtx {
        peer_id: peer_id.clone(),
        store: Arc::clone(&store),
        tracker: Arc::clone(&tracker),
        registry: Arc::clone(&registry),
    };

    // 可选：导入一个本地文件，分片入 store + 登记到 registry + 向 Tracker 宣告。
    if let Some(p) = import_path {
        let bytes = std::fs::read(&p).expect("read import file");
        let manifest = store.import_track(&bytes);
        tracker.announce(&peer_id, &manifest.chunks);
        registry.lock().unwrap().insert(
            manifest.track_hash.clone(),
            music_lib::stream::StreamEntry {
                manifest: manifest.clone(),
                mime: "audio/mpeg".into(),
            },
        );
        println!(
            "[stream_server] imported {} bytes, {} chunks",
            manifest.total_size,
            manifest.chunks.len()
        );
        println!("[stream_server] track_hash = {}", manifest.track_hash);
        println!("[stream_server] try:");
        println!(
            "  curl -v -H \"Range: bytes=0-99\" http://127.0.0.1:{port}/{}",
            manifest.track_hash
        );
        println!(
            "  curl -v -H \"Range: bytes=300000-300099\" http://127.0.0.1:{port}/{}",
            manifest.track_hash
        );
    }

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind http port");
    println!("[stream_server] listening on http://127.0.0.1:{port}/<track_hash>");

    for stream in listener.incoming() {
        let mut stream = stream.expect("accept");
        let ctx = ctx.clone();
        thread::spawn(move || {
            handle_conn(&mut stream, &ctx);
        });
    }
}

fn handle_conn(stream: &mut std::net::TcpStream, ctx: &StreamCtx) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");
    let path = first
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let range = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .map(|l| l.splitn(2, ':').nth(1).unwrap_or("").trim().to_string());

    let (status, headers, body) =
        music_lib::stream::handle_request(ctx, &path, range.as_deref());

    let status_text = match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut resp = format!("HTTP/1.1 {status} {status_text}\r\n");
    for (k, v) in &headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
    resp.push_str("\r\n");
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}
