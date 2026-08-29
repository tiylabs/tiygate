# Target 能力发现与协议转换实施方案

> 状态：修订后实施基线
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
9. 使用独立内存快照向请求热路径提供已解析能力，正常请求不得同步查询能力数据库。
10. 使用可恢复、可抢占的持久化探测任务，保证进程重启和多副本部署下的幂等执行。

### 2.2 可靠性目标

1. 能力探测不得进入正常请求热路径。
2. 探测失败不得污染现有熔断、冷却和延迟 EWMA 状态。
3. `401/403`、`429`、`5xx`、超时和网络故障不得被误判为“不支持能力”。
4. `tool_choice=auto` 下模型未调用工具不得被作为负面能力证据。
5. 探测不得执行真实写操作、shell、computer use、远程 MCP、付费搜索或其他外部副作用。
6. 探测使用上游凭证，但任何持久化、日志和 Target Key 中不得出现明文凭证。
7. 能力画像过期或重探测发生瞬时错误时采用 stale-while-revalidate，不得立即使已验证流量失去可用 Target。
8. 能力路由按 `off → shadow → enforce` 启用，升级后不得因历史 Target 缺少画像而直接中断现有流量。

### 2.3 可维护性目标

1. 新增能力时优先增加注册表条目与适用的发现实现，不要求新增数据库列，也不强制提供主动探针。
2. 未被旧版本识别的能力 ID 必须能在数据库、导入导出和 Admin API 中原样保留。
3. 协议能力矩阵、能力注册表、运行时 resolver 和跨协议测试必须保持一致。
4. 探针套件必须有独立版本；探针语义变化后可使旧结果自动过期。
5. 能力描述必须声明值类型、约束匹配语义、实现状态和探测策略，禁止由 planner 解释任意 JSON。
6. 具体协议和方言的 requirement 提取与 transform 实现位于 `protocols`，不得进入 `core`。

## 3. 非目标

本方案不试图实现以下事项：

1. 不保证通过有限探针穷举一个模型的全部行为。
2. 不通过发送超大输入探测最大上下文窗口或最大输出长度。
3. 不自动探测会产生费用、访问外部数据或触发副作用的 hosted tools。
4. 不把 Provider 名称、vendor 名称或模型名称前缀作为最终能力证明。
5. 不把健康、限流、配额、延迟等瞬时运行状态混入能力画像。
6. 不将供应商绑定的 encrypted reasoning、signature 或 replay state 跨供应商重写。
7. 不为每个协议组合实现“尽力而为”的静默降级；无法证明无损时保持 fail-closed。
8. 不向普通客户端暴露 TargetKey、API Base、account 或完整路由拓扑；逐 Target 兼容报告只进入内部遥测与 Admin API。
9. 不要求第一版实现或主动探测能力目录中的全部条目；能力目录、运行时实现和探测白名单分别版本化。

## 4. 核心概念

### 4.1 三层能力模型

```text
Protocol / Dialect Baseline
  声明 wire carrier 的 Supported、Forbidden 或 ExtensionUnknown 上限
          │
          ▼
Target Capability Profile
  以具体出站身份为范围，保存实际支持情况、约束、证据和有效期
          │
          ▼
Conversion Planner
  组合请求契约、Target 能力和已注册转换，生成每个 Target 的独立计划
```

三个层次的职责固定如下：

- `messages` 不存在 `file_id` carrier，属于协议基线的 `Forbidden`。
- 标准 Responses 未定义 CRL `additional_tools`，但 JSON item 是可扩展载体，属于 `ExtensionUnknown`。
- 某个第三方 Responses Target 不支持 `namespace`，属于 Target Profile。
- TiyGate 可以把 CRL `additional_tools` 提升到顶层 `tools`，属于 `protocols` 注册的转换能力。

协议基线使用三值上限，避免私有方言发现与 hard ceiling 形成循环：

```rust
pub enum BaselineSupport {
    Supported,
    Forbidden,
    ExtensionUnknown,
}
```

- `Forbidden` 不能被 Target 观测或人工 override 越权改为支持。
- `Supported` 仍可被具体 Target 收窄为 Unsupported 或 Constrained。
- `ExtensionUnknown` 只有在显式 dialect、精确静态映射、语义探测或人工 override 提供证据后才能变为 Supported。

Target Profile 不保存 `can_promote_additional_tools`。它只保存原始 wire 能力；是否提升由具体协议 planner 推导。

### 4.2 Protocol Dialect

协议身份由 endpoint 与 dialect 共同组成：

```text
WireProfileId = ProtocolEndpoint + DialectId
```

第一版内置 dialect：

```text
openai-chat-standard
anthropic-messages-standard
openai-responses-standard
openai-responses-codex-lite
gemini-generate-content-standard
openai-embeddings-standard
```

Ingress dialect 根据实际 wire 特征解析，不依赖客户端名称：

- `input[].type == "additional_tools"` 产生 `tools.crl.additional_tools` requirement。
- Codex/Multi-agent Beta header 或 item 只影响其对应 extension requirement。
- 未出现私有特征时保持标准 dialect。

Egress dialect 支持 `explicit` 与 `auto`：

- `explicit` 由 Route Target 配置固定，适用于已知私有端点。
- `auto` 以标准 dialect 为基线，允许 Target 证据启用 `ExtensionUnknown` 能力，但不会因单一能力成功而自动宣称整个私有 dialect 已实现。
- 探测可以给出 `detected_extensions`，只有能够唯一证明完整方言契约时才给出 `detected_dialect_id`。

`RouteTarget` 增加可选 `egress_dialect_id`；未设置时为 `auto`。dialect 参与 TargetKey、能力解析、探针选择和 Admin 展示。

### 4.3 Target 身份

能力必须绑定到实际出站目标，而不是 Provider：

```text
CanonicalTargetIdentity = {
    identity_version,
    provider_id,
    credential_scope_fingerprint,
    canonical_api_base,
    egress_protocol_suite,
    egress_endpoint_name,
    egress_endpoint_version,
    egress_dialect_id_or_auto,
    exact_model_id
}

TargetKey = SHA-256(canonical-json(CanonicalTargetIdentity))
```

规范化规则：

- scheme 与 host 小写，移除默认端口和多余尾斜杠，保留有语义的 path prefix。
- API Base 禁止 URL userinfo；query 默认禁止，确需使用时只允许显式白名单键并在身份中使用脱敏规范值。
- model ID、endpoint ID 和 dialect ID 使用精确值，不根据名称前缀归并。
- `api_base_override`、endpoint、dialect、model 或 credential scope 实质变化时生成新的 TargetKey。
- weight、enabled、Route 顺序和虚拟模型名不属于 Target 身份。

`credential_scope_fingerprint` 使用版本化 keyed HMAC，密钥来自安装级持久化随机 secret；该 secret 由 `store` 保护，不导出、不记录，主密钥轮换只重新保护 secret，不改变指纹：

- API Key：对实际有效 key 计算 HMAC，不保存可离线验证的裸哈希。
- OAuth：绑定稳定的 grant/account/tenant/scope 身份，不使用会轮换的 access token。
- IAM：绑定 account、role/principal 和权限范围，不使用临时 session token。
- 无认证：使用固定的 `anonymous` scope。

相同 Target 被多个 Route 引用时复用画像。未再被引用的旧画像通过保留期后台清理，不在配置写入时同步删除。

`RoutingTarget::health_key()` 使用同一规范化身份生成 `TargetInstanceId`，但 HealthRegistry 与 Capability Profile 始终是两个独立状态域。

### 4.4 能力值、约束与证据

```rust
pub enum CapabilityState {
    Supported,
    Unsupported,
    Constrained,
    Unknown,
}

pub enum CapabilityValue {
    Bool(bool),
    EnumSet(BTreeSet<String>),
    StringSet(BTreeSet<String>),
    IntegerRange { min: Option<i64>, max: Option<i64> },
    DecimalRange { min: Option<f64>, max: Option<f64> },
    SchemaKeywordSet(BTreeSet<String>),
    Opaque(serde_json::Value),
}

pub enum EvidenceSource {
    ExplicitOverride,
    SemanticProbe,
    SuccessfulTraffic,
    ExactModelCatalog,
    ProviderDocumentation,
    ProtocolDefault,
}

pub struct CapabilityObservation {
    pub capability_id: CapabilityId,
    pub state: CapabilityState,
    pub value: Option<CapabilityValue>,
    pub source: EvidenceSource,
    pub observed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub evidence_version: u32,
    pub probe_suite_version: Option<u32>,
    pub reason_code: Option<String>,
    pub redacted_detail: Option<String>,
}
```

状态和值必须满足注册表不变量：

- Bool 能力的 Supported/Unsupported 不携带任意对象。
- Constrained 必须携带与 descriptor `value_kind` 一致的非空 value。
- Opaque 值可以存储和往返，但不得用于 conversion-relevant capability 的自动满足判断。
- Unknown 是解析结果，不作为比已有明确证据优先级更高的新 observation。

约束匹配由 descriptor 声明，planner 不解释任意 JSON：

```text
set_contains      Target 集合必须覆盖请求集合
range_contains    Target 范围必须包含请求值或范围
exact_match       值必须相等
boolean           仅判断 Supported/Unsupported
opaque            仅展示和往返，不参与自动路由
```

证据解析规则：

1. 未过期的显式 override 最高，且与探测 observation 分开保存。
2. SemanticProbe 与 SuccessfulTraffic 同属 `VerifiedWire` 级；冲突时选择适用范围相同且时间更新的证据。
3. 明确字段拒绝产生的负面证据可以被更新的语义成功或真实成功流量推翻。
4. SuccessfulTraffic 只产生正面证据，不从“未出现某行为”推导 Unsupported。
5. ExactModelCatalog 高于 ProviderDocumentation；两者都低于 VerifiedWire。
6. ProtocolDefault 仅提供非 hard-ceiling 的初始值；过期、版本不匹配或作用域不匹配的证据不参与解析。
7. 每个 capability 独立保存 TTL；负面语义证据使用比稳定正面证据更短的默认 TTL。

### 4.5 能力描述注册表

能力定义由可扩展注册表描述：

```rust
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub value_kind: CapabilityValueKind,
    pub matcher: CapabilityMatcher,
    pub scope: CapabilityScope,
    pub implementation_status: ImplementationStatus,
    pub discovery_methods: BTreeSet<DiscoveryMethod>,
    pub routing_eligibility: RoutingEligibility,
    pub dependencies: Vec<CapabilityId>,
    pub conversion_relevant: bool,
    pub probe_id: Option<String>,
    pub owner: String,
}
```

能力生命周期使用三个正交维度，不要求所有能力经过主动探测才能参与路由：

```text
ImplementationStatus
  Cataloged     已登记，运行时不据此做转换或过滤
  Implemented   codec/planner、matcher 和测试已经实现

DiscoveryMethod（可多选）
  ExplicitOverride
  ExactModelCatalog
  ProviderDocumentation
  PassiveTraffic
  ActiveProbe

RoutingEligibility
  Disabled          不参与 shadow 或 enforce
  ShadowEligible    只生成诊断，不改变路由
  EnforceEligible   通过准入门禁后允许过滤路由
```

约束：

- `ActiveProbe` 只是发现方式之一。hosted tools、remote MCP、shell、Multi-agent 等不适合主动探测的能力，可以使用 override、精确目录或被动成功证据。
- `Cataloged` 必须对应 `RoutingEligibility::Disabled`。
- `EnforceEligible` 必须已 Implemented，且至少存在一种可提供确定性正面或明确负面证据的发现方式。
- `probe_id` 只有在 discovery methods 包含 ActiveProbe 时允许设置；包含 ActiveProbe 时必须引用受审计探针。
- 静态 `EnforceEligible` 只表示代码具备 enforce 条件；运行时是否执行仍由全局/per-route mode 和 Shadow 准入门禁决定。

目录结构：

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
    └── tools.toml
```

完整能力目录可以先处于 Cataloged/Disabled；第一版只有 CRL 工具闭环所需条目进入 Implemented，并根据证据确定性分别设置 ShadowEligible 或 EnforceEligible。

`protocol-specs` 不在请求热路径动态读取。构建校验和静态描述符生成位于 `crates/protocols`；`core` 只提供通用类型、匹配器和 planner 输入输出，不包含具体协议 baseline 或 CRL transform。探针 TOML 只能引用受审计的 `probe_id`，不得携带任意命令、URL 或可执行脚本。

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

该矩阵的详细 carrier、损失规则和例外继续以 `docs/protocol-capability-matrix.md` 为静态来源。Target Profile 可以收窄 baseline Supported，也可以用证据解析 ExtensionUnknown，但不能把 Forbidden 改成支持；成组私有契约注册为独立 dialect，单项扩展注册为独立 capability。

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

上述命名空间是完整 Catalog，不代表第一版全部实现或主动探测。第一版进入 Enforced 的能力仅限 CRL 工具闭环；其余条目保持 Cataloged，按阶段 7 的晋级规则扩展。

## 6. 探测范围与策略

### 6.1 默认主动探测

默认探测按 bundle 选择，不对所有 Target 执行全量能力扫描。

基础 bundle 适用于所有 generation Target：

| 能力 | 探测方式 | Positive 条件 | ExplicitNegative 条件 |
|------|----------|----------------|--------------------------|
| endpoint/auth/model | 最小非流文本请求 | 2xx 且返回协议合法文本 | 明确 endpoint/model 不存在 |
| SSE | 最小流式请求 | 首帧、内容帧、终止帧均合法 | 明确拒绝 stream 字段或 endpoint |

Responses 工具 bundle 仅适用于 Responses endpoint，且 Route 使用工具、Target 声明相关能力或管理员要求时执行：

| 能力 | 探测方式 | Positive 条件 | ExplicitNegative 条件 |
|------|----------|----------------|--------------------------|
| function | 强制唯一 no-op function | 返回正确 call、name 与 nonce | control 成功且明确拒绝 tools/function 字段 |
| function continuation | 回传本次 no-op output | 返回与 call_id 对应的最终 message | 明确拒绝 output carrier |
| tool choice required | `required` + 唯一函数 | 产生唯一预期调用 | 明确拒绝 required 值 |
| tool choice specific | 指定 probe 函数 | 调用名称准确 | 明确拒绝 specific choice 结构 |
| namespace | 顶层 namespace 内唯一函数 | 返回完整 namespace 身份与 nonce | control 成功且明确拒绝 namespace |
| custom | 顶层自由文本 no-op custom | 返回 custom call 与随机输入 | control 成功且明确拒绝 custom |
| CRL additional_tools | 顶层 tools control 与 CRL carrier 实验组 | 实验组返回等价调用且 control 成功 | control 成功，实验组明确拒绝或在重复受控测试中稳定忽略 |

工具探针使用每次运行唯一的随机 nonce，并把 nonce 放入必须回传的 tool schema/input。探测程序只验证模型请求调用工具，不执行真实业务逻辑。

### 6.2 条件主动探测

以下能力仅在精确模型目录、服务商声明、已有流量证据、请求实际需要或管理员显式要求时探测：

| 能力 | 原因 |
|------|------|
| reasoning effort 各值 | 需要多次模型调用 |
| reasoning summary | 不保证每次返回 |
| encrypted reasoning replay | 需要多轮状态，且 provider-bound |
| WebSocket | 依赖代理和升级链路 |
| image inline/URL | 需要测试素材和多模态模型 |
| audio/video | 成本和模型限制较高 |
| JSON Schema 关键词子集 | 组合数量大 |
| json_object / 基础 strict schema | 输出不符合约束不能单独证明不支持 |
| parallel tools | 模型未同时调用两个工具不能单独证明不支持 |
| PTC | 行为选择具有随机性，且需要完整 continuation 验证 |
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

每个探针必须返回以下四类结果之一：

```rust
pub enum ProbeOutcome {
    Positive(CapabilityObservation),
    ExplicitNegative(CapabilityObservation),
    Inconclusive(ProbeDiagnostic),
    Error(ProbeError),
}
```

```text
明确 400 unknown/unsupported field
  → 仅对被明确点名且作用域准确的能力记录 ExplicitNegative

强制工具调用返回正确 call + nonce
  → Positive

tool output 回传后产生最终 message
  → continuation Positive

auto、parallel、PTC 或 structured output 未按提示产生预期行为
  → Inconclusive，不更新为 Unsupported

401/403
  → Error(Auth)，不更新能力

429
  → Error(RateLimited)，不更新能力

5xx/timeout/network
  → Error(Transient)，不更新能力

200 但字段被静默忽略
  → 默认 Inconclusive；只有 control 成功、实验变量唯一且重复结果稳定的受控 A/B 才可记录 ExplicitNegative
```

能力不支持不是健康失败，不调用 `HealthRegistry::record_failure`；探测成功也不作为业务健康样本写入 latency EWMA。

### 6.5 探针执行安全边界

- 探针复用正常 egress 的 URL 构造、AuthApplier、代理、TLS、redirect credential stripping 和协议 codec，禁止另建行为不一致的 HTTP 路径。
- 探针跳过业务 ingress 认证、下游配额、业务 request log 和 fallback health 更新，但进入独立 probe audit、费用和指标。
- 执行前按 TargetKey 重新读取当前有效凭证；任务记录不得保存明文 credential。
- Target 已删除、禁用、身份变化或无 Route 引用时，未开始任务取消；运行中结果只有 TargetKey 仍匹配时才可提交。
- 请求体、响应体和错误文本按现有审计规则脱敏并限长；上游返回内容不得原样写入 `last_error`。
- API Base 遵循正常 egress 的地址安全策略，后台探测不得扩大可访问网络范围。

## 7. 能力解析与转换规划

### 7.1 解析优先级

```text
协议/方言 Forbidden
  → 永远不满足

显式 Target override
  > 最新且作用域匹配的 VerifiedWire 证据
  > exact-model 静态映射
  > Provider documentation
  > Protocol default
  > Unknown
```

`VerifiedWire` 包含 SemanticProbe 与 SuccessfulTraffic，冲突时按 capability、carrier、endpoint、model 和 credential scope 的适用范围选择最新证据。`ExtensionUnknown` 可以被 VerifiedWire 或 override 收窄；`Forbidden` 不可被越权扩展。

### 7.2 请求能力提取

能力需求分成三个强度：

```rust
pub enum RequirementStrength {
    Required,
    Preferred,
    Ignorable,
}

pub struct ExchangeRequirements {
    pub request: RequirementExpr,
    pub response_contract: RequirementExpr,
    pub continuation: RequirementExpr,
}
```

提取分为两层：

```rust
// core：只读取规范 IR 字段。
pub fn derive_ir_requirements(request: &IrRequest) -> ExchangeRequirements;

// protocols：读取 ingress dialect、原始/opaque carrier 和协议扩展。
pub trait ProtocolRequirementProvider {
    fn derive_wire_requirements(
        &self,
        request: &IrRequest,
        ingress_profile: &WireProfileId,
    ) -> ExchangeRequirements;
}
```

两层结果合并后一次返回完整需求集合，不只返回第一个损失维度。示例：

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

- Required 不满足时必须过滤 Target。
- Preferred 不满足时只能使用注册表明确允许的降级，并记录 conversion note。
- Ignorable 仅适用于协议矩阵已经认定不影响契约、且客户端未请求严格保留的字段。
- reasoning replay、工具定义/continuation、strict structured output 和私有 carrier 默认属于 Required。
- output annotations、stop details、usage 字段等响应能力必须进入 `response_contract`，不得只登记在 registry 而不参与规划。

### 7.3 转换计划

```rust
pub struct TransformId(String);

pub struct PlannedTransform {
    pub id: TransformId,
    pub preserves: Vec<CapabilityId>,
    pub consumes: Vec<CapabilityId>,
    pub produces: Vec<CapabilityId>,
    pub notes: Vec<ConversionNote>,
}

pub struct PlannedTarget {
    pub target: RoutingTarget,
    pub capabilities: ResolvedTargetCapabilities,
    pub transforms: Vec<PlannedTransform>,
}
```

具体 transform ID 和实现由 `protocols` 注册，例如：

```text
responses.pass_through
responses.promote_crl_additional_tools
responses.convert_from_chat
chat.map_plaintext_reasoning
```

`core` 只处理不透明 TransformId、requirements 和 capability 匹配，不包含 CRL、Responses 或其他具体协议分支。没有明确注册和测试的字段丢弃 transform 不得进入计划。

规划顺序：

```text
Ingress decode
  → IR requirements + protocol wire requirements
  → ExchangeRequirements
  → 为每个 Target 解析 effective capabilities
  → 由目标 codec/dialect planner 枚举可用转换计划
  → 过滤无法满足需求的 Target
  → 对 PlannedTarget 执行路由策略排序
  → 按 Target 自己的 plan 选择 raw passthrough 或 materialized body
  → 编码和发送
```

不同 Target 可以得到不同计划，不能为整条 Route 只生成一个全局转换结果。当前基于“Route 中存在任一同 suite Target”预先计算的 raw passthrough 必须下沉到 PlannedTarget 执行阶段。

### 7.4 CRL 场景

CRL `additional_tools` 的要求不是单一 capability，而是两条可替代路径：

```text
Native Plan:
  baseline tools.crl.additional_tools != Forbidden
  requires tools.crl.additional_tools
  transform responses.pass_through

Promotion Plan:
  requires tools.namespace
  requires tools.custom（仅当载体包含 custom）
  requires tools.function（仅当载体包含 function）
  transform responses.promote_crl_additional_tools
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
4. 普通工具以 `(type, name)` 为 identity；namespace 以完整 namespace path 为 identity；无稳定 identity 的工具不自动去重。
5. 使用 canonical JSON 做语义相等比较，对象键顺序不影响相等性，数组顺序保持语义。
6. identity 相同且定义相等时跳过重复项；identity 相同但定义冲突时返回 `400 invalid_request`。
7. 删除已提升的 carrier item。
8. 保留 `tool_choice` 与 `parallel_tool_calls`。
9. 不自动把 namespace 展开或重命名为普通 function。

Responses decoder 扩展现有 `responses_opaque_input_items` 有序机制保存 `additional_tools` 的原始 item 和 index，同时解析内部工具类型生成 requirements；不得另建平行 opaque carrier，也不得继续退化为空 developer message。

原生计划使用入口原始 body，仅应用必须的 model、认证和已登记 egress normalization。提升计划从同一原始 body 生成目标 body，不能先经过会丢失未知字段的通用 IR 重编码。

### 7.5 无兼容目标错误

对普通客户端返回受限、协议原生的错误：

```json
{
  "error": {
    "type": "no_compatible_target",
    "message": "No routing target can preserve the requested capabilities",
    "code": "no_compatible_target",
    "required": ["tools.namespace", "tools.custom"],
    "unknown": [],
    "request_id": "..."
  }
}
```

外部错误不包含 TargetKey、API Base、account、credential scope 或逐 Target 缺失清单，并对 capability 数量和文本长度设上限。完整 `CompatibilityReport` 只写入 request attempt telemetry，并通过鉴权后的 Admin API 查询。

`AppError` 增加受类型约束的安全 details，而不是允许任意 JSON 直接进入客户端响应。各 ingress protocol 的 error encoder 只序列化白名单字段。

对含工具、reasoning replay、structured output 等高语义影响请求，`Unknown` 在 enforce 模式视为不满足；管理员可通过显式 Target override 改变具体能力结论，不能覆盖协议 `Forbidden`。

### 7.6 业务流量反馈与 fallback

- 请求携带某 capability 且上游产生可验证语义结果时，可以写入 SuccessfulTraffic 正面 observation。
- 未调用工具、未返回 reasoning、未命中 cache 等缺失行为不产生负面 observation。
- 明确 capability-related 4xx 将对应 observation 标记 stale，并创建重探测任务。
- capability-related 4xx 在尚未向客户端发送响应字节时可被分类为 Target-specific incompatibility，跳到下一个已规划 Target；普通 BadRequest 不得被误分类为 fallback。
- 流式响应一旦向客户端发送字节，不再因后续 capability 问题切换 Target。

## 8. 持久化设计

### 8.1 Capability Profile

新增 SQLite 与 PostgreSQL 对等迁移：

```sql
CREATE TABLE target_capability_profiles (
    target_key TEXT PRIMARY KEY,
    identity_version INTEGER NOT NULL,
    provider_id TEXT NOT NULL,
    credential_scope_fingerprint TEXT NOT NULL,
    canonical_api_base TEXT NOT NULL,
    protocol_suite TEXT NOT NULL,
    endpoint_name TEXT NOT NULL,
    endpoint_version TEXT NOT NULL,
    dialect_id TEXT NOT NULL,
    model_id TEXT NOT NULL,

    schema_version INTEGER NOT NULL,
    profile_status TEXT NOT NULL,
    resolved_capabilities_json TEXT NOT NULL,
    observations_json TEXT NOT NULL,

    last_probe_suite_version INTEGER,
    last_successful_probe_at TEXT,
    last_probe_error_class TEXT,
    last_probe_error_redacted TEXT,
    fresh_until TEXT,
    stale_until TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

`profile_status`：

```text
pending
partial
ready
stale
error
```

`fresh_until` 到期后进入 stale-while-revalidate；在 `stale_until` 前继续使用最后一次已验证结果并异步重探测。超过 `stale_until` 后，过期 observation 不参与 enforce 解析，但仍保留用于诊断。

### 8.2 Override

人工覆盖独立存储，重探测不得修改：

```sql
CREATE TABLE target_capability_overrides (
    target_key TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    state TEXT NOT NULL,
    value_json TEXT,
    reason TEXT NOT NULL,
    actor TEXT NOT NULL,
    expires_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (target_key, capability_id)
);
```

override 必须通过 registry 校验 state/value；未知 capability ID 可以存储和导入导出，但在当前版本不参与 enforce。协议 baseline 为 `Forbidden` 时拒绝 Supported override。

### 8.3 持久化探测任务

```sql
CREATE TABLE target_probe_jobs (
    id TEXT PRIMARY KEY,
    target_key TEXT NOT NULL,
    probe_set_json TEXT NOT NULL,
    probe_set_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    priority INTEGER NOT NULL,
    attempt_count INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    next_attempt_at TEXT NOT NULL,
    lease_owner TEXT,
    lease_until TEXT,
    last_error_class TEXT,
    last_error_redacted TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (target_key, probe_set_hash)
);
```

`probe_set_hash` 基于排序后的 capability ID 和 probe version 生成；相同 TargetKey/probe set 的重探测复用并重置同一 job。任务状态为 `pending/running/complete/partial/cancelled/failed`。worker 使用单条条件更新原子 claim 到期或未持有 lease 的任务；lease 到期后可由其他副本接管，执行和结果提交均幂等。

进程启动时扫描 pending、lease 已过期的 running 和需要重探测的 stale profile。任务不保存凭证，执行时通过 TargetKey 从当前配置快照取得有效 Target；身份已变化或 Target 已无引用时取消。

### 8.4 热路径能力快照

`server::AppState` 持有独立的只读能力快照：

```rust
pub struct CapabilitySnapshot {
    pub epoch: i64,
    pub profiles: HashMap<TargetKey, ResolvedTargetCapabilities>,
}
```

- 使用 `ArcSwap` 或等价 RCU 结构原子替换。
- 服务启动时从 profile 与 override 表构建；正常请求只读取内存，不同步查询 DB。
- profile/override 提交后递增独立 `capability_epoch`，本地 write-through 更新，其他副本由后台 watcher 刷新。
- capability epoch 与配置 epoch 分离，频繁探测结果不得触发完整 Provider/Route 配置重载。
- snapshot 中保存已应用 baseline、observation、override 和 TTL 后的 resolved 结果，planner 不在热路径解析数据库 JSON。

能力画像默认保留 JSON，以便新增能力不做表结构迁移。常用筛选字段后续可以增加索引投影，但第一阶段不提前优化。

## 9. 探测生命周期

### 9.1 路由创建和更新

```text
POST/PUT Route
  1. 先验证并持久化配置
  2. 为每个启用 Target 计算 TargetKey
  3. 复用未过期且 probe_suite_version 一致的画像
  4. 根据 endpoint/dialect/Route 实际需求计算 probe bundle
  5. 对缺失或过期画像写入 pending，并在同一数据库事务创建或合并 probe job
  6. 立即返回，不等待完整探测
```

配置提交与 probe job 持久化处于同一事务，避免配置已生效但任务未入队。worker 轮询持久化 job，不依赖 Admin 进程向内存 channel 成功发送。

Target 在 `pending` 时：

- `off` 模式保持现有路由行为。
- `shadow` 模式计算并记录兼容性结论，但不改变目标选择。
- `enforce` 模式下，普通文本请求可按协议基线和现有证据使用；需要工具、reasoning replay、strict structured schema 等能力的请求不选择 Unknown Target。
- 存在未超过 `stale_until` 的最后已验证画像时继续使用该画像并触发重探测。
- enforce 模式无满足需求的 Target 时返回 `no_compatible_target`。

### 9.2 自动重探测触发条件

1. 创建或启用 Target。
2. 修改 API Base、协议、endpoint、model、account 或 credential scope。
3. profile TTL 到期。
4. `probe_suite_version` 升级。
5. 模型目录版本变化且影响相关能力。
6. 真实流量收到明确 capability/schema 4xx。
7. 管理员点击“重新探测”。
8. capability 的 ImplementationStatus、DiscoveryMethod、RoutingEligibility 或 evidence version 发生影响现有结论的变化。

重探测只创建受影响 capability 的 probe set。探针实现或成功条件变化时提升对应 evidence version；不相关能力 observation 不失效。

### 9.3 调度与成本控制

- 使用全局、per-provider 和 per-account semaphore 限制并发。
- DB job 唯一性与 lease 负责跨副本合并，进程内 single-flight 减少本副本重复执行。
- 每个探针设置独立超时和最大输出 token。
- 默认不对能力错误重试；瞬时网络错误最多进行一次受限重试。
- 设置单 Target 每日探测预算和全局预算。
- 探测请求标记内部 `probe_run_id`，不计入下游 API Key 配额。
- 探测产生的上游费用仍需在 Admin UI 中可观测。
- 达到预算时任务保持 pending 并设置 `next_attempt_at`，已有 profile 不被降级。
- 默认只运行基础 bundle；工具、PTC、reasoning、structured output 和多模态 bundle 按 endpoint、请求需求和管理员策略选择。

### 9.4 启用与升级策略

运行时设置使用枚举而不是单一 bool：

```text
target_capability_routing_mode = off | shadow | enforce
```

- 新版本默认 `off`，ProbeWorker 可以独立启用并回填现有 Target。
- profile 覆盖率、probe error 和 shadow exclusion 指标达到管理员设定门槛后切换 `shadow`。
- `shadow` 不改变业务路由，request attempt 同时记录实际选择和能力规划选择。
- `enforce` 可全局启用，也可由 Route 设置覆盖；未显式设置的 Route 继承全局模式。
- CRL promotion 仍有独立开关；关闭 promotion 时 native CRL Target 仍可由能力路由选择。
- 从 enforce 回退不删除 profile、override 或 probe job。

### 9.5 Shadow 指标与 Enforce 准入门禁

准入按 Route 和“请求能力形状”分别统计。请求能力形状是排序、去重后的 Required capability ID 与 constraint 摘要；不得把普通文本流量计入 CRL 工具请求的分母。

核心指标定义：

| 指标 | 定义 |
|------|------|
| `profile_resolution_coverage` | 已启用 Target × 本能力形状 Required capability 的判定对中，结论为 Supported、Unsupported 或 Constrained 的比例；Unknown 不计已解析 |
| `compatible_shape_coverage` | 观测到的能力形状中，至少存在一个已解析为 compatible Target 的比例 |
| `verified_success_disagreement_rate` | 实际目标产生可验证成功语义，但 Shadow planner 将其判为 incompatible 的请求数 / 可验证成功请求数 |
| `planner_unknown_rate` | Shadow planner 因 Unknown 无法确定是否 compatible 的相关请求数 / 相关请求总数 |
| `probe_terminal_error_rate` | rolling window 内 Auth、RateLimited、Transient 等 ProbeError 数 / 已终结 probe 数；Unsupported 与 Inconclusive 不计错误 |
| `planner_internal_error_rate` | requirement 提取、profile 解析或 transform 规划发生内部错误的相关请求数 / 相关请求总数 |
| `planning_latency_p95` | 只计算内存 requirement、resolver、planner 和过滤的 p95 时延，不含上游请求与异步 telemetry |

第一版 CRL enforce 的默认准入门槛：

```text
scope                           每条 Route、每种 CRL 工具能力形状
observation_window              连续 24 小时
minimum_relevant_requests       100
profile_resolution_coverage     100%
compatible_shape_coverage       100%
verified_success_disagreement   0 个高置信冲突
planner_unknown_rate            0%
probe_terminal_error_rate       <= 5%，且不存在未解决 Auth 错误
planner_internal_error_rate     0%
planning_latency_p95            <= 1 ms（发布基准）；生产环境只告警，不自动切换模式
```

准入规则：

- 覆盖率按 Target-capability 判定对计算；已验证 Unsupported 属于“已解析”，不是缺失画像。
- 只有语义可验证的 tool call、continuation 或明确上游拒绝参与 disagreement 判定；普通 `200` message 不算成功证明。
- 任一请求能力形状没有 compatible Target、存在 Unknown 或出现高置信冲突时，该形状不能进入 enforce。
- Route 达到门槛后由管理员显式切换 enforce；系统不得自动从 shadow 晋级。
- 低流量 Route 未达到 100 个样本时，可以使用“完整 probe suite 通过 + 至少一次端到端 continuation 回归 + 管理员确认”替代样本门槛；该例外必须记录审计并只作用于指定 Route/能力形状。
- 门槛可以通过运行时设置调高；调低到低于上述默认值需要二次确认和审计。
- enforce 后持续计算相同指标。出现高置信冲突、planner internal error、唯一 compatible Target 失去有效画像，或连续五分钟 `planner_unknown_rate > 0` 时，仅受影响 Route/能力形状自动降为 shadow，并产生告警；不得全局级联回退。

## 10. Admin API 与 WebUI

### 10.1 Admin API

新增接口：

```text
GET  /admin/v1/target-capabilities/:target_key
POST /admin/v1/target-capabilities/:target_key/probe
PUT  /admin/v1/target-capabilities/:target_key/overrides
DELETE /admin/v1/target-capabilities/:target_key/overrides/:capability_id
GET  /admin/v1/capability-registry
GET  /admin/v1/probe-jobs/:job_id
```

现有 request attempt 详情增加内部 `CompatibilityReport`，不新增面向普通客户端的逐 Target 诊断接口。所有列表接口必须分页，错误和 observation detail 必须脱敏、限长。

Route 创建和查询响应增加：

```json
{
  "target_key": "...",
  "egress_dialect_id": "auto",
  "profile_status": "pending",
  "probe_job_status": "pending",
  "capability_routing_mode": "shadow",
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
- probe job 状态、版本、最后探测时间、fresh/stale TTL 和 lease/重试摘要。
- Supported / Unsupported / Constrained / Unknown。
- 每项能力的证据来源和失败原因。
- “重新探测”和“人工覆盖”。
- 预计探测成本和可能调用的安全探针类别。
- Target 被某次请求排除时的 missing capabilities。
- off/shadow/enforce 当前模式、继承来源和 shadow 规划结果。

人工把 `Unsupported` 覆盖为 `Supported` 时必须二次确认，并说明这可能导致上游 4xx 或静默语义丢失。

## 11. Crate 分层

| Crate/目录 | 职责 |
|------------|------|
| `crates/core` | 通用 `CapabilityId`、typed value、matcher、RequirementExpr、证据解析、CompatibilityReport 和不透明 TransformId；纯逻辑、零 I/O、无 CRL/Responses 分支 |
| `crates/protocols` | 编译后的 registry、协议/dialect baseline、ProtocolRequirementProvider、具体 planner/transform、CRL opaque item 建模与提升 |
| `crates/store` | SQLite/PostgreSQL migration、profile/override/job CRUD、lease claim、TTL/stale 查询和 capability epoch |
| `crates/server` | CapabilitySnapshot、持久化 ProbeWorker、HTTP/SSE 探针执行、安全预算、路由前编排和按 PlannedTarget 生成请求体 |
| `crates/admin` | capability/probe/override API 与审计日志 |
| `webui` | Target 能力状态、证据详情、重探测和人工覆盖 |
| `protocol-specs` | 完整能力目录、协议/dialect baseline 与受审计 probe 元数据来源，不作为运行时动态配置 |

`crates/protocols` 的构建校验读取 `protocol-specs` 并生成静态描述符；生成结果不得要求运行时文件访问。触及 crate 依赖后必须运行 `scripts/verify-deps.sh`，确保 `core` 不依赖 `protocols`、`store`、`server`、provider SDK 或网络实现。

## 12. 实施步骤

### 阶段 0：能力清单与契约冻结

#### 目标

冻结能力、方言、约束匹配和 CRL 首期范围，建立后续代码与测试共同遵循的静态契约。

#### 进入条件

- 当前 Responses、跨协议 lossy conversion 和 fallback 测试可在未修改行为的基线上通过。
- 当前 `EndpointCapabilities`、`docs/protocol-capability-matrix.md`、Responses opaque item 与 passthrough 行为已完成对照。

#### 实施内容

1. 将完整能力命名空间写入 registry，默认设置为 Cataloged/Disabled。
2. 将 CRL 闭环所需的 `transport.http/sse`、`tools.function/continuation/choice.required/choice.specific/namespace/custom/crl.additional_tools` 定义为首期实现集合。
3. 为每项首期能力声明 value kind、matcher、scope、dependencies、discovery methods、routing eligibility、owner 和 evidence version。
4. 建立 `Supported/Forbidden/ExtensionUnknown` baseline、WireProfileId、standard/CRL dialect 和 auto/explicit egress dialect 契约。
5. 将协议矩阵条目映射到 capability ID、RequirementStrength 和 matcher；CRL 私有行为写入 dialect 文档。
6. 实现 registry 静态校验：重复 ID、未知引用、value/matcher 不匹配、非法 lifecycle 组合、循环依赖和未审计 probe ID 均失败。
7. 固化当前 Responses opaque input、raw passthrough、model override、header forwarding 和 cross-protocol 回归测试。

#### 交付物

- 完整 Catalog 与首期 CRL registry/baseline/dialect 文件。
- registry schema、构建校验器和生成的静态描述符快照。
- capability-to-matrix 映射清单和 CRL dialect 文档。
- 修改前行为的兼容基线测试。

#### 退出标准

- registry 所有非法 fixture 均按预期失败，合法 fixture 生成结果稳定且可复现。
- 每个首期 capability 都能追溯到 baseline、matrix/dialect 文档、matcher 和 owner。
- Cataloged/Disabled 能力不会进入运行时路由判断。
- 现有 Responses、lossy conversion 和 passthrough 测试无行为变化。
- 通过验收组 `AC-REG` 和适用的 `AC-QUALITY`。

#### 可启用模式

仅 `off`；本阶段不启动探针，不改变路由和请求编码。

### 阶段 1：Core 数据模型

#### 目标

实现零 I/O 的通用能力内核、协议扩展边界和稳定 Target 身份，不改变现有业务路由。

#### 进入条件

- 阶段 0 的 registry、baseline、dialect 与生命周期契约已冻结。
- 首期 capability ID 和 matcher 不再发生破坏性重命名。

#### 实施内容

1. 在 `core` 实现 CapabilityId、CapabilityState、typed CapabilityValue、CapabilityMatcher 和校验错误。
2. 实现 `AllOf/AnyOf/Not` RequirementExpr、Required/Preferred/Ignorable、ExchangeRequirements 和返回完整差异的 CompatibilityReport。
3. 实现 observation 解析、证据新鲜度/优先级、constraint matching 和 Unknown/Forbidden 规则的纯函数。
4. 使用不透明 TransformId 和 PlannedTransform；`core` 不包含具体协议或 CRL 分支。
5. 在 `protocols` 实现静态 registry/baseline 访问、ProtocolRequirementProvider 和 transform provider 接口。
6. 实现 CanonicalTargetIdentity、API Base 规范化、安装级 HMAC credential scope fingerprint 和 TargetKey。
7. 为 RouteTarget 增加可选 `egress_dialect_id`；缺失值兼容为 `auto`。
8. 使 health key 使用无碰撞 TargetInstanceId，同时与 Capability Profile 状态完全分离。
9. 保留 EndpointCapabilities 兼容适配层，现有 codec 无需一次性迁移。

#### 交付物

- `core` 通用 capability、requirements、resolver、report 和 planning 类型。
- `protocols` 的 registry/baseline/requirement/transform 扩展接口。
- CanonicalTargetIdentity、TargetKey、TargetInstanceId 和向后兼容 RouteTarget 模型。
- 单元测试与 property tests，覆盖集合、范围、证据冲突、规范化和序列化。

#### 退出标准

- 同一实际 Target 的等价 API Base 形式产生相同 TargetKey；身份实质变化产生不同 TargetKey。
- API Key/OAuth/IAM/anonymous fingerprint 满足稳定性和不可明文恢复要求。
- resolver 对 Unsupported、Constrained、Unknown、Forbidden 和证据冲突返回确定结果及完整原因。
- 未知 capability/value 能往返，但不会误判为满足 Required。
- `scripts/verify-deps.sh` 证明 `core` 未依赖 protocols/store/server 或网络实现。
- 旧 Route JSON 不含 dialect 时可正常加载，路由行为与阶段 0 相同。
- 通过验收组 `AC-REG`、`AC-IDENTITY` 和适用的 `AC-QUALITY`。

#### 可启用模式

仅 `off`；新模型仅被测试和持久化结构使用，不参与实际路由。

### 阶段 2：持久化、快照与任务恢复

#### 目标

建立 SQLite/PostgreSQL 对等的能力状态、可靠任务和零数据库查询热路径。

#### 进入条件

- 阶段 1 的 TargetKey、typed value、observation 和 resolver 序列化格式已稳定。
- profile、override、job 和 epoch 的 schema version 已确定。

#### 实施内容

1. 增加 profile、override、probe job、安装级 fingerprint secret 和 capability epoch migration。
2. 实现 profile/observation CRUD、typed value 校验、能力级 TTL、fresh/stale 查询和未知 ID 保活。
3. 实现 override CRUD、baseline Forbidden 校验、过期处理和审计所需 actor/reason 字段。
4. 实现 probe job upsert、probe_set_hash、原子 claim、lease 续期/接管、next_attempt_at、幂等提交和取消。
5. 启动时恢复 pending、过期 running 和 stale profile；无有效 Target 引用的任务安全取消。
6. 在 AppState 增加 CapabilitySnapshot，使用独立 capability epoch、原子替换和后台 watcher。
7. profile/override 写入采用本地 write-through；其他副本通过 epoch 刷新，不触发完整配置重载。
8. 更新导入导出：profile/job 不导出，override 可选择导出，fingerprint secret 永不导出。
9. 增加数据库故障、并发 claim、进程退出、lease 超时、epoch 丢事件和 snapshot 重建测试。

#### 交付物

- SQLite/PostgreSQL migration 和 store API。
- 可恢复的 probe job repository 与 lease 协议。
- AppState CapabilitySnapshot、capability epoch watcher 和启动恢复流程。
- 双数据库一致性、并发和故障恢复测试。

#### 退出标准

- 两个副本并发 claim 同一 TargetKey/probe set 时只有一个获得有效 lease。
- worker 在 claim 前、执行中和提交前退出，任务均能恢复且不会产生重复有效 observation。
- profile 进入 stale 后在 grace 内可用，超过 stale_until 后从 resolved snapshot 失效。
- snapshot 在启动、write-through 和远端 epoch 更新后与数据库最终一致。
- 请求侧 benchmark/测试证明读取 profile 不执行 SQL、不等待 watcher 且为内存只读路径。
- 明文 credential 和 fingerprint secret 不进入 profile、job、日志、导出或错误体。
- 通过验收组 `AC-IDENTITY`、`AC-PERSISTENCE` 和适用的 `AC-QUALITY`。

#### 可启用模式

仅 `off`；允许后台构建空 profile/job 基础设施，但不发送主动探测请求。

### 阶段 3：CRL 最小安全探针

#### 目标

实现低副作用、语义可验证、可限流和可恢复的 CRL 最小探针套件。

#### 进入条件

- 阶段 2 的持久 job、lease、snapshot 和 TargetKey 查询可用。
- 正常 egress 的认证、URL、代理、TLS、redirect 和协议编解码路径已有可复用边界。

#### 实施内容

1. 抽取 ProbeWorker 可复用的 egress request builder、AuthApplier、URL 和传输安全逻辑。
2. 实现 worker lease 续期、进程内 single-flight、全局/provider/account 并发限制、预算、超时和受限重试。
3. 实现 endpoint/non-stream/SSE 基础 bundle。
4. 实现 function、continuation、required、specific、namespace、custom 和 CRL A/B 工具 bundle。
5. 为每个探针定义唯一实验变量、control、nonce、Positive、ExplicitNegative 和 Inconclusive 判据。
6. 实现 Auth/RateLimited/Transient 等 ProbeError；错误不生成负面 observation，不清空已有 profile。
7. 将结果原子写入 observation/profile，递增 capability epoch 并发布 snapshot。
8. 探针事件进入独立 audit/metrics/cost，跳过业务 ingress auth、quota、request log、HealthRegistry 和 EWMA。
9. 实现 Target 删除、禁用、身份变更、预算耗尽和 worker 暂停的取消/延期行为。

#### 交付物

- `probe_suite_version=1`、受审计 probe metadata 和安全探针实现。
- ProbeWorker、预算/并发控制、结果提交和独立观测指标。
- wiremock fixture，覆盖成功、明确不支持、静默忽略、Inconclusive、认证、限流、超时和恢复。

#### 退出标准

- 只有正确 tool name/call_id/nonce 和合法 continuation 能产生 Positive。
- control 未成功、模型未按提示执行或结果无法归因时只能产生 Inconclusive/Error。
- CRL ExplicitNegative 只有在 control 成功且实验变量唯一时成立。
- 相同 job 在多副本、重试和 lease 接管下只提交一次有效结果。
- 探针请求与业务 egress 使用相同认证和传输安全逻辑，且不影响业务健康、延迟、配额和日志统计。
- 所有请求/响应/错误审计内容已脱敏限长，默认探针没有外部副作用工具。
- 通过验收组 `AC-PROBE`、`AC-PERSISTENCE` 和适用的 `AC-QUALITY`。

#### 可启用模式

路由保持 `off`；允许显式启动 ProbeWorker 回填 profile。主动探测开关可以独立关闭。

### 阶段 4：CRL 建模与提升

#### 目标

实现 CRL 请求的无损识别、原生透传和受能力约束的顶层工具提升。

#### 进入条件

- 阶段 1 的 protocol requirement/transform 接口已稳定。
- 阶段 0 的 Responses opaque/passthrough 回归基线可持续通过。
- 阶段 3 能为 native additional_tools、namespace、custom、function 和 continuation 提供证据。

#### 实施内容

1. Responses decoder 在现有 `responses_opaque_input_items` 中保存 additional_tools 原始 item/index。
2. 解析 carrier 内 namespace/function/custom 类型和嵌套 namespace path，生成 Required wire requirements。
3. 注册 `responses.pass_through` 与 `responses.promote_crl_additional_tools` transform provider。
4. 实现工具 identity、canonical JSON、稳定合并、等价去重和冲突拒绝。
5. 原生计划使用原始 body，并仅应用 model、认证和已登记 normalization。
6. promotion 计划从原始 body 生成 materialized body，移除 carrier、保留其他未知字段和 item 顺序。
7. 将 raw/materialized body 选择下沉到 PlannedTarget，消除 Route 级 passthrough 对其他 Target 的影响。
8. 确保 non-stream/SSE 的 function/custom call 和 finish reason 正确回到 Codex，continuation 可闭环。

#### 交付物

- additional_tools opaque 建模和 wire requirement 提取。
- 两个 Responses transform 与稳定 merge/conflict 实现。
- Codex → native CRL、Codex → promoted Responses 的请求/响应/continuation 集成测试。

#### 退出标准

- native 计划保持 additional_tools 和其他未知字段的 wire 等价性。
- promotion 计划只在 namespace/custom/function 依赖全部满足时可生成。
- 顶层 tools、多个 carrier、嵌套 namespace、等价重复、冲突定义和混合 opaque item 均有确定结果。
- 同 Route 内 native、promotion 和 cross-protocol Target 分别得到自己的 body 计划。
- non-stream/SSE 工具调用均得到 ToolCalls，tool output continuation 返回最终 message。
- 关闭 CRL promotion 后恢复原生 passthrough 行为，不影响其他 Responses 请求。
- 通过验收组 `AC-CRL` 和适用的 `AC-QUALITY`。

#### 可启用模式

路由保持 `off`。transform 仅供测试和后续 Shadow planner 调用，不在生产请求中自动选择。

### 阶段 5：Shadow 能力规划

#### 目标

在不改变生产路由的前提下验证 requirement、profile、planner、transform 和证据反馈的正确性与性能。

#### 进入条件

- 阶段 2–4 的 snapshot、probe profile 和 CRL transform 均通过退出标准。
- 所有首期 capability 至少为 Implemented/ShadowEligible。
- runtime mode、telemetry schema 和紧急回退设置已可用。

#### 实施内容

1. 合并 IR 与 protocol wire requirements，生成完整 ExchangeRequirements 和稳定能力形状 ID。
2. 从 CapabilitySnapshot 为每个 Target 解析 effective profile，枚举并验证 PlannedTarget。
3. 在策略排序前计算 compatible/incompatible/unknown 和 shadow ordered targets，但不改变实际 targets。
4. 同时记录实际选择、shadow 选择、missing/unknown、transform、证据摘要、规划时延和能力形状。
5. 实现客户端安全 details 类型和内部完整 CompatibilityReport；shadow 期间仍保持现有业务错误行为。
6. 从可验证 tool call/continuation 写入 SuccessfulTraffic 正面 observation。
7. 从明确 capability-related 4xx 写入 stale/reprobe 事件，但不将普通 BadRequest 误分类。
8. 实现第 9.5 节全部指标、rolling window、per-route/per-shape 聚合和准入报告。
9. 建立 replay fixture，将已脱敏真实请求离线重放给 planner，与预期兼容结论比较。

#### 交付物

- Shadow planner、能力形状 ID 和全量内部 CompatibilityReport。
- request attempt telemetry、Shadow 指标、准入报告和告警。
- SuccessfulTraffic/stale feedback、离线 replay fixture 和 planning benchmark。

#### 退出标准

- shadow 模式不改变实际 Target 顺序、请求体、fallback 和客户端响应。
- 对固定请求/profile 输入，planner 输出稳定且每个排除结论有 capability/evidence 原因。
- 生产与测试路径均证明 planner 只读 CapabilitySnapshot，不同步访问数据库。
- `planning_latency_p95 <= 1 ms` 的发布基准通过，telemetry 写入不阻塞请求路径。
- 每条候选 Route/CRL 能力形状满足第 9.5 节默认门槛，或保持 shadow 并明确列出未满足项。
- 高置信实际成功与 shadow incompatible 的冲突为零；普通 message 不被误当作工具能力成功证据。
- 通过验收组 `AC-SHADOW`、`AC-ROUTING` 和适用的 `AC-QUALITY`。

#### 可启用模式

允许 `off` 和 `shadow`；禁止 `enforce`。未达到准入门槛的 Route 必须保持 shadow。

### 阶段 6A：Admin API 与运营控制

#### 目标

在启用 enforce 前提供完整、可审计、可回滚的管理控制面。

#### 进入条件

- 阶段 5 的 profile、job、CompatibilityReport、Shadow 指标和 capability shape 已稳定。
- Admin 鉴权、审计日志和设置热更新机制可复用。

#### 实施内容

1. 增加分页的 registry、profile、probe job、CompatibilityReport 查询接口。
2. 增加 manual probe、override 创建/删除和 probe worker 暂停/恢复接口。
3. 增加全局/per-route routing mode API，校验 `off → shadow → enforce` 合法转换。
4. enforce 写入前由服务端计算准入报告；不满足门槛时拒绝并返回未满足项。
5. 实现低流量 Route 例外确认、门槛下调二次确认和作用域限定。
6. 所有 mode、probe、override 和例外操作记录 actor、reason、before/after 和 Target/Route scope。
7. API 输出对 credential、API Base query、错误文本和大 JSON 做脱敏、分页和限长。

#### 交付物

- 完整 Admin API、审计事件和服务端准入校验器。
- 命令/API 可执行的 probe、override、mode、回滚和诊断运维流程。
- Admin 权限、并发更新、审计、脱敏和非法状态转换测试。

#### 退出标准

- 未达到准入门槛的 Route 无法通过 API 进入 enforce。
- override 不能越过 baseline Forbidden，重探测不覆盖 override，过期 override 自动退出 resolved profile。
- manual probe、暂停/恢复和 mode 更新在多副本下最终一致。
- 所有写操作均有完整审计，所有敏感字段在 API 和 audit 中不可见。
- 仅使用 Admin API 即可完成 profile 检查、重探测、shadow 准入、enforce 和回滚，不依赖数据库手工操作。
- 通过验收组 `AC-ADMIN`、`AC-PERSISTENCE` 和适用的 `AC-QUALITY`。

#### 可启用模式

允许 `off` 和 `shadow`；API 中存在 enforce 枚举，但在阶段 6B 发布前由 feature gate 禁止实际启用。

### 阶段 6B：Enforce 数据路径

#### 目标

只对已通过 Shadow 门禁的 Route/能力形状启用能力过滤和 Target 独立转换。

#### 进入条件

- 阶段 6A 的服务端准入、审计和回滚 API 已通过退出标准。
- 目标 Route/能力形状满足第 9.5 节门槛并由管理员显式批准。
- 至少一个 compatible Target 具有 fresh 或 stale-valid profile，且端到端 continuation 已验证。

#### 实施内容

1. 在路由策略排序前过滤 incompatible Target；Unknown 对首期 Required capability 视为不满足。
2. 对剩余 PlannedTarget 应用现有 weighted/priority/cooldown/latency 策略和 HealthRegistry。
3. Executor 按当前 PlannedTarget 选择 raw passthrough 或 promotion body，不复用其他 Target 的 body。
4. 无 compatible Target 时返回协议原生、受限 `no_compatible_target`；完整报告留在内部。
5. 明确 target-specific capability 4xx 在未发送客户端字节时跳到下一个已规划 Target；普通 BadRequest 保持失败。
6. 实现 per-route/per-shape 自动降回 shadow、告警和 feature flag 紧急回退。
7. 对 non-stream、SSE、fallback、断流和 client disconnect 保持现有一次发送与计费语义。

#### 交付物

- enforce filter、PlannedTarget 排序/执行和安全 fallback。
- `no_compatible_target` 协议错误、自动降级与告警。
- canary 配置、回滚 runbook 和端到端测试矩阵。

#### 退出标准

- 第一 Target incompatible、第二 Target compatible 时不会向第一 Target 发送业务请求。
- 同 Route 的 native/promotion Target 收到各自计划的请求体。
- 无 compatible Target 的客户端错误不泄露 TargetKey、account、API Base 或路由拓扑。
- target-specific 4xx 只有满足明确分类且未发送字节时 fallback；流开始后不切换 Target。
- 自动降级触发器能只把受影响 Route/能力形状降到 shadow，其他 Route 不受影响。
- canary 中第 9.5 节指标持续满足一个完整观察窗口，且现有 fallback、health、quota、SSE 和 telemetry 回归全部通过。
- 通过验收组 `AC-ROUTING`、`AC-CRL`、`AC-SHADOW` 和适用的 `AC-QUALITY`。

#### 可启用模式

允许 `off`、`shadow` 和通过门禁的 per-route/per-shape `enforce`；全局 enforce 必须在所有纳入范围的 Route 分别通过门禁后启用。

### 阶段 6C：WebUI 与运营可视化

#### 目标

为已有 Admin 能力提供可解释、低误操作风险的图形管理入口。

#### 进入条件

- 阶段 6A API 契约稳定；可以与阶段 6B 并行开发。
- UI 所需分页、准入报告、审计和 mode mutation 均已有服务端接口。

#### 实施内容

1. Route Target 展示 dialect、profile summary、fresh/stale、probe job 和 capability routing mode。
2. 能力详情展示 state、constraint、evidence source、时间、版本和脱敏 reason。
3. 展示 per-route/per-shape Shadow 指标、门槛、未满足项和 planner 差异。
4. 提供 manual probe、override、worker pause/resume、shadow/enforce 和回滚操作。
5. Unsupported→Supported override、门槛下调、低流量例外和 enforce 使用二次确认，并要求 reason。
6. CompatibilityReport、job 和 audit 列表分页，长值折叠且不渲染未转义上游内容。

#### 交付物

- Target 能力详情、Shadow 准入、probe/override/mode 操作和审计查看界面。
- 加载、空状态、错误、权限、并发更新和敏感内容渲染测试。

#### 退出标准

- 运维人员无需数据库或 CLI 即可完成阶段 6A 的全部日常操作。
- UI 明确区分 Unknown、Inconclusive、ProbeError、Unsupported 和 stale，不将 HTTP 200 展示为能力成功。
- enforce 前显示作用域、门槛结果、受影响 Target 和回滚方式；高风险操作均有二次确认与审计 reason。
- 分页、错误恢复、并发更新冲突和 HTML/JSON 转义通过前端测试。
- WebUI TypeScript、lint 和构建通过，后端 API 契约测试保持通过。
- 通过验收组 `AC-ADMIN` 和 `AC-QUALITY`。

#### 可启用模式

不改变数据路径模式；UI 仅调用阶段 6A/6B 已授权的服务端能力。

### 阶段 7：其他转换维度 Roadmap

按风险和收益逐步加入：

1. parallel tools 与 PTC。
2. json_object、structured-output keyword constraints。
3. reasoning effort、summary 与 replay。
4. image URL/inline/file_id。
5. prompt caching。
6. Gemini/Anthropic 特定 thinking 约束。
7. Embeddings batch、dimensions 与 encoding format。

阶段 7 仅作为 Roadmap，不属于首期交付范围。每项后续能力单独立项，并分别声明 ImplementationStatus、DiscoveryMethod 和 RoutingEligibility；同步更新 registry、baseline、matcher、probe policy、matrix、Shadow 指标和测试，不要求必须具有 ActiveProbe 才能进入路由验证。

## 13. 验收标准

### 13.1 阶段与验收组映射

阶段退出必须同时满足第 12 节的阶段退出标准和下表所列验收组。`AC-QUALITY` 只运行与当前改动相关的子集，但阶段 6B 发布前必须运行完整质量门禁。

| 阶段 | 必须通过的验收组 | 模式上限 |
|------|------------------|----------|
| 0 | `AC-REG`、适用 `AC-QUALITY` | off |
| 1 | `AC-REG`、`AC-IDENTITY`、适用 `AC-QUALITY` | off |
| 2 | `AC-IDENTITY`、`AC-PERSISTENCE`、适用 `AC-QUALITY` | off |
| 3 | `AC-PROBE`、`AC-PERSISTENCE`、适用 `AC-QUALITY` | off + ProbeWorker |
| 4 | `AC-CRL`、适用 `AC-QUALITY` | off |
| 5 | `AC-SHADOW`、Shadow 部分 `AC-ROUTING`、适用 `AC-QUALITY` | shadow |
| 6A | `AC-ADMIN`、`AC-PERSISTENCE`、适用 `AC-QUALITY` | shadow |
| 6B | `AC-ROUTING`、`AC-CRL`、`AC-SHADOW`、完整 `AC-QUALITY` | gated enforce |
| 6C | UI 部分 `AC-ADMIN`、完整 `AC-QUALITY` | 不改变数据路径 |

### 13.2 `AC-REG`：能力注册表与分层

- [ ] 新增 capability ID 不需要增加数据库列。
- [ ] 重复 ID、未知 value kind/matcher、无效依赖和循环依赖在构建或测试时失败。
- [ ] Cataloged 必须为 Disabled；EnforceEligible 必须已 Implemented 且拥有确定性发现方式。
- [ ] ActiveProbe 与 probe_id 双向一致，未知或未审计 probe ID 构建失败。
- [ ] 未知 capability ID 可以经过 DB、Admin API 和导入导出原样往返。
- [ ] 未知 ID 或 Opaque value 不参与 conversion-relevant capability 的自动满足判断。
- [ ] 每个 conversion-relevant capability 都能追溯到协议矩阵条目或 dialect 文档。
- [ ] `core` 不执行文件、数据库或网络 I/O，也不包含 CRL、Responses 或具体协议 transform 分支。
- [ ] Cataloged/Disabled 能力不会进入 shadow 或 enforce 路由判断。

### 13.3 `AC-IDENTITY`：Target 身份

- [ ] 相同实际 Target 的等价 API Base 形式生成相同 TargetKey。
- [ ] 相同 Provider/model 但不同 API Base、endpoint、dialect、account 或 credential scope 生成不同 TargetKey。
- [ ] API Base 规范化覆盖 scheme/host 大小写、默认端口、尾斜杠、path、userinfo 和 query 安全规则。
- [ ] API Key 使用 keyed HMAC；OAuth token 刷新不改变稳定 grant fingerprint；主密钥轮换不改变 fingerprint。
- [ ] 相同实际 Target 被多个 Route 引用时复用 profile；weight、enabled 和 Route 顺序不改变身份。
- [ ] RouteTarget 未配置 dialect 时保持 auto，显式 dialect 参与 TargetKey 和 baseline 解析。
- [ ] 明文 API Key、OAuth token、IAM session token 和 fingerprint secret 不进入 TargetKey、profile、日志或错误体。

### 13.4 `AC-PERSISTENCE`：持久化、任务与快照

- [ ] SQLite 和 PostgreSQL migration、profile/override/job CRUD、TTL 和 fresh/stale 行为一致。
- [ ] Route 配置和初始 probe job 在同一事务提交，创建响应不等待完整探测。
- [ ] 相同 TargetKey/probe set 的并发 claim 在多副本下只有一个有效 lease。
- [ ] 进程在任务创建、claim、执行和提交阶段退出后，任务均能恢复且结果提交幂等。
- [ ] Target 删除、禁用或身份变化后旧任务取消，旧 observation 不进入新身份 profile。
- [ ] fresh 到期后在 stale grace 内继续提供最后已验证结果，超过 stale_until 后不再参与 enforce。
- [ ] profile/override 更新通过 capability epoch 最终发布到所有副本，且不触发完整配置重载。
- [ ] 正常请求只读 CapabilitySnapshot，不执行 SQL、不等待 watcher。
- [ ] profile/job 不随配置导出，override 可选择导出，fingerprint secret 永不导出。

### 13.5 `AC-PROBE`：探测正确性与安全

- [ ] 强制 function 探针只有收到正确 call、name 和 nonce 才产生 Positive。
- [ ] function output continuation 成功后才标记 continuation Supported。
- [ ] control 未成功或模型未按提示执行时产生 Inconclusive/Error，不误判 Unsupported。
- [ ] 明确 schema/field 4xx 只有在点名具体能力且作用域匹配时产生 ExplicitNegative。
- [ ] `401/403`、`429`、`5xx`、timeout 不生成负面 observation，也不清空已有结论。
- [ ] CRL A/B 只有 control 成功、实验变量唯一且结果稳定时产生 ExplicitNegative。
- [ ] 探针与业务 egress 使用相同 AuthApplier、URL、TLS、代理和 redirect 安全规则。
- [ ] 探测请求不修改 HealthRegistry、EWMA、下游 quota 和业务 request log。
- [ ] 默认探针不执行 hosted search、shell、computer use、MCP、Multi-agent 或其他外部副作用。
- [ ] 达到预算、worker 暂停或发生瞬时错误时任务正确延期，最后已验证 profile 不被清空。
- [ ] ProbeOutcome 的 Positive、ExplicitNegative、Inconclusive 和 Error 均有 wiremock 测试。

### 13.6 `AC-CRL`：CRL 建模与转换

- [ ] 原生 CRL Target 收到 wire 等价的 additional_tools 请求及其他未知字段。
- [ ] 标准 Responses Target 在依赖满足时收到提升后的顶层 tools，carrier 已移除。
- [ ] 已有顶层工具、多个 carrier、嵌套 namespace 和其他 opaque item 按稳定顺序合并。
- [ ] 普通工具和 namespace path 使用稳定 identity；canonical JSON 等价定义去重，冲突定义返回 400。
- [ ] namespace/custom/function 等实际载体必需能力任一缺失时不执行提升。
- [ ] additional_tools 复用现有 responses_opaque_input_items，不建立平行 carrier。
- [ ] raw passthrough 与 promotion body 按 PlannedTarget 决定，不受同 Route 其他 Target 影响。
- [ ] non-stream/SSE 工具响应均识别为 FinishReason::ToolCalls，而不是 Stop。
- [ ] tool result continuation 能返回最终 assistant message。
- [ ] 关闭 promotion 后恢复原生 passthrough，不影响普通 Responses 请求。

### 13.7 `AC-SHADOW`：Shadow 验证与准入

- [ ] capability shape 只由 Required capability/constraint 生成，普通文本不进入 CRL 指标分母。
- [ ] shadow 同时记录实际选择与规划选择，但不改变 Target 顺序、请求体、fallback 或客户端响应。
- [ ] profile coverage、compatible shape coverage、success disagreement、Unknown、probe error、planner error 和 planning latency 均按第 9.5 节定义计算。
- [ ] 每条申请 enforce 的 Route/shape 满足连续 24 小时、至少 100 个相关请求和全部默认门槛，或具有合规低流量例外。
- [ ] 高置信 verified success disagreement 为零，Unknown rate 为零，每种能力形状至少有一个 compatible Target。
- [ ] planner 对相同 request/profile 输入输出稳定，每个排除结果包含 capability 和 evidence 原因。
- [ ] SuccessfulTraffic 只从可验证工具语义产生，普通 200 message 不作为工具能力证明。
- [ ] 发布基准中 planning latency p95 不超过 1 ms，异步 telemetry 不阻塞请求。
- [ ] 系统不自动从 shadow 晋级 enforce；门槛下调和低流量例外均有作用域、reason 和审计。

### 13.8 `AC-ROUTING`：Enforce 路由与转换

- [ ] incompatible Target 在 weighted/priority/cooldown/latency 排序前被过滤。
- [ ] 第一目标 incompatible、第二目标 compatible 时，不向第一目标发送业务请求。
- [ ] 不同 Target 获得并执行各自的 transform 和请求体计划。
- [ ] Unknown 对首期 Required capability 在 enforce 中视为不满足。
- [ ] 无 compatible Target 时返回受限 no_compatible_target，完整逐 Target 报告只进入内部 telemetry/Admin。
- [ ] target-specific capability 4xx 仅在明确分类且未发送客户端字节时 fallback；普通 BadRequest 不被误分类。
- [ ] 流式响应发送首字节后不切换 Target，现有断流、计费和客户端取消语义保持不变。
- [ ] 协议 Forbidden 不能被探测或 override 越权扩展。
- [ ] 准入指标恶化时只把受影响 Route/shape 自动降为 shadow，不影响其他 Route。
- [ ] 现有 fallback、health、quota、telemetry 和跨协议 lossy conversion 测试全部通过。

### 13.9 `AC-ADMIN`：管理接口、WebUI 与可观测性

- [ ] Admin API 支持分页查询 registry/profile/job/report，支持 manual probe、override、worker 和 mode 控制。
- [ ] 不满足准入门槛的 Route 无法进入 enforce；低流量例外和门槛下调要求二次确认与审计。
- [ ] 重探测不覆盖人工 override，Forbidden 不能被 Supported override 覆盖。
- [ ] 所有 mode、override、probe 和例外写操作记录 actor、reason、scope 与 before/after。
- [ ] Admin API 和 audit 可以使用 TargetKey 关联记录，但不暴露 credential、fingerprint secret、API Base 敏感 query 或其他可恢复身份材料。
- [ ] 普通客户端错误不暴露 TargetKey、account、API Base 或完整路由拓扑。
- [ ] UI 区分 Supported、Unsupported、Constrained、Unknown、Inconclusive、ProbeError 和 stale。
- [ ] UI 展示 mode 继承、Shadow 指标、准入未满足项、证据、TTL、job 和回滚方式。
- [ ] 高风险操作具有二次确认、reason、并发更新冲突处理和明确作用域。
- [ ] request attempt 能解释 Target 被排除的 capability、evidence 和 transform 计划。
- [ ] 运维人员无需直接操作数据库即可完成日常诊断、重探测、override、准入、enforce 和回滚。

### 13.10 `AC-QUALITY`：工程质量门禁

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] `scripts/verify-deps.sh`
- [ ] `npm --prefix webui run lint`
- [ ] WebUI TypeScript 与生产构建通过。
- [ ] `git diff --check`
- [ ] SQLite/PostgreSQL 均有 migration、job lease、幂等提交和 capability epoch 测试。
- [ ] 新增探针具有 wiremock Positive、ExplicitNegative、Inconclusive 和 ProbeError 测试。
- [ ] 新增转换具有 protocols 单测、server 集成测试及 non-stream/SSE 回归测试。
- [ ] 阶段 6B 发布前保存完整命令输出、Shadow 准入报告和 canary 观察结果。

## 14. 风险与缓解

| 风险 | 缓解措施 |
|------|----------|
| 模型输出随机导致误判 | 强制工具、唯一 nonce、受控 A/B、非确定失败归为 Inconclusive |
| 探测成本增长 | 分阶段、预算、TTL、single-flight、只探测相关能力 |
| 第三方静默忽略字段 | 验证语义输出，不以 HTTP 200 为成功 |
| 能力随模型升级漂移 | probe suite version、TTL、4xx 触发 stale |
| profile 错误过滤正常流量 | off/shadow/enforce、stale grace、人工 override、证据展示和能力分级策略 |
| 能力模型无限膨胀 | Implementation/Discovery/Routing 三维状态、registry ownership、命名规范、typed matcher 和依赖校验 |
| 探测影响健康与路由 | 独立状态、独立指标、禁止写 HealthRegistry |
| 私有扩展污染标准协议 | dialect baseline 隔离，不修改协议 hard ceiling |
| 多副本重复或丢失探测 | 持久 job、DB 原子 claim、lease 接管、启动恢复和幂等提交 |
| credential fingerprint 不稳定或可枚举 | 安装级 secret、版本化 HMAC、稳定 OAuth/IAM principal 和禁止裸哈希 |
| profile 更新拖慢热路径 | 独立 capability epoch 与 RCU snapshot，正常请求零 DB 查询 |
| 客户端错误泄露路由拓扑 | 外部聚合错误限长，逐 Target 报告仅进入鉴权 Admin 与内部 telemetry |
| 能力目录首期范围过大 | 完整 Catalog 与最小 Enforced 子集分离，按垂直闭环逐项晋级 |

## 15. 回滚策略

1. 所有能力感知路由由 `target_capability_routing_mode=off|shadow|enforce` 控制，紧急回退设置为 `off`。
2. CRL 提升由独立设置 `responses_crl_tool_promotion_enabled` 控制。
3. 关闭能力路由后恢复现有目标排序和 fallback，不删除已存 profile。
4. ProbeWorker 可以独立暂停，不影响正常请求。
5. 数据库新增表为旁路表，回滚二进制后旧版本忽略，不修改现有 Provider/Route 主表语义。
6. 人工 override 在功能关闭期间保留，重新启用后继续生效。
7. ProbeWorker 暂停时不清空 job；恢复后按 next_attempt_at 和 lease 状态继续执行。
8. `egress_dialect_id` 为可选字段，旧版本或旧 Route 缺失时保持现有 endpoint 推导行为。

## 16. 完成定义

本方案完成的最低闭环是：

```text
创建第三方 Responses Target
  → 计算稳定 TargetKey 与 auto/explicit dialect
  → 持久化任务异步探测 CRL 与顶层 tools
  → 保存带证据的能力画像
  → 发布到零 DB 查询的 CapabilitySnapshot
  → Codex CRL 请求到达
  → 路由前选择原生透传或工具提升计划
  → 不兼容 Target 被过滤
  → 上游产生真实工具调用
  → TiyGate 返回 ToolCalls finish reason
  → tool output 可继续并得到最终回答
```

最低闭环同时满足进程重启恢复、多副本 lease、shadow/enforce 切换、stale-while-revalidate、内部完整诊断和外部错误脱敏。其余能力分别声明实现状态、可用发现方式和路由资格，并独立通过 Shadow 准入后才能 enforce。
