//! NAT 打洞实测工具 —— 回答"这套东西在真实网络里到底行不行"。
//!
//! 跑法：
//!   1. `music --tracker`（或指向一台公网 Tracker：`punch_test --tracker <ip:port>`）
//!   2. 两台机器各跑一次 `punch_test`
//!   3. 两边都会打印自己的 peer_id；把对方的 peer_id 传给自己：
//!        `punch_test --peer <对方peer_id>`
//!
//! 它测四件事，其中第 3 条是**决定要不要引入真 STUN 的关键数据**：
//!   1. 本机 UDP 绑到了哪个端口
//!   2. Tracker 从 TCP 连接上观测到的源地址
//!   3. **两者的端口是否一致** —— 不一致就说明 NAT 对 TCP/UDP 分别映射，
//!      靠 Tracker 观测猜 UDP 端口不可靠，必须上真正的 STUN
//!   4. 对指定 peer 打洞能不能通

use std::sync::Arc;
use std::time::Duration;

use music_lib::chunk::ChunkStore;
use music_lib::holepunch::{self, PunchResult};
use music_lib::peer::{PeerDiscovery, PeerInfo, RemoteTracker};
use music_lib::tracker::{self, TrackerRequest, TrackerResponse};
use music_lib::udp;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tracker_addr = arg(&args, "--tracker").unwrap_or_else(|| tracker::TRACKER_ADDR.to_string());
    let want_peer = arg(&args, "--peer");

    let store = Arc::new(ChunkStore::new());

    // 1. 本机 UDP 端口
    let (udp_addr, sock) = match udp::start_udp_server(Arc::clone(&store)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ 无法绑定 UDP 端口: {e}");
            std::process::exit(1);
        }
    };
    let local_udp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
    println!("── NAT 打洞实测 ──");
    println!("1. 本机 UDP:        {udp_addr} (端口 {local_udp_port})");

    // 2. Tracker 观测到的地址
    let tracker = RemoteTracker::new(&tracker_addr);
    let observed = tracker.observed_addr();
    match &observed {
        Some(a) => println!("2. Tracker 观测到:  {a}  (TCP 源地址)"),
        None => {
            eprintln!("2. Tracker 观测到:  ❌ 连不上 Tracker {tracker_addr}");
            eprintln!("   先启动 Tracker：music --tracker");
            std::process::exit(1);
        }
    }

    // 3. 关键判断：TCP 观测端口 vs UDP 本地端口
    //
    // 注意这个判断在**同一台机器内网测试时没有意义**（没经过 NAT，端口当然
    // 不同但也不需要一致）。它只在真实跨 NAT 场景下有诊断价值。
    let observed_sa: Option<std::net::SocketAddr> =
        observed.as_ref().and_then(|a| a.parse().ok());
    let behind_nat = observed_sa
        .map(|a| !a.ip().is_loopback() && !is_private(&a.ip()))
        .unwrap_or(false);

    println!("3. NAT 判定:");
    if let Some(o) = observed_sa {
        if o.ip().is_loopback() {
            println!("   本机回环 —— 没有经过 NAT，此项无诊断意义。");
            println!("   要得到有用结果，需在两台**不同网络**的机器上跑，Tracker 在公网。");
        } else if !behind_nat {
            println!("   观测到内网地址 {o} —— 同局域网，无需打洞，直连即可。");
        } else {
            println!("   观测到公网地址 {o} —— 经过了 NAT。");
            println!("   本机 UDP 端口 {local_udp_port} vs 观测到的 TCP 端口 {}", o.port());
            println!("   ⚠️ 这两个数字无法直接比较（一个是内网端口，一个是 NAT 外部端口）。");
            println!("   真正要看的是下面的打洞结果：通了说明猜测有效，");
            println!("   反复失败说明 NAT 对 TCP/UDP 分开映射，需要真正的 STUN 或 TURN 中继。");
        }
    }

    // 注册自己，让对方能发现。同时宣告哨兵分片 —— 现有 Tracker 协议只有
    // "按分片查持有者"，没有"按 peer_id 查询"，所以测试节点靠一个约定的
    // 假分片哈希互相汇合。
    let peer_id = format!("punchtest-{}", local_udp_port);
    let mut me = PeerInfo::new(peer_id.clone(), format!("127.0.0.1:{local_udp_port}"));
    me.udp_addr = Some(udp_addr.clone());
    tracker.register(me, &[PUNCH_RENDEZVOUS.to_string()]);
    println!("4. 已注册 peer_id:  {peer_id}");

    // 4. 打洞
    let Some(target_id) = want_peer else {
        println!();
        println!("现在在另一台机器上跑：");
        println!("  punch_test --tracker {tracker_addr} --peer {peer_id}");
        println!();
        println!("本进程保持运行以响应打洞（60 秒）...");
        std::thread::sleep(Duration::from_secs(60));
        return;
    };

    println!();
    println!("── 向 {target_id} 打洞 ──");

    // 通过 Tracker 查对方地址。用 Find("") 拿不到，得列全部节点 ——
    // 现有协议没有"按 peer_id 查询"，这里用 find_peers 的空分片查询变通。
    let peers = lookup_peers(&tracker, &peer_id);
    let Some(target) = peers.iter().find(|p| p.peer_id == target_id) else {
        eprintln!("❌ Tracker 上找不到 peer {target_id}");
        eprintln!("   已知节点: {:?}", peers.iter().map(|p| &p.peer_id).collect::<Vec<_>>());
        std::process::exit(1);
    };

    let cands = holepunch::candidates(target);
    println!("候选地址: {cands:?}");
    if cands.is_empty() {
        eprintln!("❌ 对方没有 UDP 地址 —— 只能走 TCP。");
        std::process::exit(1);
    }

    // 用**另一个** socket 主动打洞，不能用分片服务那个 —— 它的主循环是
    // 唯一读者，在这里 recv 会跟主循环抢包。这也如实暴露了一个局限：
    // 本工具测的是"新 socket 的映射能否打通"，而真实分片传输走的是
    // transfer.rs 里同样新建的 socket，两者一致，所以结论有效。
    let punch_sock = match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ 无法绑定打洞 socket: {e}");
            std::process::exit(1);
        }
    };
    match holepunch::punch(&punch_sock, &cands) {
        PunchResult::Open { addr, rounds } => {
            println!("✅ 打通了！{addr}（第 {rounds} 轮）");
            println!("   → 这个 NAT 组合可以直连，不需要中继。");
        }
        PunchResult::Failed { tried, reason } => {
            println!("❌ 没打通: {reason}");
            println!("   尝试过: {tried:?}");
            println!("   → 可能是对称 NAT 或 CGNAT。这类情况需要 TURN 中继");
            println!("     （唯一需要花钱的部分，当前未实现）。会回退到 TCP 路径。");
        }
    }
}

/// 判断是否 RFC1918 内网地址。
fn is_private(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        // IPv6 唯一本地地址 fc00::/7
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00 || v6.is_loopback(),
    }
}

/// 打洞测试节点的汇合哈希。见 `lookup_peers`。
const PUNCH_RENDEZVOUS: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// 从 Tracker 拉已知的打洞测试节点。
fn lookup_peers(tracker: &RemoteTracker, peer_id: &str) -> Vec<PeerInfo> {
    // 先把自己挂到哨兵哈希上
    tracker.announce(peer_id, &[PUNCH_RENDEZVOUS.to_string()]);
    match tracker.request(&TrackerRequest::Find {
        chunk: PUNCH_RENDEZVOUS.to_string(),
    }) {
        Ok(TrackerResponse::Peers { peers }) => peers,
        _ => Vec::new(),
    }
}
