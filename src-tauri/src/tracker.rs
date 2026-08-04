//! Tracker 服务 —— 独立的节点发现服务（阶段二）。
//!
//! `music.exe --tracker` 进入此模式：监听固定端口，所有客户端连它做
//! 节点发现与曲目发布。内部复用阶段一的 InMemoryTracker 存节点/分片索引，
//! 额外维护一张"已发布曲目清单"表供客户端浏览共享曲库。
//!
//! 协议：一问一答的短连接，请求与响应都是单行 JSON + '\n'。

use crate::chunk::TrackManifest;
use crate::peer::{InMemoryTracker, PeerDiscovery, PeerInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Tracker 默认监听地址。
pub const TRACKER_ADDR: &str = "127.0.0.1:9000";

/// 客户端 → Tracker 的请求。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TrackerRequest {
    /// 节点上线：注册地址 + 已持有分片。
    Register { peer: PeerInfo, chunks: Vec<String> },
    /// 宣告新持有的分片。
    Announce { peer_id: String, chunks: Vec<String> },
    /// 查询谁持有某分片。
    Find { chunk: String },
    /// 发布一首曲目（连同清单），让别的节点能发现并下载。
    Publish { manifest: TrackManifest, title: String, artist: String, #[serde(default)] mime: String },
    /// 列出所有已发布曲目。
    List,
    /// 按 track_hash 查询单个共享曲目（用于"粘贴链接→播放"流程）。
    Get { track_hash: String },
    /// 节点下线。
    Unregister { peer_id: String },
}

/// Tracker → 客户端的响应。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackerResponse {
    Ok,
    Peers { peers: Vec<PeerInfo> },
    Manifests { items: Vec<SharedTrack> },
    Track { item: Option<SharedTrack> },
    Error { message: String },
}

/// 共享曲库里的一条曲目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedTrack {
    pub manifest: TrackManifest,
    pub title: String,
    pub artist: String,
    /// 媒体 MIME 类型（如 audio/mpeg），供流式播放设置 Content-Type。
    #[serde(default)]
    pub mime: String,
}

/// 已发布曲目表：track_hash -> SharedTrack。
type ManifestStore = Arc<Mutex<HashMap<String, SharedTrack>>>;

/// 处理单个请求，返回响应。
fn handle(req: TrackerRequest, tracker: &InMemoryTracker, manifests: &ManifestStore) -> TrackerResponse {
    match req {
        TrackerRequest::Register { peer, chunks } => {
            tracker.register(peer, &chunks);
            TrackerResponse::Ok
        }
        TrackerRequest::Announce { peer_id, chunks } => {
            tracker.announce(&peer_id, &chunks);
            TrackerResponse::Ok
        }
        TrackerRequest::Find { chunk } => TrackerResponse::Peers {
            peers: tracker.find_peers(&chunk),
        },
        TrackerRequest::Publish { manifest, title, artist, mime } => {
            manifests.lock().unwrap().insert(
                manifest.track_hash.clone(),
                SharedTrack { manifest, title, artist, mime },
            );
            TrackerResponse::Ok
        }
        TrackerRequest::List => TrackerResponse::Manifests {
            items: manifests.lock().unwrap().values().cloned().collect(),
        },
        TrackerRequest::Get { track_hash } => TrackerResponse::Track {
            item: manifests.lock().unwrap().get(&track_hash).cloned(),
        },
        TrackerRequest::Unregister { peer_id } => {
            tracker.unregister(&peer_id);
            TrackerResponse::Ok
        }
    }
}

/// 处理一个客户端连接：读一行 JSON 请求，回一行 JSON 响应。
fn serve_conn(stream: TcpStream, tracker: Arc<InMemoryTracker>, manifests: ManifestStore) {
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }

    let resp = match serde_json::from_str::<TrackerRequest>(line.trim()) {
        Ok(req) => handle(req, &tracker, &manifests),
        Err(e) => TrackerResponse::Error {
            message: format!("bad request: {e}"),
        },
    };

    if let Ok(mut json) = serde_json::to_string(&resp) {
        json.push('\n');
        let _ = writer.write_all(json.as_bytes());
        let _ = writer.flush();
    }
    let _ = peer; // 保留用于将来日志
}

/// 阻塞运行 Tracker 服务（`--tracker` 模式的入口）。
pub fn run_tracker(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("[tracker] listening on {addr}");

    let tracker = Arc::new(InMemoryTracker::new());
    let manifests: ManifestStore = Arc::new(Mutex::new(HashMap::new()));

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let tracker = Arc::clone(&tracker);
                let manifests = Arc::clone(&manifests);
                std::thread::spawn(move || serve_conn(s, tracker, manifests));
            }
            Err(e) => eprintln!("[tracker] accept error: {e}"),
        }
    }
    Ok(())
}
