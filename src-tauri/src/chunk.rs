//! ChunkStore —— 内容寻址的分片存储。
//!
//! 架构文档 (docs/architecture.md) 的铁律之三：资源用内容哈希寻址。
//! 一首曲目被切成固定大小的分片，每个分片按其内容的 SHA-256 定位。
//! 这是 P2P 去重与多源下载的基础，且与账号无关。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// 默认分片大小：256 KiB。流媒体场景下小分片利于快速起播与多源调度。
pub const CHUNK_SIZE: usize = 256 * 1024;

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
pub struct ChunkStore {
    chunks: Mutex<HashMap<String, Vec<u8>>>,
}

impl ChunkStore {
    pub fn new() -> Self {
        Self {
            chunks: Mutex::new(HashMap::new()),
        }
    }

    /// 导入一整首曲目：切片、对每片算哈希、存入 store，返回清单。
    pub fn import_track(&self, data: &[u8]) -> TrackManifest {
        let mut store = self.chunks.lock().unwrap();
        let mut chunk_hashes = Vec::new();

        for chunk in data.chunks(CHUNK_SIZE) {
            let h = hash_bytes(chunk);
            // 已存在则跳过写入（去重），但仍要记入清单以保证顺序完整。
            store.entry(h.clone()).or_insert_with(|| chunk.to_vec());
            chunk_hashes.push(h);
        }

        TrackManifest {
            track_hash: hash_bytes(data),
            total_size: data.len(),
            chunk_size: CHUNK_SIZE,
            chunks: chunk_hashes,
        }
    }

    /// 是否持有某个分片。
    pub fn has(&self, hash: &str) -> bool {
        self.chunks.lock().unwrap().contains_key(hash)
    }

    /// 读取一个分片。阶段二：响应其他节点的分片拉取请求。
    #[allow(dead_code)]
    pub fn get(&self, hash: &str) -> Option<Vec<u8>> {
        self.chunks.lock().unwrap().get(hash).cloned()
    }

    /// 写入一个分片。校验内容哈希与声称的 hash 一致，防止被污染的数据混入。
    /// 返回是否写入成功（哈希不匹配则拒绝）。阶段二：接收从其他节点下载的分片。
    #[allow(dead_code)]
    pub fn put(&self, hash: &str, data: Vec<u8>) -> bool {
        if hash_bytes(&data) != hash {
            return false;
        }
        self.chunks.lock().unwrap().insert(hash.to_string(), data);
        true
    }

    /// 当前持有的所有分片哈希（供向 Tracker 注册"我有哪些分片"）。
    pub fn owned_hashes(&self) -> Vec<String> {
        self.chunks.lock().unwrap().keys().cloned().collect()
    }

    /// 按清单重组出完整曲目。缺任何一片则返回 Err(缺失的哈希)。
    pub fn reassemble(&self, manifest: &TrackManifest) -> Result<Vec<u8>, String> {
        let store = self.chunks.lock().unwrap();
        let mut out = Vec::with_capacity(manifest.total_size);
        for h in &manifest.chunks {
            match store.get(h) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => return Err(format!("missing chunk: {h}")),
            }
        }
        Ok(out)
    }
}

impl Default for ChunkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
