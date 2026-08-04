# Music — P2P 流媒体音乐播放器

一款面向 PC 端的桌面音乐播放软件，采用 **P2P 流媒体传输**技术实现"边下边播"。灵感来自当年凭借 P2P 流媒体火爆一时的快播（QVOD）—— 将其点对点分发、多源加速的思路应用到音乐场景，让用户在播放的同时互相分发数据，人越多、播放越流畅。

## 核心理念

传统音乐播放依赖中心服务器逐个分发，带宽成本高、热门资源易拥堵。本项目借鉴快播的 P2P 流媒体模型：

- **边下边播**：无需等待整首下载完成，缓冲到可播放区间即开始播放。
- **多源传输**：同一首曲目可同时从多个节点（Peer）拉取不同分片，聚合带宽。
- **节点互助**：每个客户端既是消费者也是分发者，在线用户越多，整体分发能力越强。
- **本地缓存复用**：已下载分片进入本地缓存，供他人拉取，同时避免自己重复下载。

## 目标特性

- [x] P2P 网络层：节点发现、连接管理（NAT 穿透待做）
- [x] 分片传输协议：曲目切片、分片索引、多源调度与去重
- [x] 流式播放引擎：边下边播（`stream://` 协议 + HTTP Range 按需拉分片）
- [x] 音频解码与播放：由 WebView `<audio>` 原生解码（MP3 / FLAC / AAC / OGG 等）
- [x] 本地资源库：曲库管理、缓存复用
- [ ] 桌面 UI：播放控制、进度/缓冲可视化、传输状态（进行中）
- [x] 种子/资源标识：基于内容哈希的曲目寻址（SHA-256 分片定位）

## 技术架构（规划）

```
┌───────────────────────────────────────────────┐
│                   桌面客户端                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────────┐  │
│  │  播放 UI  │  │ 播放引擎  │  │  本地曲库/缓存 │  │
│  └────┬─────┘  └────┬─────┘  └──────┬───────┘  │
│       │             │               │          │
│  ┌────┴─────────────┴───────────────┴───────┐  │
│  │            流式调度层 (Streaming)          │  │
│  │   缓冲控制 · 分片请求 · 播放/下载协调        │  │
│  └────────────────────┬──────────────────────┘  │
│  ┌────────────────────┴──────────────────────┐  │
│  │              P2P 网络层 (Peer)             │  │
│  │  节点发现 · 分片交换 · NAT 穿透 · 带宽调度   │  │
│  └────────────────────┬──────────────────────┘  │
└────────────────────────┼────────────────────────┘
                         │
              ┌──────────┴──────────┐
              │  Tracker / DHT 网络  │
              │   节点索引与资源定位   │
              └─────────────────────┘
```

## 技术栈

- **桌面框架**：Tauri 2（Rust 后端 + WebView 前端）
- **前端**：React 19 + Vite + TypeScript + Tailwind v4 + shadcn/ui
- **P2P 核心**（Rust，标准库 `std::net` + `std::thread`，无额外运行时）：
  - `chunk.rs` — 内容寻址的分片存储（256 KiB 切片、SHA-256、去重、重组、区间读取）
  - `peer.rs` — `PeerDiscovery` 抽象 + `InMemoryTracker` / `RemoteTracker`
  - `tracker.rs` — 独立 Tracker 服务（TCP，JSON 协议）
  - `transfer.rs` — 节点间分片直传（TCP，二进制协议）
  - `stream.rs` — `stream://` 流式播放协议（HTTP Range 按需拉分片，边下边播）

## 快速开始

前置：[Node.js](https://nodejs.org/)、[Rust](https://rustup.rs/)、Windows 上需
"Microsoft C++ Build Tools"。

```bash
git clone https://github.com/wyxvcyacg/Music.git
cd Music
npm install
npm run tauri dev        # 启动桌面客户端（开发模式）
```

### 体验 P2P

阶段二/流式已在真机验证通过（Win11，2026-08）。三种粒度的体验：

**A. 单实例验全链路（最简）**

```bash
# 终端 1：Tracker（节点发现，默认监听 127.0.0.1:9000）
cd src-tauri
cargo run -- --tracker

# 终端 2：桌面客户端
cd D:\Music
npm run tauri dev
```

客户端 A：
1. 导入一首本地 mp3 → 自动开始播放
2. 这一行点「发布」→ 状态变为绿色"已发布"
3. 切到「节点网络」→ 看到刚发布的曲目 → 点「播放」
4. 播放条下方"已缓冲"灰色条从左到右渐进填满（这就是边下边播）

**B. 双实例真 P2P**

再开一个终端跑 `npm run tauri dev` 启第二个客户端 B：
1. B 切到「节点网络」→ 看到 A 发布的曲目 → 点「播放」
2. B 通过 `stream://` 协议按需从 A 进程拉分片（侧栏"本地分片"会从 0 涨到 N）
3. 拖动播放条到任意位置 → 浏览器发新 Range → 继续从新位置起播
4. 也可点「下载」完整收藏到本地

**C. 命令行裸测（无 GUI）**

`stream_server` 是给真机验证用的独立 HTTP 端点，模拟 stream:// 协议：
```bash
cd src-tauri
cargo run --bin stream_server -- --import some.mp3 --port 9100
# 记下打印的 track_hash，然后用 curl 验 Range：
curl -v -H "Range: bytes=0-99" http://127.0.0.1:9100/<hash>
```

### 常见问题

- **`error: failed to remove file ... music.exe 拒绝访问`**：后台残留进程占着产物。
  杀干净再跑：
  ```bash
  cmd //c "taskkill /F /IM music.exe /T"
  cmd //c "taskkill /F /IM stream_server.exe /T"
  ```
- **`could not determine which binary to run`**：项目里有 `music` 和 `stream_server` 两个 binary。
  `Cargo.toml` 已设 `default-run = "music"`，但 `cargo run --bin stream_server ...` 仍可显式指定。
- **侧栏 Tracker 一直显示"离线"**：终端 1 没起或端口 9000 被占。

### 测试

```bash
cd src-tauri && cargo test    # 13 个单元测试：分片重组、哈希校验、节点索引、分片直传、Range 协议、跨片读取、206/404/416
```


## 合规声明

本项目仅用于 P2P 流媒体传输技术的学习与研究。请仅传输你拥有合法版权或已获授权的音乐资源，遵守当地法律法规与版权规定。

## 许可证

待定。
