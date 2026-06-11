# TiyGate 产品架构设计建议

> **项目代号：TiyGate** —— 独立 AI Gateway 产品。
>
> 目标：构建一款 **注重服务稳定性、可扩展、易维护** 的 AI Gateway，核心能力为
> **多后端服务商 / 模型接入** 与 **日志分析**。
> 方法：从 BitRouter（agent-native、协议抽象干净、零配置）与 Nyro（多 provider、动态配置、缓存、多部署模式）两款产品中提炼可取设计，给出推荐架构。

---

## 0. 一句话推荐

> 采用 **Rust 单体内核 + 数据面/控制面可拆分** 架构：**协议层用 Nyro 的三段式身份 + canonical IR + 双向 codec**，**Provider 层用 inventory 注册 + 声明式元数据 + Executor 逃生舱（BitRouter Bedrock 模式）**，**请求链用 BitRouter 的 hook pipeline（含 PassThrough 短路）**，**配置走 DB 动态 + 零配置 bootstrap 混合**，**日志分析独立成异步旁路 + 可插拔日志 sink（默认 OLTP 分区表，OLAP 后置可选）**。三大质量目标分别由：稳定性=健康熔断/降级链/旁路日志、可扩展=trait+注册表、易维护=分层+协议隔离+canonical IR 来承载。
>
> 项目取材本地路径（均在可读范围）：BitRouter `../bitrouter`，Nyro Gateway `../nyro-gateway`。

---

## 1. 从两款产品各取什么

### 1.1 取自 BitRouter（偏“内核纯度”）

| 设计 | 取用理由（对应质量目标） |
| --- | --- |
| **canonical IR + 双向 adapter**（`Prompt`/`GenerateResult`/`StreamPart`） | 易维护：N 个协议只需各写一对编解码，跨协议是 N×N 自动组合，而非 N² 手写 |
| **协议管线隔离**（`language_model` / `mcp` / `acp` hook trait 互不通用，编译期隔离） | 可扩展：新增 agent 协议不污染主链；稳定性：编译期防止 hook 误注册 |
| **Hook pipeline**（pre-request → route → execute → settle → observe + 流式 stage） | 可扩展：鉴权/限流/guardrail/计费都是 hook，核心链不变 |
| **`AuthApplier` 与协议解耦** + **单飞 token 刷新**（per-label `tokio::sync::Mutex`） | 稳定性：避免并发刷新把 refresh token 互相失效 |
| **Executor 逃生舱**（Bedrock 用 AWS SDK 自管 SigV4，不走标准 adapter） | 可扩展：非 HTTP+JSON+SSE 的 provider（云平台 SDK）有干净接入点 |
| **零配置 bootstrap**（env 自动检测 provider） | 易维护/易用：本地与冒烟测试零门槛 |
| **流式错误强约束**（`encode_error` 必须发协议原生 error 帧） | 稳定性：客户端不会被静默断流，可重试 |
| **OTel GenAI semconv**（`gen_ai.response.id`、cache/reasoning tokens） | 日志分析：标准化字段，可对接任意 APM |

### 1.2 取自 Nyro（偏“产品完整度”）

| 设计 | 取用理由 |
| --- | --- |
| **三段式协议身份** `{protocol}/{name}/{version}` | 可扩展：同协议多 schema 版本共存（`anthropic-messages/2023-06-01` 与未来版本并行） |
| **`EndpointCapabilities` 富声明**（streaming/tools/reasoning/multimodal/parallel_tool_calls/…） | 可维护：路由与转换基于声明而非硬编码 if-else |
| **`inventory` 去中心注册** | 可扩展：加 provider/endpoint = 新增一个文件 + 一行 `submit!`，无需改注册中心 |
| **Vendor + Channel + Capability 三层** | 可扩展：同一 vendor 多渠道（`openai/default` vs `openai/codex`）天然多账号 |
| **PassThrough 短路**（声明无 mutation + 同协议 → 直接转发字节） | 稳定性+性能：零转换、零丢字段 |
| **`lossy_default_reject`**（跨协议有损转换默认拒绝） | 稳定性：跨协议有损降级显式可控，不静默丢字段。**注意**：Nyro 仅 `lossy_default_reject` 落地，`allow_lossy` per-route 放行开关尚未实现；本设计**不引入 `allow_lossy`**——有损即拒绝，避免引入需自研的放行协商逻辑（详见 §3.2） |
| **`HealthRegistry`**（连续 3 次失败熔断、30s 自动恢复） | 稳定性：核心。**注意**：本设计中熔断/冷却状态为 **per-instance（每副本独立）**，不跨副本共享，详见 §3.4 |
| **路由策略可插拔**（Weighted/Priority/Cooldown/Latency） | 可扩展/稳定性：负载均衡 + 故障转移 |
| **缓存层**（**仅 embedding 缓存**；精确响应缓存 / 流重放 / 语义缓存均不做） | 成本：embedding 同输入必同输出、纯省钱、无非确定性风险。**注意**：Nyro 实际无网关级响应缓存，故此项为自研；LLM chat/completion 因 `temperature>0` 非确定性，做响应缓存价值低、风险高，故初期只支持 embedding，详见 §4.7 |
| **DB 动态配置 + Admin API + WebUI** | 易维护：运行时改 provider/route/key 不发版 |
| **多部署模式 + 多副本**（本设计只做 `all` + `proxy`/`admin` 两档拆分，共享 DB + config poll + sticky session） | 稳定性/扩展：控制面/数据面拆分，水平扩展。`standalone` 边缘模式视真实需求后置 |
| **`RawEnvelope` 无损快照** | 日志分析/审计：保留原始请求体 + headers |

---

## 2. 推荐总体架构

```
                    ┌─────────────────────── Control Plane（控制面，可独立部署）───────────────────────┐
                    │  Admin API (REST)   WebUI/CLI   Config Store(OLTP: PG/MySQL/SQLite)             │
                    │  Provider/Route/Key CRUD   OAuth 交互   配置版本(epoch)                           │
                    └───────────────▲────────────────────────────────┬──────────────────────────────┘
                                    │ config poll / push (epoch)      │
┌───────────────── Data Plane（数据面，无状态可水平扩展）─────────────┼──────────────────────────────┐
│  Ingress(HTTP/SSE)                                                  │                              │
│    │  ┌──────────────────────── Request Pipeline ────────────────┐ │                              │
│    └─▶│ authn/authz → ratelimit → cache-lookup → route → exec     │ │                              │
│       │   → (stream stage) → settle/meter → observe               │ │                              │
│       └─────────┬──────────────────────────────┬─────────────────┘ │                              │
│                 │ canonical IR                  │ events(async, 非阻塞)                            │
│       ┌─────────▼────────┐            ┌─────────▼─────────┐                                        │
│       │ Protocol Codecs   │            │ Telemetry Bus     │──▶ Log/Analytics Sink(默认 OLTP 分区表; OLAP 可选)│
│       │ (in/out × 协议)   │            │ (有界 channel)    │──▶ OTel/Prometheus exporter            │
│       └─────────┬────────┘            └───────────────────┘                                        │
│       ┌─────────▼────────┐                                                                         │
│       │ Provider/Executor │── HTTP+JSON+SSE adapter / SDK Executor 逃生舱 ──▶ 上游服务商             │
│       │ + AuthApplier     │                                                                         │
│       └──────────────────┘                                                                         │
└────────────────────────────────────────────────────────────────────────────────────────────────┘
```

要点：
1. **控制面/数据面分离**（取自 Nyro 部署模式）：数据面**配置无状态**（不持久化业务数据，配置全部来自控制面），但允许 per-instance 的**运行时易失状态**（熔断/冷却、内存配额计数）——这些状态丢失后可自愈，副本间不要求强一致。单机部署时合并为一个进程（`all` 模式）。
2. **配置以 epoch 版本号传播**：数据面轮询/订阅 DB 配置变化，秒级生效，免重启。
3. **日志分析走异步旁路**：请求链只往有界 channel 投递事件，**绝不阻塞热路径**；消费侧写**可插拔日志 sink**（默认 OLTP 分区表，高基数聚合需求出现后再切 OLAP）+ 导出 OTel。

---

## 3. 分层与模块设计

### 3.1 Crate / 模块拆分（Rust workspace）

```
tiygate/
├── crates/
│   ├── core/               # 内核：IR、pipeline、trait 定义（无 I/O、无具体 provider）
│   │   ├── ir/             # canonical Request/Response/StreamPart + RawEnvelope
│   │   ├── protocol/       # 三段式身份 + Codec trait + EndpointCapabilities + registry
│   │   ├── pipeline/       # hook 链 + stage 定义 + PassThrough 判定
│   │   ├── routing/        # RoutingTable / Strategy trait / HealthRegistry / FallbackPolicy
│   │   ├── provider/       # Vendor/Executor/AuthApplier trait + registry(inventory)
│   │   └── telemetry/      # 事件类型 + Telemetry Bus trait（落地在 server）
│   ├── protocols/          # 4+ 协议 codec 具体实现（chat/responses/messages/gemini/embeddings）
│   ├── providers/          # 各 vendor 实现（声明式 metadata + auth），inventory 注册
│   ├── provider-bedrock/   # SDK-shape provider 示例（Executor 逃生舱），独立 crate 隔离重依赖
│   ├── store/              # 配置 OLTP（sea-orm/sqlx）+ 可插拔日志 sink（默认 OLTP 分区表，OLAP 后置）
│   ├── cache/              # 缓存插件（**仅 embedding 缓存**；响应/流重放/语义缓存均不做）
│   ├── admin/              # Admin REST API + OAuth 交互
│   └── server/             # 组装：ingress、data/control plane、CLI、部署模式
└── webui/                  # 前端（可选，独立构建产物）
```

**关键纪律（易维护）**：
- `core` **不依赖任何具体 provider / 协议 / DB**，只定义 trait 与 IR。所有具体实现向核心注册。
- 重依赖（AWS SDK、向量库等）**关进独立 crate**（学 BitRouter 把 Bedrock 独立，避免污染所有 `Cargo.lock`）。
- **协议隔离**：主 LLM 链与 agent 协议（MCP 等）各自一套 hook trait，编译期不互通。

### 3.2 协议层（多模型接入的关键）

```rust
// 三段式身份（取自 Nyro），但 IR 用 BitRouter 风格的语义化 enum
pub struct ProtocolEndpoint { pub suite: Suite, pub name: &'static str, pub version: &'static str }

pub trait EndpointCodec: Send + Sync {
    fn id(&self) -> ProtocolEndpoint;
    fn capabilities(&self) -> &'static EndpointCapabilities;
    // ingress
    fn decode_request(&self, body: Value, env: &RawEnvelope) -> Result<IrRequest>;
    fn encode_response(&self, ir: &IrResponse) -> Result<Value>;
    fn stream_encoder(&self) -> Box<dyn StreamEncoder>;   // 必含 encode_error → 协议原生 error 帧
    // egress
    fn encode_request(&self, ir: &IrRequest) -> Result<(Value, HeaderMap)>;
    fn decode_response(&self, body: Value) -> Result<IrResponse>;
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;   // 显式状态机，禁止 `_ =>` 吞分支
}
inventory::collect!(CodecRegistration);   // 去中心注册
```

- **canonical IR**：`IrRequest`/`IrResponse`/`StreamPart` 显式建模 text / reasoning / tool_call / tool_result / usage(含 cache/reasoning tokens)。
- **跨协议 = ingress.decode → IR → egress.encode**；同协议且无 mutation → **PassThrough 转发原始字节**（性能 + 零丢字段）。
- **有损转换**：codec 在 `EndpointCapabilities` 标 `lossy_default_reject`，路由/协商层据此**直接拒绝**有损转换并返回清晰错误。**本设计不引入 per-route `allow_lossy` 放行开关**——Nyro 该开关本身也尚未实现（仅留注释 "later PR"），属需自研逻辑；初期坚持「有损即拒绝」，把跨协议组合限定在无损可达的范围内，待真实放行需求出现再评估（YAGNI）。
- **有损判定基准（必须显式建模，不靠经验）**：「N² 降为 N」的前提是 IR 能无损承载各协议的差异字段，而最易丢字段的恰是 **tool calling** 与 **多模态**。因此 codec 的有损判定**不得靠直觉**，必须落到一张**字段级能力矩阵**（随阶段3的 N×N 测试一起维护，见 §8 阶段3）。当前已知的高风险维度，IR 必须显式建模、转换层必须逐项判定：
  - **tool calling**：OpenAI `parallel_tool_calls`（并行调用）、Anthropic `tool_use`/`tool_result` 块结构与 `tool_choice` 语义、Gemini `functionCall`/`functionResponse`。IR 以统一的 `tool_call`/`tool_result` part 建模；**并行工具调用、`tool_choice=required/具体函数` 等无法在目标协议表达时，按 `lossy_default_reject` 拒绝**，而非静默降级为串行/auto。
  - **多模态 part**：image/audio/document 的承载方式（inline base64 vs URL vs file-id）、MIME 类型支持集、单请求体量上限因协议而异。IR 以 `media` part + 来源类型建模；**目标协议不支持的模态或承载方式触发拒绝**。
  - **reasoning / 结构化输出**：`reasoning` part、`response_format`/`structured_output`/`json_schema` 在各协议表达力不同，同样纳入矩阵判定。
  - 矩阵的每个「无损/有损/不支持」结论都要有 N×N 测试对应（见 §8 阶段3 验收），作为路由协商拒绝与否的**唯一判定来源**。
- **协议版本策略**：三段式标识符（含 `version`）**保留**，便于未来多 schema 版本并行；但**本设计暂不实现版本共存路由**——初期每个 `{suite}/{name}` 只注册单一版本，`version` 字段先作标识与日志维度用途，避免引入版本协商逻辑的复杂度（YAGNI）。

### 3.3 Provider 层（多后端接入）

```rust
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn metadata(&self) -> &'static ProviderMetadata;     // 声明式：base_url、协议、auth_mode、channels
    fn supported_protocols(&self) -> &[ProtocolEndpoint];
    fn auth(&self) -> Arc<dyn AuthApplier>;              // 鉴权与协议解耦
    fn executor(&self) -> Option<Arc<dyn Executor>>;     // None=走标准 HTTP；Some=SDK 逃生舱
}
```

- **元数据声明式**（BitRouter TOML 思路）+ **DB 覆盖**（Nyro 动态）：内置 provider 用代码/TOML 声明默认值，运行时可在 DB/WebUI 覆盖 base_url、增删自定义 OpenAI-compatible provider，**无需发版**。
- **Channel/多账号**：一个 provider 下多账号（不同 key、不同订阅渠道），路由按 `account_label` 选。
- **AuthApplier 生命周期**：`apply(headers)` + OAuth 的 `start/exchange/refresh`，**刷新单飞**（防并发失效）。
- **SDK 逃生舱**：AWS Bedrock / Vertex 这类自管签名与帧的，实现 `Executor` 直接接管请求路径，独立 crate。
- **pricing/成本来源（仅做可扩展预留，初期不接数据源）**：单价（input/output/cache token 价格、context length 等）**不写进 provider 元数据**。当前**无可靠、稳定的价目表数据源**——`models.dev/api.json` 在 BitRouter 中也仅有拉取基础设施、未真正接入启动流程，数据完整性与时效性均未验证。因此本设计**只预留接口**：定义 `trait PriceProvider { fn unit_price(&self, model, token_kind) -> Option<MicroUsd> }` 作为可插拔成本数据源，日志事件的 `cost` 字段在无 `PriceProvider` 实现时**一律留空、只记 token**。待确定可靠数据源（自维护价目表 / 商用 API / 验证后的 models.dev）后再补具体实现，不阻塞主链路。

### 3.4 路由与稳定性

- **RoutingTable** 把 `virtual_model` → 有序 `RoutingTarget` 链（多账号/多 provider 故障转移）。
- **Strategy trait**：Weighted（默认）/ Priority / Cooldown / Latency，可插拔。
- **HealthRegistry**：连续 N 次失败熔断、冷却后半开恢复（取自 Nyro）。**多副本下熔断/冷却状态为 per-instance（每副本各自维护，不跨副本共享）**——理由：共享熔断需引入额外存储与一致性开销，而各副本独立探测上游本就更鲁棒（避免单点误判全局熔断），状态丢失也能自愈。代价是不同副本对同一上游的熔断判断可能短暂不一致，属可接受的最终一致。
- **FallbackPolicy（可插拔 trait，沿用 BitRouter 设计）**：保留 `trait FallbackPolicy::classify(err, target) -> FallbackDecision` 的可插拔形态 + `ExecutionHook` 可投票覆盖（计费/支付闸门用），但把错误分类做细，并把「换 target」与「重试同 target」拆成两个独立旋钮：
  - **错误分类（比裸 HTTP 码更细）**：
    - `Transient`（5xx / 408 / transport / 超时）→ 换下一个 target，可重试。
    - `RateLimited`（429）→ 换 target，但**优先尊重上游 `Retry-After`**（见 §3.5），并对当前 target 进入冷却而非立即重打。
    - `Auth`（401/403）→ **不在同一账号上重试**（重打必然再失败），可切到「另一账号/渠道」类 target，同时触发该账号 token 刷新或标记失活。
    - `BadRequest`（400/422 参数错）→ **直接 Fail，绝不转移**（每个上游都会报同样错，转移只放大延迟）。
    - `Lossy/Capability`（目标不支持所需能力）→ 跳过该 target 或 Fail，不计为「故障」、不污染熔断计数。
  - **换 target ≠ 重试同 target**：换 target 沿用 `FallbackDecision::TryNext` 遍历路由链（**链长即天然上限**）；重试同 target 由独立 `RetryPolicy` 管（最大次数默认 2、指数退避 + jitter，仅对 `Transient`/`RateLimited` 生效）。
  - **全局预算上限（防「转移 × 重试」组合爆炸）**：单请求总尝试次数硬上限（默认 `max_total_attempts = 4`）+ 总耗时 `deadline`，二者任一触顶即放弃，无论路由链是否还有 target；已被 `HealthRegistry` 熔断的 target 在选择阶段直接跳过，不计入尝试。
  - **幂等保护**：转移/重试前确认请求可安全重放。非流式天然安全；**流式仅在「尚未产出任何字节」时可重试**——请求上下文记录 `bytes_emitted` 作为重试闸门，已出首字节则不再转移/重试（避免重复内容）。
- **超时与背压**：每跳独立超时 + 流式 keepalive；上游慢时不拖垮连接池。
- **断连计费防逃**：流式请求客户端中途断连时，用 `UsageAccumulator`（字符计数 + 估算）兜底估算 usage 落账，防止恶意断连逃费（沿用 BitRouter 机制）。

### 3.5 Gateway 职责契约：透传为主、尊重上游、如实反馈下游

Gateway 的默认立场是**最小干预**，三条原则各自落到机制上：

- **透传为主**：同协议且无 mutation 走 PassThrough 逐字节转发，最大限度不碰 body；仅跨协议才用 canonical IR 转换，且有损转换由 `lossy_default_reject` 把关（见 §3.2）。
- **尊重上游（避免成为放大上游压力的代理）**：
  - **透传上游限流信号**：把上游的 `Retry-After` / `RateLimit-*` 响应头如实传给下游，让客户端自行退避；同时 gateway 内部据此对该 target 设冷却，而非无视后立即重打。
  - **不擅自改写上游语义参数**（`max_tokens`、`temperature` 等），除非用户在配置中显式 override。
  - **限流礼让**：429 后对该账号退避，优先切其他账号/渠道。
- **如实反馈下游（语义如实、外壳适配）**：
  - **语义透传**：上游错误的类型、message、限流信息尽量保真传给客户端，不统一吞成 502。
  - **外壳翻译**：错误的协议形状转成客户端期望的协议（对应 BitRouter `encode_error` 协议原生 error 帧），即「语义如实、外壳适配」。
  - **来源可区分**：gateway 自身产生的错误（无可用 target、配额超限、全链熔断）用清晰可区分的错误码标明「这是 gateway 的决定，不是上游」；保留上游原始 status 于 error detail 或 `x-tiygate-upstream-status` 头，避免客户端误判。
  - **脱敏不冲突**：如实反馈不等于泄露——上游错误中若含内部 URL/密钥一律脱敏后再回传。

### 3.6 Ingress 入口防护（请求体上限 + 慢 body）

数据面入口提供基础 DoS 防护，**全部带默认值且可配置**（DB/env 覆盖）：

- **请求体大小上限**：`max_request_body_bytes`，默认 `10 MiB`；超限直接拒绝（413 Payload Too Large），不进入 pipeline。
- **多模态请求的体量分级**：默认 10 MiB 对含图片/音频/文档的多模态请求偏小。设计上区分两类上限并都可配：`max_request_body_bytes`（文本/普通请求，默认 10 MiB）与 `max_multimodal_body_bytes`（含 inline base64 媒体的请求，默认如 `32 MiB`，按部署规模上调）。**推荐优先用 URL/file-id 承载大媒体**（见 §3.2 多模态 part）以避免 inline base64 撑大 body；inline 媒体计入 `max_multimodal_body_bytes`。两者均超限即 413。
- **慢 body / 慢读防护**：`request_read_timeout`，默认 `30s`（读取完整请求体的最长时间）；配合请求头读取超时，防止 slowloris 类慢连接耗尽连接资源。
- 两项均可按部署规模在配置中调整；流式响应侧已有 keepalive 与每跳超时（见 §3.4），与入口防护互补。

### 3.7 请求级并发上限与排队（突发洪峰保护）

§3.4 的连接池背压只约束「对上游的出向连接」，不限制「入向并发请求总数」；突发洪峰下大量请求同时驻留 pipeline（各自持有 buffer、流式状态、上游连接）仍可能耗尽内存/FD。故数据面入口再加一道**全局并发闸 + 有界排队**，全部带默认值且可配置（DB/env 覆盖）：

- **全局并发上限**：`max_inflight_requests`（默认如 `1024`）——同时在 pipeline 中处理的请求数硬上限，用一个 `tokio::sync::Semaphore` 在 ingress 入口获取许可，请求结束（含流式终止）后释放。
- **有界等待队列**：`max_queue_depth`（默认如 `256`）+ `acquire_timeout`（默认如 `5s`）——并发已满时，新请求进入有界队列等待许可；队列也满或等待超时则**立即拒绝**，返回 `503 Service Unavailable` 并带 `Retry-After`，**而非无限堆积**。
- **按维度可选限流（后置）**：初期只做全局闸；按 `api_key` / `model` / `account_label` 的细粒度并发隔离（防单租户打满全局）列为后置可选项，避免早期复杂度膨胀。
- 与既有机制的关系：本闸在**最外层**（authn 之后、route 之前）拦截，先于路由/熔断生效；被拒绝的请求**不计入** `HealthRegistry` 故障计数（这是 gateway 主动限流，非上游故障）。被拒绝事件按 §3.5「来源可区分」原则用清晰错误码标明是 gateway 的限流决定。

### 3.8 优雅排空与停机（in-flight 请求保护）

副本下线、滚动升级、配置/二进制更新时，必须让**正在进行的请求（尤其长流式生成）平稳结束**，而非被强杀导致客户端断流、断连计费误差、上游 token 白烧。数据面实现标准的 **graceful drain**：

- **信号驱动**：收到 `SIGTERM`（或 K8s `preStop` hook）即进入 **draining 状态**，不退出进程。
- **就绪探针先摘流量**：进入 draining 后 `/readyz` 立即返回 `503`（见 §5 健康探针），负载均衡/K8s Service 在下一个探测周期把该副本摘出转发池，**停止接入新请求**；`/healthz`（liveness）仍返回 200，避免被误判为死进程而强杀。
- **拒绝新请求、放行存量**：draining 期间入口对**新请求**返回 `503 + Retry-After`；对**已在 pipeline 内的请求**继续处理直至自然完成（含 SSE 流式逐帧发完）。
- **有界排空超时**：`drain_timeout`（默认如 `30s`，可配，应 ≥ 单请求 `deadline`）——超时后对仍未结束的流式请求按 §3.5 发送**协议原生 error 帧**关闭（非静默断流），并触发 §3.4 的 `UsageAccumulator` 断连兜底计费，保证账单不丢、客户端可感知。**实现位置：`crates/server/src/ingress.rs::drive_upstream_stream`** —— 该函数同时承担 streaming 期间的 idle / total 两层超时（默认 idle 120s、total 0=关闭，分别通过 `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` / `TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS` 配置）、30s 周期 SSE keepalive 注入（`SseKeepaliveStream`），以及自然结束 / 超时 / 上游错误三类终止原因下协议原生 end / error 帧的收尾补帧。
- **资源有序释放**：排空完成或超时后，flush 日志事件 channel（§4.1，确保 in-flight 事件落盘）、归还连接池、关闭 DB/Redis 连接，再退出。
- **多副本配合**：与 §5.1 expand/contract 迁移、epoch 配置切换协同——滚动升级时旧副本 drain、新副本 ready 接流，全程对客户端无感知中断。

### 3.9 租户模型边界（初期单租户，预留维度位）

配额（§4.6）、日志（§4.4）、配置都隐含「租户」维度，但本设计**初期定位单运营者/小团队，不做租户隔离**——所有下游 `api_key` 共享同一份全局 provider/route 配置，互相之间无配置、配额、日志可见范围的隔离。为避免后续返工，明确以下契约：

- **当前行为**：下游 `api_key` 是「调用凭证 + 计量/限流主体」，**不是**配置作用域。任意有效 key 可路由到任意已配置的 `virtual_model`/provider；配额按 key 计（见 §4.6），日志按 key 打标（见 §4.2 `api_key_id`），但**不限制 key 能访问哪些 model/provider 子集**。
- **预留维度位（本期不实现）**：数据模型上为 `api_key` 预留可空的 `tenant_id` 字段、为 route/provider 预留可空的 `tenant_scope` 字段；日志事件 §4.2 预留 `tenant_id` 维度。初期全部留空（视作单一隐式全局租户），查询与限流按「全局」聚合。
- **后置增量（YAGNI，待真实多租户需求出现）**：key→允许的 provider/route 子集绑定、配额按租户归属、日志可见范围按租户隔离、租户级 Admin 子账号——均列为后置项，届时基于已预留的 `tenant_id`/`tenant_scope` 扩展，不改动核心链路。

---

## 4. 日志分析子系统（核心能力，单列）

这是产品差异化重点，建议**独立设计、独立存储、异步解耦**。

### 4.1 采集（热路径零阻塞）
- 请求链在每个 stage 边界产出结构化事件（开始/路由结果/上游耗时/usage/finish/error），投递到**有界 mpsc channel**；channel 满时按策略丢弃低价值事件（采样）而非阻塞请求。
- 保留 **`RawEnvelope`**（原始 body+headers 快照，脱敏后）用于审计/重放，可配置开关与采样率（避免存储爆炸）。
- **大 body / 多模态快照策略（防存储膨胀）**：全量快照含 inline base64 媒体的多模态 body 会让日志存储快速膨胀。`RawEnvelope` 对超过阈值（`raw_envelope_max_bytes`，默认如 `256 KiB`）的 body **默认截断**，只保留前 N 字节 + 记录原始大小与 `truncated=true` 标记；inline 媒体 part **默认只存元信息**（MIME、大小、hash），不存原始字节，可按需开启 `raw_envelope_capture_media` 完整留存（用于深度排障，代价是存储成本）。文本类 body 在阈值内完整保留以保证重放可用。

### 4.2 事件模型（结构化、可聚合）
```
request_id, ts, virtual_model, resolved_provider, resolved_model, account_label,
tenant_id(预留, 初期留空, 见 §3.9),
trace_id, span_id, traceparent(透传下游传入的 W3C trace 上下文, 见 §4.8),
ingress_protocol, egress_protocol, lossy, cache_hit(embedding_only: hit|miss|n/a),
status, error_class, http_status, error_source(gateway|upstream, 见 §3.5),
latency_ms(total/upstream/queue), ttfb_ms,
tokens(prompt/completion/reasoning/cache_read/cache_write), cost,
api_key_id, client_ip, user_agent
```
> `cost` 仅在配置了可靠 `PriceProvider`（见 §3.3）时计算；当前无可靠价目表数据源，该字段**默认留空、只记 token**。`cache_hit` 仅对 embedding 请求有意义（见 §4.7），chat/completion 恒为 `n/a`。`tenant_id` 初期恒空（单租户，见 §3.9）。`trace_id`/`traceparent` 关联分布式链路（见 §4.8）。

### 4.3 存储分层
- **OLTP（PG/MySQL/SQLite）**：配置、API key（静态加密存储，见 §4.5）。
- **日志存储（可插拔 sink）**：请求日志与指标。**默认落 OLTP 的独立分区表**（与配置表逻辑分离，按天分区），满足初期单机/中小规模需求；当高基数聚合成为瓶颈时再切 **OLAP（ClickHouse 或同类列存）**——sink 设计为 trait，切换不影响采集侧。**初期不引入 ClickHouse 部署依赖**。
- **日志保留**：sink 支持配置**最大保留天数**（`log_retention_days`，默认如 30 天），后台定时任务按分区清理过期数据，防止存储无限增长。
- **OTel/Prometheus exporter**：对接既有可观测体系；GenAI semconv 标准字段。

### 4.4 分析能力
- 实时仪表盘：QPS、错误率、p50/p95/p99 延迟、TTFB、按 provider/model 的 token（成本字段在配置 `PriceProvider` 后才有值）、embedding 缓存命中率、熔断状态。
- 慢请求/失败请求下钻 → 取 `RawEnvelope` 重放定位。
- 成本与配额：按 key/租户的 RPM/RPD/TPM/TPD 配额（取自 Nyro），超限即时阻断。配额计数方案见 §4.6。

### 4.5 安全模型（必备项）
- **密钥静态加密存储**：provider 的 API key / OAuth token / refresh token 在 OLTP 中**加密存储**（如 AES-GCM），加密主密钥来自环境变量（如 `TIYGATE_MASTER_KEY`），不与密文同库。明文仅在内存中临时持有。
- **Admin API 鉴权**：Admin API 用**环境变量配置的管理密钥**（如 `TIYGATE_ADMIN_TOKEN`）做 bearer 鉴权，**不引入复杂的 RBAC/多角色权限管理**——初期定位单运营者/小团队，管理面要么全权要么无权。
- **日志脱敏**：日志与 `RawEnvelope` 中的密钥、token、Authorization header 等敏感字段一律脱敏，UI/导出均不明文。
- **下游客户端密钥生命周期（初期从简，轮换/吊销后置）**：下游调用方的 API key 通过 Admin API 创建/删除，**初期只做「创建—启用—删除」三态**，删除即失效。**不做**自动轮换、宽限期双 key 并存、细粒度 scope 吊销等流程——这些属企业多租户场景的增量需求，与当前单运营者/小团队定位不符；接口上预留 `status` 字段为未来扩展留位，但本期不实现轮换/吊销编排（YAGNI）。

### 4.6 配额计数（多副本一致性）
- 配额（RPM/RPD/TPM/TPD）是**热路径每请求读写的高频计数**，**不放 OLTP 同步读写**（会阻塞热路径、违背旁路原则）。
- **单副本/`all` 模式**：内存原子计数 + 定期落 OLTP 快照（用于重启恢复与展示），足够。
- **多副本**：引入 **Redis 原子计数**（`INCR` + `EXPIRE` 滑动窗口或固定窗口）作为跨副本共享计数器，超限即时阻断在 Redis 侧判定，保证多副本下配额全局一致；Redis 不可用时降级为 per-instance 内存计数（宁可少算不误杀，或按策略 fail-open/fail-close 可配）。

### 4.7 缓存层（仅 embedding，不做响应缓存）

**只对 embedding 请求做缓存，不对 chat/completion 做响应缓存。** 理由：LLM 对话/补全默认 `temperature>0`，相同 prompt 也应产生不同输出，对其做响应缓存价值低、且会把「每次新鲜生成」静默换成「返回旧结果」，违背调用方预期；流重放缓存更会让客户端误以为在看实时生成。而 **embedding 是确定性的——同模型 + 同输入必同输出**，缓存语义安全、纯省成本、无误命中风险。

- **缓存键**：`hash(model + normalized_input + 关键参数如 dimensions/encoding_format)`；同输入命中即返回，省掉一次上游调用与 token 费用。
- **存储**：可插拔后端，单机用进程内 LRU（容量上限可配），多副本可选 Redis 共享；默认带 TTL（如 `embedding_cache_ttl`，默认 7 天）与最大条目数，防无限增长。
- **作用域**：仅 `embeddings` 端点生效；chat/completion 请求**直接绕过**缓存层，不查不写。
- **明确不做**：精确响应缓存、流重放缓存、语义缓存（embedding + 向量近邻匹配）均**不在范围内**——前两者因非确定性价值低/风险高，语义缓存复杂度与误命中风险更高，全部列为按需后置实验项。
- **可插拔**：`cache` 独立成 crate，trait 抽象，未来若要扩展其他缓存形态不影响主链路。

### 4.8 分布式 trace 透传（链路追踪贯通）

网关位于调用方与上游之间，是分布式链路的关键一跳，必须**贯通而非中断** trace 上下文：

- **入向提取**：从下游请求头提取 W3C Trace Context（`traceparent`/`tracestate`）；若存在则**沿用其 `trace_id` 并作为父 span 挂载本网关 span**，缺失则生成新的 root trace。
- **出向注入**：向上游发起请求时，把当前 span 的 `traceparent`/`tracestate` **注入上游请求头**，使上游（若也接入 trace）能挂到同一条链路。
- **与 OTel 一致**：网关 span 用 §1 的 GenAI semconv 命名与属性（`gen_ai.*`），trace/span id 随事件模型（§4.2 `trace_id`/`span_id`/`traceparent`）一并落日志，便于「日志 ↔ trace」互跳。
- **可配置**：trace 透传默认开启，可关；`tracestate` 透传可选（部分厂商头较大）。本项不引入额外采样器，复用 OTel SDK 的采样配置。

---

## 5. 配置与部署

- **混合配置**：零配置 bootstrap（env 自动检测，便于本地/CI）+ DB 动态（生产，WebUI/Admin API CRUD）。
- **配置版本号（epoch）**：数据面轮询/订阅变更，秒级热生效，免重启（取自 Nyro）。每次配置变更递增 epoch，数据面以原子方式切换到新快照；**回滚**通过保留历史 epoch 快照、Admin API 指定回退到上一个 epoch 实现（回滚为**多副本生产**主要场景，单机 `all` 模式可选）。
- **epoch 切换的 in-flight 一致性契约**：配置切换发生在「请求入口」——**每个请求在进入 pipeline 时快照绑定当时的 epoch，全程不变**（路由表、key 校验、provider 配置、限流参数都读这一份不可变快照）。切换瞬间已在途的请求继续用旧 epoch 完成，新进入的请求用新 epoch；**不存在请求中途看到半新半旧配置**的情况。key 被吊销/删除的失效语义同理：以请求入口快照为准，已在途请求放行至完成（避免长流式生成被半路掐断），新请求按新快照拒绝。
- **部署模式（两档）**：`all`（单进程，含内置 DB，本地/中小规模首选）/ `proxy`+`admin` 拆分（纯数据面 + 纯控制面，水平扩展场景）。**暂不做 `standalone`（无 DB YAML 边缘模式）**，视真实边缘需求再加。
- **多副本（仅水平扩展场景需要，单机 `all` 模式可全部跳过）**：数据面配置无状态水平扩展；共享 OLTP；跨副本配额计数走 Redis（见 §4.6）；OAuth 回调用 sticky session；epoch 回滚（见上）。中小部署用 `all` 单进程即可，不必引入 Redis / sticky session / 多副本 epoch 协调。
- **Redis 是可选组件（非强依赖）**：Redis 仅在「多副本 + 需要跨副本一致的配额计数 / 共享 embedding 缓存」时引入。单机 `all` 模式与「多副本但接受 per-instance 近似配额」的部署都**不需要 Redis**。降级行为明确：Redis 不可用时，配额计数降级为 per-instance 内存计数（fail-open/fail-close 可配，默认 fail-open「宁可少算不误杀」）、共享缓存降级为进程内 LRU；降级是**自动且不中断请求**的，仅牺牲跨副本精确性，运维需知晓此时配额为「每副本近似」。部署文档（§5 多副本运维手册）须把 Redis 的可选性、降级语义与默认 fail 策略列为一等说明项。
- **健康探针**：`/healthz`（liveness，draining 期间仍返回 200）+ `/readyz`（readiness，DB/依赖可达；进入 draining 即返回 503 以摘流量，见 §3.8）。
- **优雅停机**：`SIGTERM` 触发 graceful drain（摘流量 → 放行存量 → 有界超时 → 有序释放），详见 §3.8。
- **协议范围边界**：当前只支持 **HTTP + SSE**（请求/响应 + 服务端流式）。**暂不支持 WebSocket 类双向/实时协议**（如 OpenAI Realtime）——其连接模型、帧语义与计费方式与 HTTP+SSE 差异大，会冲击 canonical IR 与 pipeline 抽象。列为按需后置项，待真实需求出现再评估是否引入独立的实时管线，避免提前为其改造内核。

### 5.1 数据库迁移与版本演进

配置走 OLTP 动态化后，schema 演进必须工具化，**不手写迁移**：

- **复用 ORM 内建迁移**：`store` 已选 sea-orm/sqlx，直接用其迁移框架（`sqlx::migrate!` 或 sea-orm migration）。迁移文件版本化纳入 git、**编译期嵌入二进制**，启动时与库对账。
- **配置库与日志库分迁移轨**：配置（OLTP，改动少而关键、需强一致）与日志分区表（高写入、按天分区、需滚动清理）演进节奏不同，维护两套独立迁移序列，避免日志表频繁变更污染配置迁移历史。
- **前向兼容、可回滚**：遵循 **expand/contract（扩展-收缩）** 模式——先加列（可空/带默认）→ 双写/双读过渡 → 再删旧列，避免破坏性迁移让滚动升级中尚未更新的旧副本崩溃（多副本会短暂新旧共存读同一库）。每个迁移尽量配 down/回滚脚本；不可逆迁移（如删列）单独标注并人工确认。与 §5 的 epoch 配置回滚机制配合。
- **启动迁移策略可控**：`all`/单机模式启动时自动跑迁移；**多副本生产禁止每个副本各自跑迁移**（并发迁移冲突）——改为独立的 `migrate` 子命令/Job 先行执行，数据面副本启动时只**校验** schema 版本是否匹配，不匹配则拒绝启动并报清晰错误。
- **日志分区生命周期工具化**：建分区、滚动、按 `log_retention_days`（见 §4.3）清理过期分区，做成后台定时任务，与日志 sink 同模块管理。

---

## 6. 三大质量目标如何被架构兜住

| 目标 | 关键支撑 |
| --- | --- |
| **稳定性** | 健康熔断（per-instance）+降级链+多账号故障转移；细化 FallbackPolicy（错误分类 + 重试/转移分离 + 全局预算上限 + 幂等闸门）；尊重上游限流（透传 `Retry-After`/`RateLimit-*` + 礼让退避）；Ingress 请求体上限 + 慢 body 防护 + 全局并发闸/有界排队；优雅排空（SIGTERM drain，存量请求平稳结束）；日志旁路异步（不阻塞热路径）；流式错误协议原生帧；PassThrough 零转换；每跳超时+背压；断连计费防逃；epoch 切换 in-flight 配置一致性；Redis 不可用自动降级不中断请求；控制面挂掉不影响数据面转发 |
| **可扩展** | trait + `inventory` 去中心注册（加 provider/协议=加文件）；hook pipeline；Executor 逃生舱；三段式协议标识（预留版本位）；策略/缓存/日志 sink 可插拔；租户维度（`tenant_id`/`tenant_scope`）预留位 |
| **易维护** | `core` 零具体依赖；协议隔离编译期防错；canonical IR 把 N² 降为 N（有损边界靠字段级能力矩阵显式判定）；重依赖关进独立 crate；声明式元数据；配置/日志存储分离；分布式 trace 贯通（日志↔trace 互跳） |
| **安全** | 密钥 AES-GCM 静态加密（主密钥走 env）；Admin API 环境变量密钥鉴权；日志/UI 敏感字段脱敏；RawEnvelope 大 body/媒体截断防膨胀 |

---

## 7. 明确不要照搬的点

1. **不要**像 BitRouter 把 provider 全部编译期写死 TOML → 生产加 provider 要发版；用 **DB 动态覆盖**（内置 catalog 编译期嵌入做默认值，用户配置运行时覆盖）。
2. **不要**像 Nyro 把日志/计量塞进配置同一个 OLTP 库做高基数聚合 → **日志走独立分区表/可插拔 sink**，规模上来再切 OLAP。
3. **不要**一开始就上多形态仓库（Tauri+WebUI+Python+Rust，如 Nyro）→ 先 **单 Rust workspace + 可选 WebUI**，控制复杂度。
4. **不要**让日志写入与请求链同步耦合 → 一律 **异步有界 channel + 可降级采样**。
5. **不要**对 chat/completion 做响应缓存 / 流重放缓存 / 语义缓存 → LLM 非确定性使其价值低、风险高；**只对确定性的 embedding 做缓存**（见 §4.7）。其余如协议版本共存路由、`standalone` 边缘模式、ClickHouse 部署依赖、复杂 RBAC、下游 key 自动轮换/吊销、**WebSocket 类实时协议（如 OpenAI Realtime）** 都是**按需后置**项，避免早期复杂度膨胀。
6. **不要**把高频配额计数放 OLTP 同步读写 → 单机用内存计数，多副本用 **Redis 原子计数**。
7. **不要**把 FallbackPolicy 退化成「5xx 就无脑换 + 无限重试」 → 错误分类细化、转移与重试分离、设全局尝试/耗时预算、流式出字节后禁止重试（见 §3.4）。
8. **不要**让 gateway 无视上游限流信号自顾自重打 → 透传 `Retry-After`/`RateLimit-*` 给下游并据此礼让退避；也不擅自改写上游语义参数（见 §3.5）。
9. **不要**手写 DB 迁移、或让多副本各自并发跑迁移 → 用 ORM 内建迁移、expand/contract 前向兼容、迁移先行 + 副本启动只校验版本（见 §5.1）。
10. **不要**只靠连接池背压扛洪峰、或停机时强杀进程 → 入口加全局并发闸 + 有界排队（满则 503，见 §3.7）；停机走 SIGTERM 优雅排空，存量流式请求平稳收尾（见 §3.8）。

---

## 8. 分阶段开发计划（目标 / 交付物 / 验收标准）

总体节奏：5 个阶段，每阶段都是「可独立验收、可对外演示」的里程碑。每阶段含 **开发目标**（做什么）、**交付物**（产出哪些 crate/接口/文档）、**验收标准**（可量化、可勾选的退出条件）。后续可直接据此拆 issue 与排期。

贯穿全程的工程基线（每阶段都必须满足，不单列）：
- `cargo test --all-features`、`cargo clippy --all-features`、`cargo fmt -- --check` 全绿；不得用 `#[allow(...)]` 绕过、不得 `unwrap/expect/panic!` 制造 panic、不得保留 dead code。
- 关键路径有单元 + 集成测试；对外接口有 doc 注释；每阶段产出/更新对应文档。

---

### 阶段 1 — 内核与最小可用代理（MVP）

**开发目标**
- 搭好 Rust workspace 与分层骨架（`core` / `protocols` / `providers` / `store` / `server`）。
- 在 `core` 定义 canonical IR（`IrRequest` / `IrResponse` / `StreamPart` / `RawEnvelope`）、`EndpointCodec` trait、`Provider` / `AuthApplier` trait、最小 pipeline（authn → route → exec → observe）。
- 实现 2 个协议 codec：`chat_completions`、`messages`（含显式状态机的流式 decode/encode）。
- 接入 3 个 provider：OpenAI、Anthropic、1 个通用 OpenAI-compatible（自定义 base_url+key）。
- 跨协议打通：OpenAI 入 → Anthropic 出（`chat_completions → messages`）可正常完成与流式。
- 异步日志旁路雏形：请求事件投递到有界 channel，落 SQLite/stdout。
- 配置：零配置 bootstrap（env 检测）+ SQLite 静态配置文件。

**交付物**
- workspace + 5 个 crate 骨架；`core` 全部 trait 定义 + IR 类型。
- `protocols` 2 套 codec；`providers` 3 个 provider。
- `server` 提供 `POST /v1/chat/completions`、`POST /v1/messages` 入口 + SSE。
- 文档：`README`（启动方式）、`core` 接口说明。

**验收标准**
- [ ] 设置 `OPENAI_API_KEY` 后 `server` 零配置启动，`/v1/chat/completions` 非流式 + 流式均返回正确结果。
- [ ] 同样请求路由到 Anthropic 上游（跨协议转换）结果语义正确，tool_call / reasoning / usage 字段不丢失。
- [ ] 流式中断时按协议原生 error 帧关闭（非静默断流），有测试覆盖。
- [ ] 每个 codec 的 decode 为显式状态机，无 `_ =>` 吞分支；有快照测试。
- [ ] 日志事件异步写出，压测下日志 channel 满不阻塞主请求（有测试或基准佐证）。
- [ ] 三个 provider 各有一条 happy-path 集成测试（可用 mock 上游）。

---

### 阶段 2 — 稳定性层（可靠性达标）

**开发目标**
- `RoutingTable`：`virtual_model` → 有序 `RoutingTarget` 链；支持多账号（`account_label`）。
- `HealthRegistry`：连续 N 次失败熔断 + 冷却半开恢复（**per-instance，状态不跨副本共享**）。
- `FallbackPolicy`（细化）：错误分类（Transient/RateLimited/Auth/BadRequest/Lossy）决定转移与否；**转移（换 target）与重试（同 target，独立 `RetryPolicy`：默认 2 次、指数退避 + jitter）分离**；全局预算上限（`max_total_attempts` 默认 4 + 总耗时 `deadline`）；流式出字节后禁止重试的幂等闸门（`bytes_emitted`）。
- 可插拔路由策略：Weighted（默认）/ Priority / Cooldown / Latency。
- 每跳独立超时 + 流式 keepalive + 连接池背压保护。
- **Ingress 入口防护**：请求体大小上限（`max_request_body_bytes`，默认 10 MiB，超限 413；多模态请求另用 `max_multimodal_body_bytes` 默认 32 MiB，见 §3.6）+ 慢 body/慢读超时（`request_read_timeout`，默认 30s）+ **全局并发闸/有界排队**（`max_inflight_requests` 默认 1024、`max_queue_depth` 默认 256、`acquire_timeout` 默认 5s，满则 503，见 §3.7），均带默认值且可配置。
- **尊重上游**：透传上游 `Retry-After`/`RateLimit-*` 响应头给下游，并据此对该 target 礼让退避/冷却；不擅自改写上游语义参数。
- OAuth provider 接入：`AuthApplier` 的 `start/exchange/refresh`，token 刷新单飞（防并发失效）。
- 断连计费防逃：流式客户端中途断连时用 `UsageAccumulator` 兜底估算 usage 落账。

**交付物**
- `core::routing` 完整实现（RoutingTable / Strategy trait / HealthRegistry / FallbackPolicy + RetryPolicy + 预算/幂等闸门）。
- Ingress 防护中间件（body 上限 + 慢读超时 + 全局并发闸/有界排队，均可配置）。
- `providers` 至少 1 个 OAuth 类 provider（如 Anthropic 订阅或 Claude Code 渠道）。
- 文档：路由与故障转移说明、超时/重试矩阵、Gateway 透传/尊重上游/反馈下游契约（§3.5）、入口防护默认值与配置说明。

**验收标准**
- [ ] 主 target 注入 5xx/超时，请求自动转移到下一 target 并成功；400/422 不转移、401/403 不在同账号重试。
- [ ] 重试与转移分离生效：同 target 重试次数受 `RetryPolicy` 限制，总尝试受 `max_total_attempts`/`deadline` 限制，有测试覆盖组合上限。
- [ ] 流式已产出首字节后发生上游错误时不再重试/转移（幂等闸门测试）。
- [ ] 上游返回 429 + `Retry-After` 时，该头被透传给下游，且该 target 进入冷却（测试佐证）。
- [ ] 请求体超过 `max_request_body_bytes` 返回 413；慢 body 超过 `request_read_timeout` 被中断；并发超过 `max_inflight_requests` + 队列满/等待超时返回 503（带 `Retry-After`），被拒请求不计入熔断计数；以上默认值均可被配置覆盖。
- [ ] 连续失败触发熔断，冷却后半开自动恢复；有测试覆盖状态机。
- [ ] 4 种路由策略各有单测验证选择分布（如 Weighted 的权重比例、Priority 的分组顺序）。
- [ ] 并发刷新同一 OAuth 账号时只发生一次 refresh（单飞测试通过）。
- [ ] 慢上游（人为延迟）不耗尽连接池、不拖垮其他请求（基准/压测佐证）。
- [ ] 故障转移与熔断事件被记录进日志，可在事件流中查到。

---

### 阶段 3 — 接入广度（多协议 / 多后端）

**开发目标**
- 补齐协议：`responses`、`gemini(generate_content)`、`embeddings`（passthrough）。
- 三段式协议身份落地（`{suite}/{name}/{version}`，**单版本注册，暂不做版本共存路由**）+ `EndpointCapabilities` 富声明 + `inventory` 去中心注册。
- 批量接入 OpenAI-compatible provider（DeepSeek、Moonshot、Zhipu、xAI、Ollama 等）——以声明式元数据为主、零或极少 Rust 代码。
- SDK 逃生舱样板：`provider-bedrock` 独立 crate，演示自定义 `Executor`（自管签名 + event-stream）。
- 跨协议有损转换治理：`lossy_default_reject` 直接拒绝有损组合（**不做 `allow_lossy` 放行开关**，见 §3.2）；**维护字段级能力矩阵**（tool calling 并行/`tool_choice`、多模态承载方式、reasoning/结构化输出，见 §3.2），作为有损判定唯一来源。
- PassThrough 短路：同协议且无 mutation 时直接转发原始字节。

**交付物**
- `protocols` 增至 5 套；`providers` ≥ 8 个内置 + 自定义无限。
- `provider-bedrock` crate（重依赖隔离）。
- 协议能力矩阵文档（字段级无损/有损/不支持判定表，见 §3.2）+ provider 接入指南（如何新增一个 OpenAI-compatible provider）。

**验收标准**
- [ ] 5 个协议两两组合（N×N）转换有矩阵测试覆盖，关键字段不丢；不支持的有损转换被**直接拒绝**并返回清晰错误（无 `allow_lossy` 放行路径）。
- [ ] 能力矩阵的高风险维度有专项测试：并行 tool_call / `tool_choice=required` 无法在目标协议表达时按 `lossy_default_reject` 拒绝（非静默降级）；目标协议不支持的多模态承载方式（inline/URL/file-id）触发拒绝。
- [ ] 新增一个 OpenAI-compatible provider 仅需新增声明文件 + 一行 `inventory::submit!`，无需改注册中心（有示例 PR/测试佐证）。
- [ ] embeddings 端点 passthrough 正常，usage 计费字段被采集。
- [ ] Bedrock（或等价 SDK provider）通过 `Executor` 逃生舱完成一次非流式 + 流式调用。
- [ ] PassThrough 路径下请求体逐字节透传（无 re-serialize），有对比测试。
- [ ] `provider-bedrock` 的重依赖不出现在 `core` / 其他 provider 的依赖树中（`cargo tree` 验证）。

---

### 阶段 4 — 产品化（控制面 + 日志分析）

**开发目标**
- `admin`：Admin REST API（Provider / Route / API Key 的 CRUD）+ OAuth 交互回调；Admin API 用环境变量管理密钥（`TIYGATE_ADMIN_TOKEN`）做 bearer 鉴权，**不做复杂 RBAC**。
- DB 动态配置：运行时增删改 provider/route/key，无需发版；配置落 OLTP；**密钥 AES-GCM 静态加密**（主密钥来自 `TIYGATE_MASTER_KEY`）。
- 日志分析子系统：结构化事件 → **可插拔日志 sink，默认 OLTP 独立分区表**（与配置表分离），**暂不引入 ClickHouse**；支持配置**最大保留天数**（`log_retention_days`）按分区定时清理。事件模型含 `tenant_id`（预留留空）、`trace_id`/`span_id`/`traceparent`、`error_source` 等维度（见 §4.2）；`RawEnvelope` 对大 body 截断、inline 媒体默认只存元信息（见 §4.1）。
- 租户维度预留：`api_key` 表加可空 `tenant_id`、route/provider 加可空 `tenant_scope`，**初期留空不做隔离**（单租户，见 §3.9），仅为后续扩展预留。
- 分布式 trace 透传：入向提取/出向注入 W3C `traceparent`，网关 span 挂上下游链路（见 §4.8）。
- 分析能力：QPS / 错误率 / p50/p95/p99 延迟 / TTFB / 按 model·provider 的 token（成本字段在配置 `PriceProvider` 后才有值，初期无可靠数据源故默认留空）/ embedding 缓存命中率 / 熔断状态仪表盘；慢/失败请求下钻 + `RawEnvelope` 重放（脱敏）。
- 配额：按 key/租户 RPM/RPD/TPM/TPD，超限即时阻断；单副本内存计数 + 落库快照，多副本走 **Redis 原子计数**（见 §4.6）。
- 下游客户端 key：Admin API「创建—启用—删除」三态管理，删除即失效；**不做自动轮换/吊销编排**（见 §4.5）。
- 成本数据源：仅定义可插拔 `PriceProvider` trait 接口预留（见 §3.3），**本期不接具体数据源**。
- 缓存层：**仅 embedding 缓存（`cache` crate）**，chat/completion 不做响应缓存/流重放/语义缓存（见 §4.7）。
- WebUI（可选，独立构建产物）覆盖上述管理与分析视图。

**交付物**
- `admin` crate + `store`（配置 OLTP + 可插拔日志 sink 抽象）+ `cache` crate。
- 密钥加密模块 + Admin API bearer 鉴权中间件。
- **DB 迁移框架落地**：基于 sea-orm/sqlx 内建迁移，配置库与日志库分两套迁移序列，迁移文件编译期嵌入；提供独立 `migrate` 子命令。
- 日志事件 schema（见 §4.2）落地 + 仪表盘查询 + 保留清理任务。
- 可选 `webui/`。
- 文档：Admin API 参考、日志字段字典、配额配置说明、密钥加密与 env 密钥说明、迁移与 schema 演进指南（§5.1）。

**验收标准**
- [ ] 通过 Admin API 新增 provider/route/key 后，数据面无需重启即可路由到新配置（秒级生效）。
- [ ] `migrate` 子命令可独立执行；首次启动自动建表，schema 版本可查询。
- [ ] 日志事件落默认 sink（OLTP 独立分区表），支持按 model/provider/key/时间窗的聚合查询（响应时间达标）。
- [ ] 配置表与日志分区表逻辑分离，日志写入压力不显著影响配置读写（佐证）。
- [ ] 日志保留天数生效：超过 `log_retention_days` 的分区被定时清理。
- [ ] 配额超限请求被即时拒绝（429），计数准确，有并发测试（含多副本 Redis 计数场景）。
- [ ] embedding 缓存命中返回一致结果且省去上游调用；chat/completion 请求绕过缓存（不查不写）；响应/流重放/语义缓存均不在本期范围（有测试佐证作用域）。
- [ ] RawEnvelope 默认脱敏存储、可按采样率开关；超过 `raw_envelope_max_bytes` 的 body 被截断并标 `truncated`，inline 媒体默认只存元信息；慢请求可在仪表盘下钻并重放定位。
- [ ] 下游传入的 `traceparent` 被沿用为父链路、向上游注入；日志事件含 `trace_id`/`span_id`，可由日志跳转到对应 trace（有测试佐证）。
- [ ] provider 密钥/token 在 OLTP 中加密存储（密文落库、主密钥走 env）；日志与 UI 中敏感字段不明文泄露（安全检查）。
- [ ] Admin API 无有效 `TIYGATE_ADMIN_TOKEN` 时拒绝访问（401/403）。

---

### 阶段 5 — 规模化（高可用 / 可观测）

**开发目标**
- 控制面/数据面拆分部署：`all` + `proxy`/`admin` 拆分**两档**（`standalone` 边缘模式暂不做）。
- 数据面配置无状态水平扩展；共享 OLTP；跨副本配额走 Redis；OAuth 回调 sticky session。
- 配置 epoch 版本传播：数据面轮询/订阅变更，秒级热生效；支持按 epoch 回滚。
- **多副本迁移安全**：迁移先行（独立 `migrate` Job/子命令执行），数据面副本启动时只**校验** schema 版本、不自行跑迁移；遵循 expand/contract 保证滚动升级中新旧副本共存不崩（见 §5.1）。
- 健康探针：`/healthz`（liveness，draining 期间仍 200）+ `/readyz`（readiness，依赖可达；draining 即 503 摘流量）。
- **优雅排空与停机**：`SIGTERM`/`preStop` 触发 graceful drain——摘流量 → 放行存量请求（含长流式）→ `drain_timeout`（默认 30s）超时对未结束流发协议原生 error 帧 + `UsageAccumulator` 兜底计费 → flush 日志 channel + 有序释放资源后退出（见 §3.8）。
- 可观测对接：OTel GenAI semconv 导出 + Prometheus metrics endpoint。
- 发布工程：容器镜像 + 多平台二进制 + 升级/回滚说明。

**交付物**
- `server` 支持两档部署（`all` / `proxy`+`admin`）+ 探针 + 优雅排空（SIGTERM drain）+ epoch 传播与回滚 + 启动期 schema 版本校验。
- OTel/Prometheus exporter；容器化与发布脚本。
- 文档：部署拓扑、多副本运维手册（共享 DB、Redis 配额、sticky session、poll 间隔、回滚、迁移先行流程）。

**验收标准**
- [ ] 同一二进制可按 `--mode` 启动为 `all` / `proxy` / `admin`，行为符合预期。
- [ ] 多副本数据面共享 OLTP，一处改配置，所有副本在一个 poll 周期内生效；可按 epoch 回滚；配置切换时已在途请求用旧 epoch 完成、新请求用新 epoch，无半新半旧（in-flight 一致性测试）。
- [ ] Redis 不可用时配额计数自动降级为 per-instance 内存计数、共享缓存降级为进程内 LRU，请求不中断（降级演练佐证 fail-open 默认行为）。
- [ ] schema 版本与二进制不匹配的副本拒绝启动并报清晰错误；expand/contract 迁移下新旧副本可短暂共存不崩（滚动升级演练佐证）。
- [ ] 单副本宕机不影响整体（负载均衡后可用性达标）；控制面宕机时数据面仍可按现有配置转发。
- [ ] `/healthz`、`/readyz` 行为正确（依赖不可达时 `/readyz` 返回 503）。
- [ ] `SIGTERM` 后副本进入 draining：`/readyz` 转 503 摘流量、新请求 503、存量流式请求平稳收尾；超过 `drain_timeout` 的未结束流被协议原生 error 帧关闭并兜底计费，日志事件不丢（滚动升级演练佐证）。
- [ ] 关键指标（QPS、延迟分位、错误率、token/cost、熔断）经 OTel/Prometheus 正确导出，可被外部监控抓取。
- [ ] 提供容器镜像 + 多平台二进制 + 一键升级/回滚流程，并通过冒烟测试。

---

### 阶段对照速览

| 阶段 | 核心主题 | 主要质量目标 | 退出标志 |
| --- | --- | --- | --- |
| 1 | 内核 + 最小代理 | 易维护（分层/IR） | 跨协议打通、零配置可跑 |
| 2 | 稳定性层 | 稳定性 | 熔断/转移/超时达标 |
| 3 | 接入广度 | 可扩展 | 5 协议 + 多 provider + 逃生舱 |
| 4 | 产品化 | 易维护 + 日志分析 + 安全 | 动态配置 + 日志仪表盘 + 配额/embedding 缓存 + 密钥加密 |
| 5 | 规模化 | 稳定性 + 可观测 | 多副本 + 两档部署 + 探针 + 优雅排空 + 导出 |
