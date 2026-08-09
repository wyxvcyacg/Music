//! ChunkStore —— 内容寻址的分片存储。
//!
//! 架构文档 (docs/architecture.md) 的铁律之三：资源用内容哈希寻址。
//! 一首曲目被切成固定大小的分片，每个分片按其内容的 SHA-256 定位。
//! 这是 P2P 去重与多源下载的基础，且与账号无关。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 默认分片大小：256 KiB。流媒体场景下小分片利于快速起播与多源调度。
pub const CHUNK_SIZE: usize = 256 * 1024;

/// 缓存默认容量上限：2 GiB。超过后按 LRU 淘汰未受保护的分片。
pub const DEFAULT_CACHE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

/// 淘汰目标水位 —— 降到上限的这个比例，避免刚好卡在上限反复触发淘汰。
const EVICT_TARGET_RATIO: f64 = 0.8;

/// 一次淘汰的结果（供日志/UI 展示）。
#[derive(Debug, Default, Clone, Serialize)]
pub struct EvictReport {
    /// 删除的分片数。
    pub evicted: usize,
    /// 释放的字节数。
    pub bytes_freed: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// 把文件 mtime 刷成当前时间，作为 LRU 的"最近使用"标记。
/// 失败无害 —— 只影响淘汰顺序的精度，不影响正确性。
fn touch(path: &Path) {
    let now = std::time::SystemTime::now();
    let times = fs::FileTimes::new().set_accessed(now).set_modified(now);
    if let Ok(f) = fs::File::options().write(true).open(path) {
        let _ = f.set_times(times);
    }
}

/// 对一段字节计算内容哈希（十六进制小写）。
pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// 一首曲目切片后的清单：有序的分片哈希列表 + 元信息。
/// 类似"种子"，用于向他人描述"这首歌由哪些分片组成"。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackManifest {
    /// 整首曲目内容的哈希（作为曲目的全局唯一 id）。
    pub track_hash: String,
    /// 原始字节总长度。
    pub total_size: usize,
    /// 分片大小。
    pub chunk_size: usize,
    /// 按顺序排列的分片哈希。重组时按此顺序拼接。
    pub chunks: Vec<String>,
}

/// 内容寻址的分片存储。
///
/// 既是本地缓存（自己下载/导入的分片），也是对外分发的来源
/// —— 别的节点可以来拉取 `store` 里已有的分片。
///
/// 两种模式：
///   - `new()`   纯内存，进程退出即丢（测试用）
///   - `open(d)` 磁盘持久化：分片按内容哈希存为文件，跨重启保留，
///               内存里只保留"持有哪些 hash"的索引，不驻留分片内容。
pub struct ChunkStore {
    /// Some = 磁盘模式的根目录；None = 纯内存模式。
    dir: Option<PathBuf>,
    /// 持有哪些分片哈希。磁盘模式下这是唯一的内存开销。
    index: Mutex<HashSet<String>>,
    /// 仅内存模式使用。
    mem: Mutex<HashMap<String, Vec<u8>>>,
}

impl ChunkStore {
    /// 纯内存 store（进程退出即丢）。
    pub fn new() -> Self {
        Self {
            dir: None,
            index: Mutex::new(HashSet::new()),
            mem: Mutex::new(HashMap::new()),
        }
    }

    /// 打开磁盘持久化 store。目录不存在会创建；已有分片会被扫描进索引。
    ///
    /// 布局：`<dir>/<hash前2位>/<完整hash>`，两级目录避免单目录堆积几万文件。
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let index = Mutex::new(scan_existing(&dir));
        Ok(Self {
            dir: Some(dir),
            index,
            mem: Mutex::new(HashMap::new()),
        })
    }

    /// 分片在磁盘上的路径。
    fn path_for(&self, dir: &Path, hash: &str) -> PathBuf {
        // hash 是 64 位小写 hex，取前 2 位做子目录。
        let (prefix, _) = hash.split_at(2.min(hash.len()));
        dir.join(prefix).join(hash)
    }

    /// 导入一整首曲目：切片、对每片算哈希、存入 store，返回清单。
    pub fn import_track(&self, data: &[u8]) -> TrackManifest {
        let mut chunk_hashes = Vec::new();
        for chunk in data.chunks(CHUNK_SIZE) {
            let h = hash_bytes(chunk);
            // 已存在则跳过写入（去重），但仍要记入清单以保证顺序完整。
            if !self.has(&h) {
                self.write_chunk(&h, chunk);
            }
            chunk_hashes.push(h);
        }

        TrackManifest {
            track_hash: hash_bytes(data),
            total_size: data.len(),
            chunk_size: CHUNK_SIZE,
            chunks: chunk_hashes,
        }
    }

    /// 是否持有某个分片。只查内存索引，不碰磁盘。
    pub fn has(&self, hash: &str) -> bool {
        self.index.lock().unwrap().contains(hash)
    }

    /// 读取一个分片。响应其他节点的分片拉取请求，以及本地重组/区间读取。
    ///
    /// 磁盘模式下顺带把文件 mtime 刷成当前时间 —— 这就是 LRU 的"最近使用"
    /// 记录，不需要额外的状态文件，且天然崩溃安全。
    pub fn get(&self, hash: &str) -> Option<Vec<u8>> {
        if !self.has(hash) {
            return None;
        }
        match &self.dir {
            Some(dir) => {
                let path = self.path_for(dir, hash);
                let data = fs::read(&path).ok()?;
                touch(&path);
                Some(data)
            }
            None => self.mem.lock().unwrap().get(hash).cloned(),
        }
    }

    /// 写入一个分片。校验内容哈希与声称的 hash 一致，防止被污染的数据混入。
    /// 返回是否写入成功（哈希不匹配则拒绝）。
    pub fn put(&self, hash: &str, data: Vec<u8>) -> bool {
        if hash_bytes(&data) != hash {
            return false;
        }
        self.write_chunk(hash, &data);
        true
    }

    /// 实际落盘/入内存 + 更新索引。调用方须已校验哈希。
    fn write_chunk(&self, hash: &str, data: &[u8]) {
        match &self.dir {
            Some(dir) => {
                let path = self.path_for(dir, hash);
                if let Some(parent) = path.parent() {
                    if fs::create_dir_all(parent).is_err() {
                        return;
                    }
                }
                // 原子写：先写临时文件再 rename，避免崩溃留下半截文件
                // 被后续扫描当成有效分片。
                let tmp = path.with_extension("tmp");
                if fs::write(&tmp, data).is_err() {
                    return;
                }
                if fs::rename(&tmp, &path).is_err() {
                    let _ = fs::remove_file(&tmp);
                    return;
                }
            }
            None => {
                self.mem
                    .lock()
                    .unwrap()
                    .insert(hash.to_string(), data.to_vec());
            }
        }
        self.index.lock().unwrap().insert(hash.to_string());
    }

    /// 当前持有的所有分片哈希（供向 Tracker 注册"我有哪些分片"）。
    pub fn owned_hashes(&self) -> Vec<String> {
        self.index.lock().unwrap().iter().cloned().collect()
    }

    /// 持有的分片数量。比 `owned_hashes().len()` 便宜 —— 状态轮询用这个，
    /// 避免每次克隆几千个字符串。
    pub fn chunk_count(&self) -> usize {
        self.index.lock().unwrap().len()
    }

    /// 缓存占用的字节数（磁盘模式统计文件大小；内存模式统计内容长度）。
    pub fn cache_bytes(&self) -> u64 {
        match &self.dir {
            Some(dir) => {
                let hashes = self.owned_hashes();
                hashes
                    .iter()
                    .filter_map(|h| fs::metadata(self.path_for(dir, h)).ok())
                    .map(|m| m.len())
                    .sum()
            }
            None => self
                .mem
                .lock()
                .unwrap()
                .values()
                .map(|v| v.len() as u64)
                .sum(),
        }
    }

    /// 按 LRU 淘汰缓存，直到占用降到 `limit` 的 `EVICT_TARGET_RATIO` 以下。
    ///
    /// `protected` 里的分片永不删除 —— 那是曲库里"我的收藏"用到的分片。
    /// 其余（流式播放顺带缓存的过路分片）按 mtime 升序（最久未用优先）删除。
    ///
    /// 未超限则什么也不做。内存模式下不淘汰（进程退出即释放）。
    pub fn evict_if_needed(&self, limit: u64, protected: &HashSet<String>) -> EvictReport {
        let mut report = EvictReport::default();
        let Some(dir) = &self.dir else { return report };

        let used = self.cache_bytes();
        report.bytes_before = used;
        if used <= limit {
            report.bytes_after = used;
            return report;
        }

        // 收集可淘汰的候选：未受保护的分片 + 其 mtime 与大小。
        let mut candidates: Vec<(std::time::SystemTime, u64, String)> = self
            .owned_hashes()
            .into_iter()
            .filter(|h| !protected.contains(h))
            .filter_map(|h| {
                let meta = fs::metadata(self.path_for(dir, &h)).ok()?;
                let mtime = meta.modified().ok()?;
                Some((mtime, meta.len(), h))
            })
            .collect();

        // 最久未用的排在前面。
        candidates.sort_by_key(|(mtime, _, _)| *mtime);

        let target = (limit as f64 * EVICT_TARGET_RATIO) as u64;
        let mut current = used;

        for (_, size, hash) in candidates {
            if current <= target {
                break;
            }
            if fs::remove_file(self.path_for(dir, &hash)).is_ok() {
                self.index.lock().unwrap().remove(&hash);
                current = current.saturating_sub(size);
                report.evicted += 1;
                report.bytes_freed += size;
            }
        }

        report.bytes_after = current;
        report
    }

    /// 按清单重组出完整曲目。缺任何一片则返回 Err(缺失的哈希)。
    pub fn reassemble(&self, manifest: &TrackManifest) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(manifest.total_size);
        for h in &manifest.chunks {
            match self.get(h) {
                Some(bytes) => out.extend_from_slice(&bytes),
                None => return Err(format!("missing chunk: {h}")),
            }
        }
        Ok(out)
    }

    /// 读取曲目 [start, start+len) 字节区间（用于流式播放的 Range 响应）。
    /// 跨分片自动拼接。任一覆盖到的分片缺失则返回 Err(缺失的哈希)，
    /// 供上层按需从 peer 拉取后重试。
    pub fn read_range(
        &self,
        manifest: &TrackManifest,
        start: usize,
        len: usize,
    ) -> Result<Vec<u8>, String> {
        let end = (start + len).min(manifest.total_size);
        if start >= manifest.total_size || end <= start {
            return Ok(Vec::new());
        }
        let cs = manifest.chunk_size;
        let mut out = Vec::with_capacity(end - start);
        let mut pos = start;
        while pos < end {
            let idx = pos / cs; // 该字节落在第几个分片
            let hash = manifest
                .chunks
                .get(idx)
                .ok_or_else(|| format!("range out of bounds: chunk index {idx}"))?;
            let chunk = self
                .get(hash)
                .ok_or_else(|| format!("missing chunk: {hash}"))?;
            let off = pos - idx * cs; // 分片内偏移
            let take = (chunk.len() - off).min(end - pos);
            out.extend_from_slice(&chunk[off..off + take]);
            pos += take;
        }
        Ok(out)
    }
}

/// 扫描已有分片目录，重建"持有哪些 hash"的索引。
///
/// 只认文件名本身就是合法 64 位 hex 的文件 —— 这样残留的 `.tmp`
/// 半截文件会被自动忽略，不会被当成有效分片。
fn scan_existing(dir: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let subdirs = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return set,
    };
    for sub in subdirs.flatten() {
        if !sub.path().is_dir() {
            continue;
        }
        if let Ok(files) = fs::read_dir(sub.path()) {
            for f in files.flatten() {
                if let Some(name) = f.file_name().to_str() {
                    if is_chunk_hash(name) {
                        set.insert(name.to_string());
                    }
                }
            }
        }
    }
    set
}

/// 文件名是否是合法的分片哈希（64 位小写 hex）。
fn is_chunk_hash(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 计算 [start, start+len) 区间覆盖的分片下标（含端点）。纯函数，便于测试。
/// 返回 (first_idx, last_idx)；空区间返回 None。
pub fn chunks_for_range(
    total_size: usize,
    chunk_size: usize,
    start: usize,
    len: usize,
) -> Option<(usize, usize)> {
    let end = (start + len).min(total_size);
    if start >= total_size || end <= start || chunk_size == 0 {
        return None;
    }
    Some((start / chunk_size, (end - 1) / chunk_size))
}

impl Default for ChunkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个本次测试专属的临时目录（无需外部 crate）。
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("music_test_{tag}_{pid}_{n}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn disk_store_persists_across_reopen() {
        let dir = temp_dir("persist");

        let payload = b"chunk that must survive a restart".to_vec();
        let h = hash_bytes(&payload);

        // 第一次打开：写入一个分片。
        {
            let store = ChunkStore::open(&dir).unwrap();
            assert!(store.put(&h, payload.clone()));
            assert!(store.has(&h));
            assert_eq!(store.chunk_count(), 1);
        } // store 离开作用域，模拟进程退出

        // 重新打开同一目录：分片应仍在，内容一致。
        {
            let reopened = ChunkStore::open(&dir).unwrap();
            assert!(reopened.has(&h), "chunk lost across reopen");
            assert_eq!(reopened.get(&h).unwrap(), payload);
            assert_eq!(reopened.chunk_count(), 1);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_store_import_and_read_range() {
        let dir = temp_dir("range");
        let store = ChunkStore::open(&dir).unwrap();

        // 跨多个分片的数据，走完整 import -> reassemble -> read_range 路径。
        let data: Vec<u8> = (0..(CHUNK_SIZE + 5000)).map(|i| (i % 251) as u8).collect();
        let manifest = store.import_track(&data);
        assert_eq!(manifest.chunks.len(), 2);

        assert_eq!(store.reassemble(&manifest).unwrap(), data);

        // 跨分片边界读一段。
        let start = CHUNK_SIZE - 50;
        let got = store.read_range(&manifest, start, 100).unwrap();
        assert_eq!(got, &data[start..start + 100]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_ignores_non_hash_files() {
        let dir = temp_dir("scan");
        fs::create_dir_all(dir.join("ab")).unwrap();
        // 残留的 .tmp（崩溃留下的半截文件）与其它杂项都不该被当成分片。
        fs::write(dir.join("ab").join("not-a-hash"), b"junk").unwrap();
        fs::write(dir.join("ab").join("abcd.tmp"), b"partial").unwrap();
        // 一个合法的 64 位 hex 文件名应被识别。
        let valid = "a".repeat(64);
        fs::write(dir.join("ab").join(&valid), b"x").unwrap();

        let store = ChunkStore::open(&dir).unwrap();
        assert_eq!(store.chunk_count(), 1, "only the valid hash should load");
        assert!(store.has(&valid));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_store_rejects_tampered_put() {
        let dir = temp_dir("tamper");
        let store = ChunkStore::open(&dir).unwrap();

        let good = b"authentic bytes".to_vec();
        let h = hash_bytes(&good);
        // 用真 hash 配假数据 —— 磁盘模式下同样必须拒绝，且不留下文件。
        assert!(!store.put(&h, b"tampered".to_vec()));
        assert!(!store.has(&h));
        assert_eq!(store.chunk_count(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_bytes_reports_size() {
        let dir = temp_dir("size");
        let store = ChunkStore::open(&dir).unwrap();
        let payload = vec![7u8; 4096];
        let h = hash_bytes(&payload);
        store.put(&h, payload);
        assert_eq!(store.cache_bytes(), 4096);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 放 n 个各 `size` 字节的分片，返回它们的哈希（按放入顺序）。
    /// 每次之间隔一点时间，让 mtime 有可区分的先后。
    fn fill(store: &ChunkStore, n: usize, size: usize) -> Vec<String> {
        let mut hashes = Vec::new();
        for i in 0..n {
            let payload = vec![i as u8; size];
            let h = hash_bytes(&payload);
            store.put(&h, payload);
            hashes.push(h);
            // 文件时间戳精度有限，睡一下确保顺序可分辨。
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        hashes
    }

    #[test]
    fn evict_does_nothing_under_limit() {
        let dir = temp_dir("evict_noop");
        let store = ChunkStore::open(&dir).unwrap();
        let hashes = fill(&store, 3, 1000);

        // 上限远大于占用 → 不该删任何东西。
        let report = store.evict_if_needed(1_000_000, &HashSet::new());
        assert_eq!(report.evicted, 0);
        for h in &hashes {
            assert!(store.has(h));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_removes_least_recently_used_first() {
        let dir = temp_dir("evict_lru");
        let store = ChunkStore::open(&dir).unwrap();
        // 5 片 × 1000 字节 = 5000。
        let hashes = fill(&store, 5, 1000);

        // 读第 0 片，把它刷成"最近使用" —— 它应当最后才被淘汰。
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(store.get(&hashes[0]).is_some());

        // 上限 3000 → 目标水位 2400，需要腾出 ~2600 字节（约 3 片）。
        let report = store.evict_if_needed(3000, &HashSet::new());
        assert!(report.evicted >= 2, "expected eviction, got {report:?}");
        assert!(
            report.bytes_after <= 2400,
            "should reach target watermark, got {report:?}"
        );

        // 刚被读过的第 0 片应当还在（它是最近使用的）。
        assert!(
            store.has(&hashes[0]),
            "recently-used chunk should survive eviction"
        );
        // 最久未用的第 1 片应当已被删。
        assert!(!store.has(&hashes[1]), "LRU chunk should be evicted");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_never_removes_protected_chunks() {
        let dir = temp_dir("evict_protect");
        let store = ChunkStore::open(&dir).unwrap();
        let hashes = fill(&store, 5, 1000);

        // 把最久未用的两片标为受保护（模拟"曲库里的收藏"）。
        let protected: HashSet<String> =
            [hashes[0].clone(), hashes[1].clone()].into_iter().collect();

        // 上限很小，逼它尽量淘汰。
        let report = store.evict_if_needed(1000, &protected);

        // 受保护的必须还在，哪怕它们是最久未用的。
        for h in &protected {
            assert!(store.has(h), "protected chunk was evicted: {h}");
        }
        // 未受保护的应当被删光（3 片）。
        assert_eq!(report.evicted, 3, "should evict all unprotected: {report:?}");
        assert_eq!(store.chunk_count(), 2, "only protected should remain");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_and_reassemble_roundtrip() {
        let store = ChunkStore::new();
        // 造一段跨多个分片的数据。
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 123)).map(|i| (i % 251) as u8).collect();

        let manifest = store.import_track(&data);
        assert_eq!(manifest.total_size, data.len());
        assert_eq!(manifest.chunks.len(), 3); // 2 满片 + 1 余片

        let restored = store.reassemble(&manifest).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn put_rejects_tampered_data() {
        let store = ChunkStore::new();
        let good = b"hello world".to_vec();
        let h = hash_bytes(&good);

        assert!(store.put(&h, good.clone()));
        assert!(store.has(&h));

        // 用真 hash 配假数据 —— 必须被拒绝。
        assert!(!store.put(&h, b"tampered".to_vec()));
    }

    #[test]
    fn reassemble_reports_missing_chunk() {
        let store = ChunkStore::new();
        let manifest = TrackManifest {
            track_hash: "x".into(),
            total_size: 10,
            chunk_size: CHUNK_SIZE,
            chunks: vec!["deadbeef".into()],
        };
        assert!(store.reassemble(&manifest).is_err());
    }

    #[test]
    fn chunks_for_range_indices() {
        // total 1000，分片 256 → 4 片：[0,256,512,768]
        assert_eq!(chunks_for_range(1000, 256, 0, 100), Some((0, 0)));
        // 跨片：200..300 覆盖片 0 和片 1
        assert_eq!(chunks_for_range(1000, 256, 200, 100), Some((0, 1)));
        // 末尾对齐：768..1000 只在片 3
        assert_eq!(chunks_for_range(1000, 256, 768, 500), Some((3, 3)));
        // 越界 start
        assert_eq!(chunks_for_range(1000, 256, 1000, 10), None);
        // 空长度
        assert_eq!(chunks_for_range(1000, 256, 100, 0), None);
    }

    #[test]
    fn read_range_spans_chunks() {
        // 用小分片造 2.5 片数据，便于验证跨片读取。
        let data: Vec<u8> = (0..650).map(|i| (i % 256) as u8).collect();
        let store = ChunkStore::new();
        let cs = 256usize;
        let mut chunks = Vec::new();
        for c in data.chunks(cs) {
            let h = hash_bytes(c);
            store.put(&h, c.to_vec());
            chunks.push(h);
        }
        let manifest = TrackManifest {
            track_hash: hash_bytes(&data),
            total_size: data.len(),
            chunk_size: cs,
            chunks,
        };

        // 读一段跨越片0/片1边界的区间。
        let got = store.read_range(&manifest, 200, 120).unwrap();
        assert_eq!(got, &data[200..320]);

        // 读到文件末尾（区间超出总长应被截断）。
        let tail = store.read_range(&manifest, 600, 999).unwrap();
        assert_eq!(tail, &data[600..650]);

        // 缺片时报错。
        let missing = TrackManifest {
            chunks: vec!["deadbeef".into()],
            total_size: 300,
            chunk_size: cs,
            track_hash: "x".into(),
        };
        assert!(store.read_range(&missing, 0, 100).is_err());
    }
}