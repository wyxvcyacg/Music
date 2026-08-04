//! Music —— P2P 流媒体音乐播放器（Tauri 后端）。
//!
//! 阶段二：跨进程真 P2P。
//!   - 独立 Tracker 服务（`--tracker` 模式，见 main.rs / tracker.rs）做节点发现。
//!   - 每个客户端启动时后台开一个分片服务（transfer.rs），并连上 Tracker 注册。
//!   - 下载时按清单逐分片 find_peers -> fetch_chunk -> 校验入库 -> 重组播放。
//!
//! 流式播放：自定义 `stream://` 协议（stream.rs）实现边下边播 —— `<audio>` 按
//! 字节区间请求，每个区间背后按需拉取所需分片。
//!
//! 账号系统留到阶段三，这里只有 peer_id，没有 user_id。

pub mod chunk;
pub mod peer;
pub mod stream;
pub mod tracker;
pub mod transfer;

pub use tracker::{run_tracker, TRACKER_ADDR};

use chunk::{ChunkStore, TrackManifest};
use peer::{PeerDiscovery, PeerInfo, RemoteTracker};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use stream::{StreamCtx, StreamEntry};
use tauri::{Manager, State};
use tracker::{SharedTrack, TrackerRequest, TrackerResponse};
use uuid::Uuid;

/// 应用级共享状态。
struct AppState {
    /// 本节点 id —— 启动时随机生成，与账号无关（铁律之一）。
    peer_id: String,
    /// 本节点分片服务的监听地址（供别的节点来拉分片）。
    chunk_addr: String,
    /// 本地分片存储；Arc 与分片服务、流式协议共享同一份。
    store: Arc<ChunkStore>,
    /// 远程 Tracker 客户端（实现 PeerDiscovery）；Arc 便于共享进协议线程。
    tracker: Arc<RemoteTracker>,
    /// 可流式播放曲目的登记表；与 stream 协议共享。
    stream_registry: Arc<Mutex<HashMap<String, StreamEntry>>>,
}

impl AppState {
    /// 派生流式协议所需的共享句柄。
    fn stream_ctx(&self) -> StreamCtx {
        StreamCtx {
            peer_id: self.peer_id.clone(),
            store: Arc::clone(&self.store),
            tracker: Arc::clone(&self.tracker),
            registry: Arc::clone(&self.stream_registry),
        }
    }
}

/// 导入一首曲目（字节数组）：切片、算哈希、存入本地 store，
/// 并把持有的分片向 Tracker 宣告，返回清单。
#[tauri::command]
fn import_track(state: State<AppState>, data: Vec<u8>) -> TrackManifest {
    let manifest = state.store.import_track(&data);
    state.tracker.announce(&state.peer_id, &manifest.chunks);
    manifest
}

/// 登记一首曲目为可流式播放，返回 `<audio>` 可用的 stream URL。
/// track_hash 对应的分片可以尚未全部在本地 —— 播放时按需从 peer 拉取。
#[tauri::command]
fn prepare_stream(state: State<AppState>, manifest: TrackManifest, mime: String) -> String {
    let track_hash = manifest.track_hash.clone();
    state.stream_ctx().register(manifest, mime);
    // 自定义协议在各平台的 URL 形式不同：
    //   Windows / Android: http://<scheme>.localhost/<path>
    //   macOS / iOS / Linux: <scheme>://localhost/<path>
    #[cfg(any(windows, target_os = "android"))]
    {
        format!("http://stream.localhost/{track_hash}")
    }
    #[cfg(not(any(windows, target_os = "android")))]
    {
        format!("stream://localhost/{track_hash}")
    }
}

/// 发布一首曲目到共享曲库，让别的节点能发现并下载/流式播放。
#[tauri::command]
fn publish_track(
    state: State<AppState>,
    manifest: TrackManifest,
    title: String,
    artist: String,
    mime: String,
) -> Result<(), String> {
    // 确保分片已在本地并向 Tracker 宣告（发布者即首个种子）。
    state.tracker.announce(&state.peer_id, &manifest.chunks);
    match state
        .tracker
        .request(&TrackerRequest::Publish { manifest, title, artist, mime })
    {
        Ok(TrackerResponse::Ok) => Ok(()),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 列出 Tracker 上所有已发布的共享曲目。
#[tauri::command]
fn list_shared(state: State<AppState>) -> Result<Vec<SharedTrack>, String> {
    match state.tracker.request(&TrackerRequest::List) {
        Ok(TrackerResponse::Manifests { items }) => Ok(items),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 下载进度报告。
#[derive(serde::Serialize)]
struct DownloadResult {
    /// 重组后的完整曲目字节。
    data: Vec<u8>,
    /// 本次实际从网络拉取的分片数（其余为本地已有）。
    fetched: usize,
    /// 命中本地缓存、无需下载的分片数。
    cached: usize,
}

/// 按清单下载一首曲目：逐分片 find_peers -> fetch_chunk -> 校验入库 -> announce，
/// 全部就绪后重组返回。本地已有的分片跳过下载（P2P 缓存复用）。
#[tauri::command]
fn download_track(state: State<AppState>, manifest: TrackManifest) -> Result<DownloadResult, String> {
    let mut fetched = 0usize;
    let mut cached = 0usize;

    for hash in &manifest.chunks {
        if state.store.has(hash) {
            cached += 1;
            continue;
        }

        // 找持有该分片的节点（排除自己）。
        let peers: Vec<PeerInfo> = state
            .tracker
            .find_peers(hash)
            .into_iter()
            .filter(|p| p.peer_id != state.peer_id)
            .collect();

        if peers.is_empty() {
            return Err(format!("no peer has chunk {hash}"));
        }

        // 依次尝试每个候选节点，任一成功即止。
        let mut got = None;
        let mut last_err = String::new();
        for p in &peers {
            match transfer::fetch_chunk(&p.addr, hash) {
                Ok(bytes) => {
                    got = Some(bytes);
                    break;
                }
                Err(e) => last_err = e,
            }
        }

        let bytes = got.ok_or_else(|| format!("fetch {hash} failed: {last_err}"))?;
        // put 会校验内容哈希，防止被污染的数据混入。
        if !state.store.put(hash, bytes) {
            return Err(format!("chunk {hash} failed hash verification"));
        }
        // 下载到新分片后向 Tracker 宣告：本节点现在也是这片的源了。
        state.tracker.announce(&state.peer_id, std::slice::from_ref(hash));
        fetched += 1;
    }

    let data = state.store.reassemble(&manifest)?;
    Ok(DownloadResult { data, fetched, cached })
}

/// 本地是否持有某分片。
#[tauri::command]
fn has_chunk(state: State<AppState>, chunk_hash: String) -> bool {
    state.store.has(&chunk_hash)
}

/// 按清单重组曲目字节（本地缓存齐全时可离线播放）。
#[tauri::command]
fn reassemble(state: State<AppState>, manifest: TrackManifest) -> Result<Vec<u8>, String> {
    state.store.reassemble(&manifest)
}

/// P2P 运行状态快照（供 UI 的"节点网络"面板展示）。
#[derive(serde::Serialize)]
struct P2pStatus {
    peer_id: String,
    chunk_addr: String,
    /// Tracker 是否可达。
    tracker_online: bool,
    owned_chunks: usize,
}

#[tauri::command]
fn p2p_status(state: State<AppState>) -> P2pStatus {
    // 用一次 List 探测 Tracker 是否可达。
    let tracker_online = state.tracker.request(&TrackerRequest::List).is_ok();
    P2pStatus {
        peer_id: state.peer_id.clone(),
        chunk_addr: state.chunk_addr.clone(),
        tracker_online,
        owned_chunks: state.store.owned_hashes().len(),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let peer_id = Uuid::new_v4().to_string();
    let store = Arc::new(ChunkStore::new());

    // 后台启动分片服务，拿到本节点对外地址。
    let chunk_addr = transfer::start_chunk_server(Arc::clone(&store))
        .expect("failed to start chunk server");
    println!("[client] peer_id={peer_id} chunk_addr={chunk_addr}");

    // 连接 Tracker 并注册自己（此刻还没有分片）。
    let tracker = Arc::new(RemoteTracker::new(tracker::TRACKER_ADDR));
    tracker.register(
        PeerInfo {
            peer_id: peer_id.clone(),
            addr: chunk_addr.clone(),
        },
        &[],
    );

    let state = AppState {
        peer_id,
        chunk_addr,
        store,
        tracker,
        stream_registry: Arc::new(Mutex::new(HashMap::new())),
    };
    // 协议处理器需要的共享句柄（在 build 前克隆出来 move 进闭包）。
    let stream_ctx = state.stream_ctx();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .register_asynchronous_uri_scheme_protocol("stream", move |_ctx, request, responder| {
            let sc = stream_ctx.clone();
            std::thread::spawn(move || {
                let path = request.uri().path().to_string();
                let range = request
                    .headers()
                    .get("range")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let (status, headers, body) =
                    stream::handle_request(&sc, &path, range.as_deref());

                let mut builder = tauri::http::Response::builder().status(status);
                for (k, v) in headers {
                    builder = builder.header(k, v);
                }
                match builder.body(body) {
                    Ok(resp) => responder.respond(resp),
                    Err(e) => eprintln!("[stream] build response failed: {e}"),
                }
            });
        })
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            import_track,
            prepare_stream,
            publish_track,
            list_shared,
            download_track,
            has_chunk,
            reassemble,
            p2p_status,
        ])
        .setup(|app| {
            let _ = app.state::<AppState>();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
