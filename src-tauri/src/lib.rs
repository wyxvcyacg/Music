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

pub mod accounts;
pub mod chunk;
pub mod library;
pub mod peer;
pub mod playlist;
pub mod stream;
pub mod tracker;
pub mod transfer;

pub use tracker::{run_tracker, run_tracker_with_data_dir, TRACKER_ADDR};

use chunk::{ChunkStore, TrackManifest};
use library::{Library, LibraryTrack};
use playlist::{Playlist, Playlists};
use peer::{PeerDiscovery, PeerInfo, RemoteTracker};
use serde::{Deserialize, Serialize};
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
    /// 持久化的播放列表。只存 track_hash 引用，曲目真身在 `lib` 里。
    playlists: Arc<Playlists>,
    /// 当前登录会话（阶段三）。与 `peer_id` **并存且互不干扰** ——
    /// 铁律之一的落地：登出/换账号都不影响 P2P 传输。
    session: Mutex<Option<Session>>,
    /// session.json 路径；None 表示不持久化。
    session_path: Option<std::path::PathBuf>,
}

/// 一次登录会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    token: String,
    username: String,
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

    /// 当前 token（未登录则为空串）。
    fn token(&self) -> String {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.token.clone())
            .unwrap_or_default()
    }

    /// 设置或清除会话，并持久化。
    fn set_session(&self, next: Option<Session>) {
        *self.session.lock().unwrap() = next.clone();
        let Some(path) = &self.session_path else { return };
        match &next {
            Some(s) => {
                if let Ok(json) = serde_json::to_string_pretty(s) {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let tmp = path.with_extension("json.tmp");
                    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_err() {
                        let _ = std::fs::remove_file(&tmp);
                    }
                }
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
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

/// 列出所有播放列表。
#[tauri::command]
fn list_playlists(state: State<AppState>) -> Vec<Playlist> {
    state.playlists.list()
}

/// 新建播放列表，返回它的 id。
#[tauri::command]
fn create_playlist(state: State<AppState>, name: String) -> String {
    state.playlists.create(name)
}

/// 重命名播放列表。
#[tauri::command]
fn rename_playlist(state: State<AppState>, id: String, name: String) -> bool {
    state.playlists.rename(&id, name)
}

/// 删除播放列表。**不动曲库、不动分片** —— 只是删掉一份顺序清单。
#[tauri::command]
fn delete_playlist(state: State<AppState>, id: String) -> bool {
    state.playlists.delete(&id)
}

/// 往播放列表末尾追加一首曲目（按 track_hash 引用）。
#[tauri::command]
fn add_to_playlist(state: State<AppState>, id: String, track_hash: String) -> bool {
    state.playlists.add_track(&id, track_hash)
}

/// 按位置从播放列表移除一首（同一首歌可重复出现，所以用位置而非 hash）。
#[tauri::command]
fn remove_from_playlist(state: State<AppState>, id: String, index: usize) -> bool {
    state.playlists.remove_at(&id, index)
}

/// 整表提交新的曲目顺序。
#[tauri::command]
fn reorder_playlist(state: State<AppState>, id: String, tracks: Vec<String>) -> bool {
    state.playlists.reorder(&id, tracks)
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
/// 需要先登录 —— 发布是唯一鉴权的操作。
#[tauri::command]
fn publish_track(
    state: State<AppState>,
    manifest: TrackManifest,
    title: String,
    artist: String,
    mime: String,
) -> Result<(), String> {
    let token = state.token();
    if token.is_empty() {
        return Err("发布需要先登录".into());
    }
    // 确保分片已在本地并向 Tracker 宣告（发布者即首个种子）。
    state.tracker.announce(&state.peer_id, &manifest.chunks);
    match state.tracker.request(&TrackerRequest::Publish {
        manifest,
        title,
        artist,
        mime,
        token,
    }) {
        Ok(TrackerResponse::Ok) => Ok(()),
        Ok(TrackerResponse::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 当前登录用户名；未登录返回 None。
#[tauri::command]
fn current_user(state: State<AppState>) -> Option<String> {
    let session = state.session.lock().unwrap().clone();
    let Some(s) = session else { return None };
    // 向 Tracker 确认 token 还有效（它可能已重启或 token 已过期）。
    match state.tracker.request(&TrackerRequest::WhoAmI {
        token: s.token.clone(),
    }) {
        Ok(TrackerResponse::Identity { username: Some(u) }) => Some(u),
        Ok(TrackerResponse::Identity { username: None }) => {
            // token 失效 —— 清掉本地会话，让用户重新登录。
            state.set_session(None);
            None
        }
        // Tracker 不可达时保留本地会话，不因网络抖动把人登出。
        _ => Some(s.username),
    }
}

/// 注册新账号（成功即登录）。
#[tauri::command]
fn register_account(
    state: State<AppState>,
    username: String,
    password: String,
) -> Result<String, String> {
    match state
        .tracker
        .request(&TrackerRequest::RegisterAccount { username, password })
    {
        Ok(TrackerResponse::Auth { token, username }) => {
            state.set_session(Some(Session {
                token,
                username: username.clone(),
            }));
            Ok(username)
        }
        Ok(TrackerResponse::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 登录。
#[tauri::command]
fn login(state: State<AppState>, username: String, password: String) -> Result<String, String> {
    match state
        .tracker
        .request(&TrackerRequest::Login { username, password })
    {
        Ok(TrackerResponse::Auth { token, username }) => {
            state.set_session(Some(Session {
                token,
                username: username.clone(),
            }));
            Ok(username)
        }
        Ok(TrackerResponse::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e),
    }
}

/// 登出：吊销 Tracker 侧 token 并清空本地会话。
/// 注意这**完全不影响 P2P 传输** —— peer_id 与分片供源照常（铁律之一）。
#[tauri::command]
fn logout(state: State<AppState>) {
    let token = state.token();
    if !token.is_empty() {
        let _ = state.tracker.request(&TrackerRequest::Logout { token });
    }
    state.set_session(None);
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
            list_playlists,
            create_playlist,
            rename_playlist,
            delete_playlist,
            add_to_playlist,
            remove_from_playlist,
            reorder_playlist,
            current_user,
            register_account,
            login,
            logout,
        ])
        .setup(|app| {
            let peer_id = Uuid::new_v4().to_string();

            // 分片缓存、曲库、播放列表都放在 app_data_dir 下。拿不到目录时回退到
            // 内存模式（都不持久化），不影响基本可用性。
            let data_dir = app.path().app_data_dir().ok();
            let (store, lib, playlists) = match &data_dir {
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
                    let playlists =
                        Arc::new(Playlists::open(playlist::playlists_path(base)));
                    println!(
                        "[client] playlists: {} restored",
                        playlists.list().len()
                    );
                    (store, lib, playlists)
                }
                None => {
                    eprintln!("[client] no app data dir, using memory cache + library");
                    (
                        Arc::new(ChunkStore::new()),
                        Arc::new(Library::new()),
                        Arc::new(Playlists::new()),
                    )
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

            // 恢复上次的登录会话（token 可能已失效，current_user 会校验）。
            let session_path = data_dir.as_ref().map(|d| d.join("session.json"));
            let session = session_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str::<Session>(&t).ok());
            if let Some(s) = &session {
                println!("[client] restored session for {}", s.username);
            }

            let state = AppState {
                peer_id,
                chunk_addr,
                store,
                tracker,
                stream_registry: Arc::new(Mutex::new(HashMap::new())),
                lib,
                playlists,
                session: Mutex::new(session),
                session_path,
            };
            // 启动时也检查一次容量 —— 上次运行可能在淘汰前就退出了。
            state.enforce_cache_limit();

            app.manage(state);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
