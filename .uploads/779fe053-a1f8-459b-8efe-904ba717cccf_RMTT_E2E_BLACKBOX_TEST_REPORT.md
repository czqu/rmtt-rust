# RMTT 跨语言端到端黑盒测试报告

- 测试日期：2026-11-01
- 测试对象：`rmtt-go`、`rmtt-java`、`rmtt-rust`（均使用 **develop** 分支）
- 测试类型：黑盒、跨语言、端到端（真实进程 + 真实网络传输）
- 测试代码与原始数据：`/workspace/rmtt-blackbox/`
- 原始结果：[results.json](file:///workspace/rmtt-blackbox/results/results.json)、[results.txt](file:///workspace/rmtt-blackbox/results/results.txt)

## 1. 结论摘要

| 指标 | 数值 |
|---|---|
| 自动化用例总数 | **435** |
| 通过 | **416** |
| 失败 | **19**（其中 14 个由同一个 Java 8 KiB 帧上限导致） |
| 总体通过率 | **95.6%**（排除 8 KiB 限制后为 98.9%） |
| TCP 语言组合 | 4 服务端 × 4 客户端 = **16/16 全部执行** |
| 传输组合 | TCP/TLS/WS/WSS/KCP/QUIC 共 **87** 项，全部通过 |
| 协议级用例 | 4 个服务端 × 34 项 = **136** 项，131 通过 |
| 客户端行为用例 | 4 个客户端 × 9 项 = **36** 项，34 通过 |

**总体判断：** 三种实现在 TCP 上的核心协议（握手、鉴权、心跳、上下行 PUSH、会话接管、踢下线、重连）互操作良好，16 个语言组合全部完成了端到端消息往返；TLS/WS/WSS/KCP/QUIC 传输矩阵全部通过。主要问题集中在 **Java 实现的两个硬编码上限（8 KiB 消息、23 字节凭据）**，以及 **Java 服务端关闭时不下发 DISCONNECT(0x03)**、**Go 服务端对流水线 PUSH 不保序**、**Rust 服务端对超长帧头不及时拒绝** 等实现差异。

按严重级别统计：**P1 × 2、P2 × 4、P3 × 6**。

## 2. 被测版本与环境

| 仓库 | 分支 | develop 提交 |
|---|---|---|
| `czqu/rmtt-go` | develop | `0cd1bf0ff3439fe53118a55b82034c6831430d26` |
| `czqu/rmtt-java` | develop | `37c312f6be17c874d75f7dac948b68100fb7af3c` |
| `czqu/rmtt-rust` | develop | `d01f42b067c6463f5229d68182ccacc119bfa7f2` |

- OS：Ubuntu 24.04 LTS，x86_64
- Go 1.25.1；Rust 1.92.0；JDK 25.0.4（Temurin）；Maven 3.9.10；Python 3.14.8
- Java 依赖为本地 `mvn install` 的 develop 构建；Rust 通过 path 依赖引用 develop 源码；Go 直接引用 workspace 中的 develop 源码。
- TLS/WSS 使用自签证书（SAN: `localhost`、`127.0.0.1`），客户端按测试目的使用 trust-all / `insecure`，因此本报告验证的是 TLS 握手与帧传输，不验证 PKI 校验策略。

## 3. 测试方法

### 3.1 真实实现 + 独立线协议探针

为避免“用自己的库测自己”，测试分成三层：

1. **服务端协议一致性（Python 独立实现）**
   `py/wire.py` 独立实现 RMTT 定长头、varint 剩余长度、CONNECT/CONNACK/PUSH/PINGREQ/PINGRESP/DISCONNECT 六种报文；`py/suite_server.py` 直接以字节流驱动任意服务端，检查 CONNACK 字段、会话行为与 DISCONNECT 返回码。

2. **客户端行为（Python 脚本化服务端）**
   `py/fake_server.py` 是独立的脚本化 RMTT 服务端，可控制 CONNACK 返回码、server_kp、分片 CONNACK、下行 PUSH、主动 DISCONNECT、异常断链；`py/suite_client.py` 驱动真实客户端并检查其发出的 CONNECT 字段、PINGREQ 节奏、PUSH/DISCONNECT 行为及重连行为。

3. **跨语言端到端矩阵（真实服务端 + 真实客户端）**
   `py/run_matrix.py` 启动真实服务端进程（Go / Java Netty / Java AIO / Rust），再用真实客户端进程（同样四种）执行功能场景。

### 3.2 服务端控制通道

为了黑盒触发服务端 API（push / kick / online / shutdown），每个测试服务端额外监听一个 **仅本机可见的文本控制端口**，命令与 RMTT 协议无关：

```
PUSH <device> <text>     KICK <device> <code>
ONLINE <device>          COUNT
SHUTDOWN
```

### 3.3 覆盖矩阵

**TCP 语言矩阵（16 组合，全部执行）**

| 服务端 \ 客户端 | Go | Java Netty | Java AIO | Rust |
|---|---|---|---|---|
| Go | 11/11 | 10/11 | 10/11 | 11/11 |
| Java Netty | 10/11 | 10/11 | 10/11 | 10/11 |
| Java AIO | 10/11 | 10/11 | 10/11 | 10/11 |
| Rust | 11/11 | 10/11 | 10/11 | 11/11 |

每个组合执行 11 个场景：`connect_echo`、`binary_256`、`medium_4000`、`concurrent_5`、`large_1MiB`、`auth_reject`、`downlink`、`kick`、`session_takeover`、`keepalive_5s`、`auto_reconnect`。

**传输矩阵（87 项，全部通过）**

| 传输 | 服务端 | 客户端 | 组合数 | 结果 |
|---|---|---|---|---|
| TLS | Go、Java AIO | Go、Java AIO、Rust | 6 | 18/18 |
| WS | Go、Java Netty、Java AIO | Go、Java Netty、Java AIO | 9 | 27/27 |
| WSS | Go、Java Netty、Java AIO | Go、Java Netty、Java AIO | 9 | 27/27 |
| KCP | Go、Java Netty | Go、Java Netty | 4 | 12/12 |
| QUIC | Go | Go | 1 | 3/3 |

每个传输组合执行 `connect_echo`、`medium_4000`、`downlink`。

**服务端协议用例（每个服务端 34 项）**

分组覆盖：握手与鉴权（9）、PING（1）、PUSH 载荷（4）、流式解析（3）、会话与多设备（4）、服务端 API（4）、协议违例（4）、心跳（4）、服务端关闭（1）。

**客户端行为用例（每个客户端 9 项）**

CONNECT 字段、干净断开码、分片 CONNACK、鉴权拒绝、server_kp 驱动的 PING 节奏、server_kp=0 存活、1 MiB 下行、服务端主动 DISCONNECT、异常断链自动重连。

### 3.4 一个必要的测试约束

Java 服务端硬编码限制凭据 ≤ 23 字节（见问题 F2）。为了让矩阵中的其它功能能够测到 Java 实现，矩阵统一使用短凭据；凭据长度边界由独立的 23/24 字节协议用例验证。

## 4. 详细结果

### 4.1 跨语言 TCP 矩阵（176 项）

| 场景 | 通过/总数 | 说明 |
|---|---|---|
| connect_echo | 16/16 | 握手、上行 PUSH、服务端回显 |
| binary_256 | 16/16 | 0x00–0xFF 全字节载荷 |
| medium_4000 | 16/16 | 超过常见单段缓冲的载荷 |
| concurrent_5 | 16/16 | 5 条并发上行及回显 |
| **large_1MiB** | **4/16** | 仅 Go/Rust 参与的组合通过；任何一侧为 Java 均失败（F1） |
| auth_reject | 16/16 | CONNACK 0x03 与连接关闭 |
| downlink | 16/16 | 服务端主动下行 PUSH |
| kick | 16/16 | DISCONNECT 0x06 |
| session_takeover | 16/16 | 旧连接 DISCONNECT 0x02，新连接可用 |
| keepalive_5s | 16/16 | server_kp=5 下维持 6 秒并 PING |
| auto_reconnect | 16/16 | 杀掉服务端并重启后自动重连、恢复收发 |

### 4.2 服务端协议一致性（136 项）

| 服务端 | 通过/总数 | 失败用例 |
|---|---|---|
| Go | 33/34 | `stream.order_preserved`（F4） |
| Java Netty | 33/34 | `server.shutdown_code`（F3） |
| Java AIO | 32/34 | `server.shutdown_code`（F3）、`keepalive.ping_keeps_alive`（一次性抖动，复测 5/5 通过） |
| Rust | 33/34 | `protocol.oversized_length`（F6） |

四个服务端均正确通过的协议点包括：CONNECT magic、协议版本拒绝（0x01）、鉴权拒绝（0x03）、CONNACK server_kp 的上下限钳制/默认值/透传、PINGRESP、空/二进制/8000 字节回显、逐字节分片 CONNECT、多帧合并解析、会话接管 0x02、管理员踢下线 0x06、离线推送拒绝、在线状态、干净断开、20 个并发设备、未知类型 DISCONNECT 0x04、keepalive 超时 0x05、keepalive 禁用（server_kp=0）。超长帧行为存在差异，见 F6。

### 4.3 客户端行为（36 项）

| 客户端 | 通过/总数 | 失败用例 |
|---|---|---|
| Go | 9/9 | — |
| Java Netty | 8/9 | `downlink_1MiB`（F1） |
| Java AIO | 8/9 | `downlink_1MiB`（F1） |
| Rust | 9/9 | — |

其中值得强调的通过项：

- 四个客户端发出的 CONNECT 均为 magic `0x637a7175`、version 1、reserved 0，keepalive 提议与配置一致。
- 四个客户端都能在 `server_kp=2` 时把 PINGREQ 节奏调整到约 2 秒（实测 2.0–2.5 秒）。
- 四个客户端在 `server_kp=0` 时都不会因自身心跳超时而断开。
- 四个客户端都能处理逐字节下发的 CONNACK。
- 四个客户端都能在被服务端主动 DISCONNECT(0x06) 后结束会话（Go 的表现为连接状态变为断开，但不触发 `OnConnectionLost` 回调，见 F12）。
- 四个客户端都能在异常断链后重连（把 server_kp 调到 2 秒以缩短检测窗口后验证）。

### 4.4 传输矩阵

TLS、WS、WSS、KCP、QUIC 共 29 个服务端/客户端组合、87 个用例全部通过，包括跨语言方向（例如 Java AIO TLS 服务端 ↔ Rust TLS 客户端、Go WSS 服务端 ↔ Java Netty WSS 客户端、Java Netty KCP 服务端 ↔ Go KCP 客户端）。

Java QUIC 未纳入本次矩阵：Java 的 QUIC 依赖可选的 netty native quiche，本次 Java 构建使用 `-P!quic` 以避开平台原生库问题；Go 的 QUIC 已通过 Go↔Go 验证。

## 5. 缺陷与实现差异

严重级别：P1 阻断互操作；P2 功能/健壮性缺陷；P3 低风险或体验问题。

### F1（P1）Java 实现消息帧上限 8 KiB，与 Go/Rust 的 16 MiB 不兼容

- **现象**：payload 8191 字节（帧 8192）可正常收发；payload 8192 字节（帧 8193）立即失败。矩阵中所有涉及 Java 服务端或 Java 客户端的 1 MiB 用例共 12 个失败，客户端行为套件中 2 个 1 MiB 下行用例失败。
- **证据**：
  - `large_1MiB`：Go→Java Netty 客户端日志出现 `EVENT SENT len=1048576` 后连接关闭；Java AIO 明确记录 `protocol violation on decode: message too large: 1048577 bytes`。
  - Java Netty 客户端在超限后记录 `RmttDecoder - protocol violation on decode: unsupported packet type RESERVED`（真正的首个异常是 `message too large: 8193 bytes`，随后的 channelInactive 解码又产生了一条误导性日志）。
  - Java 常量：`DEFAULT_MAX_BYTES_IN_MESSAGE = 8192`。
- **影响**：任何跨语言的大消息（图片、批量遥测、固件分片）在 Java 侧不可用；Java 服务端也无法接收 Go/Rust 客户端的大 PUSH。
- **建议**：把最大帧大小做成服务端/客户端可配置项，并将默认值与其他实现对齐（16 MiB）；至少在文档中明确 8 KiB 限制。

### F2（P1）Java 服务端硬编码凭据 ≤ 23 字节，Go/Rust 支持到 65535 字节

- **现象**：23 字节凭据四个服务端均接受；24 字节凭据在 Java Netty / Java AIO 返回 CONNACK 0x03，在 Go/Rust 正常接受。
- **证据**：`handshake.credential_23` 全部 PASS；`handshake.credential_24`：Java 为 `connack=0x03`，Go/Rust 为 `connack=0x00`。Java 日志：`CONNECT rejected: credential too long (24 > max 23)`。
- **影响**：无法使用 JWT、OAuth token、带前缀的设备证书等常见凭据；当凭据超过 23 字节时，Java 与 Go/Rust 客户端均无法连上 Java 服务端。该限制也与 README 中“JWT / user-pass”的描述冲突。
- **建议**：将 `DEFAULT_MAX_CREDENTIAL_LENGTH` 改为可配置（默认至少 4 KiB），或在协议层统一约定并写入规范。

### F3（P2）Java 服务端关闭时不发送 DISCONNECT(0x03)

- **现象**：控制通道 `SHUTDOWN` 后，Go 与 Rust 服务端会向在线设备发送 DISCONNECT(0x03)；Java Netty 与 Java AIO 直接关闭 TCP，客户端只看到 `peer closed`。
- **证据**：`server.shutdown_code`：Go `disconnect=0x03`、Rust `disconnect=0x03`、Java Netty/AIO `no DISCONNECT(0x03): Closed('peer closed')`。
- **影响**：客户端无法区分“服务端计划内重启”和“网络故障”，无法执行优雅下线/延迟重连策略。
- **建议**：为 Java 服务端提供 `shutdown()`，先向所有会话写 DISCONNECT(0x03)，再关闭 listener 与连接。

### F4（P2）Go 服务端对流水线 PUSH 不保序

- **现象**：同一 TCP 段内连续发送 `co-1`、`co-2` 两个 PUSH，Go 服务端回显顺序为 `echo:co-2`、`echo:co-1`；Java Netty、Java AIO、Rust 均保持原序。
- **证据**：`stream.order_preserved` 仅 Go 失败，结果为 `pipelined order b'echo:co-2' then b'echo:co-1'`；`stream.coalesced_push` 通过（两条都收到，但顺序反了）。
- **根因**：Go 服务端对每个上行 PUSH 使用 `go s.options.MessageHandler(...)` 异步派发（`server/server.go`），同一连接的多个处理协程竞争执行。
- **影响**：依赖消息顺序的业务（指令序列、增量状态）会出现乱序；单条发送并等待的应用不受影响。
- **建议**：按连接串行派发，或为每个 device 维护有序队列；如坚持并发，应在文档中明确“不保证顺序”。

### F5（P2）Java AIO 客户端依赖心跳超时感知断链，TCP FIN 不立即触发重连

- **现象**：服务端进程被杀死后，Java AIO 客户端在 25 秒内没有任何 `connection lost` 日志，也没有发起重连；把 server_kp 调到 2 秒后，约 3 秒内完成重连并通过 `auto_reconnect` 用例。Java Netty 客户端会立即记录 `channel closed` 并重连。
- **影响**：默认 server_kp=30 时，故障切换最多可能延迟约 45 秒（1.5×server_kp）。
- **建议**：AIO 读回调在 `-1`/EOF 时立即关闭会话并触发重连，而不是只依赖心跳超时。

### F6（P2）Rust 服务端对超长帧头不做前置拒绝

- **现象**：客户端声明 `remaining length = 16 MiB + 1` 后只发送帧头，Rust 服务端 5 秒内既不应答也不关闭，socket 保持打开；Go 立即关闭，Java AIO 返回 DISCONNECT(0x04)，Java Netty 关闭连接。
- **证据**：`protocol.oversized_length`：Rust `no DISCONNECT/close within 5.0s (socket still open)`。
- **影响**：攻击者可用极小流量让服务端为每个连接预留最大 16 MiB 读缓冲，直到心跳超时（默认约 45 秒）；属于低带宽 DoS 面。
- **建议**：解析固定头后先校验 `remaining length <= max_frame`，超限立即 DISCONNECT(0x04) 并关闭。

### F7（P3）Go 服务端对“非零固定头 flags”直接 RST，未返回 DISCONNECT(0x04)

- **现象**：发送 `0xFF 0x00`（类型 15、flags 非 0）时，Go 服务端直接关闭并产生 `ConnectionResetError`；Java Netty/AIO、Rust 均返回 DISCONNECT(0x04)。对于“未知类型 + flags=0”，Go 能正确返回 0x04。
- **影响**：对端拿不到明确的协议违例原因，且 RST 可能丢弃尚未读取的数据。
- **建议**：把 flags 校验失败也纳入统一的协议违例处理路径，发送 DISCONNECT(0x04) 后再关闭。

### F8（P3）Go 客户端在 server_kp=1 秒时可能错过首个心跳窗口

- **现象**：服务端 keepalive 固定 1 秒时，连续 3 次测试有 2 次被服务端以 DISCONNECT(0x05) 回收；server_kp=5 秒时 16/16 组合通过。
- **根因**：`client/keepalive.go` 的检查周期为 `max(heartbeat/4, 1s)`，而发送条件是“距上次发送 ≥ heartbeat”。kp=1 时第一次 tick 在 1.0 秒，可能因微秒级差值不满足 `>=`，下一次 tick 已在 2 秒，超过服务端 1.5 秒窗口。
- **影响**：仅影响 server_kp 接近 1 秒的极端配置（服务端默认最小 30 秒），实际风险低。
- **建议**：检查周期下限降到 100–250 ms，或在 0.75×server_kp 时提前发送 PINGREQ。

### F9（P3）Rust 客户端 `disconnect()` 不保证 DISCONNECT(0x00) 已写出

- **现象**：客户端调用 `disconnect()` 后立即退出进程时，脚本化服务端未收到 DISCONNECT；在退出前等待 300 ms 后可稳定收到。
- **影响**：短生命周期进程可能表现为“异常断开”，服务端无法记录正常的 0x00 结束码。
- **建议**：让 `disconnect()` 同步等待 IO 线程写完（或提供 `disconnect_timeout`），并在文档中说明当前的异步语义。

### F10（P3）Java AIO 服务端心跳回收存在 ~1 秒粒度 overshoot

- **现象**：策略 1.5×server_kp 时，实测回收时间在 1.54–2.48 秒之间；检查任务每 1 秒运行一次，而判定条件是 `> 1.5×kp`。
- **影响**：回收延迟最多约 1 秒；不影响正确性。
- **建议**：将检查周期改为 `max(200ms, server_kp*1000/4)`。

### F11（P3）Java Netty 超限帧会额外打印误导性的 decoder 日志

- **现象**：1 MiB 下行时，真正的首个异常是 `message too large: 8193 bytes`，但连接关闭阶段又对残留字节执行一次解码，最后一条日志变成 `protocol violation on decode: unsupported packet type RESERVED`，容易误导排障。
- **影响**：仅日志可读性。
- **建议**：超限异常后丢弃/跳过剩余字节，或在 `channelInactive` 时不再重复解码。

### F12（P3）Go 客户端把 0x02/0x06 当作正常断开，不触发 OnConnectionLost

- **现象**：Go 客户端收到会话接管（0x02）或管理员踢下线（0x06）时走 `Disconnect(100)` 路径，连接状态变为断开，但注册的 `OnConnectionLost` 不会被调用；Java/Rust 客户端会记录/上报连接丢失。
- **根因**：`client/client.go` 的 `CloseConnect` 对 `DiscSessionTakenOver`、`DiscKickedByAdmin` 调用 `c.Disconnect(100)`，只有其它返回码才走 `internalConnLost`。
- **影响**：依赖 `OnConnectionLost` 更新 UI、清理会话或触发重连策略的应用，无法感知“被踢/被接管”；`IsConnected()` 仍会变为 false。
- **建议**：为管理类 DISCONNECT 提供显式的 reason 回调，或让 `OnConnectionLost` 携带 DISCONNECT 返回码。

### 关于一次性抖动的说明

Java AIO 的 `keepalive.ping_keeps_alive` 在 399 项全量运行中出现过 1 次 `recv timeout`；随后在独立环境连续复测 5 次全部通过，且回收时间在 1.54–2.48 秒之间。该次失败更可能是测试机负载/调度导致的一次性抖动，未列为独立缺陷，但与 F10 的 1 秒检查粒度有关。

## 6. 通过项与积极结论

- **握手与鉴权语义一致**：四个服务端对 magic、版本、鉴权拒绝、CONNACK 字段的处理完全一致；四个客户端都能正确处理分片 CONNACK 与拒绝码。
- **心跳协商一致**：client_kp 的 min/max 钳制、0 → default、allow-disable → 0 在四个服务端上均一致；静默连接在 1.5×server_kp 后被 DISCONNECT(0x05) 回收；持续 PING 的连接可长期存活。
- **会话管理一致**：会话接管 0x02、管理员踢下线 0x06、干净断开 0x00、在线状态、20 设备并发、多设备隔离全部通过。
- **传输实现成熟**：TLS/WSS/KCP/QUIC 的跨语言组合全部通过，说明各实现的传输抽象与帧编解码在真实网络下可互操作。
- **重连可用**：四个客户端都能在服务端重启后恢复连接并继续收发；Go/Rust 在 1 MiB 场景下表现最好。

## 7. 测试局限

1. Java QUIC 未验证（原生 quiche 依赖未纳入本次构建）；KCP/QUIC 未在丢包、延迟、抖动网络下测试。
2. TLS 使用 trust-all/insecure，未验证证书链、主机名校验、双向 TLS。
3. 未覆盖 IPv6、WebSocket 子协议协商失败、代理/防火墙、MTU 分片等网络边界。
4. 未做长时间稳定性/内存泄漏/吞吐压测；仅做了 6 秒级心跳存活与 20 设备并发。
5. 协议没有正式的规范文档，部分结论基于代码与 README；消息大小/凭据长度等“协议未规定项”正是本次跨语言差异的根源。
6. Java AIO 客户端断链感知的结论基于默认 30 秒 keepalive 下 25 秒观察窗口；理论上它会在 45 秒心跳超时后重连（未等到该时刻）。

## 8. 复现方式

```bash
# 1. develop 代码
git -C /workspace checkout develop
git clone https://github.com/czqu/rmtt-java.git /tmp/rmtt-java && git -C /tmp/rmtt-java checkout develop
git clone https://github.com/czqu/rmtt-rust.git /tmp/rmtt-rust && git -C /tmp/rmtt-rust checkout develop

# 2. 构建（Java 使用 -P!quic，避免原生 QUIC 依赖）
cd /tmp/rmtt-java && mvn -B install -DskipTests -Dmaven.javadoc.skip=true -Dsource.skip=true -P!quic
cd /workspace && go build -o /tmp/bb-bin/go-server ./rmtt-blackbox/go-server \
  && go build -o /tmp/bb-bin/go-client ./rmtt-blackbox/go-client
cd /workspace/rmtt-blackbox/rust-server && cargo build --release
cd /workspace/rmtt-blackbox/rust-client && cargo build --release
cd /workspace/rmtt-blackbox && for d in java-server-netty java-server-aio java-client-netty java-client-aio; do
  (cd $d && mvn -B package -DskipTests -Dmaven.javadoc.skip=true -Dsource.skip=true -P!quic); done

# 3. 全量矩阵（协议 + 16 组合 + 传输）
cd /workspace/rmtt-blackbox/py && python3 run_matrix.py --only all

# 4. 客户端行为套件
for i in go rust java-netty java-aio; do python3 suite_client.py --impl $i; done
```

## 9. 附：测试资产

| 路径 | 说明 |
|---|---|
| `rmtt-blackbox/py/wire.py` | 独立 RMTT 线协议实现 |
| `rmtt-blackbox/py/suite_server.py` | 服务端协议一致性套件（34 项/服务端） |
| `rmtt-blackbox/py/fake_server.py` | 脚本化 RMTT 服务端 |
| `rmtt-blackbox/py/suite_client.py` | 客户端行为套件（9 项/客户端） |
| `rmtt-blackbox/py/run_matrix.py` | 跨语言矩阵与传输矩阵编排器 |
| `rmtt-blackbox/go-server`、`go-client` | Go 测试服务端/客户端驱动 |
| `rmtt-blackbox/java-server-netty`、`java-server-aio` | Java 两个栈的测试服务端 |
| `rmtt-blackbox/java-client-netty`、`java-client-aio` | Java 两个栈的客户端驱动 |
| `rmtt-blackbox/rust-server`、`rust-client` | Rust 测试服务端/客户端驱动 |
| `rmtt-blackbox/results/results.json` | 399 项矩阵/协议/传输原始结果 |
| `rmtt-blackbox/results/results.txt` | 同上，纯文本摘要 |
| `rmtt-blackbox/results/client-suites.txt` | 4 个客户端 × 9 项行为套件原始输出 |
