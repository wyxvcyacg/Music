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
pub mod library;
pub mod peer;
pub mod stream;
pub mod tracker;
pub mod transfer;

pub use tracker::{run_tracker, TRACKER_ADDR};

use chunk::{ChunkStore, TrackManifest};
use library::{Library, LibraryTrack};
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
    /// 持久化的本地曲库 —— 同时定义了缓存淘汰的"受保护"分片集合。
    lib: Arc<Library>,
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

    /// 加入曲库后触发一次缓存淘汰检查。
    /// 曲库里的分片受保护，只回收流式播放顺带缓存的过路分片。
    fn enforce_cache_limit(&self) {
        let protected = self.lib.protected_hashes();
        let report = self
            .store
            .evict_if_needed(chunk::DEFAULT_CACHE_LIMIT, &protected);
        if report.evicted > 0 {
            println!(
                "[cache] evicted {} chunks, freed {} bytes ({} -> {})",
                report.evicted, report.bytes_freed, report.bytes_before, report.bytes_after
            );
        }
    }
}

/// 导入一首曲目（字节数组）：切片、算哈希、存入本地 store，
/// 向 Tracker 宣告，加入曲库并持久化，返回清单。
#[tauri::command]
fn import_track(
    state: State<AppState>,
    data: Vec<u8>,
    title: String,
    artist: String,
    mime: String,
) -> TrackManifest {
    let manifest = state.store.import_track(&data);
    state.tracker.announce(&state.peer_id, &manifest.chunks);
    state.lib.add(library::make_track(
        manifest.clone(),
        title,
        artist,
        mime,
    ));
    state.enforce_cache_limit();
    manifest
}

/// 列出持久化的本地曲库（启动时恢复用）。
#[tauri::command]
fn list_library(state: State<AppState>) -> Vec<LibraryTrack> {
    state.lib.list()
}

/// 从曲库移除一首曲目。只移除条目，分片留给缓存淘汰按需回收
/// —— 移除后它们不再受保护。
#[tauri::command]
fn remove_from_library(state: State<AppState>, track_hash: String) -> bool {
    let removed = state.lib.remove(&track_hash);
    if removed {
        state.enforce_cache_limit();
    }
    removed
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

/// 按 track_hash 查询单个共享曲目（粘贴链接后用）。
/// 返回 None 表示 Tracker 上没有此 hash。
#[tauri::command]
fn lookup_track(state: State<AppState>, track_hash: String) -> Result<Option<SharedTrack>, String> {
    match state
        .tracker
        .request(&TrackerRequest::Get { track_hash })
    {
        Ok(TrackerResponse::Track { item }) => Ok(item),
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

/// 按清单下载一首曲目：缺失的分片**并行**从各持有者拉取（多源加速），
/// 校验入库后批量向 Tracker 宣告，全部就绪后重组返回。
/// 本地已有的分片跳过下载（P2P 缓存复用）。
#[tauri::command]
fn download_track(
    state: State<AppState>,
    manifest: TrackManifest,
    title: String,
    artist: String,
    mime: String,
) -> Result<DownloadResult, String> {
    // 分出"本地已有"与"需要拉取"。
    let mut missing = Vec::new();
    let mut cached = 0usize;
    for hash in &manifest.chunks {
        if state.store.has(hash) {
            cached += 1;
        } else {
            missing.push(hash.clone());
        }
    }

    // 并行拉取缺失分片（排除自己作为源）。
    let fetched_hashes = transfer::fetch_chunks_parallel(
        &missing,
        |hash| {
            state
                .tracker
                .find_peers(hash)
                .into_iter()
                .filter(|p| p.peer_id != state.peer_id)
                .collect()
        },
        &state.store,
        transfer::MAX_CONCURRENT_FETCHES,
    )?;

    // 批量宣告：本节点现在也是这些分片的源了。
    if !fetched_hashes.is_empty() {
        state.tracker.announce(&state.peer_id, &fetched_hashes);
    }

    let data = state.store.reassemble(&manifest)?;
    // 完整下载 = 收藏到本地：加入曲库，其分片从此受保护。
    state.lib.add(library::make_track(
        manifest.clone(),
        title,
        artist,
        mime,
    ));
    state.enforce_cache_limit();
    Ok(DownloadResult {
        data,
        fetched: fetched_hashes.len(),
        cached,
    })
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
        // 用 chunk_count 而非 owned_hashes().len()：这个命令每 2 秒被轮询一次，
        // 不该每次都克隆全部哈希字符串。
        owned_chunks: state.store.chunk_count(),
    }
}

/// 本地分片缓存统计（供 UI 展示占用）。
#[derive(serde::Serialize)]
struct CacheStats {
    chunks: usize,
    bytes: u64,
    /// 容量上限；超过后按 LRU 淘汰未受保护的分片。
    limit: u64,
}

#[tauri::command]
fn cache_stats(state: State<AppState>) -> CacheStats {
    CacheStats {
        chunks: state.store.chunk_count(),
        bytes: state.store.cache_bytes(),
        limit: chunk::DEFAULT_CACHE_LIMIT,
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .register_asynchronous_uri_scheme_protocol("stream", |ctx, request, responder| {
            // 在请求时从 AppHandle 取状态 —— 这样 store 可以在 setup() 里
            // 用 app_data_dir() 打开磁盘模式，而不必在 Builder 之前就构造。
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || {
                let sc = match app.try_state::<AppState>() {
                    Some(state) => state.stream_ctx(),
                    None => {
                        // 理论上不会发生（setup 先于窗口加载），保险起见回 503。
                        let resp = tauri::http::Response::builder()
                            .status(503)
                            .body(b"state not ready".to_vec());
                        if let Ok(r) = resp {
                            responder.respond(r);
                        }
                        return;
                    }
                };
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
        .invoke_handler(tauri::generate_handler![
            import_track,
            prepare_stream,
            publish_track,
            list_shared,
            lookup_track,
            download_track,
            has_chunk,
            reassemble,
            p2p_status,
            cache_stats,
            list_library,
            remove_from_library,
        ])
        .setup(|app| {
            let peer_id = Uuid::new_v4().to_string();

            // 分片缓存与曲库都放在 app_data_dir 下。拿不到目录时回退到
            // 内存模式（缓存与曲库都不持久化），不影响基本可用性。
            let data_dir = app.path().app_data_dir().ok();
            let (store, lib) = match &data_dir {
                Some(base) => {
                    let dir = base.join("chunks");
                    let store = match ChunkStore::open(&dir) {
                        Ok(s) => {
                            println!(
                                "[client] chunk cache: {} ({} chunks restored)",
                                dir.display(),
                                s.chunk_count()
                            );
                            Arc::new(s)
                        }
                        Err(e) => {
                            eprintln!("[client] disk cache unavailable ({e}), using memory");
                            Arc::new(ChunkStore::new())
                        }
                    };
                    let lib = Arc::new(Library::open(library::library_path(base)));
                    println!("[client] library: {} tracks restored", lib.list().len());
                    (store, lib)
                }
                None => {
                    eprintln!("[client] no app data dir, using memory cache + library");
                    (Arc::new(ChunkStore::new()), Arc::new(Library::new()))
                }
            };

            // 后台启动分片服务，拿到本节点对外地址。
            let chunk_addr = transfer::start_chunk_server(Arc::clone(&store))
                .expect("failed to start chunk server");
            println!("[client] peer_id={peer_id} chunk_addr={chunk_addr}");

            // 连接 Tracker 并注册自己，连同已恢复的分片一起宣告
            // —— 重启后立刻能继续为这些分片供源。
            let tracker = Arc::new(RemoteTracker::new(tracker::TRACKER_ADDR));
            tracker.register(
                PeerInfo {
                    peer_id: peer_id.clone(),
                    addr: chunk_addr.clone(),
                },
                &store.owned_hashes(),
            );

            let state = AppState {
                peer_id,
                chunk_addr,
                store,
                tracker,
                stream_registry: Arc::new(Mutex::new(HashMap::new())),
                lib,
            };
            // 启动时也检查一次容量 —— 上次运行可能在淘汰前就退出了。
            state.enforce_cache_limit();

            app.manage(state);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
