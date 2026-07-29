//! PeerDiscovery —— 节点发现的抽象接口与内存实现。
//!
//! 架构文档 (docs/architecture.md) 的铁律之二：网络层依赖抽象接口。
//! 上层永远通过 `PeerDiscovery` trait 找节点；底层今天是进程内内存实现，
//! 阶段二可无缝换成远程 Tracker，播放器代码不用改。
//!
//! 铁律之一：节点身份 ≠ 用户身份。这里只认 `peer_id`，
//! 账号（user_id）是阶段三才绑定上来的一层。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// 一个节点的可寻址信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    /// 节点地址（阶段一本机测试即 "127.0.0.1:port"）。
    pub addr: String,
}

/// 节点发现接口。找节点、宣告自己持有的分片、上下线。
pub trait PeerDiscovery: Send + Sync {
    /// 节点上线：注册自己的地址及持有的分片哈希。
    fn register(&self, peer: PeerInfo, owned_chunks: &[String]);
    /// 更新某节点新持有的分片（下载到新分片后调用）。
    fn announce(&self, peer_id: &str, chunk_hashes: &[String]);
    /// 查询：谁持有这个资源分片。
    fn find_peers(&self, chunk_hash: &str) -> Vec<PeerInfo>;
    /// 节点下线。阶段二：客户端断连或退出时调用。
    #[allow(dead_code)]
    fn unregister(&self, peer_id: &str);
    /// 当前在线节点数（调试/展示用）。
    fn peer_count(&self) -> usize;
}

/// 进程内内存 Tracker —— 阶段一实现。
///
/// 维护两张表：
///   - peers:  peer_id -> PeerInfo
///   - index:  chunk_hash -> {持有它的 peer_id 集合}
struct Inner {
    peers: HashMap<String, PeerInfo>,
    index: HashMap<String, HashSet<String>>,
}

pub struct InMemoryTracker {
    inner: Mutex<Inner>,
}

impl InMemoryTracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                peers: HashMap::new(),
                index: HashMap::new(),
            }),
        }
    }
}

impl Default for InMemoryTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerDiscovery for InMemoryTracker {
    fn register(&self, peer: PeerInfo, owned_chunks: &[String]) {
        let mut g = self.inner.lock().unwrap();
        let id = peer.peer_id.clone();
        g.peers.insert(id.clone(), peer);
        for h in owned_chunks {
            g.index.entry(h.clone()).or_default().insert(id.clone());
        }
    }

    fn announce(&self, peer_id: &str, chunk_hashes: &[String]) {
        let mut g = self.inner.lock().unwrap();
        // 只为已注册的节点登记分片。
        if !g.peers.contains_key(peer_id) {
            return;
        }
        for h in chunk_hashes {
            g.index.entry(h.clone()).or_default().insert(peer_id.to_string());
        }
    }

    fn find_peers(&self, chunk_hash: &str) -> Vec<PeerInfo> {
        let g = self.inner.lock().unwrap();
        match g.index.get(chunk_hash) {
            Some(ids) => ids
                .iter()
                .filter_map(|id| g.peers.get(id).cloned())
                .collect(),
            None => Vec::new(),
        }
    }

    fn unregister(&self, peer_id: &str) {
        let mut g = self.inner.lock().unwrap();
        g.peers.remove(peer_id);
        // 从所有分片索引中摘除该节点，并清理空条目。
        g.index.retain(|_, set| {
            set.remove(peer_id);
            !set.is_empty()
        });
    }

    fn peer_count(&self) -> usize {
        self.inner.lock().unwrap().peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> PeerInfo {
        PeerInfo {
            peer_id: id.into(),
            addr: format!("127.0.0.1:0/{id}"),
        }
    }

    #[test]
    fn register_and_find() {
        let t = InMemoryTracker::new();
        t.register(peer("A"), &["chunk1".into(), "chunk2".into()]);
        t.register(peer("B"), &["chunk2".into()]);

        // chunk2 由 A、B 两个节点持有。
        let mut ids: Vec<_> = t.find_peers("chunk2").iter().map(|p| p.peer_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["A", "B"]);

        // chunk1 只有 A。
        assert_eq!(t.find_peers("chunk1").len(), 1);
        // 无人持有的分片返回空。
        assert!(t.find_peers("nope").is_empty());
    }

    #[test]
    fn announce_requires_registration() {
        let t = InMemoryTracker::new();
        // 未注册的节点 announce 应被忽略。
        t.announce("ghost", &["c1".into()]);
        assert!(t.find_peers("c1").is_empty());

        t.register(peer("A"), &[]);
        t.announce("A", &["c1".into()]);
        assert_eq!(t.find_peers("c1").len(), 1);
    }

    #[test]
    fn unregister_cleans_index() {
        let t = InMemoryTracker::new();
        t.register(peer("A"), &["c1".into()]);
        assert_eq!(t.peer_count(), 1);

        t.unregister("A");
        assert_eq!(t.peer_count(), 0);
        assert!(t.find_peers("c1").is_empty());
    }
}
