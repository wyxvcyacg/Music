//! Music —— P2P 流媒体音乐播放器（Tauri 后端）。
//!
//! 阶段一：本地跑通 ChunkStore（分片/哈希）+ 内存 Tracker（节点发现）。
//! 账号系统留到阶段三，这里只有 peer_id，没有 user_id。

mod chunk;
mod peer;

use chunk::{ChunkStore, TrackManifest};
use peer::{InMemoryTracker, PeerDiscovery, PeerInfo};
use tauri::{Manager, State};
use uuid::Uuid;

/// 应用级共享状态。
struct AppState {
    /// 本节点的 id —— 启动时随机生成，与账号无关（铁律之一）。
    peer_id: String,
    store: ChunkStore,
    tracker: InMemoryTracker,
}

/// 导入一首曲目（字节数组）：切片、算哈希、存入本地 store，
/// 并把持有的分片向 Tracker 宣告，返回清单。
#[tauri::command]
fn import_track(state: State<AppState>, data: Vec<u8>) -> TrackManifest {
    let manifest = state.store.import_track(&data);
    state.tracker.announce(&state.peer_id, &manifest.chunks);
    manifest
}

/// 查询某个分片当前有哪些节点持有。
#[tauri::command]
fn find_peers(state: State<AppState>, chunk_hash: String) -> Vec<PeerInfo> {
    state.tracker.find_peers(&chunk_hash)
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
    peer_count: usize,
    owned_chunks: usize,
}

#[tauri::command]
fn p2p_status(state: State<AppState>) -> P2pStatus {
    P2pStatus {
        peer_id: state.peer_id.clone(),
        peer_count: state.tracker.peer_count(),
        owned_chunks: state.store.owned_hashes().len(),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let peer_id = Uuid::new_v4().to_string();

    let tracker = InMemoryTracker::new();
    // 本节点在自己的内存 Tracker 上登记（阶段一：单进程即一个节点）。
    tracker.register(
        PeerInfo {
            peer_id: peer_id.clone(),
            addr: "127.0.0.1:0".into(),
        },
        &[],
    );

    let state = AppState {
        peer_id,
        store: ChunkStore::new(),
        tracker,
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            import_track,
            find_peers,
            has_chunk,
            reassemble,
            p2p_status,
        ])
        .setup(|app| {
            // 触碰一次 state，确保初始化无误。
            let _ = app.state::<AppState>();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
