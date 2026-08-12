//! Tracker 服务 —— 独立的节点发现服务（阶段二）。
//!
//! `music.exe --tracker` 进入此模式：监听固定端口，所有客户端连它做
//! 节点发现与曲目发布。内部复用阶段一的 InMemoryTracker 存节点/分片索引，
//! 额外维护一张"已发布曲目清单"表供客户端浏览共享曲库。
//!
//! 协议：一问一答的短连接，请求与响应都是单行 JSON + '\n'。

use crate::accounts::Accounts;
use crate::chunk::TrackManifest;
use crate::peer::{InMemoryTracker, PeerDiscovery, PeerInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Tracker 默认监听地址。
pub const TRACKER_ADDR: &str = "127.0.0.1:9000";

/// 客户端 → Tracker 的请求。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TrackerRequest {
    /// 节点上线：注册地址 + 已持有分片。
    Register { peer: PeerInfo, chunks: Vec<String> },
    /// 宣告新持有的分片。
    Announce { peer_id: String, chunks: Vec<String> },
    /// 查询谁持有某分片。
    Find { chunk: String },
    /// 发布一首曲目（连同清单），让别的节点能发现并下载。
    /// 需要有效 token —— 发布是唯一需要登录的操作（阶段三）。
    Publish {
        manifest: TrackManifest,
        title: String,
        artist: String,
        #[serde(default)]
        mime: String,
        #[serde(default)]
        token: String,
    },
    /// 列出所有已发布曲目。
    List,
    /// 按 track_hash 查询单个共享曲目（用于"粘贴链接→播放"流程）。
    Get { track_hash: String },
    /// 节点下线。
    Unregister { peer_id: String },
    /// 注册新账号（阶段三）。成功即登录，返回 token。
    RegisterAccount { username: String, password: String },
    /// 登录，返回 token。
    Login { username: String, password: String },
    /// 登出，吊销 token。
    Logout { token: String },
    /// 用 token 查询当前身份。
    #[serde(rename = "whoami")]
    WhoAmI { token: String },
    /// 问 Tracker："你看到我的地址是什么？"—— 最简地址发现（阶段四）。
    ///
    /// 不需要注册也能问，因为它就是为了在注册前先搞清楚自己的公网地址。
    WhereAmI,
}

/// Tracker → 客户端的响应。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackerResponse {
    Ok,
    Peers { peers: Vec<PeerInfo> },
    Manifests { items: Vec<SharedTrack> },
    Track { item: Option<SharedTrack> },
    /// 登录/注册成功。
    Auth { token: String, username: String },
    /// whoami 结果；未登录或 token 失效时 username 为 None。
    Identity { username: Option<String> },
    /// Tracker 观测到的请求方地址（地址发现，阶段四）。
    ///
    /// **这是 TCP 连接的源地址**。打洞用的是 UDP 映射，很多 NAT 对两种协议
    /// 分开映射 —— 两者可能不同。见 docs/nat-plan.md。
    Observed { addr: Option<String> },
    Error { message: String },
}

/// 共享曲库里的一条曲目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedTrack {
    pub manifest: TrackManifest,
    pub title: String,
    pub artist: String,
    /// 媒体 MIME 类型（如 audio/mpeg），供流式播放设置 Content-Type。
    #[serde(default)]
    pub mime: String,
    /// 发布者用户名（阶段三）。老数据可能没有，故 default。
    #[serde(default)]
    pub publisher: String,
}

/// 已发布曲目表：track_hash -> SharedTrack。
type ManifestStore = Arc<Mutex<HashMap<String, SharedTrack>>>;

/// 处理单个请求，返回响应。
///
/// `observed` 是 Tracker 从 TCP 连接上看到的请求方源地址 —— 地址发现的依据。
/// 客户端**不能**自己声明公网地址（会被覆盖）：自报的地址不可信，而连接的
/// 源地址是传输层给的事实。
fn handle(
    req: TrackerRequest,
    tracker: &InMemoryTracker,
    manifests: &ManifestStore,
    accounts: &Accounts,
    observed: Option<std::net::SocketAddr>,
) -> TrackerResponse {
    match req {
        TrackerRequest::Register { mut peer, chunks } => {
            // 用观测到的地址覆盖客户端自报的 public_addr。
            peer.public_addr = observed.map(|a| a.to_string());
            tracker.register(peer, &chunks);
            TrackerResponse::Ok
        }
        TrackerRequest::Announce { peer_id, chunks } => {
            tracker.announce(&peer_id, &chunks);
            TrackerResponse::Ok
        }
        TrackerRequest::Find { chunk } => TrackerResponse::Peers {
            peers: tracker.find_peers(&chunk),
        },
        TrackerRequest::Publish { manifest, title, artist, mime, token } => {
            // 发布需要登录 —— 这是唯一鉴权的操作。浏览/下载/播放保持开放，
            // 以维持 P2P "人越多越流畅"的性质。
            let Some(publisher) = accounts.verify(&token) else {
                return TrackerResponse::Error {
                    message: "发布需要先登录".into(),
                };
            };
            manifests.lock().unwrap().insert(
                manifest.track_hash.clone(),
                SharedTrack { manifest, title, artist, mime, publisher },
            );
            TrackerResponse::Ok
        }
        TrackerRequest::List => TrackerResponse::Manifests {
            items: manifests.lock().unwrap().values().cloned().collect(),
        },
        TrackerRequest::Get { track_hash } => TrackerResponse::Track {
            item: manifests.lock().unwrap().get(&track_hash).cloned(),
        },
        TrackerRequest::Unregister { peer_id } => {
            tracker.unregister(&peer_id);
            TrackerResponse::Ok
        }
        TrackerRequest::RegisterAccount { username, password } => {
            match accounts.register(&username, &password) {
                Ok(auth) => TrackerResponse::Auth {
                    token: auth.token,
                    username: auth.username,
                },
                Err(message) => TrackerResponse::Error { message },
            }
        }
        TrackerRequest::Login { username, password } => {
            match accounts.login(&username, &password) {
                Ok(auth) => TrackerResponse::Auth {
                    token: auth.token,
                    username: auth.username,
                },
                Err(message) => TrackerResponse::Error { message },
            }
        }
        TrackerRequest::Logout { token } => {
            accounts.logout(&token);
            TrackerResponse::Ok
        }
        TrackerRequest::WhoAmI { token } => TrackerResponse::Identity {
            username: accounts.verify(&token),
        },
        TrackerRequest::WhereAmI => TrackerResponse::Observed {
            addr: observed.map(|a| a.to_string()),
        },
    }
}

/// 处理一个客户端连接：读一行 JSON 请求，回一行 JSON 响应。
fn serve_conn(
    stream: TcpStream,
    tracker: Arc<InMemoryTracker>,
    manifests: ManifestStore,
    accounts: Arc<Accounts>,
) {
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }

    let resp = match serde_json::from_str::<TrackerRequest>(line.trim()) {
        Ok(req) => handle(req, &tracker, &manifests, &accounts, peer),
        Err(e) => TrackerResponse::Error {
            message: format!("bad request: {e}"),
        },
    };

    if let Ok(mut json) = serde_json::to_string(&resp) {
        json.push('\n');
        let _ = writer.write_all(json.as_bytes());
        let _ = writer.flush();
    }
}

/// 阻塞运行 Tracker 服务（`--tracker` 模式的入口）。
///
/// `data_dir` 用于持久化账号；None 表示账号只存在内存（进程退出即丢）。
pub fn run_tracker(addr: &str) -> std::io::Result<()> {
    run_tracker_with_data_dir(addr, None)
}

/// 同 `run_tracker`，但可指定账号持久化目录。
pub fn run_tracker_with_data_dir(
    addr: &str,
    data_dir: Option<std::path::PathBuf>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("[tracker] listening on {addr}");

    let tracker = Arc::new(InMemoryTracker::new());
    let manifests: ManifestStore = Arc::new(Mutex::new(HashMap::new()));
    let accounts = Arc::new(match &data_dir {
        Some(dir) => {
            let path = dir.join(crate::accounts::ACCOUNTS_FILE);
            let a = Accounts::open(&path);
            println!(
                "[tracker] accounts: {} ({} registered)",
                path.display(),
                a.account_count()
            );
            a
        }
        None => {
            println!("[tracker] accounts: in-memory (not persisted)");
            Accounts::new()
        }
    });

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let tracker = Arc::clone(&tracker);
                let manifests = Arc::clone(&manifests);
                let accounts = Arc::clone(&accounts);
                std::thread::spawn(move || serve_conn(s, tracker, manifests, accounts));
            }
            Err(e) => eprintln!("[tracker] accept error: {e}"),
        }
    }
    Ok(())
}
