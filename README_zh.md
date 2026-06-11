<div align="center">

# TiyGate

**一个面向稳定性、可扩展性与可运维性的开源 AI 网关。**

多服务商 / 多模型接入,内置可观测性、动态配置与优雅运维。

[![许可证: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: 1.78+](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![Edition: 2021](https://img.shields.io/badge/edition-2021-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![版本: 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey.svg)](Cargo.toml)
[![Workspace: 8 crates](https://img.shields.io/badge/workspace-8%20crates-blueviolet.svg)](Cargo.toml)

[English](README.md) | 简体中文

</div>

---

## TiyGate 是什么?

TiyGate 是一款用 **Rust** 编写的**独立 AI 网关产品**。它位于应用与上游 LLM 服务商(OpenAI、Anthropic、Bedrock 以及任何 OpenAI 兼容服务)之间,为路由、可观测性与策略提供统一、稳定的控制点。

它最擅长两件事:

1. **多后端 / 多模型接入** —— 一个标准入口,多个服务商。跨协议转换(例如 OpenAI `chat_completions` → Anthropic `messages`)是一等能力,不是临时拼接。
2. **日志与分析** —— 每次请求都被结构化采集,通过**异步旁路**(不阻塞热路径)落入可插拔 sink。不会丢,不会卡。

## 为什么选择 TiyGate?

多数网关只优化一个维度。TiyGate 在工程上同时兜住三个。

| 质量目标 | 兜底机制 |
|---|---|
| **稳定性** | per-instance 熔断 + 细粒度 `FallbackPolicy`(错误分类、转移与重试分离、全局尝试/耗时预算、幂等闸门)、透传上游 `Retry-After`、入口请求体/慢读/并发闸、SIGTERM 优雅排空、日志旁路离热路径 |
| **可扩展** | trait + `inventory` 去中心注册(加 provider = 加文件 + 一行 `submit!`)、hook pipeline、`Executor` 逃生舱(SDK 型 provider)、三段式协议身份、可插拔路由策略/缓存/日志 sink |
| **易维护** | `core` 对具体 provider/协议/DB 零依赖;canonical IR 把 N² 协议转换降为 N;**字段级能力矩阵**显式判定有损;重依赖关进独立 crate |

完整设计论证见 [`docs/ai-gateway-architecture-design.md`](docs/ai-gateway-architecture-design.md)。`lossy_default_reject` 所用的字段级有损判定表见 [`docs/protocol-capability-matrix.md`](docs/protocol-capability-matrix.md)。

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

## 快速开始

### 前置条件

- **Rust 1.78+**(`rustup update stable`)
- 至少一个上游服务商 Key,如 `OPENAI_API_KEY` 或 `ANTHROPIC_API_KEY`

### 编译并启动(零配置 bootstrap)

```bash
# 克隆并编译
git clone https://github.com/tiylabs/tiygate.git
cd tiygate
cargo build --release

# 设置服务商 Key —— 网关会在首个请求时自动检测
export OPENAI_API_KEY="sk-..."

# 启动网关(默认模式:all-in-one,默认端口:8080)
./target/release/tiygate
```

启动日志为 JSON 格式,可见 `TiyGate AI Gateway v0.1.0` 与 `Listening on ...`。

### 冒烟测试

```bash
curl -sS http://localhost:8080/v1/chat/completions \
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

在 `all` / `admin` 模式下,二进制会在 **`/admin/ui`** 提供内嵌的 React 控制台(如 `http://localhost:8080/admin/ui`)。它覆盖完整控制面 —— 供应商、路由、API 密钥(一次性 secret + 配额编辑与实时用量)、OAuth 授权码流程 —— 以及分析:按模型 / 供应商 / API 密钥的统计、熔断器状态、请求日志下钻与回放、审计记录,支持中英文双语。

鉴权复用单一的 `TIYGATE_ADMIN_TOKEN`:在登录页粘贴(经 Admin API 校验后存于浏览器)。UI 通过 `rust-embed`(opt-in 的 `webui` feature)编译进二进制,因此前端必须先于 Rust crate 构建 —— 运行 `scripts/build-with-webui.sh`,或先 `cd webui && npm install && npm run build` 再 `cargo build -p tiygate-server --features webui`。开发细节见 `webui/README.md`。

## 运维

### 优雅排空

发送 `SIGTERM`(或 K8s `preStop`)后,网关:

1. 将 `/readyz` 翻转为 `503`,让负载均衡摘除该副本
2. 对**新请求**返回 `503 + Retry-After`
3. **放行存量请求**(含长 SSE 流式)自然结束
4. 超过 `drain_timeout`(默认 30s,应 ≥ 单请求 `deadline`)时,对仍未结束的流发送**协议原生 error 帧**,并跑 `UsageAccumulator` 兜底计费,防止账单漂移。流式路径实现在 `crates/server/src/ingress.rs::drive_upstream_stream` —— 它同时叠加 120s idle 计时(可通过 `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` 调节)、可选用 total 总时长预算(`TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS`,默认关闭)以及 30s 周期 SSE keepalive(`SseKeepaliveStream`),防止中间代理对长时间静默的流做隐式断连
5. flush 日志 channel、归还资源、退出

### 环境变量

TiyGate 全部可调参数均通过环境变量读取,未识别的键会被忽略。网关启动时还会从工作目录加载 `.env`(在 `dotenv` feature 开启时)。

#### Server 核心

| 变量 | 默认 | 用途 |
| --- | --- | --- |
| `TIYGATE_LISTEN_ADDR` | `0.0.0.0:3000` | HTTP server 监听地址。 |
| `TIYGATE_MODE` | `all` | 部署模式。`all`(数据面+控制面同进程)、`proxy`(纯数据面)、`admin`(纯控制面)。 |
| `TIYGATE_MAX_BODY_BYTES` | `10485760`(10 MiB) | 普通文本 / JSON 请求体大小上限。 |
| `TIYGATE_MAX_INFLIGHT` | `1024` | 最大并发在途请求数。超过后新请求排队,排满后被 `503 + Retry-After` 拒绝。 |
| `TIYGATE_ROUTING_STRATEGY` | `weighted` | 跨 target 的路由策略。`weighted`(默认,§3.4)、`priority`、`cooldown`、`latency`。 |

#### 流式生命周期(实现见 `crates/server/src/ingress.rs::drive_upstream_stream`)

| 变量 | 默认 | 用途 |
| --- | --- | --- |
| `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` | `120` | 上游流式响应的 idle 窗口。若该时长内无 chunk 到达,以协议原生 end 帧关闭流。 |
| `TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS` | `0`(关闭) | 上游流式响应的总时长预算。到期后以协议原生 error 帧关闭流。设为 `0` 表示不启用。 |

#### Provider 启动自动注册路由

| 变量 | 用途 |
| --- | --- |
| `OPENAI_API_KEY` | 若设置,自动注册 `gpt-4o` / `gpt-4o-mini` / `gpt-3.5-turbo` 路由,指向 `https://api.openai.com/v1`。 |
| `ANTHROPIC_API_KEY` | 若设置,自动注册 `claude-sonnet-4-20250514` 路由,指向 `https://api.anthropic.com/v1`。 |
| `OPENAI_COMPATIBLE_BASE_URL` | 通用 OpenAI 兼容 provider(Ollama、vLLM、DeepSeek、Moonshot 等)的 base URL。该 provider 注册的必要条件。 |
| `OPENAI_COMPATIBLE_API_KEY` | 上述通用 provider 的 API key。未设置时默认 `not-needed`(适用于本地 / 无鉴权端点)。 |

#### 安全

| 变量 | 默认 | 用途 |
| --- | --- | --- |
| `TIYGATE_ADMIN_TOKEN` | 未设置 | Admin API 要求的 bearer 鉴权 token。未设置时 Admin API 请求会被拒绝。 |
| `TIYGATE_MASTER_KEY` | 未设置 | 用于 AES-GCM 静态加密 provider key / token 的主密钥。**计划在 DB 持久化阶段引入,当前内存 ConfigStore 暂未读取**——目前未设置等同于"不加密"。 |

#### 可观测性

| 变量 | 默认 | 用途 |
| --- | --- | --- |
| `RUST_LOG` | `info` | `tracing` / `tracing-subscriber` 过滤器。示例:`info`、`tiygate=debug`、`tiygate_server::ingress=trace`。 |

### 配置

- **零配置 bootstrap**:自动检测 `OPENAI_API_KEY` 等环境变量
- **DB 动态配置**(OLTP):通过 Admin API 增删改 provider / route / API key,无需重启
- **epoch 版本号**:数据面轮询配置变更,原子切换到新快照;**在途请求保持旧 epoch 直到结束**——不会看到半新半旧配置
- **密钥加密**:provider key / token 在数据库中 AES-GCM 静态加密;主密钥来自 `TIYGATE_MASTER_KEY`

### 缓存

**只缓存 embedding 请求**。LLM chat/completion **不做响应缓存** —— 这是有意的设计:非确定性使响应缓存价值低、风险高。缓存可插拔:默认进程内 LRU,多副本可选 Redis 共享后端。

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

## 项目状态

TiyGate 处于 **v0.1.0**,公开 API 尚未稳定。完整架构已在 [`docs/ai-gateway-architecture-design.md`](docs/ai-gateway-architecture-design.md) 中设计完成,拆为 5 个阶段:

| 阶段 | 主题 | 退出标志 |
|---|---|---|
| 1 | 内核 + 最小代理 | 跨协议打通、零配置可跑 |
| 2 | 稳定性层 | 熔断 / 转移 / 超时 / 入口防护 |
| 3 | 接入广度 | 5 协议、多 provider、Executor 逃生舱 |
| 4 | 产品化 | 动态配置、日志仪表盘、配额、密钥加密 |
| 5 | 规模化 | 多副本、两档部署、探针、优雅排空 |

每个阶段都可独立验收、独立演示。完整交付物、验收标准与风险见架构设计文档。

## 贡献

欢迎 Issue 与 PR。涉及非平凡变更前请先阅读架构设计文档 —— 设计是有立场的,与分层对抗的贡献(例如给 `core` 加具体 provider 依赖、引入 `allow_lossy`)会被拒。

## 许可证

[Apache-2.0](LICENSE)

---

<div align="center">
<sub>由 <a href="https://github.com/tiylabs">tiylabs</a> 构建 · <a href="docs/ai-gateway-architecture-design.md">架构设计</a> · <a href="docs/protocol-capability-matrix.md">能力矩阵</a></sub>
</div>
