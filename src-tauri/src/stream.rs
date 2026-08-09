//! 流式播放协议 —— 自定义 `stream://` (Windows 上为 http://stream.localhost) URI scheme。
//!
//! 这是"边下边播"的核心：`<audio src="stream://localhost/<track_hash>">` 让浏览器
//! 原生按字节区间（HTTP Range）请求媒体，每个区间背后按需从 P2P 网络拉取所需分片。
//! 浏览器负责缓冲、解码、seek —— 我们只负责"把某段字节喂给它，缺的分片现拉"。
//!
//! 相比 MediaSource 手动喂流，这个方案格式通吃（MP3/FLAC/AAC…）、seek 天然可用，
//! 且播放条的"已缓冲"底层会自动反映真实的渐进缓冲。

use crate::chunk::ChunkStore;
use crate::peer::{PeerDiscovery, RemoteTracker};
use crate::chunk::TrackManifest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 每次 Range 响应返回的最大窗口（1 MiB）。
/// 不返回整首 —— 保证分片是"边播边拉"而非一次拉完。
const WINDOW: usize = 1024 * 1024;

/// 一首可流式播放曲目的登记信息。
#[derive(Clone)]
pub struct StreamEntry {
    pub manifest: TrackManifest,
    pub mime: String,
}

/// 流式播放所需的共享句柄 —— 从 AppState 派生，可 clone 进协议处理线程。
#[derive(Clone)]
pub struct StreamCtx {
    pub peer_id: String,
    pub store: Arc<ChunkStore>,
    pub tracker: Arc<RemoteTracker>,
    /// track_hash -> 登记信息。前端 prepare_stream 时写入。
    pub registry: Arc<Mutex<HashMap<String, StreamEntry>>>,
}

impl StreamCtx {
    pub fn register(&self, manifest: TrackManifest, mime: String) {
        self.registry
            .lock()
            .unwrap()
            .insert(manifest.track_hash.clone(), StreamEntry { manifest, mime });
    }

    fn get(&self, track_hash: &str) -> Option<StreamEntry> {
        self.registry.lock().unwrap().get(track_hash).cloned()
    }
}

/// 解析 HTTP Range 头，返回 (start, end_inclusive_opt)。仅支持 `bytes=start-[end]`。
fn parse_range(header: &str, total: usize) -> Option<(usize, Option<usize>)> {
    let spec = header.strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let start: usize = if s.is_empty() { 0 } else { s.parse().ok()? };
    let end = if e.is_empty() {
        None
    } else {
        Some(e.parse::<usize>().ok()?.min(total.saturating_sub(1)))
    };
    Some((start, end))
}

/// 确保 [start, end] 覆盖的分片都在本地；缺的**并行**从 peer 拉取并校验入库。
///
/// 多源加速：该区间缺多少片，就并发拉多少片（受并发上限约束），
/// 而不是逐片串行等待。
fn ensure_range_available(
    ctx: &StreamCtx,
    entry: &StreamEntry,
    start: usize,
    end: usize,
) -> Result<(), String> {
    let m = &entry.manifest;
    let (first, last) = match crate::chunk::chunks_for_range(m.total_size, m.chunk_size, start, end - start + 1) {
        Some(r) => r,
        None => return Ok(()),
    };

    // 先算出本区间缺哪些分片。
    let missing: Vec<String> = (first..=last)
        .filter_map(|idx| m.chunks.get(idx))
        .filter(|h| !ctx.store.has(h))
        .cloned()
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    // 并行拉取（排除自己作为源）。
    let fetched = crate::transfer::fetch_chunks_parallel(
        &missing,
        |hash| {
            ctx.tracker
                .find_peers(hash)
                .into_iter()
                .filter(|p| p.peer_id != ctx.peer_id)
                .collect()
        },
        &ctx.store,
        crate::transfer::MAX_CONCURRENT_FETCHES,
    )?;

    // 批量宣告：本节点现在也是这些分片的源了（一次 Tracker 往返而非 N 次）。
    if !fetched.is_empty() {
        ctx.tracker.announce(&ctx.peer_id, &fetched);
    }
    Ok(())
}

/// 处理一个 stream 请求：解析 track_hash + Range，按需拉分片，返回 206 响应体与头。
/// 返回 (status, headers, body)。
pub fn handle_request(
    ctx: &StreamCtx,
    path: &str,
    range_header: Option<&str>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    // path 形如 "/<track_hash>"，去掉前导 '/'。
    let track_hash = path.trim_start_matches('/');
    let entry = match ctx.get(track_hash) {
        Some(e) => e,
        None => return (404, vec![], b"unknown track".to_vec()),
    };
    let total = entry.manifest.total_size;
    let mime = if entry.mime.is_empty() {
        "audio/mpeg".to_string()
    } else {
        entry.mime.clone()
    };

    // 解析 Range；无 Range 时按开头一个窗口处理（媒体元素几乎总是发 Range）。
    let (start, end_req) = range_header
        .and_then(|h| parse_range(h, total))
        .unwrap_or((0, None));

    if start >= total {
        return (
            416,
            vec![("Content-Range".into(), format!("bytes */{total}"))],
            Vec::new(),
        );
    }

    // 限制单次窗口大小，保证边播边拉。
    let end = match end_req {
        Some(e) => e.min(start + WINDOW - 1),
        None => (start + WINDOW - 1).min(total - 1),
    };

    if let Err(e) = ensure_range_available(ctx, &entry, start, end) {
        eprintln!("[stream] {e}");
        return (503, vec![], e.into_bytes());
    }

    let body = match ctx.store.read_range(&entry.manifest, start, end - start + 1) {
        Ok(b) => b,
        Err(e) => return (500, vec![], e.into_bytes()),
    };

    let headers = vec![
        ("Content-Type".into(), mime),
        ("Accept-Ranges".into(), "bytes".into()),
        (
            "Content-Range".into(),
            format!("bytes {start}-{end}/{total}"),
        ),
        ("Content-Length".into(), body.len().to_string()),
        // 自定义协议跨 Origin，放开 CORS 以防被拦。
        ("Access-Control-Allow-Origin".into(), "*".into()),
    ];
    (206, headers, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::hash_bytes;

    #[test]
    fn parse_range_variants() {
        assert_eq!(parse_range("bytes=0-", 1000), Some((0, None)));
        assert_eq!(parse_range("bytes=100-199", 1000), Some((100, Some(199))));
        // end 超总长会被夹到 total-1
        assert_eq!(parse_range("bytes=0-99999", 1000), Some((0, Some(999))));
        assert_eq!(parse_range("nonsense", 1000), None);
    }

    /// 构造一个所有分片都在本地的 StreamCtx（不触发 peer 拉取）。
    fn local_ctx(data: &[u8], cs: usize) -> (StreamCtx, TrackManifest) {
        let store = Arc::new(ChunkStore::new());
        let mut chunks = Vec::new();
        for c in data.chunks(cs) {
            let h = hash_bytes(c);
            store.put(&h, c.to_vec());
            chunks.push(h);
        }
        let manifest = TrackManifest {
            track_hash: hash_bytes(data),
            total_size: data.len(),
            chunk_size: cs,
            chunks,
        };
        let ctx = StreamCtx {
            peer_id: "self".into(),
            store,
            // 指向一个不会被用到的地址（分片全在本地，不会 find_peers）。
            tracker: Arc::new(RemoteTracker::new("127.0.0.1:1")),
            registry: Arc::new(Mutex::new(HashMap::new())),
        };
        ctx.register(manifest.clone(), "audio/mpeg".into());
        (ctx, manifest)
    }

    #[test]
    fn handle_request_returns_partial_range() {
        let data: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        let (ctx, manifest) = local_ctx(&data, 256);
        let path = format!("/{}", manifest.track_hash);

        // 请求 bytes=100-299 → 206，body 与源数据一致，头含正确 Content-Range。
        let (status, headers, body) = handle_request(&ctx, &path, Some("bytes=100-299"));
        assert_eq!(status, 206);
        assert_eq!(body, &data[100..300]);
        let cr = headers.iter().find(|(k, _)| k == "Content-Range").unwrap();
        assert_eq!(cr.1, "bytes 100-299/2000");
    }

    #[test]
    fn handle_request_unknown_track_404() {
        let data = vec![1u8, 2, 3];
        let (ctx, _) = local_ctx(&data, 256);
        let (status, _, _) = handle_request(&ctx, "/nonexistent", Some("bytes=0-"));
        assert_eq!(status, 404);
    }

    #[test]
    fn handle_request_out_of_range_416() {
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let (ctx, manifest) = local_ctx(&data, 256);
        let path = format!("/{}", manifest.track_hash);
        let (status, _, _) = handle_request(&ctx, &path, Some("bytes=9999-"));
        assert_eq!(status, 416);
    }
}
