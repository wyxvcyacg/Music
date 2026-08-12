//! 分片传输 —— 节点之间直接用 TCP 传分片数据（阶段二）。
//!
//! 每个客户端后台跑一个"分片服务"，监听随机端口，响应其他节点的分片拉取。
//! 下载方用 `fetch_chunk` 主动连接持有者拉取。
//!
//! 线协议（二进制）：
//!   请求:  [u16 be: hash 长度][hash utf8 bytes]
//!   响应:  [u32 be: chunk 长度][chunk bytes]     （长度为 0 表示对方没有此分片）

use crate::chunk::ChunkStore;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// 处理一个分片拉取连接：读 hash，回分片字节。
fn serve_conn(mut stream: TcpStream, store: Arc<ChunkStore>) {
    // 读 hash 长度（u16 be）。
    let mut len_buf = [0u8; 2];
    if stream.read_exact(&mut len_buf).is_err() {
        return;
    }
    let hash_len = u16::from_be_bytes(len_buf) as usize;
    if hash_len == 0 || hash_len > 128 {
        return; // sha256 hex 是 64 字符，给点余量，异常长度直接拒绝
    }

    let mut hash_buf = vec![0u8; hash_len];
    if stream.read_exact(&mut hash_buf).is_err() {
        return;
    }
    let hash = match String::from_utf8(hash_buf) {
        Ok(h) => h,
        Err(_) => return,
    };

    // 查本地 store，回分片（无则回长度 0）。
    let data = store.get(&hash).unwrap_or_default();
    let out_len = (data.len() as u32).to_be_bytes();
    let _ = stream.write_all(&out_len);
    if !data.is_empty() {
        let _ = stream.write_all(&data);
    }
    let _ = stream.flush();
}

/// 启动分片服务，绑定到随机端口，返回实际监听地址（"127.0.0.1:xxxxx"）。
/// 后台线程持续 accept，不阻塞调用方。
pub fn start_chunk_server(store: Arc<ChunkStore>) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let store = Arc::clone(&store);
                    std::thread::spawn(move || serve_conn(s, store));
                }
                Err(e) => eprintln!("[transfer] accept error: {e}"),
            }
        }
    });

    Ok(addr)
}

/// 从指定 peer 地址拉取一个分片。返回分片字节；对方没有或出错则 Err。
pub fn fetch_chunk(peer_addr: &str, hash: &str) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(peer_addr)
        .map_err(|e| format!("connect {peer_addr} failed: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    // 发请求：[u16 len][hash]
    let hash_bytes = hash.as_bytes();
    let len = hash_bytes.len() as u16;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| format!("send len failed: {e}"))?;
    stream
        .write_all(hash_bytes)
        .map_err(|e| format!("send hash failed: {e}"))?;
    stream.flush().ok();

    // 读响应：[u32 len][bytes]
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read len failed: {e}"))?;
    let data_len = u32::from_be_bytes(len_buf) as usize;
    if data_len == 0 {
        return Err(format!("peer {peer_addr} does not have chunk {hash}"));
    }

    let mut data = vec![0u8; data_len];
    stream
        .read_exact(&mut data)
        .map_err(|e| format!("read data failed: {e}"))?;
    Ok(data)
}

/// 按 peer 的能力选择传输方式拉取分片 —— UDP 优先，失败回退 TCP。
///
/// 这是整个代码库里**唯一**决定用哪种传输协议的地方。`fetch_chunks_parallel`、
/// `stream.rs`、`lib.rs` 都只认识 `PeerInfo`，所以新增传输方式只改这一处，
/// 不碰并行拉取、Range 流式播放、缓存淘汰、曲库。
///
/// 顺序的理由：
/// - 有 `udp_addr` 才试 UDP。老节点没有这个字段 → 直接走 TCP，新旧节点互通。
/// - UDP 失败**总是**回退 TCP，不把错误抛给调用方。UDP 是加速手段，
///   不是新的失败点：打洞打不通、对称 NAT、丢包太多，都该退回原来能用的路。
/// - 两条路的数据最终都进 `ChunkStore::put`，SHA-256 校验一视同仁。
pub fn fetch_chunk_via(peer: &crate::peer::PeerInfo, hash: &str) -> Result<Vec<u8>, String> {
    if peer.udp_addr.is_some() {
        let targets = crate::holepunch::candidates(peer);
        if !targets.is_empty() {
            match udp_attempt(&targets, hash) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    // 不是错误，是正常的能力协商结果 —— 记一笔便于诊断，然后回退。
                    eprintln!("[transfer] udp path failed ({e}), falling back to tcp");
                }
            }
        }
    }
    fetch_chunk(&peer.addr, hash)
}

/// UDP 路径：先打洞，通了再在**同一个 socket** 上拉分片。
///
/// 必须复用同一个 socket：打洞打通的是这个 socket 在 NAT 上的映射，
/// 换 socket 就等于换端口，映射作废，白打一遍。
fn udp_attempt(targets: &[std::net::SocketAddr], hash: &str) -> Result<Vec<u8>, String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("udp bind failed: {e}"))?;
    let punched = crate::holepunch::punch(&sock, targets);
    let addr = punched
        .addr()
        .ok_or_else(|| match &punched {
            crate::holepunch::PunchResult::Failed { reason, .. } => reason.clone(),
            _ => unreachable!(),
        })?;
    crate::udp::fetch_chunk_udp(&sock, addr, hash)
}

/// 并行拉取的默认并发上限。本机/局域网够用，也不至于打爆对端。
pub const MAX_CONCURRENT_FETCHES: usize = 4;

/// 并行拉取多个分片 —— 这是"多源加速"的核心。
///
/// 相比逐片串行，N 个缺失分片可同时从（可能不同的）持有者拉取，
/// 耗时从 N 次往返降到约 1 次往返（受并发上限约束）。
///
/// - `hashes`：待拉取的分片（调用方应已过滤掉本地已有的）
/// - `resolve`：查询某分片的候选源（注入 Tracker 查询，便于测试）
/// - `store`：拉到后写入此处；`put` 内部校验 SHA-256，并行不影响完整性
/// - 任一分片的所有候选源都失败 → 返回 Err（与串行版行为一致）
///
/// 返回成功拉取的分片哈希（供调用方批量 announce）。
pub fn fetch_chunks_parallel<F>(
    hashes: &[String],
    resolve: F,
    store: &ChunkStore,
    max_concurrent: usize,
) -> Result<Vec<String>, String>
where
    F: Fn(&str) -> Vec<crate::peer::PeerInfo> + Send + Sync,
{
    use std::sync::Mutex;

    if hashes.is_empty() {
        return Ok(Vec::new());
    }

    // 共享任务队列 + 结果收集。
    let queue = Mutex::new(hashes.to_vec().into_iter().collect::<std::collections::VecDeque<_>>());
    let fetched: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let first_err: Mutex<Option<String>> = Mutex::new(None);

    let worker_count = max_concurrent.max(1).min(hashes.len());

    // scoped threads：借用 store / resolve 而无需 Arc/clone。
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| loop {
                // 已有分片彻底失败则提前收工。
                if first_err.lock().unwrap().is_some() {
                    return;
                }
                let hash = match queue.lock().unwrap().pop_front() {
                    Some(h) => h,
                    None => return,
                };
                // 另一个 worker 可能刚好拉到同一片（去重）。
                if store.has(&hash) {
                    continue;
                }

                let peers = resolve(&hash);
                if peers.is_empty() {
                    let mut e = first_err.lock().unwrap();
                    if e.is_none() {
                        *e = Some(format!("no peer has chunk {hash}"));
                    }
                    return;
                }

                // 依次尝试候选源，任一成功即止。
                let mut ok = false;
                let mut last_err = String::new();
                for p in &peers {
                    match fetch_chunk_via(p, &hash) {
                        Ok(bytes) => {
                            if store.put(&hash, bytes) {
                                ok = true;
                                break;
                            }
                            last_err = format!("chunk {hash} failed hash verification");
                        }
                        Err(e) => last_err = e,
                    }
                }

                if ok {
                    fetched.lock().unwrap().push(hash);
                } else {
                    let mut e = first_err.lock().unwrap();
                    if e.is_none() {
                        *e = Some(format!("fetch {hash} failed: {last_err}"));
                    }
                    return;
                }
            });
        }
    });

    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    Ok(fetched.into_inner().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::hash_bytes;

    #[test]
    fn serve_and_fetch_roundtrip() {
        let store = Arc::new(ChunkStore::new());
        let payload = b"a chunk of audio bytes".to_vec();
        let h = hash_bytes(&payload);
        store.put(&h, payload.clone());

        let addr = start_chunk_server(Arc::clone(&store)).unwrap();
        // 从服务拉回来，应与原分片一致。
        let got = fetch_chunk(&addr, &h).unwrap();
        assert_eq!(got, payload);

        // 拉一个不存在的分片，应报错。
        assert!(fetch_chunk(&addr, "deadbeef").is_err());
    }

    #[test]
    fn parallel_fetch_across_multiple_peers() {
        use crate::peer::PeerInfo;

        // 三个"远端"节点，各持有一个不同的分片。
        let mut sources = Vec::new();
        let mut wanted = Vec::new();
        for i in 0..3u8 {
            let payload = vec![i; 1000];
            let h = hash_bytes(&payload);
            let remote = Arc::new(ChunkStore::new());
            remote.put(&h, payload.clone());
            let addr = start_chunk_server(remote).unwrap();
            sources.push((h.clone(), addr, payload));
            wanted.push(h);
        }

        // 本地空 store，并行拉这三片。
        let local = ChunkStore::new();
        let resolve = |hash: &str| -> Vec<PeerInfo> {
            sources
                .iter()
                .filter(|(h, _, _)| h == hash)
                .map(|(_, addr, _)| PeerInfo::new("remote", addr.clone()))
                .collect()
        };

        let fetched = fetch_chunks_parallel(&wanted, resolve, &local, 4).unwrap();
        assert_eq!(fetched.len(), 3);

        // 三片都进了本地 store，且内容正确（顺序无关）。
        for (h, _, payload) in &sources {
            assert!(local.has(h), "missing chunk {h}");
            assert_eq!(local.get(h).unwrap(), *payload);
        }
    }

    #[test]
    fn parallel_fetch_errors_when_no_peer() {
        use crate::peer::PeerInfo;
        let local = ChunkStore::new();
        let wanted = vec!["deadbeef".to_string()];
        // resolve 返回空 → 应报 "no peer has chunk"
        let resolve = |_: &str| -> Vec<PeerInfo> { Vec::new() };
        let err = fetch_chunks_parallel(&wanted, resolve, &local, 4).unwrap_err();
        assert!(err.contains("no peer"), "unexpected error: {err}");
    }

    #[test]
    fn parallel_fetch_skips_already_local() {
        use crate::peer::PeerInfo;
        let local = ChunkStore::new();
        let payload = b"already here".to_vec();
        let h = hash_bytes(&payload);
        local.put(&h, payload);

        // resolve 若被调用就会因无源报错；本地已有应直接跳过，不触发查询。
        let resolve = |_: &str| -> Vec<PeerInfo> { Vec::new() };
        let fetched = fetch_chunks_parallel(&[h], resolve, &local, 4).unwrap();
        assert!(fetched.is_empty(), "should not refetch local chunk");
    }

    /// 证明拉取真的是并行的：resolve 里人为加延迟，
    /// 4 片并发拉取的总耗时应显著小于串行（4 × 延迟）。
    #[test]
    fn parallel_fetch_is_actually_concurrent() {
        use crate::peer::PeerInfo;
        use std::time::{Duration, Instant};

        const DELAY: Duration = Duration::from_millis(120);
        const N: usize = 4;

        // 建 N 个源，各持一片。
        let mut sources = Vec::new();
        let mut wanted = Vec::new();
        for i in 0..N as u8 {
            let payload = vec![i; 512];
            let h = hash_bytes(&payload);
            let remote = Arc::new(ChunkStore::new());
            remote.put(&h, payload);
            let addr = start_chunk_server(remote).unwrap();
            sources.push((h.clone(), addr));
            wanted.push(h);
        }

        let local = ChunkStore::new();
        // 在 resolve 里 sleep，模拟"查源 + 网络往返"的耗时。
        let resolve = |hash: &str| -> Vec<PeerInfo> {
            std::thread::sleep(DELAY);
            sources
                .iter()
                .filter(|(h, _)| h == hash)
                .map(|(_, addr)| PeerInfo::new("remote", addr.clone()))
                .collect()
        };

        let t0 = Instant::now();
        let fetched = fetch_chunks_parallel(&wanted, resolve, &local, N).unwrap();
        let elapsed = t0.elapsed();

        assert_eq!(fetched.len(), N);
        // 串行会是 N × DELAY (480ms)；并行应接近 1 × DELAY。
        // 留足余量避免 CI 抖动导致假失败，但仍能区分串/并行。
        let serial_time = DELAY * N as u32;
        assert!(
            elapsed < serial_time / 2,
            "expected concurrent fetch (<{:?}), took {:?}",
            serial_time / 2,
            elapsed
        );
    }

    #[test]
    fn fetch_via_uses_tcp_when_peer_has_no_udp() {
        // 老节点（没有 udp_addr）必须仍然能拉到分片 —— 新旧节点互通。
        use crate::peer::PeerInfo;
        let remote = Arc::new(ChunkStore::new());
        let data = b"legacy peer payload".to_vec();
        let h = crate::chunk::hash_bytes(&data);
        remote.put(&h, data.clone());
        let addr = start_chunk_server(remote).unwrap();

        let peer = PeerInfo::new("old-node", addr);
        assert!(peer.udp_addr.is_none());
        assert_eq!(fetch_chunk_via(&peer, &h).unwrap(), data);
    }

    #[test]
    fn fetch_via_falls_back_to_tcp_when_udp_is_dead() {
        // UDP 声明了但实际不可达 —— 必须回退 TCP 而不是把错误抛出去。
        // 这是"UDP 是加速手段，不是新的失败点"的回归测试。
        use crate::peer::PeerInfo;
        let remote = Arc::new(ChunkStore::new());
        let data = b"fallback works".to_vec();
        let h = crate::chunk::hash_bytes(&data);
        remote.put(&h, data.clone());
        let tcp_addr = start_chunk_server(remote).unwrap();

        // 一个绑了就释放的 UDP 端口 —— 大概率无人接管。
        let dead_udp = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let a = s.local_addr().unwrap().to_string();
            drop(s);
            a
        };

        let peer = PeerInfo::new("half-broken", tcp_addr).with_udp(dead_udp);
        assert_eq!(
            fetch_chunk_via(&peer, &h).unwrap(),
            data,
            "dead UDP must fall back to the working TCP path"
        );
    }

    #[test]
    fn fetch_via_prefers_udp_when_available() {
        // UDP 可用时应该走 UDP。验证方式：TCP 地址故意指向一个没人监听的
        // 端口 —— 只有真的走了 UDP 才能成功。
        use crate::peer::PeerInfo;
        let store = Arc::new(ChunkStore::new());
        let data: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let h = crate::chunk::hash_bytes(&data);
        store.put(&h, data.clone());

        let (udp_addr, sock) = crate::udp::start_udp_server(Arc::clone(&store)).unwrap();
        std::mem::forget(sock);

        let dead_tcp = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let a = l.local_addr().unwrap().to_string();
            drop(l);
            a
        };

        let peer = PeerInfo::new("udp-only", dead_tcp).with_udp(udp_addr);
        assert_eq!(
            fetch_chunk_via(&peer, &h).unwrap(),
            data,
            "should have transferred over UDP (TCP address is dead)"
        );
    }
}
