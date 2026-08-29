# Target 能力发现与协议转换实施方案

> 状态：设计草案
> 适用范围：TiyGate 当前支持的 Chat Completions、Anthropic Messages、OpenAI Responses、Gemini GenerateContent 与 Embeddings 协议
> 目标读者：`core`、`protocols`、`store`、`server`、`admin` 与 `webui` 维护者

## 1. 背景

TiyGate 当前通过 `ProtocolEndpoint` 与 `EndpointCapabilities` 描述协议级能力，并在 `check_lossy_conversion` 中拒绝已知有损的跨协议转换。这套机制能够回答“目标协议理论上能否表达某个字段”，但不能回答“这个具体上游 Target 是否真正实现了该能力”。

该差异在聚合服务和 OpenAI-compatible 服务中尤其明显：同一个 Provider、API Base 或 `/v1/responses` 端点可以暴露来自不同厂商的模型，各模型可能只实现 Responses API 的不同子集。例如某个 Target 可以支持普通文本和 SSE，却不支持 `namespace`、`custom`、Programmatic Tool Calling（PTC）或 Codex Responses Lite（CRL）的 `input[].additional_tools` 私有载体。

CRL 请求体现了当前问题：

```json
{
  "input": [
    {
      "type": "additional_tools",
      "role": "developer",
      "tools": [
        {
          "type": "namespace",
          "name": "functions",
          "tools": [
            { "type": "custom", "name": "exec" },
            { "type": "function", "name": "wait" }
          ]
        }
      ]
    }
  ],
  "tool_choice": "auto"
}
```

当该请求以 Responses → Responses 原样发送到只兼容公开 Responses 基础字段的第三方服务时，上游可能接受 HTTP 请求，却忽略 `additional_tools`，最终只返回普通 `message`。此时 `finish_reason=stop` 是正确结果，真正的问题是路由前没有识别 Target 的 wire capability，也没有选择 `additional_tools → 顶层 tools` 的兼容转换计划。

官方 OpenAI 文档也明确表明：使用相同 Responses API 外形的不同承载目标可以只支持能力子集，模型、区域与平台的能力可能不同，依赖具体工具或响应模式时应按实际工作负载验证：

- <https://developers.openai.com/api/docs/guides/tools>
- <https://developers.openai.com/api/docs/guides/amazon-bedrock#responses-api-feature-availability>

因此，Provider 身份、协议 suite、模型目录或一次 HTTP 200 都不足以证明某项能力可用。能力判断必须落到具体出站 Target。

## 2. 需求目标

### 2.1 核心目标

1. 建立可扩展、版本化的 Target 能力模型，不再依赖持续扩张的固定 bool 字段。
2. 将“协议固有表达能力”“Target 实际能力”和“TiyGate 转换能力”分离建模。
3. 在创建或修改路由 Target 时异步触发安全探测，并持久化带证据、TTL 与失败原因的能力画像。
4. 解析每个请求真实需要的能力，在 weighted、priority、cooldown、latency 排序前过滤不兼容 Target。
5. 为每个兼容 Target 生成独立的转换计划，例如原样透传、CRL 工具提升或标准跨协议转换。
6. 当没有兼容 Target 时返回明确的 `no_compatible_target` 错误，禁止静默丢字段后继续请求。
7. 让 Responses、Chat Completions、Messages、Gemini 和 Embeddings 共用同一能力注册、证据解析和路由过滤机制。
8. 保持 `core` 零 I/O；探测网络请求位于 `server`，持久化位于 `store`，管理接口位于 `admin`。

### 2.2 可靠性目标

1. 能力探测不得进入正常请求热路径。
2. 探测失败不得污染现有熔断、冷却和延迟 EWMA 状态。
3. `401/403`、`429`、`5xx`、超时和网络故障不得被误判为“不支持能力”。
4. `tool_choice=auto` 下模型未调用工具不得被作为负面能力证据。
5. 探测不得执行真实写操作、shell、computer use、远程 MCP、付费搜索或其他外部副作用。
6. 探测使用上游凭证，但任何持久化、日志和 Target Key 中不得出现明文凭证。

### 2.3 可维护性目标

1. 新增能力时优先增加注册表条目与探针实现，不要求新增数据库列。
2. 未被旧版本识别的能力 ID 必须能在数据库、导入导出和 Admin API 中原样保留。
3. 协议能力矩阵、能力注册表、运行时 resolver 和跨协议测试必须保持一致。
4. 探针套件必须有独立版本；探针语义变化后可使旧结果自动过期。

## 3. 非目标

本方案不试图实现以下事项：

1. 不保证通过有限探针穷举一个模型的全部行为。
2. 不通过发送超大输入探测最大上下文窗口或最大输出长度。
3. 不自动探测会产生费用、访问外部数据或触发副作用的 hosted tools。
4. 不把 Provider 名称、vendor 名称或模型名称前缀作为最终能力证明。
5. 不把健康、限流、配额、延迟等瞬时运行状态混入能力画像。
6. 不将供应商绑定的 encrypted reasoning、signature 或 replay state 跨供应商重写。
7. 不为每个协议组合实现“尽力而为”的静默降级；无法证明无损时保持 fail-closed。

## 4. 核心概念

### 4.1 三层能力模型

```text
Protocol Baseline
  协议或方言在 wire 上能够表达的最大集合，静态、版本化
          │
          ▼
Target Capability Profile
  具体 Target 实际实现的子集，动态、带证据和 TTL
          │
          ▼
Conversion Planner
  根据请求需求和 Target 能力生成转换计划，纯逻辑、无 I/O
```

三个层次不得混用：

- `messages` 不存在 `file_id` carrier，属于协议固有边界。
- 某个第三方 Responses Target 不支持 `namespace`，属于 Target 实现能力。
- TiyGate 可以把 CRL `additional_tools` 提升到顶层 `tools`，属于网关转换能力。

Target Profile 不应保存 `can_promote_additional_tools=true`。它只应保存：

```text
tools.crl.additional_tools = unsupported
tools.namespace = supported
tools.custom = supported
```

是否执行提升由 Conversion Planner 推导。

### 4.2 Target 身份

能力必须绑定到实际出站目标，而不是 Provider：

```text
TargetKey = SHA-256(
    provider_id,
    credential_scope_fingerprint,
    effective_api_base,
    egress_protocol_suite,
    egress_endpoint_name,
    egress_endpoint_version,
    exact_model_id
)
```

约束：

- `credential_scope_fingerprint` 是不可逆标识，不是 API Key 哈希的可验证副本。
- `api_base_override`、`api_key_override`、account、endpoint 或 model 发生变化时必须生成新的 TargetKey。
- 相同 Target 被多个 Route 引用时复用同一能力画像。
- 未再被引用的旧画像通过保留期后台清理，不在配置写入时同步删除。

当前 `RoutingTarget::health_key()` 只有 `provider_id:model_id`，本方案实施时应同步扩展为与实际目标实例一致的 key，避免多 API Base、多账号状态互相污染。

### 4.3 能力值与证据

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Constrained,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvidenceSource {
    ExplicitOverride,
    SemanticProbe,
    SuccessfulTraffic,
    ProviderDocumentation,
    ExactModelCatalog,
    ProtocolDefault,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityObservation {
    pub state: CapabilityState,
    pub value: Option<serde_json::Value>,
    pub source: EvidenceSource,
    pub confidence: Confidence,
    pub observed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub probe_version: Option<u32>,
    pub reason: Option<String>,
}
```

`Constrained` 用于表达集合、范围或子集，而不是强行压成 bool，例如：

```json
{
  "reasoning.effort.values": ["low", "medium", "high"],
  "structured.json_schema.keywords": [
    "properties",
    "required",
    "additionalProperties",
    "enum"
  ],
  "media.input.mime_types": ["image/png", "image/jpeg"]
}
```

### 4.4 能力描述注册表

能力定义应由可扩展注册表描述：

```rust
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub value_kind: CapabilityValueKind,
    pub scope: CapabilityScope,
    pub discovery: DiscoveryPolicy,
    pub dependencies: Vec<CapabilityId>,
    pub conversion_relevant: bool,
    pub probe_id: Option<String>,
}
```

建议目录：

```text
protocol-specs/capabilities/
├── registry.toml
├── baselines/
│   ├── chat-completions.toml
│   ├── messages.toml
│   ├── responses.toml
│   ├── responses-codex-lite.toml
│   ├── gemini.toml
│   └── embeddings.toml
└── probes/
    ├── core.toml
    ├── tools.toml
    ├── reasoning.toml
    └── structured-output.toml
```

`protocol-specs` 不在请求热路径动态读取。构建阶段校验 TOML 并生成静态 Rust 描述符；运行时只读取已编译的 registry。探针 TOML 只能引用受审计的 `probe_id`，不得携带任意命令、URL 或可执行脚本。

## 5. 当前协议能力整理

本节描述的是当前 TiyGate codec 与 IR 可以表达或转换的协议固有能力上限，不代表每个上游 Target 都已实现。

### 5.1 主要能力矩阵

| 能力 | Chat Completions | Messages | Responses | Gemini | Embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| 文本生成 | ✅ | ✅ | ✅ | ✅ | N/A |
| SSE streaming | ✅ | ✅ | ✅ | ✅ | ❌ |
| function tools | ✅ | ✅ | ✅ | ✅ | ❌ |
| `tool_choice=required` | ✅ | ✅ | ✅ | ✅ | ❌ |
| 指定具体函数 | ✅ | ✅ | ✅ | ✅ | ❌ |
| parallel tool 语义 | ✅ | 固有有损 | ✅ | 固有有损 | ❌ |
| custom tools | ✅ | ❌ | ✅ | ❌ | ❌ |
| namespace tools | 当前不建模 | ❌ | ✅ | ❌ | ❌ |
| hosted tools | ❌ | ❌ | ✅ | ❌ | ❌ |
| PTC/program/caller | ❌ | ❌ | ✅ | ❌ | ❌ |
| CRL `additional_tools` | ❌ | ❌ | CRL 方言 | ❌ | ❌ |
| reasoning 内容块 | 有限 | ✅ | ✅ | ✅ | ❌ |
| encrypted reasoning | 有损 | ✅ | ✅ | 有损 | ❌ |
| reasoning mode/context | ❌ | ❌ | ✅ | ❌ | ❌ |
| JSON object/schema | ✅ | ✅，Schema 子集 | ✅ | ✅，Schema 子集 | ❌ |
| deterministic seed | ✅ | ❌ | ❌ | ❌ | ❌ |
| image inline | ✅ | ✅ | ✅ | ✅ | ❌ |
| image URL | ✅ | 固有不能直接表达 | ✅ | ✅ | ❌ |
| file_id | ❌ | ❌ | ✅ | ❌ | ❌ |
| audio inline | ❌ | ❌ | ✅ | ✅ | ❌ |
| video inline | ❌ | ❌ | ❌ | ✅ | ❌ |
| explicit cache breakpoint | ✅ | 无等价载体 | ✅ | 无等价载体 | ❌ |
| verbosity | ✅ | ❌ | ✅ | ❌ | ❌ |
| structured stop details | 有损 | ✅ | 有损 | 有损 | N/A |
| Multi-agent | ❌ | ❌ | Responses-only | ❌ | ❌ |
| embedding vector | ❌ | ❌ | ❌ | ❌ | ✅ |

该矩阵的详细 carrier、损失规则和例外继续以 `docs/protocol-capability-matrix.md` 为静态来源。本方案新增的 Target Profile 只能收窄协议基线，不能把协议固有不支持改成支持；服务商私有扩展必须注册为独立 dialect。

### 5.2 Generation 参数

| 参数 | Chat | Messages | Responses | Gemini |
|------|:---:|:---:|:---:|:---:|
| max tokens | ✅ | ✅，必填 | ✅ | ✅ |
| temperature | ✅ | ✅ | ✅ | ✅ |
| top_p | ✅ | ✅ | ✅ | ✅ |
| top_k | ❌ | ✅ | ❌ | ✅ |
| stop sequences | ✅ | ✅ | ✅ | ✅ |
| frequency penalty | ✅ | ❌ | 当前未映射 | 当前未映射 |
| presence penalty | ✅ | ❌ | 当前未映射 | 当前未映射 |
| seed | ✅ | ❌ | ❌ | ❌ |
| reasoning effort | ✅ | ✅ | ✅ | ✅，部分近似 |
| verbosity | ✅ | ❌ | ✅ | ❌ |

### 5.3 能力命名空间

第一版 registry 至少覆盖：

```text
transport.http
transport.sse
transport.websocket

generation.max_output_tokens
generation.temperature
generation.top_p
generation.top_k
generation.stop_sequences
generation.frequency_penalty
generation.presence_penalty
generation.seed

tools.function
tools.function.continuation
tools.choice.required
tools.choice.specific
tools.parallel
tools.namespace
tools.custom
tools.hosted.web_search
tools.hosted.file_search
tools.hosted.code_interpreter
tools.hosted.computer_use
tools.remote_mcp
tools.programmatic
tools.crl.additional_tools

reasoning.plaintext
reasoning.summary
reasoning.encrypted_replay
reasoning.effort.values
reasoning.budget_tokens
reasoning.mode
reasoning.context

structured.json_object
structured.json_schema
structured.json_schema.strict
structured.json_schema.keywords

media.input.image.inline
media.input.image.url
media.input.file_id
media.input.audio.inline
media.input.video.inline
media.image.detail.values

cache.prompt.key
cache.prompt.retention
cache.prompt.options
cache.prompt.breakpoint

output.annotations.url
output.annotations.file
output.refusal
output.stop_details
output.usage.reasoning_tokens
output.usage.cache_read_tokens
output.usage.cache_write_tokens

extensions.codex.local_shell
extensions.codex.custom_tool
extensions.codex.tool_search_items
extensions.multi_agent

embeddings.input.batch
embeddings.dimensions
embeddings.encoding_formats
```

## 6. 探测范围与策略

### 6.1 默认主动探测

默认探测必须低成本、无副作用，并且具有确定的成功条件。

| 能力 | 探测方式 | 成功条件 |
|------|----------|----------|
| endpoint/auth/model | 最小非流文本请求 | 2xx 且返回协议合法文本 |
| SSE | 最小流式请求 | 首帧、内容帧、终止帧均合法 |
| function | 强制 no-op function | 返回预期 call 与 nonce |
| function continuation | 回传 no-op output | 返回最终 message |
| tool choice required | `required` + 唯一函数 | 必须产生调用 |
| tool choice specific | 指定 probe 函数 | 调用名称准确 |
| parallel tools | 两个只读 no-op 函数 | 同一 turn 返回两个调用 |
| namespace | namespace 内 no-op 函数 | 调用身份和参数准确 |
| custom | 自由文本 no-op custom | 返回 custom call 与原始输入 |
| CRL additional_tools | CRL 与顶层 tools A/B | 分别记录两种 wire 结果 |
| PTC | no-op programmatic tool | 返回 program/caller 并可续传 |
| json_object | 固定小对象 | 输出可解析 JSON |
| json_schema strict | 小型 strict schema | 输出满足 schema |

工具探针必须使用随机 nonce，防止模型根据提示直接伪造静态结果。探测程序只验证“模型请求调用工具”，不执行真实业务逻辑。

### 6.2 条件主动探测

以下能力仅在模型目录、服务商声明、已有流量证据或管理员显式要求时探测：

| 能力 | 原因 |
|------|------|
| reasoning effort 各值 | 需要多次模型调用 |
| reasoning summary | 不保证每次返回 |
| encrypted reasoning replay | 需要多轮状态，且 provider-bound |
| WebSocket | 依赖代理和升级链路 |
| image inline/URL | 需要测试素材和多模态模型 |
| audio/video | 成本和模型限制较高 |
| JSON Schema 关键词子集 | 组合数量大 |
| prompt cache | 需要重复请求验证命中 |
| embedding dimensions/encoding | 仅 embedding Target 需要 |

### 6.3 不自动主动探测

以下能力只接受显式配置、官方目录或被动流量证据：

| 能力 | 原因 |
|------|------|
| hosted web/file search | 可能计费并访问外部数据 |
| remote MCP | 需要真实外部 MCP Server |
| shell/computer use | 有副作用和权限风险 |
| image generation | 成本较高 |
| 最大 context/output | 不能通过超大请求安全撞限额 |
| rate limit/quota | 属于瞬时运行状态 |
| safety/refusal | 不应主动构造违规内容 |
| citations | 无法稳定强制生成 |
| Multi-agent | Beta、成本高且可能创建执行单元 |
| 跨供应商 encrypted signature | 数据供应商绑定，禁止通用转换 |
| cache retention 实际时长 | 服务端内部行为，成本不可控 |

### 6.4 证据分类规则

```text
明确 400 unknown/unsupported field
  → 对应能力 Unsupported

强制工具调用返回正确 call + nonce
  → 对应调用能力 Supported

tool output 回传后产生最终 message
  → continuation Supported

auto 模式未调用工具
  → 不更新能力，保持 Unknown

401/403
  → ProbeError::Auth，不更新能力

429
  → ProbeError::RateLimited，不更新能力

5xx/timeout/network
  → ProbeError::Transient，不更新能力

200 但字段被静默忽略
  → 只有在 forced/controlled A/B 探针下才能判 Unsupported
```

能力不支持不是健康失败，不调用 `HealthRegistry::record_failure`；探测成功也不作为业务健康样本写入 latency EWMA。

## 7. 能力解析与转换规划

### 7.1 解析优先级

```text
显式 Target 覆盖
  > 未过期的语义探测
  > 已验证的真实流量
  > exact-model 静态映射
  > 模型目录
  > Unknown
```

协议基线是 hard ceiling，不参与上述覆盖：如果基础协议无法表达某项能力，Target 观测不能将其改为支持。若上游有扩展，需要选择包含该扩展的 dialect baseline。

### 7.2 请求能力提取

新增纯函数：

```rust
pub fn derive_request_requirements(
    request: &IrRequest,
    ingress: &ProtocolEndpoint,
) -> RequestRequirements;
```

`RequestRequirements` 应一次返回完整需求集合，而不是像当前 lossy guard 一样只返回第一个损失维度。示例：

```json
{
  "required": [
    "transport.sse",
    "tools.function",
    "tools.custom",
    "reasoning.plaintext"
  ],
  "constraints": {
    "reasoning.effort.values": ["high"],
    "media.input.mime_types": ["image/png"]
  }
}
```

### 7.3 转换计划

```rust
pub enum RequestTransform {
    PassThrough,
    PromoteCrlAdditionalTools,
    ConvertProtocol,
    MapPlaintextReasoning,
    DropDocumentedOptionalField,
}

pub struct PlannedTarget {
    pub target: RoutingTarget,
    pub capabilities: ResolvedTargetCapabilities,
    pub transforms: Vec<RequestTransform>,
}
```

规划顺序：

```text
Ingress decode
  → RequestRequirements
  → 为每个 Target 解析 effective capabilities
  → 枚举可用转换计划
  → 过滤无法满足需求的 Target
  → 对 PlannedTarget 执行路由策略排序
  → 按 Target 自己的 plan 编码和发送
```

不同 Target 可以得到不同计划，不能为整条 Route 只生成一个全局转换结果。

### 7.4 CRL 场景

CRL `additional_tools` 的要求不是单一 capability，而是两条可替代路径：

```text
Native Plan:
  requires tools.crl.additional_tools
  transform PassThrough

Promotion Plan:
  requires tools.namespace
  requires tools.custom（仅当载体包含 custom）
  requires tools.function（仅当载体包含 function）
  transform PromoteCrlAdditionalTools
```

示例：

```text
Target A
  additional_tools = Supported
  → 原样透传

Target B
  additional_tools = Unsupported
  namespace = Supported
  custom = Supported
  → 将 additional_tools.tools 合并到顶层 tools

Target C
  additional_tools = Unsupported
  namespace = Unsupported
  → 路由前排除
```

提升规则：

1. 收集全部 `input[].type == "additional_tools"`。
2. 保留工具出现顺序。
3. 与已有顶层 `tools` 合并。
4. 按 `(type, name)` 去重。
5. 完全相同的重复定义跳过。
6. 同身份但定义冲突时返回 `400 invalid_request`。
7. 删除已提升的 carrier item。
8. 保留 `tool_choice` 与 `parallel_tool_calls`。
9. 不自动把 namespace 展开或重命名为普通 function。

Responses decoder 需要先把 `additional_tools` 作为有序 opaque item 保存，并解析其内部工具类型生成 requirements。它不能继续退化为空 developer message。

### 7.5 无兼容目标错误

```json
{
  "error": {
    "type": "no_compatible_target",
    "message": "No routing target can preserve the requested capabilities",
    "required": [
      "tools.namespace",
      "tools.custom"
    ],
    "targets": [
      {
        "target_key": "...",
        "missing": ["tools.custom"],
        "unknown": []
      }
    ]
  }
}
```

对含工具、reasoning replay、structured output 等高语义影响请求，`Unknown` 默认视为不满足；管理员可通过显式 Target override 改变该策略。

## 8. 持久化设计

新增 SQLite 与 PostgreSQL 对等迁移：

```sql
CREATE TABLE target_capability_profiles (
    target_key TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    account_fingerprint TEXT,
    api_base TEXT NOT NULL,
    protocol_suite TEXT NOT NULL,
    endpoint_name TEXT NOT NULL,
    endpoint_version TEXT NOT NULL,
    model_id TEXT NOT NULL,

    schema_version INTEGER NOT NULL,
    probe_suite_version INTEGER NOT NULL,
    probe_status TEXT NOT NULL,
    capabilities_json TEXT NOT NULL,
    evidence_json TEXT NOT NULL,

    last_error_class TEXT,
    last_error TEXT,
    probed_at TEXT,
    expires_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

`probe_status`：

```text
pending
running
complete
partial
failed
stale
```

另外新增 `target_capability_overrides`，或在 profile 内使用独立 `overrides_json`。人工覆盖必须与探测结果分开保存，重探测不得覆盖人工配置。

能力画像默认保留 JSON，以便新增能力不做表结构迁移。常用筛选字段后续可以增加索引投影，但第一阶段不提前优化。

## 9. 探测生命周期

### 9.1 路由创建和更新

```text
POST/PUT Route
  1. 先验证并持久化配置
  2. 为每个启用 Target 计算 TargetKey
  3. 复用未过期且 probe_suite_version 一致的画像
  4. 对缺失或过期画像写入 pending
  5. 向后台 ProbeScheduler 入队
  6. 立即返回，不等待完整探测
```

Target 在 `pending` 时：

- 普通文本请求可按协议基线和现有证据使用。
- 需要工具、reasoning replay、structured schema 等能力的请求不选择 Unknown Target。
- 若 Route 没有任何满足需求的 Target，返回 `no_compatible_target`。

### 9.2 自动重探测触发条件

1. 创建或启用 Target。
2. 修改 API Base、协议、endpoint、model、account 或 credential scope。
3. profile TTL 到期。
4. `probe_suite_version` 升级。
5. 模型目录版本变化且影响相关能力。
6. 真实流量收到明确 capability/schema 4xx。
7. 管理员点击“重新探测”。

### 9.3 调度与成本控制

- 使用全局和 per-provider semaphore 限制并发。
- 相同 TargetKey 的并发任务合并为 single-flight。
- 每个探针设置独立超时和最大输出 token。
- 默认不对能力错误重试；瞬时网络错误最多进行一次受限重试。
- 设置单 Target 每日探测预算和全局预算。
- 探测请求标记内部 `probe_run_id`，不计入下游 API Key 配额。
- 探测产生的上游费用仍需在 Admin UI 中可观测。

## 10. Admin API 与 WebUI

### 10.1 Admin API

新增接口：

```text
GET  /admin/v1/target-capabilities/:target_key
POST /admin/v1/target-capabilities/:target_key/probe
PUT  /admin/v1/target-capabilities/:target_key/overrides
DELETE /admin/v1/target-capabilities/:target_key/overrides/:capability_id
GET  /admin/v1/capability-registry
```

Route 创建和查询响应增加：

```json
{
  "target_key": "...",
  "probe_status": "pending",
  "capability_summary": {
    "supported": 4,
    "unsupported": 1,
    "unknown": 7
  }
}
```

### 10.2 WebUI

Route Target 行增加能力状态入口，详情页显示：

- 当前 dialect。
- probe 状态、版本、最后探测时间、TTL。
- Supported / Unsupported / Constrained / Unknown。
- 每项能力的证据来源和失败原因。
- “重新探测”和“人工覆盖”。
- 预计探测成本和可能调用的安全探针类别。
- Target 被某次请求排除时的 missing capabilities。

人工把 `Unsupported` 覆盖为 `Supported` 时必须二次确认，并说明这可能导致上游 4xx 或静默语义丢失。

## 11. Crate 分层

| Crate/目录 | 职责 |
|------------|------|
| `crates/core` | `CapabilityId`、值类型、requirements、resolver、planner、兼容性错误；纯逻辑、零 I/O |
| `crates/protocols` | 协议基线、IR requirement 提取辅助、各协议转换规则、CRL opaque item 建模 |
| `crates/store` | SQLite/PostgreSQL migration、profile/override CRUD、TTL 查询 |
| `crates/server` | ProbeScheduler、HTTP/SSE 探针执行、安全预算、路由前规划、按 Target 应用 transform |
| `crates/admin` | capability/probe/override API 与审计日志 |
| `webui` | Target 能力状态、证据详情、重探测和人工覆盖 |
| `protocol-specs` | registry、协议/dialect baseline 与 probe 元数据来源 |

触及 crate 依赖后必须运行 `scripts/verify-deps.sh`，确保 `core` 不依赖 `store`、`server`、provider SDK 或网络实现。

## 12. 实施步骤

### 阶段 0：能力清单与契约冻结

1. 将本方案的能力命名空间写入 `protocol-specs/capabilities/registry.toml`。
2. 为五类协议建立 baseline，并为 Responses 增加 `responses-codex-lite` dialect。
3. 将 `docs/protocol-capability-matrix.md` 的每个转换维度映射到 capability ID。
4. 为 registry 增加唯一性、value kind、依赖关系和 baseline 引用校验。
5. 明确第一阶段探测白名单；未列入白名单的 `probe_id` 构建失败。

交付物：能力 ID 注册表、协议 baseline、校验测试和更新后的能力矩阵。

### 阶段 1：Core 数据模型

1. 在 `core` 新增 capability 模块。
2. 实现 `CapabilityState`、`CapabilityObservation`、`ResolvedTargetCapabilities`。
3. 实现 `RequestRequirements` 和逻辑表达式 `AllOf/AnyOf/Not`。
4. 实现纯函数 `CapabilityResolver`。
5. 实现返回所有缺失项的 `CompatibilityReport`。
6. 保留现有 `EndpointCapabilities` 作为兼容层，由 baseline 适配器读取，避免一次性重写所有 codec。

交付物：无 I/O 的可单测能力解析与兼容判断。

### 阶段 2：TargetKey 与持久化

1. 实现规范化 TargetKey，覆盖 effective API Base、endpoint、model、account 与 credential scope。
2. 修正 health key 的碰撞问题，但保持健康与能力状态存储分离。
3. 增加 SQLite/PostgreSQL migration。
4. 在 `store` 增加 profile、override、TTL 和 stale 查询。
5. 更新配置导入导出策略：能力画像默认不随业务配置导出；人工 override 可选择导出。
6. 增加旧数据库迁移和重复 Target 复用测试。

交付物：持久化能力画像和人工覆盖。

### 阶段 3：安全探测框架

1. 实现 `ProbeScheduler`、single-flight、并发限制、预算和超时。
2. 实现基础 HTTP/non-stream/SSE 探针。
3. 实现 function、continuation、required、specific 和 parallel 探针。
4. 实现 namespace、custom、CRL A/B 与 PTC 探针。
5. 实现 json_object 与基础 strict schema 探针。
6. 建立错误分类，不把 auth/rate-limit/transient 归类为 Unsupported。
7. 探测事件进入独立审计/指标，不进入业务请求配额和 HealthRegistry。

交付物：第一版 `probe_suite_version=1`。

### 阶段 4：CRL 建模与提升

1. Responses decoder 识别 `input[].additional_tools`。
2. 保存原始 item、原始 index 和内部工具类型。
3. 消除当前未知 item 退化为空 developer message 的行为。
4. 实现 `PromoteCrlAdditionalTools` 的合并、去重和冲突拒绝。
5. 原生 CRL Target 保持 raw passthrough。
6. 标准 Responses Target 仅在依赖能力全部满足时提升。
7. 增加 non-stream、SSE 和 continuation 回归测试。

交付物：当前 Codex → 第三方 Responses 工具调用场景闭环。

### 阶段 5：路由前过滤与按 Target 规划

1. 在策略排序前调用 `derive_request_requirements`。
2. 为每个 Target 解析 profile 并生成 `PlannedTarget`。
3. 过滤 incompatible Target，Unknown 按保守策略处理。
4. 将 weighted/priority/cooldown/latency 应用于剩余 `PlannedTarget`。
5. Executor 使用当前 Target 自己的 transform plan。
6. 无候选时返回包含完整缺失维度的 `no_compatible_target`。
7. 将 capability filter 决策写入 request attempt telemetry。

交付物：能力感知路由和明确失败。

### 阶段 6：Admin API 与 WebUI

1. 增加 capability registry、profile、probe 和 override API。
2. Route 创建/更新异步入队探测。
3. WebUI 展示 probe 状态、TTL、证据和限制。
4. 增加手动重探测和人工 override。
5. 审计所有 override 和 probe 操作。

交付物：可运营、可解释的目标能力管理界面。

### 阶段 7：扩展到其他转换维度

按风险和收益逐步加入：

1. reasoning effort、summary 与 replay。
2. structured-output keyword constraints。
3. image URL/inline/file_id。
4. prompt caching。
5. Gemini/Anthropic 特定 thinking 约束。
6. Embeddings batch、dimensions 与 encoding format。
7. 被动流量证据和 capability-related 4xx 自动 stale/reprobe。

每新增一项必须同步更新 registry、baseline、probe policy、matrix 和测试。

## 13. 验收标准

### 13.1 能力注册表

- [ ] 新增 capability ID 不需要增加数据库列。
- [ ] 重复 ID、未知 value kind、无效依赖和循环依赖在构建或测试时失败。
- [ ] 未知 capability ID 可以经过 DB、Admin API 和导入导出原样往返。
- [ ] 每个 conversion-relevant capability 都能追溯到协议矩阵条目或 dialect 文档。
- [ ] `core` 不执行文件、数据库或网络 I/O。

### 13.2 Target 身份与持久化

- [ ] 相同 Provider/model 但不同 API Base 或 account 生成不同 TargetKey。
- [ ] 相同实际 Target 被多个 Route 引用时复用 profile。
- [ ] API Base、endpoint、model、account 或 credential scope 更新后旧结果不再生效。
- [ ] SQLite 和 PostgreSQL migration、CRUD、TTL、stale 行为一致。
- [ ] 明文 API Key、OAuth token 和其他凭证不进入 TargetKey、profile、日志或错误体。

### 13.3 探测正确性

- [ ] 强制 function 探针只有收到正确 call 和 nonce 才判 Supported。
- [ ] function output continuation 成功后才标记 continuation Supported。
- [ ] `tool_choice=auto` 未调用工具保持 Unknown。
- [ ] 明确 schema 400 可判 Unsupported。
- [ ] `401/403`、`429`、`5xx`、timeout 保持 Unknown 并记录正确 ProbeError。
- [ ] 探测请求不修改 HealthRegistry、EWMA、下游 quota 和业务 request log 统计。
- [ ] 默认探针不执行 hosted search、shell、computer use、MCP、Multi-agent 或其他外部副作用。
- [ ] 相同 TargetKey 的并发探测只产生一次上游执行。

### 13.4 CRL 场景

- [ ] 原生 CRL Target 收到字节等价的 `additional_tools` 请求。
- [ ] 标准 Responses Target 在能力满足时收到提升后的顶层 `tools`，且 carrier 已移除。
- [ ] 已有顶层工具和 carrier 工具按稳定顺序合并。
- [ ] 相同定义去重，冲突定义明确返回 400。
- [ ] namespace/custom/PTC 任一必需能力缺失时不执行提升。
- [ ] 工具调用响应被识别为 `FinishReason::ToolCalls`，而不是 `Stop`。
- [ ] tool result continuation 能返回最终 assistant message。

### 13.5 路由与转换

- [ ] incompatible Target 在 weighted/priority/latency 排序前被过滤。
- [ ] 第一目标不兼容、第二目标兼容时，直接选择第二目标，不向第一目标发送业务请求。
- [ ] 不同 Target 可以获得不同转换计划。
- [ ] 无兼容目标时返回 `no_compatible_target` 和完整 missing/unknown 列表。
- [ ] 协议固有不支持不能被 Target 探测结果越权扩展。
- [ ] 现有跨协议 lossy conversion 测试全部继续通过。

### 13.6 Admin 与可观测性

- [ ] Route 创建不等待完整探测，响应能显示 `pending`。
- [ ] UI 能区分 Supported、Unsupported、Constrained、Unknown 和 Probe Error。
- [ ] UI 能显示证据来源、探测版本、最后时间、TTL 和失败原因。
- [ ] 管理员可以重探测和添加/删除 override。
- [ ] 重探测不覆盖人工 override。
- [ ] 所有 override 和 probe 操作均写入 audit log。
- [ ] request attempt 日志能解释 Target 因哪项能力被排除。

### 13.7 工程质量门禁

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] `scripts/verify-deps.sh`
- [ ] `npm --prefix webui run lint`
- [ ] `git diff --check`
- [ ] 新增探针均有 wiremock 成功、明确不支持和瞬时错误测试。
- [ ] 新增转换维度均有 `crates/protocols/tests/` 与 `crates/server/tests/` 回归测试。

## 14. 风险与缓解

| 风险 | 缓解措施 |
|------|----------|
| 模型输出随机导致误判 | 强制工具、唯一 nonce、受控 A/B、失败保持 Unknown |
| 探测成本增长 | 分阶段、预算、TTL、single-flight、只探测相关能力 |
| 第三方静默忽略字段 | 验证语义输出，不以 HTTP 200 为成功 |
| 能力随模型升级漂移 | probe suite version、TTL、4xx 触发 stale |
| profile 错误过滤正常流量 | 人工 override、证据展示、普通文本与高级能力分级策略 |
| 能力模型无限膨胀 | registry ownership、命名规范、value kind 和依赖校验 |
| 探测影响健康与路由 | 独立状态、独立指标、禁止写 HealthRegistry |
| 私有扩展污染标准协议 | dialect baseline 隔离，不修改协议 hard ceiling |
| 多副本重复探测 | DB lease 或分布式 single-flight，第一阶段至少进程内合并 |

## 15. 回滚策略

1. 所有能力感知路由由运行时设置 `target_capability_routing_enabled` 控制。
2. CRL 提升由独立设置 `responses_crl_tool_promotion_enabled` 控制。
3. 关闭能力路由后恢复现有目标排序和 fallback，不删除已存 profile。
4. ProbeScheduler 可以独立暂停，不影响正常请求。
5. 数据库新增表为旁路表，回滚二进制后旧版本忽略，不修改现有 Provider/Route 主表语义。
6. 人工 override 在功能关闭期间保留，重新启用后继续生效。

## 16. 完成定义

本方案完成的最低闭环是：

```text
创建第三方 Responses Target
  → 异步探测 CRL 与顶层 tools
  → 保存带证据的能力画像
  → Codex CRL 请求到达
  → 路由前选择原生透传或工具提升计划
  → 不兼容 Target 被过滤
  → 上游产生真实工具调用
  → TiyGate 返回 ToolCalls finish reason
  → tool output 可继续并得到最终回答
```

在此闭环通过后，再将同一机制扩展到 reasoning、structured output、multimodal、prompt caching 和其他协议转换维度。
