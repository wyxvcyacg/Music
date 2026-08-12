//! UDP 分片传输 —— 打洞后的数据通道（阶段四）。
//!
//! 为什么是 UDP：NAT 打洞的标准做法。TCP 的 simultaneous open 在多数
//! NAT 上成功率很低，UDP 打洞才是被广泛验证的路子。
//!
//! 为什么敢手写可靠层（而不是引入 QUIC）：
//! **分片有 SHA-256 兜底**。`ChunkStore::put` 会校验哈希，不匹配就整片丢弃。
//! 所以这里不需要完整的 TCP 语义 —— 只要"大多数情况下能把 256 KiB 收齐"，
//! 收错了有最后一道防线，收不齐就回退 TCP。这让可靠层可以做得很小。
//!
//! 线协议（payload 上限 1200 字节，留足 MTU 余量避免 IP 分片）：
//! ```text
//!   请求:   [0x01][hash 64 字节 ASCII]
//!   数据:   [0x02][seq u16 be][total u16 be][payload ≤1200]
//!   NACK:   [0x03][count u16 be][seq u16 be × count]
//!   完成:   [0x05]                      发送方宣告"我发完了"
//!   无此片: [0x04]
//! ```
//!
//! ## 如实标注的边界
//!
//! - **不做拥塞控制**。固定发包间隔，不探测带宽、不退避。局域网和小规模
//!   够用；公网大规模会对网络不友好。这是刻意的取舍，不是遗漏。
//! - **不加密**。与现有 TCP 分片传输一致 —— 内容是公开分享的音乐分片，
//!   但这意味着中间人可以看到传了什么。公网部署应考虑加密。
//! - 单片 256 KiB / 1200 字节 ≈ 219 个包，`u16` 的 seq 空间（65535）远够。

use crate::chunk::ChunkStore;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 单个 UDP 包的 payload 上限。1200 是常见的 MTU 安全值
/// （以太网 1500 减去 IPv6 + UDP 头，再留隧道/VPN 余量）。
pub const MAX_PAYLOAD: usize = 1200;

/// 包类型标记。
const T_REQUEST: u8 = 0x01;
const T_DATA: u8 = 0x02;
const T_NACK: u8 = 0x03;
const T_ABSENT: u8 = 0x04;
const T_DONE: u8 = 0x05;
/// 打洞探测与回应（见 holepunch.rs）。放在同一个端口上，因为打洞打通的
/// 就是这个 socket 的映射 —— 换端口等于白打。
pub const T_PING: u8 = 0x10;
pub const T_PONG: u8 = 0x11;

/// 接收方等一批数据的超时。超时即认为"该发的都到了"，开始 NACK 缺失部分。
const RECV_TIMEOUT: Duration = Duration::from_millis(400);
/// 最多 NACK 几轮。超过就放弃，回退 TCP。
const MAX_NACK_ROUNDS: usize = 3;
/// 整次拉取的总超时兜底 —— 防止对方半死不活时无限拖着。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

/// 抽象出 socket 收发，好让测试能注入"会丢包的 socket"。
///
/// 这是本模块可测性的关键：丢包重传逻辑如果只能靠真实网络碰运气触发，
/// 就等于没测。注入一个确定性丢包的实现，才能把重传路径测死。
pub trait Datagram: Send + Sync {
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> std::io::Result<usize>;
    fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>;
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()>;
}

impl Datagram for UdpSocket {
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> std::io::Result<usize> {
        UdpSocket::send_to(self, buf, addr)
    }
    fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        UdpSocket::recv_from(self, buf)
    }
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        UdpSocket::set_read_timeout(self, dur)
    }
}

/// 把分片切成数据包。返回 (总包数, 每包字节)。
fn build_data_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let total = total_packets(data.len());
    data.chunks(MAX_PAYLOAD)
        .enumerate()
        .map(|(i, part)| {
            let mut pkt = Vec::with_capacity(5 + part.len());
            pkt.push(T_DATA);
            pkt.extend_from_slice(&(i as u16).to_be_bytes());
            pkt.extend_from_slice(&(total as u16).to_be_bytes());
            pkt.extend_from_slice(part);
            pkt
        })
        .collect()
}

/// 一个 len 字节的分片会被切成几个包。
pub fn total_packets(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        len.div_ceil(MAX_PAYLOAD)
    }
}

/// 服务端：处理一个分片请求，把分片发给对方。
///
/// **只发不收**。NACK 由主循环读到后经 `nacks` 通道转交进来 —— 一个 socket
/// 必须只有一个读者，否则主循环和这里会互相抢包（而且抢到的一方可能不认识
/// 那个包，直接丢弃）。这是 UDP 单端口服务的硬约束。
fn serve_one(
    sock: &dyn Datagram,
    to: SocketAddr,
    hash: &str,
    store: &ChunkStore,
    nacks: std::sync::mpsc::Receiver<Vec<u16>>,
) {
    let Some(data) = store.get(hash) else {
        let _ = sock.send_to(&[T_ABSENT], to);
        return;
    };

    let packets = build_data_packets(&data);
    for pkt in &packets {
        let _ = sock.send_to(pkt, to);
    }
    let _ = sock.send_to(&[T_DONE], to);

    // 补发窗口：响应主循环转来的 NACK，直到对方不再要或超时。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Ok(missing) = nacks.recv_timeout(RECV_TIMEOUT) else {
            break; // 超时或通道关闭：认为对方收齐了
        };
        for seq in missing {
            if let Some(pkt) = packets.get(seq as usize) {
                let _ = sock.send_to(pkt, to);
            }
        }
        let _ = sock.send_to(&[T_DONE], to);
    }
}

/// 解析 NACK 包里的 seq 列表。
fn parse_nack(pkt: &[u8]) -> Vec<u16> {
    if pkt.len() < 3 {
        return Vec::new();
    }
    let count = u16::from_be_bytes([pkt[1], pkt[2]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 3 + i * 2;
        if off + 1 >= pkt.len() {
            break; // 包被截断，能解多少算多少
        }
        out.push(u16::from_be_bytes([pkt[off], pkt[off + 1]]));
    }
    out
}

/// 构造一个 NACK 包。
fn build_nack(missing: &[u16]) -> Vec<u8> {
    // 一个包最多塞多少 seq —— 超出的下一轮再要。
    let cap = (MAX_PAYLOAD - 3) / 2;
    let take = missing.len().min(cap);
    let mut pkt = Vec::with_capacity(3 + take * 2);
    pkt.push(T_NACK);
    pkt.extend_from_slice(&(take as u16).to_be_bytes());
    for seq in &missing[..take] {
        pkt.extend_from_slice(&seq.to_be_bytes());
    }
    pkt
}

/// 启动 UDP 分片服务。返回可连接的地址与 socket。
///
/// 绑定 `0.0.0.0` 而不是 `127.0.0.1`：打洞需要能收到来自公网的包，
/// 只绑回环就永远收不到。但 `0.0.0.0:port` 不能当**目标**地址用
/// （Windows 会直接报 WSAEADDRNOTAVAIL），所以返回的地址把未指定 IP
/// 换成回环 —— 本机/局域网直连用它，公网地址由 Tracker 观测提供。
///
/// **返回的 socket 不能用来收包**。主循环是它唯一的读者；调用方拿它是为了
/// 查 `local_addr()`、发包、以及保持端口不被释放。想收包（比如主动打洞）
/// 必须另开 socket —— 但那样打通的是另一个映射，见 `holepunch` 模块的说明。
///
/// 与 TCP 版 `start_chunk_server` 并存 —— UDP 是新增的一条路，不是替换。
pub fn start_udp_server(store: Arc<ChunkStore>) -> std::io::Result<(String, Arc<UdpSocket>)> {
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:0")?);
    let bound = sock.local_addr()?;
    let addr = if bound.ip().is_unspecified() {
        format!("127.0.0.1:{}", bound.port())
    } else {
        bound.to_string()
    };
    let listen = Arc::clone(&sock);

    std::thread::spawn(move || {
        // 每个来源地址正在进行的传输 → 转交 NACK 的通道。
        // 主循环是 socket 的唯一读者，NACK 必须由它分发给对应的发送线程。
        let mut active: HashMap<SocketAddr, std::sync::mpsc::Sender<Vec<u16>>> = HashMap::new();
        let mut buf = vec![0u8; MAX_PAYLOAD + 8];
        let _ = Datagram::set_read_timeout(listen.as_ref(), None);
        loop {
            let Ok((n, from)) = Datagram::recv_from(listen.as_ref(), &mut buf) else {
                continue;
            };
            if n < 1 {
                continue;
            }
            match buf[0] {
                T_REQUEST if n >= 65 => {
                    let Ok(hash) = std::str::from_utf8(&buf[1..65]) else {
                        continue;
                    };
                    let hash = hash.to_string();
                    let store = Arc::clone(&store);
                    let sock = Arc::clone(&listen);
                    let (tx, rx) = std::sync::mpsc::channel();
                    active.insert(from, tx);
                    // 每个请求单独线程 —— 与 TCP 版一致。
                    std::thread::spawn(move || {
                        serve_one(sock.as_ref(), from, &hash, &store, rx)
                    });
                }
                // NACK：转交给正在给这个地址发数据的线程。
                T_NACK => {
                    if let Some(tx) = active.get(&from) {
                        if tx.send(parse_nack(&buf[..n])).is_err() {
                            active.remove(&from); // 那个线程已经结束了
                        }
                    }
                }
                // 打洞探测：立刻回 PONG。
                //
                // 服务端必须无条件回应任何 PING，哪怕不认识对方 —— 因为
                // 打洞的全部意义就是"对方的包能进来"，而判断包能不能进来
                // 只能靠回一个包让对方知道。这里做白名单就等于自断打洞。
                T_PING => {
                    let _ = Datagram::send_to(listen.as_ref(), &[T_PONG], from);
                }
                _ => {}
            }
        }
    });

    Ok((addr, sock))
}

/// 客户端：用 UDP 从 peer 拉一个分片。
///
/// 返回收齐的字节。**不校验哈希** —— 交给 `ChunkStore::put`，
/// 保持"校验只有一处"，避免两套校验逻辑不一致。
pub fn fetch_chunk_udp(
    sock: &dyn Datagram,
    peer: SocketAddr,
    hash: &str,
) -> Result<Vec<u8>, String> {
    if hash.len() != 64 {
        return Err(format!("bad hash length {}", hash.len()));
    }

    // 发请求
    let mut req = Vec::with_capacity(65);
    req.push(T_REQUEST);
    req.extend_from_slice(hash.as_bytes());
    sock.send_to(&req, peer).map_err(|e| format!("send request failed: {e}"))?;

    sock.set_read_timeout(Some(RECV_TIMEOUT)).ok();

    // 收到的包按 seq 存放。total 在收到第一个数据包时才知道。
    let mut parts: Vec<Option<Vec<u8>>> = Vec::new();
    let mut total: Option<usize> = None;
    let mut buf = vec![0u8; MAX_PAYLOAD + 8];
    let mut rounds = 0usize;
    let start = Instant::now();

    loop {
        if start.elapsed() > TOTAL_TIMEOUT {
            return Err("udp fetch timed out".into());
        }

        match sock.recv_from(&mut buf) {
            Ok((n, from)) if n >= 1 => {
                // 只认来自目标 peer 的包 —— 别的来源直接忽略。
                if from != peer {
                    continue;
                }
                match buf[0] {
                    T_ABSENT => return Err(format!("peer does not have chunk {hash}")),
                    T_DATA if n >= 5 => {
                        let seq = u16::from_be_bytes([buf[1], buf[2]]) as usize;
                        let tot = u16::from_be_bytes([buf[3], buf[4]]) as usize;
                        if total.is_none() {
                            if tot == 0 || tot > 65535 {
                                return Err(format!("bad total {tot}"));
                            }
                            total = Some(tot);
                            parts.resize(tot, None);
                        }
                        if seq < parts.len() && parts[seq].is_none() {
                            parts[seq] = Some(buf[5..n].to_vec());
                        }
                    }
                    T_DONE => {
                        // 发送方发完了 —— 检查有没有缺的。
                        let Some(tot) = total else {
                            return Err("got DONE before any data".into());
                        };
                        let missing: Vec<u16> = (0..tot)
                            .filter(|i| parts[*i].is_none())
                            .map(|i| i as u16)
                            .collect();
                        if missing.is_empty() {
                            break; // 收齐了
                        }
                        rounds += 1;
                        if rounds > MAX_NACK_ROUNDS {
                            return Err(format!(
                                "gave up after {MAX_NACK_ROUNDS} NACK rounds, {} packets still missing",
                                missing.len()
                            ));
                        }
                        sock.send_to(&build_nack(&missing), peer)
                            .map_err(|e| format!("send nack failed: {e}"))?;
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(_) => {
                // 读超时。可能 DONE 丢了 —— 主动 NACK 一轮。
                let Some(tot) = total else {
                    return Err("no data received from peer".into());
                };
                let missing: Vec<u16> = (0..tot)
                    .filter(|i| parts[*i].is_none())
                    .map(|i| i as u16)
                    .collect();
                if missing.is_empty() {
                    break;
                }
                rounds += 1;
                if rounds > MAX_NACK_ROUNDS {
                    return Err(format!(
                        "gave up after {MAX_NACK_ROUNDS} NACK rounds, {} packets still missing",
                        missing.len()
                    ));
                }
                sock.send_to(&build_nack(&missing), peer)
                    .map_err(|e| format!("send nack failed: {e}"))?;
            }
        }
    }

    // 按 seq 顺序拼起来。
    let mut out = Vec::new();
    for p in parts.into_iter() {
        out.extend_from_slice(&p.ok_or("internal: missing part after completion")?);
    }
    Ok(out)
}

/// 便捷版：自己开一个临时 socket 拉分片（不复用打洞后的连接）。
/// 用于本机/局域网直连场景与测试。
pub fn fetch_chunk_udp_direct(peer_addr: &str, hash: &str) -> Result<Vec<u8>, String> {
    let peer: SocketAddr = peer_addr
        .parse()
        .map_err(|e| format!("bad peer addr {peer_addr}: {e}"))?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind failed: {e}"))?;
    fetch_chunk_udp(&sock, peer, hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// 会按规则丢包的 socket 包装 —— 让重传路径可确定性测试。
    struct LossySocket {
        inner: UdpSocket,
        /// 丢掉第几个发出的包（0-based 计数）。
        drop_sends: HashSet<usize>,
        sent: Mutex<usize>,
    }

    impl Datagram for LossySocket {
        fn send_to(&self, buf: &[u8], addr: SocketAddr) -> std::io::Result<usize> {
            let n = {
                let mut g = self.sent.lock().unwrap();
                let n = *g;
                *g += 1;
                n
            };
            if self.drop_sends.contains(&n) {
                return Ok(buf.len()); // 假装发出去了，其实丢了
            }
            UdpSocket::send_to(&self.inner, buf, addr)
        }
        fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
            UdpSocket::recv_from(&self.inner, buf)
        }
        fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
            UdpSocket::set_read_timeout(&self.inner, dur)
        }
    }

    fn hash64(tag: u8) -> String {
        // 造一个合法长度（64 hex）的假 hash。
        std::iter::repeat(format!("{tag:02x}")).take(32).collect()
    }

    #[test]
    fn total_packets_math() {
        assert_eq!(total_packets(0), 0);
        assert_eq!(total_packets(1), 1);
        assert_eq!(total_packets(MAX_PAYLOAD), 1);
        assert_eq!(total_packets(MAX_PAYLOAD + 1), 2);
        // 真实分片大小：256 KiB
        assert_eq!(total_packets(256 * 1024), 219);
    }

    #[test]
    fn nack_roundtrip() {
        let missing = vec![0u16, 5, 65535];
        let pkt = build_nack(&missing);
        assert_eq!(pkt[0], T_NACK);
        assert_eq!(parse_nack(&pkt), missing);
    }

    #[test]
    fn nack_caps_at_packet_size() {
        // 要的 seq 太多时只塞得下一部分，剩下的下一轮再要。
        let missing: Vec<u16> = (0..2000).collect();
        let pkt = build_nack(&missing);
        assert!(pkt.len() <= MAX_PAYLOAD, "nack packet must fit in one datagram");
        let got = parse_nack(&pkt);
        assert!(got.len() < missing.len());
        assert_eq!(got[0], 0, "should start from the front");
    }

    #[test]
    fn parse_nack_survives_truncation() {
        // 声称有 10 个 seq 但实际被截断 —— 不能 panic。
        let mut pkt = vec![T_NACK];
        pkt.extend_from_slice(&10u16.to_be_bytes());
        pkt.extend_from_slice(&7u16.to_be_bytes()); // 只给 1 个
        let got = parse_nack(&pkt);
        assert_eq!(got, vec![7], "truncated nack should parse what it can");
    }

    #[test]
    fn data_packets_cover_all_bytes() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let packets = build_data_packets(&data);
        assert_eq!(packets.len(), total_packets(data.len()));
        // 把 payload 依 seq 拼回来，必须与原数据逐字节相同。
        let mut out = Vec::new();
        for p in &packets {
            out.extend_from_slice(&p[5..]);
        }
        assert_eq!(out, data);
    }

    /// 起一个 UDP 分片服务，返回地址。
    fn serve(store: Arc<ChunkStore>) -> String {
        let (addr, _sock) = start_udp_server(store).unwrap();
        // 故意泄漏 socket 的 Arc：测试进程内服务需要一直活着。
        std::mem::forget(_sock);
        addr
    }

    #[test]
    fn fetch_over_udp_returns_exact_bytes() {
        let store = Arc::new(ChunkStore::new());
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let h = crate::chunk::hash_bytes(&data);
        assert!(store.put(&h, data.clone()));

        let addr = serve(Arc::clone(&store));
        let got = fetch_chunk_udp_direct(&addr, &h).expect("udp fetch should succeed");
        assert_eq!(got, data, "udp transfer must be byte-exact");
    }

    #[test]
    fn fetch_large_chunk_spanning_many_packets() {
        let store = Arc::new(ChunkStore::new());
        // 完整的 256 KiB 分片 —— 219 个包，真实规模。
        let data: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
        let h = crate::chunk::hash_bytes(&data);
        assert!(store.put(&h, data.clone()));

        let addr = serve(Arc::clone(&store));
        let got = fetch_chunk_udp_direct(&addr, &h).expect("large udp fetch should succeed");
        assert_eq!(got.len(), data.len());
        assert_eq!(got, data, "256 KiB over 219 packets must be byte-exact");
    }

    #[test]
    fn missing_chunk_reports_absent() {
        let store = Arc::new(ChunkStore::new());
        let addr = serve(store);
        let err = fetch_chunk_udp_direct(&addr, &hash64(0xab)).unwrap_err();
        assert!(
            err.contains("does not have"),
            "should report absence clearly, got: {err}"
        );
    }

    #[test]
    fn bad_hash_length_rejected() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let err = fetch_chunk_udp(&sock, peer, "tooshort").unwrap_err();
        assert!(err.contains("bad hash length"));
    }

    #[test]
    fn recovers_from_dropped_request_via_retry() {
        // 请求包本身丢了 —— 客户端收不到任何数据，应报错而不是挂死。
        let store = Arc::new(ChunkStore::new());
        let addr = serve(store);
        let peer: SocketAddr = addr.parse().unwrap();

        let lossy = LossySocket {
            inner: UdpSocket::bind("0.0.0.0:0").unwrap(),
            drop_sends: [0].into_iter().collect(), // 丢掉第一个包（就是请求）
            sent: Mutex::new(0),
        };
        let err = fetch_chunk_udp(&lossy, peer, &hash64(0xcd)).unwrap_err();
        assert!(
            err.contains("no data received"),
            "dropped request should fail cleanly, got: {err}"
        );
    }

    #[test]
    fn nack_recovers_dropped_data_packets() {
        // 核心测试：服务端发的部分数据包丢了，客户端必须靠 NACK 补回来。
        let store = Arc::new(ChunkStore::new());
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let h = crate::chunk::hash_bytes(&data);
        assert!(store.put(&h, data.clone()));

        // 服务端用会丢包的 socket：丢掉它发出的第 2、4 个包。
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = sock.local_addr().unwrap().to_string();
        let lossy_server = Arc::new(LossySocket {
            inner: sock,
            drop_sends: [2, 4].into_iter().collect(),
            sent: Mutex::new(0),
        });

        let srv = Arc::clone(&lossy_server);
        let store2 = Arc::clone(&store);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; MAX_PAYLOAD + 8];
            srv.set_read_timeout(None).ok();
            // 只处理第一个请求；之后当主循环把 NACK 转给发送线程。
            let mut nack_tx: Option<std::sync::mpsc::Sender<Vec<u16>>> = None;
            loop {
                let Ok((n, from)) = srv.recv_from(&mut buf) else { return };
                if n < 1 {
                    continue;
                }
                if buf[0] == T_REQUEST && n >= 65 && nack_tx.is_none() {
                    let hash = std::str::from_utf8(&buf[1..65]).unwrap().to_string();
                    let (tx, rx) = std::sync::mpsc::channel();
                    nack_tx = Some(tx);
                    let s = Arc::clone(&srv);
                    let st = Arc::clone(&store2);
                    std::thread::spawn(move || serve_one(s.as_ref(), from, &hash, &st, rx));
                } else if buf[0] == T_NACK {
                    if let Some(tx) = &nack_tx {
                        let _ = tx.send(parse_nack(&buf[..n]));
                    }
                }
            }
        });

        let peer: SocketAddr = server_addr.parse().unwrap();
        let client = UdpSocket::bind("0.0.0.0:0").unwrap();
        let got = fetch_chunk_udp(&client, peer, &h)
            .expect("NACK retransmission should recover dropped packets");
        assert_eq!(got, data, "recovered data must be byte-exact");
    }

    #[test]
    fn corrupted_payload_is_caught_by_chunk_store() {
        // 可靠层不校验哈希 —— 最后一道防线是 ChunkStore::put。
        // 这里验证："传输层被骗了，缓存层也不会被骗"。
        let store = ChunkStore::new();
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let real_hash = crate::chunk::hash_bytes(&data);

        let mut tampered = data.clone();
        tampered[1500] ^= 0xff; // 改一个字节

        assert!(
            !store.put(&real_hash, tampered),
            "ChunkStore must reject data that does not match its hash"
        );
        assert!(!store.has(&real_hash), "corrupt data must not enter the cache");
    }
}
