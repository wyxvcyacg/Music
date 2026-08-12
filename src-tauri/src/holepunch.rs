//! UDP 打洞 —— 让两台都在 NAT 后面的机器直连（阶段四）。
//!
//! ## 原理（为什么这能行）
//!
//! NAT 的规则是"只放行我先发出去过的目标回来的包"。所以 A 和 B 同时向
//! 对方发包：A 的包到 B 的 NAT 时被丢弃（B 还没发过给 A），但它已经在
//! A 的 NAT 上打出了一个"A→B 已发过"的映射；B 的包随后到达 A 的 NAT 时
//! 就能匹配上这个映射被放行。反之亦然。第一批包必然有一方被丢，
//! **所以必须重试**，而不是发一次就判定失败。
//!
//! ## 为什么需要 Tracker 参与
//!
//! A 和 B 都不知道对方的公网地址（自己都不知道自己的）。必须有一个双方
//! 都能连上的第三方来交换地址 —— 这就是信令。Tracker 已经是这个角色，
//! 不需要新服务器。
//!
//! ## 如实标注的边界
//!
//! - **不是所有 NAT 都能打通**。对称 NAT（symmetric NAT）为每个目标分配
//!   不同的外部端口，Tracker 观测到的端口对打洞无效。这类情况只能靠
//!   TURN 中继，而中继是**唯一要花钱**的部分，本阶段不做 —— 打不通就
//!   回退到已有的 TCP 路径（局域网内仍然可用）。
//! - **国内家宽普遍 CGNAT**，打洞成功率显著低于国外统计数字。这是现实
//!   约束，不是实现缺陷。
//! - Tracker 观测到的是 **TCP** 源地址，而打洞用 **UDP** 映射。很多 NAT
//!   对两种协议分别映射，两者可能不同。`punch_test` 就是用来实测这一点的；
//!   如果实测发现不一致，才需要引入真正的 STUN。

use crate::peer::PeerInfo;
use crate::udp::{Datagram, T_PING, T_PONG};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// 每轮探测之间的间隔。
const PUNCH_INTERVAL: Duration = Duration::from_millis(200);
/// 最多探测几轮。第一批包注定有一方被丢，所以下限必须 > 1。
const PUNCH_ROUNDS: usize = 10;

/// 打洞结果。
#[derive(Debug, Clone, PartialEq)]
pub enum PunchResult {
    /// 打通了，`addr` 是确认可达的对方地址（后续分片请求发到这里）。
    Open { addr: SocketAddr, rounds: usize },
    /// 没打通。`tried` 是尝试过的候选地址，用于诊断。
    Failed { tried: Vec<SocketAddr>, reason: String },
}

impl PunchResult {
    pub fn is_open(&self) -> bool {
        matches!(self, PunchResult::Open { .. })
    }
    /// 打通后的地址；没打通返回 None。
    pub fn addr(&self) -> Option<SocketAddr> {
        match self {
            PunchResult::Open { addr, .. } => Some(*addr),
            PunchResult::Failed { .. } => None,
        }
    }
}

/// 从 PeerInfo 里挑出所有值得尝试的 UDP 候选地址。
///
/// 顺序很重要：先试同网段/本机地址（快且不需要打洞），再试公网观测地址。
/// 一个 peer 可能同时给出两者（比如局域网邻居也注册到了公网 Tracker）,
/// 局域网直连显然更优。
///
/// 去重：两个字段可能给出同一个地址，重复探测纯属浪费。
pub fn candidates(peer: &PeerInfo) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    let mut push = |s: &str| {
        if let Ok(a) = s.parse::<SocketAddr>() {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    };
    if let Some(u) = &peer.udp_addr {
        push(u);
    }
    // 公网观测地址：Tracker 看到的 TCP 源 IP + 对方自报的 UDP 端口。
    //
    // 为什么这样拼：观测地址的**端口**是 TCP 的，对 UDP 无意义；但 **IP**
    // 是可信且协议无关的。所以取观测的 IP，配上对方自报的 UDP 端口 ——
    // 这是"不引入真 STUN"能做到的最好猜测。猜错就打不通，回退 TCP。
    if let (Some(pubaddr), Some(udp)) = (&peer.public_addr, &peer.udp_addr) {
        if let (Ok(p), Ok(u)) = (pubaddr.parse::<SocketAddr>(), udp.parse::<SocketAddr>()) {
            let guess = SocketAddr::new(p.ip(), u.port());
            if !out.contains(&guess) {
                out.push(guess);
            }
        }
    }
    out
}

/// 向一组候选地址打洞，返回第一个回应 PONG 的地址。
///
/// 每轮向**所有**候选地址各发一个 PING，然后短暂收包。不串行逐个试：
/// 打洞讲究双方同时发包，串行会错过时间窗口。
pub fn punch(sock: &dyn Datagram, targets: &[SocketAddr]) -> PunchResult {
    if targets.is_empty() {
        return PunchResult::Failed {
            tried: Vec::new(),
            reason: "no UDP candidate addresses for this peer".into(),
        };
    }

    sock.set_read_timeout(Some(PUNCH_INTERVAL)).ok();
    let mut buf = [0u8; 64];

    for round in 1..=PUNCH_ROUNDS {
        for t in targets {
            let _ = sock.send_to(&[T_PING], *t);
        }

        // 收包直到本轮时间用尽 —— 期间可能收到 PING（对方也在打）或 PONG。
        let until = Instant::now() + PUNCH_INTERVAL;
        while Instant::now() < until {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if n >= 1 => {
                    if !targets.contains(&from) {
                        continue; // 不是我们在打的对象
                    }
                    match buf[0] {
                        // 对方回应了 —— 洞通了。
                        T_PONG => {
                            return PunchResult::Open { addr: from, rounds: round }
                        }
                        // 对方的 PING 到了，说明入向已通。回 PONG 让对方也确认，
                        // 然后我们也认为通了 —— 收到对方的包本身就是最强证据。
                        T_PING => {
                            let _ = sock.send_to(&[T_PONG], from);
                            return PunchResult::Open { addr: from, rounds: round };
                        }
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(_) => break, // 读超时，进入下一轮
            }
        }
    }

    PunchResult::Failed {
        tried: targets.to_vec(),
        reason: format!("no response after {PUNCH_ROUNDS} rounds"),
    }
}

/// 便捷版：对一个 peer 打洞。
pub fn punch_peer(sock: &dyn Datagram, peer: &PeerInfo) -> PunchResult {
    punch(sock, &candidates(peer))
}

/// 只回应、不主动打的一方（被动侧）在后台跑这个。
///
/// 实际上 `udp::start_udp_server` 已经无条件回应 PING 了，所以正常路径
/// 不需要单独调这个函数 —— 它存在是为了测试和不带分片服务的纯打洞场景。
pub fn respond_to_pings(sock: &UdpSocket, duration: Duration) {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let deadline = Instant::now() + duration;
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n >= 1 && buf[0] == T_PING {
                let _ = sock.send_to(&[T_PONG], from);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_with(udp: Option<&str>, public: Option<&str>) -> PeerInfo {
        let mut p = PeerInfo::new("P", "127.0.0.1:1");
        p.udp_addr = udp.map(String::from);
        p.public_addr = public.map(String::from);
        p
    }

    #[test]
    fn candidates_empty_when_peer_has_no_udp() {
        // 老节点（没有 udp_addr）不该产生任何候选 —— 直接回退 TCP。
        let p = peer_with(None, None);
        assert!(candidates(&p).is_empty());
        // 只有公网地址但没 UDP 端口，也没法猜端口。
        let p = peer_with(None, Some("203.0.113.9:443"));
        assert!(candidates(&p).is_empty());
    }

    #[test]
    fn candidates_prefer_direct_before_public() {
        let p = peer_with(Some("192.168.1.5:40000"), Some("203.0.113.9:12345"));
        let c = candidates(&p);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].to_string(), "192.168.1.5:40000", "direct address first");
        // 公网候选 = 观测到的 IP + 自报的 UDP 端口（不是观测到的 TCP 端口）。
        assert_eq!(c[1].to_string(), "203.0.113.9:40000");
    }

    #[test]
    fn candidates_deduplicate() {
        // udp_addr 和拼出来的公网候选相同时不该探测两遍。
        let p = peer_with(Some("203.0.113.9:40000"), Some("203.0.113.9:55555"));
        assert_eq!(candidates(&p).len(), 1);
    }

    #[test]
    fn candidates_skip_unparseable() {
        let p = peer_with(Some("not-an-address"), Some("also bad"));
        assert!(candidates(&p).is_empty());
    }

    #[test]
    fn punch_with_no_candidates_fails_fast() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = punch(&sock, &[]);
        assert!(!r.is_open());
        match r {
            PunchResult::Failed { reason, .. } => assert!(reason.contains("no UDP candidate")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn punch_succeeds_against_a_responder() {
        let responder = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = responder.local_addr().unwrap();
        std::thread::spawn(move || {
            respond_to_pings(&responder, Duration::from_secs(3));
        });

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = punch(&sock, &[target]);
        assert!(r.is_open(), "punch against a live responder should open: {r:?}");
        assert_eq!(r.addr(), Some(target));
    }

    #[test]
    fn punch_finds_the_live_candidate_among_dead_ones() {
        // 真实场景：候选里有猜错的地址。不能因为第一个不通就放弃。
        let responder = UdpSocket::bind("127.0.0.1:0").unwrap();
        let live = responder.local_addr().unwrap();
        std::thread::spawn(move || {
            respond_to_pings(&responder, Duration::from_secs(3));
        });

        // 一个没人监听的端口（绑了又立刻释放，大概率无人接管）。
        let dead: SocketAddr = {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            let a = s.local_addr().unwrap();
            drop(s);
            a
        };

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = punch(&sock, &[dead, live]);
        assert_eq!(r.addr(), Some(live), "should find the reachable candidate");
    }

    #[test]
    fn punch_against_dead_address_gives_up_and_reports() {
        let dead: SocketAddr = {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            let a = s.local_addr().unwrap();
            drop(s);
            a
        };
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = punch(&sock, &[dead]);
        assert!(!r.is_open(), "unreachable address must not report open");
        match r {
            PunchResult::Failed { tried, reason } => {
                assert_eq!(tried, vec![dead], "should report what was tried");
                assert!(reason.contains("no response"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn simultaneous_punch_both_sides_open() {
        // 双方同时主动打 —— 这是真实打洞的形态，两边都该判定成功。
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();

        let hb = std::thread::spawn(move || punch(&b, &[a_addr]));
        let ra = punch(&a, &[b_addr]);
        let rb = hb.join().unwrap();

        assert!(ra.is_open(), "side A should open: {ra:?}");
        assert!(rb.is_open(), "side B should open: {rb:?}");
    }

    #[test]
    fn chunk_server_answers_pings_on_its_own_socket() {
        // 关键性质：打洞打通的必须是**分片服务那个** socket 的映射。
        // 如果分片服务不回 PING，就得另开端口打洞，而另一个端口的映射
        // 对分片传输毫无用处。
        let store = std::sync::Arc::new(crate::chunk::ChunkStore::new());
        let (addr, sock) = crate::udp::start_udp_server(store).unwrap();
        std::mem::forget(sock);

        let target: SocketAddr = addr.parse().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = punch(&client, &[target]);
        assert!(r.is_open(), "chunk server socket must answer punches: {r:?}");
        assert_eq!(r.addr(), Some(target));
    }
}
