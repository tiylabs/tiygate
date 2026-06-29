# 部署与运维

这里收纳 TiyGate 的部署模式、运行时配置与生产运维说明。

## 部署模式

`tiygate` 二进制支持三种模式(通过 `--mode` / 环境变量 / 配置文件选择):

| 模式 | 进程内容 | 适用场景 |
|---|---|---|
| `all` | 数据面 + 控制面 + DB 单进程 | 本地开发、单节点、小团队 |
| `proxy` | 仅数据面(无状态、可水平扩展) | 生产数据面 |
| `admin` | 仅控制面(Admin API + WebUI) | 生产控制面 |

健康探针默认就绪:

- `GET /healthz` —— liveness,即使在 draining 期间也返回 200(避免被强杀)
- `GET /readyz` —— readiness,进入 draining 后返回 503(让 LB 摘流量)

### 管理控制台(WebUI)

在 `all` / `admin` 模式下,二进制会在 **`/admin/ui`** 提供内嵌的 React 控制台(如 `http://localhost:8080/admin/ui`)。它覆盖完整控制面 —— 供应商、路由、API 密钥(一次性 secret + 配额编辑与实时用量)、OAuth 授权码流程、运行时设置(路由、入口、上游、header 转发、payload 归档、后台任务)—— 以及分析:按模型 / 供应商 / API 密钥的统计、熔断器状态、请求日志下钻与回放、审计记录,支持中英文双语。

鉴权复用单一的 `TIYGATE_ADMIN_TOKEN`:在登录页粘贴(经 Admin API 校验后存于浏览器)。UI 通过 `rust-embed`(opt-in 的 `webui` feature)编译进二进制,因此前端必须先于 Rust crate 构建 —— 运行 `scripts/build-with-webui.sh`,或先 `cd webui && npm install && npm run build` 再 `cargo build -p tiygate-server --features webui`。开发细节见 `webui/README.md`。

## 运维

### 优雅排空

发送 `SIGTERM`(或 K8s `preStop`)后,网关:

1. 将 `/readyz` 翻转为 `503`,让负载均衡摘除该副本
2. 对**新请求**返回 `503 + Retry-After`
3. **放行存量请求**(含长 SSE 流式)自然结束
4. 超过 `drain_timeout`(默认 30s,应 ≥ 单请求 `deadline`)时,对仍未结束的流发送**协议原生 error 帧**,并跑 `UsageAccumulator` 兜底计费,防止账单漂移。流式路径实现在 `crates/server/src/ingress.rs::drive_upstream_stream` —— 它同时叠加 120s idle 计时(可在管理控制台 Upstream 设置中调节)、可选用 total 总时长预算(默认关闭)以及 30s 周期 SSE keepalive(`SseKeepaliveStream`),防止中间代理对长时间静默的流做隐式断连
5. flush 日志 channel、归还资源、退出

### 配置

TiyGate 配置分为两层:

**1. 启动时环境变量** —— 进程启动时读取一次,修改需重启:

| 变量 | 默认 | 用途 |
| --- | --- | --- |
| `TIYGATE_LISTEN_ADDR` | `0.0.0.0:3000` | HTTP server 监听地址。 |
| `TIYGATE_MODE` | `all` | 部署模式。`all`(数据面+控制面同进程)、`proxy`(纯数据面)、`admin`(纯控制面)。 |
| `TIYGATE_DATABASE_URL` | 未设置 | 数据库连接串(SQLite 或 Postgres)。未设置时回退到内存 ConfigStore(无 Admin API)。 |
| `TIYGATE_ADMIN_TOKEN` | 未设置 | Admin API 要求的 bearer 鉴权 token。未设置时 Admin API 请求会被拒绝。 |
| `TIYGATE_MASTER_KEY` | 未设置 | AES-256-GCM 主密钥,用于静态加密 provider key、OAuth token、S3 凭证。接受 64 位 hex 或标准 base64。未设置时 secret 明文存储(服务会打印 warning,仅适合本地开发)。 |
| `TIYGATE_REDIS_URL` | 未设置 | 设置后(且以 `redis-quota` feature 编译),配额计数器通过 Redis 跨副本共享,替代单副本内存计数。 |
| `RUST_LOG` | `info` | `tracing` / `tracing-subscriber` 过滤器。示例:`info`、`tiygate=debug`、`tiygate_server::ingress=trace`。 |

**2. 运行时可调设置** —— 通过管理控制台 **`/admin/ui/settings`** 管理(底层为 `settings` 表,API 为 `GET/PUT /admin/v1/settings`)。这些参数热加载:数据面轮询变更并原子切换到新快照,无需重启。

首次启动时,下列 env 值作为初始默认值写入 `settings` 表;此后 **settings 表为唯一权威来源** —— 再次编辑 `.env` 不再生效(除非清空 settings 表)。

Settings 页面分为五个卡片:

| 卡片 | 控制内容 | 种子 env |
| --- | --- | --- |
| **路由与入口** | 默认路由策略、请求体上限、多模态请求体上限、最大并发、队列深度、获取超时、raw-envelope 捕获媒体类型 | `TIYGATE_ROUTING_STRATEGY`、`TIYGATE_MAX_BODY_BYTES`、`TIYGATE_MAX_MULTIMODAL_BODY_BYTES`、`TIYGATE_MAX_INFLIGHT`、`TIYGATE_RAW_ENVELOPE_CAPTURE_MEDIA` |
| **上游** | 流式 idle / total 超时、TCP keepalive、连接池 idle 超时、TCP nodelay | `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`、`TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS`、`TIYGATE_UPSTREAM_TCP_KEEPALIVE_SECS`、`TIYGATE_UPSTREAM_POOL_IDLE_TIMEOUT_SECS`、`TIYGATE_UPSTREAM_TCP_NODELAY` |
| **Header 转发** | 请求 / 响应 header 黑名单(逗号分隔) | `TIYGATE_FORWARD_REQUEST_HEADER_DENY`、`TIYGATE_FORWARD_RESPONSE_HEADER_DENY` |
| **Payload 归档** | S3 兼容对象存储归档完整请求/响应 payload(开关、端点、region、bucket、凭证、prefix、force-path-style、扫描间隔、批次大小、并发、超时、最大重试) | `TIYGATE_PAYLOAD_ARCHIVE_*` 系列 |
| **后台任务** | 日志保留间隔与天数、epoch 轮询间隔、token 统计间隔与回看天数 | `TIYGATE_LOG_RETENTION_*`、`TIYGATE_EPOCH_POLL_INTERVAL_SECS`、`TIYGATE_TOKEN_STATS_*` |

- **epoch 版本号**:数据面轮询配置变更,原子切换到新快照;**在途请求保持旧 epoch 直到结束**——不会看到半新半旧配置。
- **密钥加密**:provider key / OAuth token / 加密的 S3 设置在数据库中 AES-GCM 静态加密,主密钥来自 `TIYGATE_MASTER_KEY`。加密设置在 `GET /admin/v1/settings` 时脱敏返回。

### 缓存

**只缓存 embedding 请求**。LLM chat/completion **不做响应缓存** —— 这是有意的设计:非确定性使响应缓存价值低、风险高。缓存可插拔:默认进程内 LRU,多副本可选 Redis 共享后端。

### Payload 归档到 S3 对象存储

启用后,后台 worker 会将每个请求的完整请求/响应 payload 详情(每请求 8 个对象——4 段链路各自的 raw body + parsed metadata:client→gateway、gateway→provider、provider→gateway、gateway→client)gzip 压缩后上传至 S3 兼容对象存储,校验 sha256/size,然后在同一事务中清空数据库中的 payload 文本。这使数据库在高流量部署下保持精简,同时完整保留回放所需的详情。

Admin 控制台的请求回放功能会按需从 S3 透明水合已归档对象(校验 → 解压 → 返回),因此无论 payload 存在于数据库还是对象存储,用户体验完全一致。

对象生命周期与 DB 保留策略解耦——worker 不会删除 S3 中的对象,请通过 bucket lifecycle policy 管理过期。

在管理控制台 **Settings → Payload Archive** 中启用并配置 payload 归档。env 变量(`TIYGATE_PAYLOAD_ARCHIVE_*`)仅在首次启动时作为初始默认值;此后以 settings 表为准,修改即时生效、无需重启。完整变量列表见 `.env.example`。

### 分布式追踪

入向从请求头提取 W3C `traceparent` / `tracestate`,出向重新注入上游请求。网关 span 作为父 span 挂到调用方 trace。日志与 trace 可通过 `trace_id` 互跳。
