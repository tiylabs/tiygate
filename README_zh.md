<div align="center">

# TiyGate

**致力于提供高可用 LLM 服务的轻量级网关。**

通过一个控制面接入 OpenAI-Compatible、Responses、Messages、Gemini 等协议。用虚拟模型按策略路由多服务商 / 多模型，捕获完整请求响应日志，并支持个人桌面端零配置启用与企业容器化部署。

[![许可证: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Edition: 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![版本: 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey.svg)](Cargo.toml)
[![Workspace: 8 crates](https://img.shields.io/badge/workspace-8%20crates-blueviolet.svg)](Cargo.toml)

[English](README.md) | 简体中文

</div>

---

<div align="center">
  <img width="1546" height="1079" alt="TiyGate Dashboard 截图" src="https://github.com/user-attachments/assets/c95d421a-9274-4804-a160-a7c9cee7e36c" />
</div>

## TiyGate 是什么?

TiyGate 是一款用 **Rust** 编写的**开源 AI 网关**，适合同时使用多个 LLM 服务商、订阅套餐、协议或 API Key 的个人与团队。它位于应用与上游服务商（OpenAI、Anthropic、Bedrock、Gemini 以及 OpenAI-Compatible 服务）之间，把分散的模型接入、路由策略、日志与统计统一到一个稳定控制面。

如果你遇到这些问题，TiyGate 可以直接解决：

1. **还在多个订阅套餐间来回切换** —— 接入多个服务商后，通过统一网关按策略使用。
2. **还在因服务商不稳定频繁改模型配置** —— 虚拟模型可按优先级、权重、吞吐、延迟等策略在多服务商 / 多模型间自动容灾切换与恢复。
3. **还在为不明原因请求失败而挠头定位** —— 实时捕获客户端 ↔ 网关 ↔ 服务商之间的详细请求、响应日志，便于回放与排查。
4. **还在为不同服务商、API Key 用量统计烦恼** —— 聚合服务商、模型、API Key 等多维度统计，统一查看用量数据。

## 为什么选择 TiyGate？

| 能力 | 你能获得什么 |
|---|---|
| **统一接入** | 支持 OpenAI-Compatible、Responses、Messages、Gemini、Embeddings 等协议，并通过 canonical IR 支撑可扩展的 N×N 协议转换。 |
| **策略容灾** | 基于虚拟模型路由，将后端多服务商 / 多模型按优先级、权重、吞吐、延迟等策略自动容灾切换与恢复。 |
| **数据沉淀** | 实时沉淀客户端 → 网关 → 服务商链路上的请求、响应详情，支持策略清理与 S3 兼容对象存储投递。 |
| **轻量应用** | 支持个人场景的 macOS / Windows 桌面端零配置启用，也支持企业场景容器化规模部署；桌面端可管理本地与云端多实例。 |
| **安全加密** | 服务商 API Key 使用 `TIYGATE_MASTER_KEY` 静态加密存储，请求 / 响应日志中的敏感字段支持脱敏。 |
| **备份恢复** | 配置支持加密导出与导入，便于实例迁移、备份与恢复。 |
| **数据统计** | 汇聚多服务商数据，支持按服务商、模型、API Key 等维度查看统计。 |

## 工程设计原则

TiyGate 在设计上既保证请求热路径稳定，也保留协议、服务商与运维能力的可扩展性。

| 质量目标 | 兜底机制 |
|---|---|
| **稳定性** | per-instance 熔断 + 细粒度 `FallbackPolicy`(错误分类、转移与重试分离、全局尝试/耗时预算、幂等闸门)、透传上游 `Retry-After`、入口请求体/慢读/并发闸、SIGTERM 优雅排空、日志旁路离热路径 |
| **可扩展** | trait + `inventory` 去中心注册(加 provider = 加文件 + 一行 `submit!`)、hook pipeline、`Executor` 逃生舱(SDK 型 provider)、三段式协议身份、可插拔路由策略/缓存/日志 sink |
| **易维护** | `core` 对具体 provider/协议/DB 零依赖;canonical IR 把 N² 协议转换降为 N;**字段级能力矩阵**显式判定有损;重依赖关进独立 crate |

`lossy_default_reject` 所用的字段级有损判定表见 [`docs/protocol-capability-matrix.md`](docs/protocol-capability-matrix.md)。

## Workspace 结构

```
tiygate/
├── crates/
│   ├── core/               # Canonical IR、trait、pipeline。零 I/O、零具体依赖。
│   ├── protocols/          # 协议 codec(chat_completions / messages / responses / gemini / embeddings)
│   ├── providers/          # 内置 provider 元数据 + 鉴权
│   ├── provider-bedrock/   # SDK 型 provider(`Executor` 逃生舱),重依赖隔离
│   ├── store/              # 配置 OLTP(SQLite/Postgres)+ 可插拔日志 sink
│   ├── cache/              # Embedding 缓存(仅确定性;LLM chat/completion 不做)
│   ├── admin/              # Admin REST API + OAuth 交互
│   └── server/             # Ingress、数据面/控制面组装、部署模式
├── webui/                  # 内嵌管理控制台(React + TS + Vite,挂载于 /admin/ui)
├── docs/                   # 架构设计 + 协议能力矩阵
└── scripts/                # 运维脚本
```

## 选择你的版本

| 版本 | 适用场景 | 获取方式 |
|---|---|---|
| 🖥️ **桌面版**(个人使用推荐) | 想要一键安装、原生 UI 的本地网关,无需 Docker、无需服务器配置的个人用户。macOS(Apple Silicon / Intel)和 Windows 安装包发布在 [Releases](https://github.com/tiylabs/tiygate/releases) 页面。 | 从最新的 [Release](https://github.com/tiylabs/tiygate/releases) 下载对应平台的安装包并运行。 |
| 🐳 **Docker 版**(企业级 / 生产环境推荐) | 需要水平扩展、多节点数据面/控制面分离、容器编排(K8s、Swarm 等)的团队和生产部署。 | `docker run -d -p 3000:3000 jorbenzhu/tiygate:latest` —— 详见 [Docker 镜像](https://hub.docker.com/r/jorbenzhu/tiygate)及下方部署模式。 |

> 两个版本共享同一套核心引擎和管理控制台 —— 你可以先用桌面版本地探索,准备好扩展时再切换到 Docker。

## 快速开始

### 前置条件

- **Rust 1.88+**(`rustup update stable`)
- **Node.js 20+**(用于构建内嵌 WebUI)
- 无需提前准备上游服务商 Key —— 启动后在管理控制台配置供应商

### 编译并启动

```bash
git clone https://github.com/tiylabs/tiygate.git
cd tiygate
```

从模板创建 `.env` 并填入必要配置:

```bash
cp .env.example .env
```

编辑 `.env`,WebUI 可用的三个必填项:

```bash
# SQLite 是最简单的本地后端(文件不存在时自动创建)
TIYGATE_DATABASE_URL=sqlite://./tiygate.db?mode=rwc

# Admin API token —— WebUI 登录页需粘贴此值
TIYGATE_ADMIN_TOKEN=dev-admin-token-change-me

# (可选但推荐)AES-GCM 主密钥,用于静态加密 provider key / OAuth token / S3 凭证。
# 详见下方"安全"小节。
# TIYGATE_MASTER_KEY=4f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8
```

其余项(监听地址、部署模式、日志级别等)见 `.env.example`。**运行时可调参数**(路由策略、入口限制、上游流式超时、连接池调优、header 转发黑名单、payload 归档到 S3、后台任务间隔等)均**通过管理控制台** `/admin/ui/settings` 管理。首次启动时,env 值作为初始默认值写入 `settings` 表;此后以 settings 表为唯一权威来源,修改即时生效、无需重启。服务启动时会自动加载 `.env`(dotenv feature 开启时)。

以内嵌 WebUI 启动网关:

```bash
make dev
```

`make dev` 会先构建前端(供 `rust-embed` 嵌入),再以 `webui` feature 启动服务。默认监听地址为 `0.0.0.0:3000`。

### 进入管理控制台

服务启动后,在浏览器打开 **`http://localhost:3000/admin/ui`**,在登录页粘贴 `TIYGATE_ADMIN_TOKEN` 即可进入控制台。在控制台中可管理供应商、路由、API 密钥、运行时设置,并查看分析数据。

### 冒烟测试

```bash
curl -sS http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "用一句话打个招呼。"}]
  }'
```

流式请求加 `"stream": true`,网关端到端走 Server-Sent Events。

### 跨协议转换

同一个网关入口可以接收 `chat_completions` 并在路由到 Anthropic 上游时自动转换为 `messages` —— **字段级能力矩阵**判定哪些组合是无损的,无法无损表达的有损组合会被直接拒绝并返回清晰错误。

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
| **路由与入口** | 默认路由策略、请求体上限、最大并发、队列深度、获取超时、raw-envelope 捕获媒体类型 | `TIYGATE_ROUTING_STRATEGY`、`TIYGATE_MAX_BODY_BYTES`、`TIYGATE_MAX_INFLIGHT`、`TIYGATE_RAW_ENVELOPE_CAPTURE_MEDIA` |
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

## 开发

```bash
# 全量测试
cargo test --all-features

# Lint(workspace 全局禁用 unsafe_code,deny unwrap/expect/panic)
cargo clippy --all-features -- -D warnings

# 格式检查
cargo fmt --all -- --check

# 全 workspace 依赖树
cargo tree --workspace

# 验证重依赖隔离(AWS SDK 等不应进入 core)
cargo tree -p tiygate-core | grep -i aws        # 应为空
cargo tree -p tiygate-provider-bedrock | head   # AWS SDK 仅在此处
```

CI 基线严格:不允许 `#[allow(...)]` 绕过、库代码中不允许 `unwrap/expect/panic!`、不允许 dead code。

### 编译组合(`tiygate-server` features)

`tiygate` 二进制是 feature 化的。挑与你部署匹配的最小集,别为不会上线的组件付编译时间与二进制体积。

| Feature | 引入内容 | 何时需要 |
|---|---|---|
| `admin` | `tiygate-admin`(控制面、Admin API、OAuth) | `admin` / `all` 部署模式 |
| `cache` | `tiygate-cache`(进程内响应缓存) | 任何受益于缓存的场景 |
| `providers` | `tiygate-providers`(OpenAI / Anthropic / 通用 OpenAI 兼容) | 非 Bedrock 的 LLM 流量 |
| `bedrock` | `tiygate-provider-bedrock`(AWS SDK) | 路由到 AWS Bedrock |
| `tracing` | `tracing-subscriber` + JSON formatter | 默认 `tiygate` 二进制 |
| `dotenv` | `dotenvy` —— 启动时自动加载 `.env` | 本地开发 |
| `webui` | `rust-embed` —— 内嵌 `webui/dist`,在 `/admin/ui` 提供管理控制台 | 带 UI 的 `admin` / `all` 部署模式 |

**默认值**:`admin`、`cache`、`providers`、`tracing`、`dotenv` —— 覆盖常见场景。**`bedrock` 是 opt-in**(它会拉整个 AWS SDK),需要 Bedrock 路由时再显式开启。**`webui` 同样是 opt-in**:它在编译期读取 `webui/dist`,因此**必须先构建前端**(`cd webui && npm install && npm run build`)再以 `--features webui` 编译,或直接运行 `scripts/build-with-webui.sh`(按顺序完成两步)。

```bash
# 默认编译(Bedrock 之外的全功能)
cargo build -p tiygate-server --release

# 需要 Bedrock 时显式打开
cargo build -p tiygate-server --release --features bedrock

# 最小数据面代理 —— 砍掉 admin / cache / bedrock
cargo build -p tiygate-server --release \
  --no-default-features --features "providers,tracing,dotenv"

# 仅 Bedrock —— 跳过 OpenAI / Anthropic 让二进制更瘦
cargo build -p tiygate-server --release \
  --no-default-features --features "bedrock,tracing,dotenv"

# 仅控制面 —— 给 `admin` 部署模式用
cargo build -p tiygate-server --release \
  --no-default-features --features "admin,tracing,dotenv"

# 查看实际编译进了什么
cargo tree -p tiygate-server -e features --depth 1
```

> **`bedrock` 默认不编,是刻意的**。编译 AWS SDK 是冷启动耗时的大头,所以我们把它从 default 中拿掉。路由到 Bedrock 的部署,显式 opt-in 即可:
>
> ```bash
> cargo build -p tiygate-server --release --features bedrock
> ```
>
> **CI 冒烟**:`bash scripts/verify-deps.sh` 在任意 feature 组合下都应通过,因为依赖隔离守在 `core` / `providers` 层,与 `server` 的编译矩阵相互独立。

## 贡献

欢迎 Issue 与 PR。设计是有立场的,与分层对抗的贡献(例如给 `core` 加具体 provider 依赖、引入 `allow_lossy`)会被拒。

## 许可证

[Apache-2.0](LICENSE)

---

<div align="center">
<sub>由 <a href="https://github.com/tiylabs">tiylabs</a> 构建 · <a href="docs/protocol-capability-matrix.md">能力矩阵</a></sub>
</div>
