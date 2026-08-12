# NAT 穿透 —— 阶段四（第一步：本机可测的部分）

## 目标与非目标

**目标**：把"打洞"这件事的**全部可在本机验证的逻辑**写完并测透 —— 地址发现、
信令交换、UDP 可靠传输、打洞状态机。做完之后,你用「家里 WiFi + 手机热点」
就能验证最后那一下洞到底通不通,不需要买任何机器。

**非目标**（明确不做）：
- TURN 中转 —— 唯一真烧钱的部分,留到确认打洞成功率之后再说。
- 真 STUN 客户端（RFC 5389）—— 见下面"已知的一个诚实缺口"。
- 替换现有 TCP 分片传输 —— UDP 是**新增的一条路**,不是替换。

## 两个已定的决策

| | 选择 | 理由 |
|---|---|---|
| 传输层 | **UDP + 手写可靠层** | 打洞的标准做法;分片有 SHA-256 兜底,不需要完整 TCP 语义 |
| 地址发现 | **Tracker 自己观测** | `tracker.rs:169` 已经拿到 `stream.peer_addr()`,零新代码零外部依赖 |

## 已知的一个诚实缺口（必须写进文档）

选了"Tracker 观测"就必然有这个问题：

> **Tracker 观测到的是 TCP 连接的映射,而打洞用的是 UDP 映射。
> 很多 NAT 对两种协议分开映射,两者可能不同。**

所以第一步拿到的地址**可能是错的**。我的处理方式：

1. 把地址发现放在一个 trait 后面（`AddrDiscovery`),TCP 观测只是第一个实现。
2. 真机测试时**同时打印两个地址**（Tracker 观测到的 TCP 源地址 + 本地 UDP socket
   实际绑定的端口),如果真机上发现不一致,就知道必须上真 STUN。
3. 文档里明写这个缺口,不假装已经解决。

这样第一步零成本、本机可测,而"要不要上真 STUN"这个判断**交给真机数据**,
不靠猜。铁律之二（依赖抽象接口）在这里第二次派上用场。

## 为什么改动能这么小

关键发现：整个代码库里**只有一处**决定了"用什么协议拉分片" ——

```rust
// transfer.rs:169，在 fetch_chunks_parallel 的 worker 里
match fetch_chunk(&p.addr, &hash) {
```

`fetch_chunks_parallel` / `stream.rs` / `lib.rs` 全都只认 `PeerInfo`,不关心底层
协议。所以 UDP 可以从这一个点接进去,**并行拉取、Range 流式、缓存淘汰、
曲库全都不用动**。这和阶段三"`peer.rs` 零改动"是同一个红利。

## 实施步骤

### 1. `PeerInfo` 增加 UDP 地址字段（`peer.rs`）

```rust
pub struct PeerInfo {
    pub peer_id: String,
    pub addr: String,              // 现有：TCP 分片服务地址
    #[serde(default)]
    pub udp_addr: Option<String>,  // 新增：UDP 打洞地址
    #[serde(default)]
    pub public_addr: Option<String>, // 新增：Tracker 观测到的公网地址
}
```

用 `Option` + `#[serde(default)]`：**旧节点和新节点能互通**,老的 JSON 也能解析。
仍然没有用户字段 —— 铁律之一继续成立。

### 2. Tracker 回报观测地址（`tracker.rs`）

把 `serve_conn` 里那个 `let _ = peer;` 真正用起来：`Register` 时把观测到的
源地址填进 `PeerInfo.public_addr`,并在响应里告诉客户端"我看到你是谁"。

新增 `TrackerResponse::Observed { addr }`。

### 3. UDP 可靠传输层（新文件 `udp.rs`，约 350 行含测试）

线协议（每片 256 KiB ≈ 220 个包，MTU 安全值 1200 字节 payload）：

```
请求:   [0x01][hash 64 bytes]
数据:   [0x02][seq u16][total u16][payload ≤1200]
NACK:   [0x03][count u16][seq u16 × count]
无此片: [0x04]
```

可靠性策略（刻意做简单，边界写进文档）：
- 接收方按 `seq` 填进定长 buffer，超时后 NACK 缺失的 seq，发送方补发。
- 重试 3 轮后放弃 → 回退到现有 TCP 路径。
- **不做拥塞控制**（固定发包间隔）—— 局域网/小规模够用，公网大规模会不友好，
  这一点必须在文档里如实标注，不能假装是完整实现。
- 收齐后交给 `ChunkStore::put`，**SHA-256 校验是最后一道防线**：
  哪怕可靠层有 bug 导致数据错乱，坏分片也进不了缓存。

### 4. 打洞状态机（新文件 `holepunch.rs`，约 250 行含测试）

```
A 想连 B：
  1. A 向 Tracker 发 RequestPunch { target: B }
  2. Tracker 把 A 的 public_addr 转给 B（信令）
  3. A、B 同时向对方 public_addr 发探测包
  4. 谁先收到对方的包 → 回 ACK → 洞通了
  5. 超时（3 秒）→ 失败，回退 TCP
```

Tracker 新增 `RequestPunch` / `PunchOffer` 消息。信令**只转发几十字节**，
不碰数据 —— 这也是为什么将来买最便宜的机器就够。

### 5. 接进现有传输（`transfer.rs`，改动约 30 行）

`fetch_chunk` 变成一个**策略选择**：

```rust
pub fn fetch_chunk_via(peer: &PeerInfo, hash: &str) -> Result<Vec<u8>, String> {
    // 1. 有 udp_addr 且洞已通 → 走 UDP
    // 2. 否则 → 走现有 TCP（本机/局域网场景不变）
}
```

现有 `fetch_chunk(addr, hash)` **签名保留** —— 17 个已有测试继续当回归网用。

### 6. 真机验证脚本（`src/bin/punch_test.rs`）

一个独立小程序，打印：
- 本地 UDP socket 绑定的端口
- Tracker 观测到的 TCP 源地址
- **两者是否一致**（这是判断要不要上真 STUN 的关键数据）
- 打洞尝试结果

这样你在「家里 WiFi + 手机热点」两端各跑一次就能拿到结论。

## 测试计划

本机能测的（进 `cargo test`）：
- UDP 可靠层：丢包重传、乱序重组、NACK 正确性、超时放弃
- **故意丢包**：注入一个丢掉 30% 包的假 socket，验证最终仍能收齐
- **故意损坏**：改掉一个字节，验证 `ChunkStore::put` 拒绝入库
- 打洞状态机：超时、双方同时发起、对方不在线
- 地址发现：Tracker 观测到的地址正确回报
- 回归：现有 49 个测试全部不动

本机**测不出来**的（必须真机）：
- 洞到底能不能打通
- TCP 映射与 UDP 映射是否一致

后者我会在 `punch_test` 里明确输出，不靠猜。

## 风险与如实标注

写进 `docs/architecture.md` 的边界：
1. TCP 观测地址 ≠ UDP 映射地址，第一步可能不准（有 trait 可换真 STUN）。
2. 无拥塞控制，不适合公网大规模。
3. 无 TURN，打洞失败就退回"两边直连不了就传不了"。
4. 中国大陆 CGNAT 环境下成功率预期偏低 —— 这是现实约束，不是 bug。

## 交付后你能做什么

```
一台机器连家里 WiFi，另一台开手机热点（走 4G/5G）
两边各跑 punch_test → 看洞通不通、看两个地址是否一致
```

一分钱不花，拿到真实数据。数据说通 → 再考虑买最便宜的机器做信令；
数据说不通 → 知道是 CGNAT 还是地址不准，再决定上真 STUN 还是上 TURN。

---

# 实施结果（2026-08-12 完成）

计划全部落地。以下是**与计划不同的地方**和**实测发现的问题**，如实记录。

## 与计划的偏差

### 1. 没有新增 `RequestPunch` / `PunchOffer` 信令消息

计划里第 4 步要给 Tracker 加两条信令消息。实现时发现**不需要**：

现有的 `Find { chunk }` 返回的 `Vec<PeerInfo>` 已经带上了 `udp_addr` 和
`public_addr`（第 1 步加的字段）。拉分片的一方本来就要先 `Find` 找持有者，
拿到结果时地址已经在手上了 —— 再加一轮信令是多余的往返。

代价是**没有"通知对方也开始打洞"的机制**：只有主动拉分片的一方在打。
被动方靠 `start_udp_server` 无条件回应 PING 来配合。这在"一方在 NAT 后、
另一方可直连"时够用；**双方都在对称 NAT 后时不够** —— 但那种情况本来
就需要 TURN，不是加信令能解决的。

### 2. 没有引入 `AddrDiscovery` trait

计划说把地址发现放到 trait 后面以便将来换真 STUN。实际只有
`RemoteTracker::observed_addr()` 一个函数、一个调用点，加 trait 是
为想象中的第二个实现付预付款。**真要换 STUN 时改这一个函数即可**，
那时再抽象。

### 3. 公网候选地址是"拼"出来的

计划没说清楚怎么用观测到的地址。实际做法（`holepunch::candidates`）：

```
公网候选 = Tracker 观测到的 IP  +  对方自报的 UDP 端口
```

因为观测到的**端口**是 TCP 的、对 UDP 无意义，但 **IP** 是协议无关且可信的。
这是"不引入真 STUN"能做到的最好猜测。猜错就打不通，回退 TCP。

## 实测发现的一个真 bug（已修）

第一次跑 `punch_test` 时**卡死**。原因：

> 一个 UDP socket 上有了两个读者。`start_udp_server` 的主循环在 `recv_from`，
> 而 `serve_one`（补发线程）和 `punch`（打洞）也想在同一个 socket 上收包。
> 三方互相抢包，抢到的一方不认识那个包就直接丢弃 —— NACK 永远送不到
> 补发线程，PONG 永远送不到打洞方。

**UDP 单端口服务的硬约束：一个 socket 只能有一个读者。** 修法：

- 主循环成为唯一读者，读到 NACK 时经 `mpsc` 通道转交给对应的发送线程
  （`active: HashMap<SocketAddr, Sender<Vec<u16>>>`）。
- 主动打洞另开 socket（`transfer.rs::udp_attempt`），打通后**在同一个
  新 socket 上拉分片** —— 打洞打通的是这个 socket 的映射，换 socket 白打。

修完后本机实测：`✅ 打通了！127.0.0.1:54330（第 1 轮）`。

这个 bug 单元测试**测不出来**：测试里每个 socket 都只有一个读者。
只有把 `punch_test` 真跑起来才暴露。

## 四条如实标注的边界

1. **Tracker 观测到的是 TCP 源地址，打洞用 UDP 映射。** 很多 NAT 对两种
   协议分开映射。当前用"观测 IP + 自报 UDP 端口"猜，猜错就回退 TCP。
   `punch_test` 会输出实际结果 —— 真机反复失败就说明得上真 STUN。
2. **不做拥塞控制。** 固定发包间隔，不探测带宽、不退避。局域网和小规模
   够用；公网大规模会对网络不友好。刻意取舍，不是遗漏。
3. **不加密。** 与现有 TCP 分片传输一致。内容是公开分享的音乐分片，
   但中间人能看到传了什么。公网部署应考虑加密。
4. **没有 TURN 中继。** 对称 NAT / CGNAT 打不通就回退 TCP —— 也就是
   "局域网内还能用，跨 NAT 就传不了"。中继是唯一要花钱的部分。
   **中国大陆家宽普遍 CGNAT，打洞成功率预期显著低于国外统计数字。**
   这会削弱"人越多越流畅"的卖点，是现实约束不是 bug。

## UDP 是加速手段，不是新的失败点

`fetch_chunk_via` 的核心不变量：**UDP 失败总是回退 TCP，绝不把 UDP 的
错误抛给调用方。** 三个回归测试锁死这一点：

| 测试 | 锁住的性质 |
|---|---|
| `fetch_via_uses_tcp_when_peer_has_no_udp` | 老节点（无 `udp_addr`）照常工作 |
| `fetch_via_falls_back_to_tcp_when_udp_is_dead` | UDP 声明了但不可达 → 回退成功 |
| `fetch_via_prefers_udp_when_available` | TCP 地址故意填死端口 → 只有真走 UDP 才过 |

第三个测试是关键：它证明 UDP 路径**真的在被使用**，而不是永远静默回退。

