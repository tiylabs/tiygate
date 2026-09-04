# Target 能力发现与协议转换实施方案

> 状态：修订后实施基线
> 首期交付：阶段 0–6C 的可实施、可验证计划；阶段 7 仅保留为 Roadmap
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
- CRL `namespace` 递归产生带完整 namespace path 的 typed `tools.namespace` constraint；仅验证某个 namespace 不得授权其他 path。
- Codex/Multi-agent Beta header 或 item 只影响其对应 extension requirement。
- 未出现私有特征时保持标准 dialect。

Egress dialect 支持 `explicit` 与 `auto`：

- `explicit` 由 Route Target 配置固定，适用于已知私有端点。
- `auto` 以标准 dialect 为基线，允许 Target 证据启用 `ExtensionUnknown` 能力，但不会因单一能力成功而自动宣称整个私有 dialect 已实现。
- 探测可以给出 `detected_extensions`，只有能够唯一证明完整方言契约时才给出 `detected_dialect_id`。

`RouteTarget` 增加可选 `egress_dialect_id`；未设置时为 `auto`。dialect 参与 TargetKey、能力解析、探针选择和 Admin 展示。

对于未注册的 OpenAI-compatible Provider，显式设置
`egress_dialect_id=openai-responses-standard` 或
`openai-responses-codex-lite` 同时选择 Responses egress endpoint；未设置时仍使用
Provider 的默认 Chat Completions endpoint。该快捷配置只改变 wire profile，不宣称
目标已支持任何能力，仍必须经过对应 TargetKey 的探测或显式证据。

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
    Unknown { kind: String, value: serde_json::Value },
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
- 未知 value kind 反序列化为保留原始 `kind/value` 的 `Unknown`（等价于 Opaque），可在 DB、Admin 和导入导出中往返，但不得参与自动路由。
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

首期默认 TTL 固定为：SemanticProbe/SuccessfulTraffic 正面证据 fresh 24 小时、stale grace 7 天；明确拒绝的负面证据 fresh 6 小时、stale grace 24 小时；Inconclusive 诊断保留 1 小时且永不改变路由结论；ProviderDocumentation/ExactModelCatalog 由来源版本控制，版本失效立即转为 Unknown。override 的 `expires_at` 由 API 明确返回，设置时最长 30 天；兼容旧数据的空值 override 必须在 Admin/UI 标为长期人工结论并要求重新确认，续期需要新的 actor/reason 和审计事件。TTL 默认值随 capability schema/evidence 版本管理，变更时必须触发受影响画像重建，不把空值解释为永久有效。

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
├── matrix.toml
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

完整能力目录可以先处于 Cataloged/Disabled；第一版只有 CRL 工具闭环所需的 HTTP/SSE、函数/continuation、`tool_choice` required/specific、namespace、custom 和 `additional_tools` 条目进入 Implemented，并根据证据确定性分别设置 ShadowEligible 或 EnforceEligible。通用能力过滤不等于新增转换维度，未登记 transform 的协议组合仍保持旧路径或 fail-closed。

`registry.toml` 中的 `enforce_eligible_ids` 是首期 Enforce 白名单；构建期要求每个
`EnforceEligible` descriptor 都在该列表中，且列表中的 ID 必须是已实现的
`EnforceEligible` descriptor。新增 descriptor 即使完成 codec，也必须先更新白名单、
matrix、版本与验收 fixture，不能仅修改状态字段扩大生产路由范围。

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
  requires tools.crl.additional_tools = Supported（或满足约束的 Constrained）
  transform responses.pass_through

Promotion Plan:
  requires egress 顶层 tools carrier 且 tools.namespace = Supported（或满足约束的 Constrained）
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

Responses decoder 扩展现有 `responses_opaque_input_items` 有序机制保存 `additional_tools` 的原始 item 和 index，同时解析内部工具类型生成 requirements；namespace requirement 同步保存完整 path 的 typed constraint；不得另建平行 opaque carrier，也不得继续退化为空 developer message。

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
    registry_version INTEGER NOT NULL,
    baseline_version INTEGER NOT NULL,
    profile_status TEXT NOT NULL,
    resolved_capabilities_json TEXT NOT NULL,
    observations_json TEXT NOT NULL,

    last_probe_suite_version INTEGER,
    last_probe_judge_version INTEGER,
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

安装级 fingerprint secret 单独管理，不与当前 `TIYGATE_MASTER_KEY` 的轮换耦合：

```sql
CREATE TABLE installation_secrets (
    name TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    encrypted_value TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

首次启动在事务中以 CSPRNG 创建 `target-key-hmac/v1`；读取和 HMAC 计算只发生在 `store`，明文只存在于进程内短生命周期。配置了 `TIYGATE_MASTER_KEY` 时，`encrypted_value` 使用独立 purpose 加密；无主密钥的开发/兼容模式只允许持久化随机 base64 材料并明确告警，生产部署必须配置主密钥。主密钥轮换仅重新加密 `encrypted_value`。如确需主动更换 fingerprint secret，必须先建立旧 key → 新 key 的受控重建窗口，在所有 Target profile/job/admission 重算前保持 `off`，不得静默造成 TargetKey 碰撞或画像错配。

`capability_probe_budgets(scope, day, used)` 与 installation secret 一并由 config migration 建立；`scope` 使用 TargetKey 或 `__global__`，`day` 为 UTC 日期，消费通过事务同时更新两级计数，任一级超限即回滚。

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

首期配置导出不包含 profile、observation、probe job 或 fingerprint secret；若管理员选择导出
override，新增 `capability_overrides` 数组，并用不含凭证的目标定位器绑定到导出中的
`route_id + target_index + provider_id + model_id + egress_dialect_id + account_label`。导入时
先完成 Provider/Route 写入，再按定位器在当前安装重新计算 TargetKey；定位不到、匹配不唯一或
身份字段不一致的 override 必须跳过并在 ImportReport 中计数，禁止按“相同模型名”静默套用到
其他 Target。`capability_id`、state、typed value、reason 和 expires_at 原样保留并限长，未知
ID 仍可往返但不能参与 enforce；该定位规则和跨安装失败/恢复样例纳入 `V-IDENTITY`、
`V-DB-RECOVERY` 与 `AC-PERSISTENCE`。

`capability_overrides` 作为 `ConfigExport.schema_version=2` 的可选扩展加入（不改变既有
字段语义）；新字段使用 `#[serde(default)]`，新导入器读取旧 bundle 时视为空，旧导入器
可安全忽略该未知字段。若后续改变 selector 或状态语义再提升 schema 版本，遇到新版本
时必须返回明确的 unsupported-version 错误。`ImportSelection` 与 `ImportReport` 同步
增加 override 的选择和 imported/skipped 计数，选择集合为空仍表示不导入任何 override。

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
    next_probe_index INTEGER NOT NULL DEFAULT 0,
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

`probe_set_hash` 基于排序后的 capability ID 和 probe version 生成；相同 TargetKey/probe set 的重探测复用并重置同一 job。任务状态为 `pending/running/complete/partial/cancelled/failed`。`partial` 状态同时保存 `next_probe_index`：优雅停止或预算耗尽时从游标继续，语义结果 Inconclusive 时将游标归零并延迟重试，避免重复调用或对静默响应热循环。worker 使用单条条件更新原子 claim 到期或未持有 lease 的任务；lease 到期后可由其他副本接管，执行和结果提交均幂等。

进程启动时扫描 pending、lease 已过期的 running 和需要重探测的 stale profile。任务不保存凭证，执行时通过 TargetKey 从当前配置快照取得有效 Target；身份已变化或 Target 已无引用时取消。

### 8.4 热路径能力快照

`server::AppState` 持有独立的只读能力快照：

```rust
pub struct CapabilitySnapshot {
    pub epoch: i64,
    pub loaded: bool,
    pub profiles: HashMap<TargetKey, TargetCapabilityProfile>,
    pub admissions: HashMap<(String, String), CapabilityRouteAdmission>,
}
```

- 使用 `ArcSwap` 或等价 RCU 结构原子替换。
- 服务启动时从 profile 与 override 表构建；正常请求只读取内存，不同步查询 DB。
- profile/override 提交后递增独立 `capability_epoch`，本地 write-through 更新，其他副本由后台 watcher 刷新。
- capability epoch 与配置 epoch 分离，频繁探测结果不得触发完整 Provider/Route 配置重载。
- snapshot 中保存已应用 baseline、observation、override 和 TTL 后的 resolved 结果，以及当前版本的 Route × shape admission；planner 不在热路径解析数据库 JSON。`loaded=false` 或版本/迁移校验失败的快照只能提供诊断，不能参与 Shadow/Enforce。

能力画像默认保留 JSON，以便新增能力不做表结构迁移。常用筛选字段后续可以增加索引投影，但第一阶段不提前优化。

### 8.5 Route × capability shape 准入记录

全局/per-route `CapabilityRoutingMode` 只提供模式上限；要满足“按能力形状灰度、准入和自动回退”，必须另存一条最小准入记录。该记录在阶段 6A 建立（配置迁移版本为 `20260829000003_capability_route_admissions`，typed requirement 列由 `20260829000008_capability_admission_requirements` 补充，PostgreSQL 使用同名迁移），阶段 2 只预留 epoch/版本边界：

```sql
CREATE TABLE capability_route_admissions (
    route_id TEXT NOT NULL,
    capability_shape_hash TEXT NOT NULL,
    required_capabilities_json TEXT NOT NULL,
    required_requirements_json TEXT NOT NULL DEFAULT '[]',
    mode TEXT NOT NULL,
    gate_policy_version INTEGER NOT NULL,
    report_json TEXT NOT NULL,
    approved_by TEXT,
    approved_at TEXT,
    expires_at TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (route_id, capability_shape_hash)
);
```

约束：

- `capability_shape_hash` 由排序、去重后的 Required capability ID 与 constraint 摘要计算，不包含请求内容、TargetKey 或凭证。
- `required_capabilities_json` 是兼容展示字段；`required_requirements_json` 保存规范化的 Required 叶子及 typed constraint。旧行为空数组时按 ID 恢复无约束 requirement，新写入必须同时校验两者的 ID 集合一致。
- shape hash 算法带版本前缀（当前 `shape/v1`）；Required capability 与 constraint 数量、`report_json` 大小有上限，报告只保存脱敏摘要而非原始请求/响应。
- `mode=enforce` 必须有通过当前 gate policy 的 `report_json`、批准人和批准时间；`expires_at` 到期自动回到 shadow。
- Route 级 mode 是上限：Route 为 `off` 时所有 shape 均 off；Route 为 `shadow` 时不得执行 shape enforce；只有 Route 与 shape 均为 enforce 才能过滤目标。
- planner、Admin API 和 watcher 只按 `(route_id, shape_hash, revision)` 读取；准入记录更新递增 capability epoch，不触发完整配置重载。
- Route 删除或能力 registry/evidence 版本变化时，相关 admission 标记 stale，不能继续 enforce，历史报告保留用于审计。

SQLite/PostgreSQL 迁移、CRUD、条件更新和过期处理纳入 `AC-ADMIN` 与 `AC-PERSISTENCE`；没有该记录时保持现有 `off/shadow` 行为。

### 8.6 Shadow 计划与业务反馈遥测

Shadow 诊断与请求主日志分离，但必须使用同一套异步 OLTP 管道持久化，不能只依赖进程内计数器：

```sql
CREATE TABLE request_capability_plans (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    target TEXT NOT NULL,
    ts TEXT NOT NULL,
    mode TEXT NOT NULL,
    shape_hash TEXT NOT NULL,
    planning_micros INTEGER NOT NULL,
    status TEXT NOT NULL,
    requirements_json TEXT NOT NULL,
    missing_json TEXT NOT NULL,
    unknown_json TEXT NOT NULL,
    transform TEXT,
    evidence_json TEXT NOT NULL,
    UNIQUE (request_id, target)
);

CREATE TABLE request_capability_feedback (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    shape_hash TEXT NOT NULL,
    target TEXT NOT NULL,
    capability TEXT NOT NULL,
    outcome TEXT NOT NULL,
    ts TEXT NOT NULL,
    UNIQUE (request_id, target, capability)
);

CREATE TABLE request_capability_telemetry_gaps (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    shape_hash TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL,
    dropped_count INTEGER NOT NULL DEFAULT 1,
    first_ts TEXT NOT NULL,
    last_ts TEXT NOT NULL,
    PRIMARY KEY (request_id, route_id, shape_hash, target, reason)
);

CREATE TABLE capability_probe_runs (
    run_id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    probe_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    duration_micros INTEGER NOT NULL DEFAULT 0,
    budget_weight INTEGER NOT NULL DEFAULT 1,
    error_class TEXT,
    ts TEXT NOT NULL
);
```

- `status` 固定为 `compatible`、`incompatible`、`unknown` 或 `planner_error`；`outcome` 固定为 `success`、`capability_rejection`、`inconclusive` 或 `error`，其中 `error` 必须带受限 `error_class`。
- `request_capability_telemetry_gaps` 记录计划/反馈无法确认落盘的窗口；相同 request、Route、shape、target 和 reason 幂等累加 `dropped_count`，窗口存在任一 gap 即不可准入。
- `capability_probe_runs` 只记录受审计的 probe id、TargetKey、结果、时延和预算权重，用于独立探针审计/成本统计，不进入业务 request quota、HealthRegistry 或 EWMA。
- `target` 只存内部 TargetKey/health key，表中不得出现 API Base、账号、凭证、prompt、工具 schema 或原始上下游 body；`requirements_json`、`missing_json`、`unknown_json` 和 `evidence_json` 均有条数与字节上限。
- 写入采用至少一次投递和幂等 upsert；乱序、重复、进程重启和消费失败不得重复放大分子。能力计划/反馈属于准入关键事件，队列背压时必须进入可恢复 outbox 或明确计入 `telemetry_gap`，不能静默丢弃；存在 gap 的观察窗口不得通过 enforce 门禁。聚合查询按 `route_id × shape_hash × [since, until)` 计算，并同时读取 probe job 的终结错误窗口。
- 聚合结果必须显式给出 `profile_resolution_coverage`、`compatible_shape_coverage`、`verified_success_disagreement_rate`、`planner_unknown_rate`、`probe_terminal_error_rate`、`planner_internal_error_rate` 和 `planning_latency_p95`；没有对应分母时返回“无样本”，不能返回伪造的 100%。
- 遥测表遵循 request log 的保留期与清理策略；迁移、索引、幂等和脱敏列入 `V-SHADOW`、`V-DB-RECOVERY` 和 `AC-PERSISTENCE`。

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
- 探测请求标记内部 `probe_run_id`，不计入 TiyGate 客户端配额、业务请求计数或 fallback 预算；
  上游服务商的 API Key 限流与计费不假定可绕过。
- 探测产生的上游费用和限流响应仍需在 Admin UI 中可观测。
- 达到预算时任务保持 pending 并设置 `next_attempt_at`，已有 profile 不被降级。
- 默认只运行基础 bundle；工具、PTC、reasoning、structured output 和多模态 bundle 按 endpoint、请求需求和管理员策略选择。

### 9.4 启用与升级策略

运行时设置使用枚举而不是单一 bool：

```text
gateway.capabilities.routing_mode = off | shadow | enforce
```

探测控制采用以下运行时设置（均为可热更新的非敏感值）：

```text
gateway.capabilities.probe_enabled
gateway.capabilities.probe_daily_budget
gateway.capabilities.probe_global_budget
gateway.capabilities.probe_global_concurrency
gateway.capabilities.probe_provider_concurrency
gateway.capabilities.probe_account_concurrency
gateway.responses.crl_tool_promotion_enabled
```

预算在数据库中按 `scope × UTC day` 原子计数，跨进程共享；并发限制在进程内按全局、Provider 和 account 三层动态 gate 生效。预算耗尽的 job 延迟到下一个 UTC 日，不消耗重试次数；并发/网络错误仍遵循 job 的有限重试策略。

- 新版本 `gateway.capabilities.routing_mode` 默认 `off`；ProbeWorker 默认按
  `gateway.capabilities.probe_enabled` 异步回填现有 Target，且不改变业务路由。
- `gateway.responses.crl_tool_promotion_enabled` 默认 `false`；只有管理员显式开启且
  Route/shape 通过准入后才可生成 promotion body，历史部署的显式设置按配置优先级保留。
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

业务流量的可验证成功语义另存于 `request_capability_feedback`（按 request、Target、shape、capability 幂等），用于计算 `verified_success_disagreement_rate`；未调用工具、普通 `message` 或无法归因的输出不写入成功或负面证据。

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
GET  /admin/v1/target-capabilities
GET  /admin/v1/target-capabilities/:target_key
POST /admin/v1/target-capabilities/:target_key/probe
GET  /admin/v1/target-capabilities/:target_key/probe-runs
GET  /admin/v1/target-capabilities/:target_key/probe-jobs
GET  /admin/v1/target-capabilities/:target_key/probe-jobs/:job_id/runs
GET  /admin/v1/target-capabilities/:target_key/probe-runs/:run_id
PUT  /admin/v1/target-capabilities/:target_key/overrides
DELETE /admin/v1/target-capabilities/:target_key/overrides/:capability_id
GET  /admin/v1/capability-registry
GET  /admin/v1/capability-metrics
GET  /admin/v1/probe-jobs/:job_id
PUT  /admin/v1/capability-probes
GET  /admin/v1/routes/:route_id/capability-admissions
POST /admin/v1/routes/:route_id/capability-admissions
DELETE /admin/v1/routes/:route_id/capability-admissions/:shape_hash
```

现有 request attempt 详情增加内部 `CompatibilityReport`，不新增面向普通客户端的逐 Target 诊断接口。profile、job、metrics、admission 和 report 列表必须分页；registry 是编译期的有界静态集合（超过上限时也按相同分页契约返回），并返回 `contract_schema_version` 与 registry/baseline/matrix/probe 数量摘要。所有错误和 observation detail 必须脱敏、限长。

`probe-jobs` 返回目标的持久化任务历史；任务下的 `probe-jobs/:job_id/runs` 返回每次运行摘要，`probe-runs/:run_id` 返回版本化的详情对象。详情可包含多条有序 exchange（请求路径、脱敏请求头/请求体、HTTP 响应状态/类型/响应体）及结构化 judge 结果；不包含 API Base、认证凭证或账户标识，单次响应和详情总大小均受限，历史记录缺失详情时返回 `null`。

Admin mutation 的统一响应契约：

| 情况 | HTTP | 稳定 code | 处理要求 |
|------|------|-----------|----------|
| probe 入队 | `202` | `probe_queued` | 返回 job 摘要，不等待上游探测 |
| 同一幂等键重放 | 原操作状态 | `replayed` | 返回首次响应，不能重复写状态或审计 |
| 同键不同载荷 | `409` | `idempotency_conflict` | 返回冲突，不执行 mutation |
| revision/ETag 过期 | `409` | `revision_conflict` | 返回当前 revision 摘要，要求客户端重新读取 |
| 请求违反 registry、baseline 或门禁 | `400`/`409` | `invalid_capability` / `admission_required` | 不产生部分写入 |
| capability store/migration/snapshot 不可用 | `503` | `capability_unavailable` | 返回受限原因和 request_id，不伪造 Unknown 报告 |

错误体只允许 `code`、限长 `message`、`request_id` 和脱敏 `details`；响应状态、错误 code、幂等重放和审计结果必须在 OpenAPI/JSON fixture 中固定。

准入写入请求同时支持兼容的 ID 形式和带约束的规范形式：

```json
{
  "required_capabilities": ["transport.http", "tools.namespace"],
  "required_requirements": [
    {"id": "transport.http", "strength": "required"},
    {
      "id": "tools.namespace",
      "strength": "required",
      "value": {"kind": "enum_set", "value": ["functions"]}
    }
  ],
  "mode": "enforce",
  "shape_hash": "shape/v1:…",
  "expected_revision": 1,
  "expires_at": "…",
  "reason": "已验证 functions namespace"
}
```

`required_requirements` 省略时，服务端把 `required_capabilities` 展开为无约束 Required 叶子；两者同时提供时必须拥有相同的 ID 集合。服务端按规范化 requirement 重新计算 `shape_hash`，校验 descriptor 的 value kind/matcher、Forbidden baseline、门禁报告和版本，再写入 `required_requirements_json`；客户端提交的 hash、指标或通过标记不具备授权效力。带 namespace、范围或枚举约束的真实请求必须使用带约束的独立 shape，不能复用无约束 admission。

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

### 12.0 阶段编排与通用交付约束

阶段 0–6C 是首期可交付闭环；阶段 7 只保留为 Roadmap，不纳入本期实现、资源和发布验收。

| 阶段 | 主要代码边界 | 前置阶段 | 允许并行关系 | 生产数据路径上限 |
|------|--------------|----------|--------------|------------------|
| 0 | `protocol-specs`、`docs`、`crates/protocols` 校验 | 无 | 无 | `off` |
| 1 | `crates/core`、`crates/protocols` | 0 | 无 | `off` |
| 2 | `crates/store`、`crates/server` 快照与配置写入 | 1 | 无 | `off` |
| 3 | `crates/server` 探针、`crates/store` 任务恢复 | 2 | 可与阶段 4 的离线 transform 开发并行 | `off` + 独立 ProbeWorker |
| 4 | `crates/protocols` CRL codec/transform、`crates/server` 计划执行 | 1、2；生产准入依赖 3 | 可与阶段 3 并行开发 | `off` |
| 5 | `crates/core` planner 输入、`crates/server` shadow、`crates/store` 遥测 | 2、3、4 | 无 | `off` 或 `shadow` |
| 6A | `crates/admin`、`crates/store`、`crates/server` 控制面 | 5 | 无 | `off` 或 `shadow` |
| 6B | `crates/server` enforce/fallback、`crates/protocols` 目标计划执行 | 5、6A | 可与 6C 并行 | 仅通过门禁的 per-route/per-shape `enforce` |
| 6C | `webui`、`crates/admin` API 契约适配 | 6A | 可与 6B 并行 | 不改变数据路径 |

表中的“并行”只允许在不改变生产数据路径的离线代码、fixture 和契约工作包之间进行；任何
worker 启动、Shadow 采样、准入计算或 Enforce 流量都必须等待对应前置阶段的退出标准，不能以
并行开发代替阶段交接。

#### 12.0.1 首期范围与运行前提

阶段 0–6C 交付一个可发布的垂直闭环，而不是一次性实现目录中的全部能力：

- **可进入 Enforce 的完整转换闭环**是 OpenAI Responses ingress/egress 的 CRL `input[].additional_tools`，包括 native passthrough、提升到顶层 `tools`、function/custom/namespace、`tool_choice` required/specific、SSE 和 continuation。通用 function/transport 能力仍可用于跨协议的目标过滤，但没有已注册 transform 的组合不得改变请求编码；其余高级能力保持 Shadow/Disabled。
- **通用基础设施必须跨协议可复用**：Chat Completions、Anthropic Messages、Responses、Gemini 和 Embeddings 都必须能生成稳定的 capability shape 与 CompatibilityReport；没有已注册 transform 的组合保持旧路径或 fail-closed。
- **持久化是 Shadow/Enforce 的硬前提**：没有 `database_url`、能力 migration 未完成或 capability snapshot 无法建立时，只允许 `off`；不得在内存模式中伪造 profile、lease、准入或跨进程预算。Admin 控制面应返回明确的不可用状态，而不是创建易失任务。
- **模式解析是确定性的**：`effective_route_mode = route.capability_routing_mode ?? global_mode`；`off` 始终优先，`shadow` 只记录规划，`enforce` 还必须同时满足未过期的 Route × shape admission、有效 gate policy、可用 snapshot 和目标级计划。缺少任一条件自动按 `shadow` 处理。
- **首期默认值固定为**：`gateway.capabilities.routing_mode=off`、`gateway.capabilities.probe_enabled=true`、`gateway.responses.crl_tool_promotion_enabled=false`。路由模式关闭保证既有业务请求不改变；探测可通过运行时设置独立关闭，CRL 提升必须显式开启，历史部署若已有显式设置必须优先保留该设置。

首期能力白名单与交付边界固定如下；不在表内的能力不得因“已有 codec 支持”自动进入 Enforce：

| Capability ID | 首期用途 | 主要阶段 | 首期路由资格 | 确定性证据 |
|---------------|----------|----------|--------------|------------|
| `transport.http` | 出站端点可用性前置条件 | 3 | EnforceEligible | 基础非流探针/精确目录 |
| `transport.sse` | Responses 流式生命周期 | 3、4 | EnforceEligible | SSE 生命周期探针/被动成功 |
| `tools.function` | 顶层函数及 promotion 依赖 | 3、4 | EnforceEligible | 强制函数调用/显式拒绝 |
| `tools.function.continuation` | tool output 后继续生成 | 3、4 | EnforceEligible | call_id 对应的 continuation |
| `tools.choice.required` | 保留 `tool_choice=required` | 3、4 | EnforceEligible | 强制 required 调用/结构化拒绝 |
| `tools.choice.specific` | 保留指定函数选择 | 3、4 | EnforceEligible | 指定函数调用/结构化拒绝 |
| `tools.namespace` | promotion 的 namespace 能力与路径约束 | 3、4 | EnforceEligible | namespace 唯一函数调用 |
| `tools.custom` | promotion 的 custom 工具依赖 | 3、4 | EnforceEligible | custom call 与 nonce |
| `tools.crl.additional_tools` | native carrier 识别与 A/B 判定 | 3、4 | EnforceEligible | control 成功后的受控 A/B |

`shape/v1` 只允许扁平 `AllOf` 中的 `Required` 叶子；每个叶子包含 capability ID 和可选的规范化 typed constraint。首期 Admin 请求可只提交 ID（等价于 constraint 为 null），但带约束的真实请求不得复用无约束 admission；包含 `AnyOf`、`Not` 或仅 `Preferred` 的形状保持 Shadow，直到后续版本明确其 hash 与准入语义。shape hash 必须由相同的规范化输入重新计算，服务端不得信任客户端传入的 hash。

首期 gate policy 对 CRL shape 启用完整 native/promotion 转换准入；不含 CRL carrier 的普通 Responses、Chat、Messages、Gemini 和 Embeddings shape 只能执行已注册的通用能力过滤，不得因本期 CRL transform 证据而自动改变其请求编码。`tools.function`、`tools.choice.*`、namespace、custom 与 transport 条目既可作为 CRL 依赖，也可在自身已有确定性 transform/过滤契约时单独参与 Shadow/Enforce。

阶段 6B 的发布对象仅是“通过准入的 Route × capability shape”；不能用一次全局开关或一次全量 profile 成功替代逐项准入。阶段 7 不得作为本期任何阶段的隐含依赖。

#### 12.0.2 阶段交付记录与回滚边界

每个阶段的验收记录使用同一最小字段：`stage_id`、代码/配置版本、migration/schema 版本、输入契约版本、交付物清单、验证命令及 fixture、指标窗口、已知限制、回滚开关、批准人。记录必须能由下一阶段直接复核。

回滚边界按阶段固定：阶段 0–4 只能关闭新增 worker/transform 并回到 `off`；阶段 5–6A 只能撤销 shadow/admission；阶段 6B 先把受影响 shape 降为 `shadow`，必要时再把全局模式设为 `off`；阶段 6C 只回滚 UI 资产，不回滚数据表或数据路径。任何回滚都保留 profile、observation、job 和审计记录，避免重新探测时失去诊断依据。

#### 12.0.3 跨阶段版本、状态机与异步可靠性契约

阶段 0–6C 共用同一组可审计的版本元组。版本元组必须随 profile、probe job、admission、telemetry 事件和验收记录保存；只增加可选字段时使用向后兼容的 `serde(default)`，改变 matcher、baseline、transform、hash 或判定语义时提升对应版本并使受影响结论失效。

| 契约 | 首期版本 | 失配处理 | 验证集 |
|------|----------|----------|--------|
| registry/baseline schema | `1` | 构建失败；运行时不发布描述符 | `V-REG` |
| capability profile schema | `1` | profile 仅可诊断，resolved 结果按 Unknown 处理 | `V-DB-RECOVERY` |
| Target identity | `identity/v1` | 重新计算 TargetKey，旧 profile/job/admission 不得写入新身份 | `V-IDENTITY` |
| probe suite/judge | `suite/1`、`judge/1` | 受影响 observation 过期并重新入队；旧证据不得直接授权 enforce | `V-PROBE` |
| capability shape hash | `shape/v1` | admission 立即变为 shadow，必须重新计算 hash 和报告 | `V-SHADOW` |
| gate policy | `1` | admission 不得 enforce，等待新的准入报告 | `V-SHADOW`、`V-ADMIN-UI` |
| config export | `2` | 旧 bundle 可导入；未知主版本返回稳定的 unsupported-version 错误 | `V-IDENTITY`、`V-ADMIN-UI` |
| capability telemetry/migration | `20260829000001`–`20260829000004`、`20260904000001`（log）及对应 config profile/job/admission versions `20260829000006`–`20260829000008` | sink 不得静默丢弃事件；迁移未完成时保持 off-only | `V-TELEMETRY-GAP`、`V-DB-RECOVERY` |

启动和热路径都遵循 fail-closed：未知主版本、迁移缺失、registry 校验失败、shape hash 或 gate policy 失配，只能保留诊断和历史数据，不能生成 Supported、compatible 或 enforce 结论。版本失配、TTL 过期和 Target 身份变化不得删除原始 observation；清理只在诊断保留期之后执行。

状态机固定如下：

| 对象 | 合法状态转换 | 关键不变量 |
|------|--------------|------------|
| Profile | `pending → partial/ready`；`ready/partial → stale`；`stale/error → pending`；探测成功后回到 `ready` | stale grace 内可读，超过 grace 不参与 enforce；错误不清空最后已验证 observation |
| Probe job | `pending → running → complete/partial/failed/cancelled`；lease 到期的 `running → pending` | claim、结果提交和取消均条件更新且幂等；凭证只在执行时从当前 Target 读取 |
| Route × shape admission | `shadow → enforce` 仅经 Admin 准入；`enforce → shadow/off` 可由过期、撤销或 guard 触发 | 系统不得自动晋级；Route mode 是上限，缺少 admission 一律不 enforce |

ProbeWorker、CapabilitySnapshot reloader、admission guard、stale/reprobe 反馈任务和 telemetry outbox consumer 必须由同一组可停止的后台句柄拥有。关闭顺序为“停止新 claim/接收 → 等待当前 I/O 到安全边界 → 释放 lease 或写入 partial → join”；禁止留下 detached task。能力计划/反馈事件只有在持久 outbox 或等价的至少一次管道确认后才算进入观察窗口；首期由保留能力队列和持久 `telemetry_gap` 作为等价路径，队列背压、数据库不可用、序列化/脱敏失败或进程退出导致事件未确认时，必须写入带 `route_id`、`shape_hash`、时间窗、原因和计数的 `telemetry_gap` 记录。存在 gap 的窗口不可通过 enforce 门禁，补偿重放完成后才重新聚合。

planner 的 requirement 提取、profile 解析或 transform 规划出现内部错误时，必须生成 `status=planner_error` 的目标诊断和告警，保持旧数据路径；不得把内部错误伪装成 Unsupported、Unknown 已解决或 `no_compatible_target`。`TargetKey`、`TargetInstanceId` 和 `HealthKey` 为不同的强类型/命名空间，只有在明确的边界适配处转换，禁止以可碰撞的裸字符串混用能力画像与健康状态。

#### 12.0.4 首期验证矩阵

以下 fixture/测试集是阶段验收的固定输入；新增能力或协议只能追加条目，不能用“全量测试通过”替代对应语义验证。

| 验证集 | 覆盖阶段 | 固定输入与判据 | 最低执行方式 |
|--------|----------|----------------|--------------|
| `V-REG` | 0–1 | 合法/非法 registry、baseline、dialect、probe 引用；生成描述符字节稳定 | 构建校验 + `cargo test -p tiygate-protocols capabilities` |
| `V-IDENTITY` | 1–2 | 等价 URL、身份字段变化、API Key/OAuth/IAM scope、主密钥轮换 | core/store 单测与 property test |
| `V-DB-RECOVERY` | 2–3、6A | 双数据库 migration、事务回滚、claim/lease、进程退出、epoch 丢事件和 stale grace | SQLite 集成；PostgreSQL CI fixture；重启脚本 |
| `V-PROBE` | 3 | HTTP/SSE/function/continuation/CRL A/B 的 Positive、ExplicitNegative、Inconclusive、Auth/RateLimited/Transient | wiremock fixture + worker 集成测试 |
| `V-CRL` | 4、6B | native/promotion、混合 Target、顺序/去重/冲突、malformed 与 no-compatible 错误 | protocols 快照 + server wiremock non-stream/SSE |
| `V-SHADOW` | 5–6A | 固定 request/profile 输入、shape hash、实际/规划目标差异、planner/probe error、evidence、指标窗口和阈值 | planner replay + 持久 telemetry 聚合查询 |
| `V-TELEMETRY-GAP` | 5–6B | 队列背压、数据库不可用、序列化/脱敏失败、进程重启、outbox 重放；gap 阻断 gate，补偿后恢复 | 可控故障注入 + outbox/聚合重放测试 |
| `V-LIFECYCLE` | 2–6B | worker/reloader/guard/反馈任务优雅停止、lease 接管、取消和无 detached task | Tokio shutdown fixture + 进程重启脚本 |
| `V-ENFORCE` | 6B | 过滤前置、per-target body、首字节前 fallback、流式断流、自动降级与脱敏 | server 端到端矩阵 + canary smoke |
| `V-ADMIN-UI` | 6A–6C | API 分页/条件更新/幂等/审计/敏感字段，UI 状态、确认、权限、错误恢复 | Admin integration + `npm --prefix webui run lint` + `npm --prefix webui run build` + 浏览器 smoke |

每个验证集都必须同时包含正向、负向和恢复样例；没有对应 fixture 的阶段不得标记完成。`V-DB-RECOVERY` 的 PostgreSQL 用例由 CI service container（固定镜像版本、健康检查和迁移日志）执行，开发者本地至少运行 SQLite 等价用例并记录差异；CI 未执行 PostgreSQL 时不得把阶段标记为通过。

每个阶段必须同时交付以下四类可复用资产，缺一项不得进入下一阶段：

1. **契约**：版本化的数据结构、状态机、错误码、配置键、API/迁移兼容规则，以及与前一阶段的输入输出边界。
2. **实现**：按 crate 分层的代码、feature gate 和默认关闭策略；不得以临时脚本或手工数据库操作替代正式路径。
3. **验证**：正向、负向、边界、并发/恢复（适用时）测试；测试必须能在 CI 或明确声明的 SQLite/PostgreSQL fixture 中重复执行。
4. **运维证据**：指标/日志字段、脱敏规则、回滚开关、故障处置步骤和阶段验收报告。

阶段之间的发布门禁固定为：

```text
spec/contract locked
  → implementation compiled and layered
  → deterministic tests passed
  → failure/recovery tests passed
  → telemetry and rollback verified
  → corresponding AC group signed off
```

所有新配置默认保持兼容行为（`gateway.capabilities.routing_mode=off`、CRL promotion 独立开关关闭）；探测虽默认开启但不进入业务请求热路径，且可独立关闭。阶段验收只能提升模式上限，不能绕过上一阶段的退出标准。生产切换以 Route 与能力形状为最小作用域，禁止以全局开关代替准入报告。

阶段验收记录至少包含：阶段版本与 schema/migration 版本、变更 crate/文件、执行命令及通过结果、测试 fixture/数据库类型、性能与指标窗口、已知限制、回滚开关和批准人。记录保存在仓库文档或受控构建产物中，可由下一阶段复核；未记录的人工操作不计入验收证据。

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
8. 固定首期版本元组（registry/baseline `1`、capability schema `1`、identity `identity/v1`、probe suite/judge `1`、shape hash `shape/v1`、gate policy `1`）及其失效规则。
9. 生成可审计的 registry、baseline、matrix 和 probe manifest 摘要；摘要写入构建产物，供阶段 1–6C 的验收记录和运行时诊断引用。

#### 交付物

- 完整 Catalog 与首期 CRL registry/baseline/dialect 文件。
- conversion-relevant capability 到 `docs/protocol-capability-matrix.md` 的机器可校验映射文件。
- registry schema、构建校验器和生成的静态描述符快照。
- capability-to-matrix 映射清单和 CRL dialect 文档。
- 修改前行为的兼容基线测试。
- 首期范围决议：仅白名单中的 HTTP/SSE 传输、函数/continuation、
  `tool_choice` required/specific、namespace、custom 和
  `tools.crl.additional_tools` 进入 `Implemented`；其他 Catalog 条目保留
  `Cataloged/Disabled` 或 `Implemented/ShadowEligible`，不得进入首期 Enforce。

#### 退出标准

- registry 所有非法 fixture 均按预期失败，合法 fixture 生成结果稳定且可复现。
- 每个首期 capability 都能追溯到 baseline、matrix/dialect 文档、matcher 和 owner。
- Cataloged/Disabled 能力不会进入运行时路由判断。
- 现有 Responses、lossy conversion 和 passthrough 测试无行为变化。
- 版本元组、首期 Enforce 白名单和失效规则有唯一机器可读来源；修改任一语义时能确定受影响的 profile、job、admission 和 fixture。
- 通过验收组 `AC-REG` 和适用的 `AC-QUALITY`。

#### 可启用模式

仅 `off`；本阶段不启动探针，不改变路由和请求编码。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S0-1` 目录盘点 | 逐项登记 Chat、Messages、Responses/CRL、Gemini、Embeddings 的 carrier、损失规则、适用 scope 和 owner | `docs/protocol-capability-matrix.md`、registry 与 baseline 的逐项映射表；缺失项构建失败 |
| `S0-2` registry schema | 固定 TOML schema、ID 命名规则、状态组合、matcher/value kind、依赖、受审计 probe ID 和首期 Enforce 白名单 | 合法/非法 fixture 各一组；重复 ID、循环依赖、非法生命周期、白名单外 enforce 和未登记 probe 均得到稳定错误 |
| `S0-3` CRL 契约 | 固定 `additional_tools` carrier、namespace path、function/custom 依赖、native/promotion 两条路径及冲突错误 | 脱敏请求/响应 fixture；opaque item 顺序、未知字段和错误体快照 |
| `S0-4` 兼容基线 | 固定现有 raw passthrough、model override、header forwarding、lossy conversion 的行为 | 运行旧回归集并保存基线输出；无能力模式下请求/响应字节不变 |
| `S0-5` 版本与生成物 | 固定版本元组、变更失效矩阵、生成描述符摘要和 fixture 命名；构建期输出不得依赖运行时文件 | 修改 schema/matcher/baseline/probe judge 的 fixture 能指出必须失效的对象；生成物在相同输入下字节稳定 |

建议验证命令：

```bash
cargo test -p tiygate-core capability
cargo test -p tiygate-protocols capabilities
cargo test -p tiygate-protocols --test lossy_conversion
scripts/verify-deps.sh
```

阶段 0 的唯一决策是“条目是否进入首期 Implemented/ShadowEligible/EnforceEligible”；不得在本阶段引入运行时探测、数据库表或路由过滤。

#### 阶段交接与回滚

- 交接给阶段 1：冻结后的 registry/baseline/dialect/probe manifest 版本、能力矩阵映射和兼容基线 fixture；后续阶段不得自行增加未登记的 capability ID。
- 回滚：仅撤销生成的静态描述符和构建校验接线；不修改既有 codec、Route 数据或生产请求行为。

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
10. 明确身份计算边界：`core` 只负责 canonical identity、TargetKey/TargetInstanceId 的纯哈希和序列化；安装级 fingerprint secret、credential scope 提取和密钥生命周期只由 `store` 提供受控接口。
11. 统一 `TargetKey`、`TargetInstanceId`、`HealthKey` 的类型转换入口；健康注册表、能力画像和 telemetry 各自只接受对应类型，禁止调用方直接拼接字符串。

序列化契约必须显式带 `schema_version` 和 `identity_version`：未知枚举值、字段和
`CapabilityValue::Opaque` 原样保留；读取旧 Route 时补 `egress_dialect_id=auto`，
读取未知版本时只允许降级为 `Unknown`/`off`，不得猜测为 Supported。TargetKey 的
canonical JSON 字段顺序和字符串规范化规则作为测试 fixture 固定，任何变更都必须
提升 identity 版本并触发阶段 2 的 profile/job 重建。

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
- 纯 core 构建不包含 secret、网络、数据库或 provider 依赖；credential scope 只能通过 store 的受控 provider 得到，且无法从 TargetKey 反推出原值。
- HealthRegistry 使用 TargetInstanceId/HealthKey 的显式适配，能力画像使用 TargetKey；相同值域不会发生跨域误用。
- `scripts/verify-deps.sh` 证明 `core` 未依赖 protocols/store/server 或网络实现。
- 旧 Route JSON 不含 dialect 时可正常加载，路由行为与阶段 0 相同。
- 通过验收组 `AC-REG`、`AC-IDENTITY` 和适用的 `AC-QUALITY`。

#### 可启用模式

仅 `off`；新模型仅被测试和持久化结构使用，不参与实际路由。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S1-1` 纯能力内核 | `crates/core/src/capability.rs` 只提供 typed value、matcher、requirement、resolver、report 和不透明 `TransformId`；禁止协议分支和 I/O | 单测/property test 覆盖 AllOf/AnyOf/Not、集合/范围、Unknown/Forbidden、证据冲突与 TTL |
| `S1-2` 协议适配边界 | `crates/protocols/src/capabilities.rs` 提供静态 registry/baseline、requirement provider 和 transform provider 接口 | 编译期 registry 校验；core 不出现 Responses、CRL、Gemini 或 provider 类型引用 |
| `S1-3` Target 身份 | 在 `core`/`store` 固定 API Base 规范化、版本化 HMAC scope fingerprint、TargetKey/TargetInstanceId 和 `egress_dialect_id=auto` 默认值 | 等价 URL、path/query 边界、API Key/OAuth/IAM/anonymous、dialect/model/account 变化的确定性测试 |
| `S1-4` 兼容适配 | 保留 `EndpointCapabilities` 读适配层，旧 Route JSON 可反序列化；新 planner 类型不改变旧 codec 输出 | 旧 Route fixture、序列化往返、`scripts/verify-deps.sh` 和 workspace check |
| `S1-5` 身份/健康边界 | 由 store 注入 fingerprint secret；core 仅接收脱敏 scope fingerprint；HealthRegistry 不接受 TargetKey 字符串替代 TargetInstanceId | 编译依赖检查、类型边界测试、密钥不可逆/轮换测试、健康与能力状态隔离测试 |

阶段 1 必须明确两个不可变约束：TargetKey 只标识出站身份，不承载 weight/enabled/Route 顺序；`CapabilityProfile` 与 HealthRegistry 使用不同命名空间。建议验证命令：

```bash
cargo test -p tiygate-core capability
cargo test -p tiygate-store capabilities
cargo test -p tiygate-protocols capabilities
scripts/verify-deps.sh
```

#### 阶段交接与回滚

- 交接给阶段 2：稳定的 capability/requirement/report/TargetKey 序列化版本、未知字段保留规则和 resolver 语义测试结果；store 只能依赖公开类型，不得复制 matcher 逻辑。
- 回滚：保留旧 Route 反序列化适配层，停止新 planner 调用即可恢复阶段 0 的路由和 codec 行为；不得删除已写入的未知 capability 数据。

### 阶段 2：持久化、快照与任务恢复

#### 目标

建立 SQLite/PostgreSQL 对等的能力状态、可靠任务和零数据库查询热路径。

#### 进入条件

- 阶段 1 的 TargetKey、typed value、observation 和 resolver 序列化格式已稳定。
- profile、override、job 和 epoch 的 schema version 已确定。

#### 实施内容

1. 增加 profile、override、probe job、安装级 fingerprint secret、按日探测预算和 capability epoch migration。
2. 实现 profile/observation CRUD、typed value 校验、能力级 TTL、fresh/stale 查询和未知 ID 保活。
3. 实现 override CRUD、baseline Forbidden 校验、过期处理和审计所需 actor/reason 字段。
4. 实现 probe job upsert、probe_set_hash、原子 claim、lease 续期/接管、next_attempt_at、幂等提交和取消。
5. 启动时恢复 pending、过期 running 和 stale profile；无有效 Target 引用的任务安全取消。
6. 在 AppState 增加 CapabilitySnapshot，使用独立 capability epoch、原子替换和后台 watcher。
7. profile/override 写入采用本地 write-through；其他副本通过 epoch 刷新，不触发完整配置重载。
8. 更新导入导出：profile/job 不导出，override 可选择导出；使用 route/target 定位器在目标安装重新计算 TargetKey，未知 capability ID 原样保留，fingerprint secret 永不导出。
9. 增加数据库故障、并发 claim、进程退出、lease 超时、epoch 丢事件和 snapshot 重建测试。
10. 持久化并校验 profile/job/admission 的版本元组；未知主版本、migration 缺失或 JSON 校验失败时只允许诊断读取，禁止构建可 enforce 的 snapshot。
11. 固定 profile、job、admission 的状态机和条件更新语义；任何终态、过期或取消操作都必须保留原因、时间和最后有效 observation。
12. 将 snapshot reload、probe worker、admission guard 和 stale/reprobe 反馈任务纳入统一生命周期句柄，定义暂停、优雅关闭、重启接管和异常恢复的顺序。

迁移顺序固定为“能力旁路表 → epoch/secret/budget → profile/override/job CRUD →
snapshot/reloader”；任一 migration 或 schema 校验失败时，启动检查必须阻止
Shadow/Enforce，而不是以空 profile 继续运行。无 `database_url` 的 legacy/in-memory
模式不创建能力任务、不执行主动探测，所有 capability-aware mode 保持 `off`。
预算、并发、TTL、page size 和 stale grace 等运行参数必须有最小/最大边界；非法或
溢出更新返回校验错误并保留上一版本，不能把 `0`、负数或极大值解释为“无限制”。

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
- 版本元组失配、迁移不完整、快照重建失败和解析异常均保持 `off`/诊断模式，不产生伪造的 Supported 或 enforce 结论。
- profile/job/admission 状态转换、条件更新和统一 shutdown 在 SQLite 与 PostgreSQL 上均可重放；不存在无法接管的 running job 或 detached background task。
- 通过验收组 `AC-IDENTITY`、`AC-PERSISTENCE` 和适用的 `AC-QUALITY`。

#### 可启用模式

仅 `off`；允许后台构建空 profile/job 基础设施，但不发送主动探测请求。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S2-1` schema 与密钥 | SQLite/PostgreSQL 建立 profile、override、job、epoch、安装级 fingerprint secret 和 Route × shape admission；admission 同时保存 ID 与 typed requirement JSON；所有 JSON 字段带 schema version；在 CI 增加固定 PostgreSQL service container 与迁移门禁 | 双数据库迁移版本一致；主密钥轮换只重保护 secret，不改变既有 TargetKey；typed namespace/range shape 可往返且 hash 稳定；导出不含 profile/job/secret 且可选携带 override；CI 未运行 PostgreSQL 时阶段不通过 |
| `S2-2` 配置写入事务 | Route、Provider credential、API Base、endpoint、model、dialect、account/scope 等身份变更与 `ensure_target_capability` 在同一事务提交；OAuth access-token refresh 不改变 scope fingerprint；配置导入按目标定位器重绑定 override | 提交成功必有对应 profile/job；任一事务回滚时配置、profile、job、epoch 均不对外可见；并发更新不产生孤儿任务；无法唯一匹配的 override 可解释地跳过 |
| `S2-3` 任务状态机 | 固定 `pending/running/complete/partial/cancelled/failed`、lease、重试和幂等提交；`partial` 持久化 `next_probe_index` 并从游标恢复，Inconclusive 重试归零游标且受 `max_attempts` 限制；旧 TargetKey 结果不得写入新 profile；admission 采用 revision 条件更新 | 双副本并发 claim、lease 接管、worker 在 claim/执行/提交前退出、游标恢复、Inconclusive 延迟与 attempts exhausted、重复提交、admission revision 冲突和 Target 删除测试 |
| `S2-4` snapshot 发布 | 启动加载、write-through、epoch watcher、解析失败保留旧快照；profile/override 变更不触发完整配置重载 | 快照 epoch 单调、跨副本最终一致；请求路径仅内存读取；数据库不可用时保留最后可用快照并告警 |
| `S2-5` 生命周期与清理 | profile 过期/删除保留诊断窗口；无 Route 引用的旧画像和已完成 job 延迟清理；导入 override 时按目标定位器重绑定或显式跳过 | stale grace、清理保留期、重启恢复、清理与读取并发、跨安装 override 匹配/跳过测试 |
| `S2-6` 版本与统一生命周期 | profile/job/admission 保存版本元组；reloader、worker、guard、反馈队列由可停止句柄拥有；失配只读诊断 | 未知版本 fail-closed、迁移缺失、snapshot 解析失败、优雅关闭、lease 接管和无 detached task 检查 |

身份变更无法与现有配置事务共享连接时，必须采用带 `config_epoch` 的持久 outbox/reconciliation，并将“配置已生效但能力任务未入队”作为未通过状态；不得用内存 channel 作为唯一投递保证。

建议验证命令：

```bash
cargo test -p tiygate-store capabilities
cargo test -p tiygate-store --test postgres_capabilities -- --nocapture
cargo test -p tiygate-server --test epoch_e2e
cargo test -p tiygate-server --test capability_routing
```

#### 阶段交接与回滚

- 交接给阶段 3/5：双数据库 migration 版本、store repository API、snapshot epoch 语义、job 状态机和恢复 fixture；没有持久化 store 的运行模式明确标记为 `off-only`。
- 回滚：停止 ProbeWorker/reloader，保留旁路表和最后快照；旧二进制可忽略新增表，配置主表与既有 route 行为不回退或重写。

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
10. 为每次执行保存受限的 `probe_run_id`、probe/judge 版本、attempt generation 和结果摘要；同一提交键重复到达时只保留一次 observation 和一次计费/审计记录。
11. 将认证刷新、stale 标记和重探测入队放入可恢复的后台队列；请求返回前不等待这些 I/O，关闭时按统一句柄完成或安全延期。

每个 probe manifest 必须包含 `probe_id`、适用 `WireProfileId`、输入 schema、唯一
变量、control、超时/最大 token、预算权重、`probe_suite_version` 和判定器版本。
判定器只能读取结构化 HTTP/JSON/SSE 结果及 nonce，不得通过自由文本相似度推断支持；
同一 job 的提交键为 `target_key + probe_set_hash + probe_id + attempt_generation`，
重复提交只能保留一个 observation。

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
- 每个结果都能关联 TargetKey、probe_set_hash、probe/judge 版本和唯一 attempt generation；重复运行不会重复改变 resolved profile、预算或费用统计。
- worker、lease renew、stale/reprobe 入队和认证刷新在优雅关闭/进程重启后可恢复，不存在 detached task 或永久 running job。
- 通过验收组 `AC-PROBE`、`AC-PERSISTENCE` 和适用的 `AC-QUALITY`。

#### 可启用模式

路由保持 `off`；允许显式启动 ProbeWorker 回填 profile。主动探测开关可以独立关闭。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S3-1` egress 复用 | 探针复用正常 egress 的 URL、AuthApplier、代理、TLS、redirect stripping、超时和 codec；不复制一套可漂移的 HTTP 客户端 | 相同 Target 的业务请求与探针在认证、URL 和代理配置下生成一致出站元数据 |
| `S3-2` 安全执行器 | `probe_id` 只能来自编译后的 allow-list；nonce、最大 token、请求/响应限长、SSRF/地址安全、全局/provider/account semaphore 和预算在执行器统一校验；DNS 解析结果与每次 redirect 目标重新校验 | 未登记 probe、私网/非法地址、超预算、超时、取消和并发上限均被拒绝或延期；无 hosted/shell/MCP/写操作 |
| `S3-3` 证据判定 | 每个 probe 明确 control、实验变量、成功条件、ExplicitNegative 条件；namespace 必须验证 namespace 身份+nonce；模型未按提示调用、静默忽略和随机输出只产生 Inconclusive | Positive、ExplicitNegative、Inconclusive、Auth/RateLimited/Transient Error 的 wiremock fixture 均有稳定结果；错误不清空旧 profile；扁平 function call 不得标记 namespace Supported |
| `S3-4` worker 生命周期 | ProbeWorker、lease renew、恢复扫描和 snapshot 发布纳入 `App` 的 stop handle；优雅关闭先停止 claim，再等待当前 probe 到安全边界 | SIGTERM/应用 shutdown 后无新 claim，运行中任务 lease 可接管，重启后只恢复可执行任务；不会遗留 detached task |
| `S3-5` 首期协议范围 | 首期只对 Responses/CRL 与通用 HTTP/SSE/function/continuation 产生可 enforce 证据；Anthropic/Gemini 未实现 continuation 的探针必须标记 Inconclusive，并保持非 EnforceEligible | registry 中的 probe allow-list、协议适用性和 profile 状态一致；未闭环协议不会被误判为 Unsupported |
| `S3-6` 幂等与后台反馈 | 以 `target_key + probe_set_hash + probe_id + attempt_generation` 为唯一提交键；stale/reprobe、认证刷新和费用/审计事件走可恢复队列 | 重复提交、队列背压、进程退出和恢复扫描只产生一份结果；请求延迟不等待后台 I/O，所有句柄可 join |

建议验证命令：

```bash
cargo test -p tiygate-server probe
cargo test -p tiygate-server --test capability_routing
cargo test -p tiygate-store capabilities
```

#### 阶段交接与回滚

- 交接给阶段 5：`probe_suite_version`、allow-list、Outcome/Error 分类、预算与 lease 测试报告；只有 Positive/ExplicitNegative 能进入 resolver，Inconclusive/Error 必须可追踪但不得改变能力结论。
- 回滚：将 `gateway.capabilities.probe_enabled` 设为 `false` 或暂停 worker；保留 profile、job 和错误证据，业务路由继续使用既有健康与 fallback 状态。

### 阶段 4：CRL 建模与提升

#### 目标

实现 CRL 请求的无损识别、原生透传和受能力约束的顶层工具提升。

#### 进入条件

- 阶段 1 的 protocol requirement/transform 接口已稳定。
- 阶段 0 的 Responses opaque/passthrough 回归基线可持续通过。
- 阶段 3 能为 native additional_tools、namespace、custom、function 和 continuation 提供证据。

#### 实施内容

1. Responses decoder 在现有 `responses_opaque_input_items` 中保存 additional_tools 原始 item/index。
2. 解析 carrier 内 namespace/function/custom 类型和嵌套 namespace path，生成带 typed constraint 的 Required wire requirements；namespace capability 必须覆盖请求中的完整 path 集合。
3. 注册 `responses.pass_through` 与 `responses.promote_crl_additional_tools` transform provider。
4. 实现工具 identity、canonical JSON、稳定合并、等价去重和冲突拒绝。
5. 原生计划使用原始 body，并仅应用 model、认证和已登记 normalization。
6. promotion 计划从原始 body 生成 materialized body，移除 carrier、保留其他未知字段和 item 顺序。
7. 将 raw/materialized body 选择下沉到 PlannedTarget，消除 Route 级 passthrough 对其他 Target 的影响。
8. 确保 non-stream/SSE 的 function/custom call 和 finish reason 正确回到 Codex，continuation 可闭环。
9. 为每个 transform 固定 `preserves/consumes/produces`、允许的 ingress/egress `WireProfileId`、body size 上限和失败错误码；planner 只能选择已注册且通过静态契约校验的 transform。
10. 在发送前执行 materialized body 的 JSON、大小、敏感字段和 opaque-item 保留校验；校验失败不得发出上游请求，也不得退化为删除字段后透传。

规划前先完成 carrier 结构校验：`additional_tools` 类型、`tools` 数组、namespace
path 和 tool 定义不合法时，直接返回 ingress `invalid_request`；结构合法但所有
Target 均无法选择 native 或 promotion 计划时，才返回 `no_compatible_target`。只有
`openai-responses-standard` 或明确允许顶层 `tools` 的 egress dialect 才能生成 promotion
计划；`auto` 不得凭一次普通文本 200 自动宣称 CRL 方言完整支持。

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
- transform 只在声明的 wire profile、能力依赖和版本元组均匹配时可执行；未知 transform、body 超限和保留性校验失败均产生可解释的 planner/invalid_request 结果。
- 关闭 CRL promotion 后恢复原生 passthrough 行为，不影响其他 Responses 请求。
- 通过验收组 `AC-CRL` 和适用的 `AC-QUALITY`。

#### 可启用模式

路由保持 `off`。transform 仅供测试和后续 Shadow planner 调用，不在生产请求中自动选择。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S4-1` ingress 识别 | Responses decoder 只在现有 `responses_opaque_input_items` 中保存 carrier 原文、index 和 dialect；IR 不吞掉未知 item | 标准 Responses、CRL、混合 opaque item 和 malformed carrier 的 decode/encode 快照 |
| `S4-2` requirement provider | 从 carrier 递归提取 namespace path、function、custom 和 continuation 需求；namespace path 生成 typed set constraint；与 IR requirements 合并并去重 | 同一请求的 Required/Preferred/Ignorable、constraint 和 capability shape 稳定可复现；不同 namespace path 产生不同 shape |
| `S4-3` transform | native passthrough 与 promotion 均以原始 JSON 为源；promotion 只删除已提升 carrier，保留未知字段、item 顺序、tool choice 和 parallel 标志 | 顶层工具、多 carrier、嵌套 namespace、等价重复、冲突定义、空数组和非法类型的确定性测试 |
| `S4-4` per-target body | `PlannedTarget` 持有自己的 raw/materialized body；禁止以 Route 中任一 Target 的 suite 预先决定全局 body | native、promotion、跨协议 Target 同 Route 并存时分别收到预期 body；失败 Target 不污染后续 Target |
| `S4-5` response/continuation | non-stream 与 SSE 的 function/custom 输出均归一为 ToolCalls；回传 tool output 后才结束为最终 message | Responses wiremock 集成测试覆盖首帧、终止帧、call_id、重复 output、断流和客户端取消 |
| `S4-6` transform 契约与发送前校验 | descriptor 声明 wire profile、版本、preserves/consumes/produces、大小与错误码；materialized body 发送前做完整校验 | 未注册/不适用 transform、版本失配、body 超限、opaque 丢失和敏感字段泄漏均在出站前失败；不产生上游请求 |

任何 transform 若不能证明字段语义被保留，必须返回规划失败并进入 `no_compatible_target`；不得以“删除未知字段”作为隐式降级。请求体 materialization 还必须经过现有 body-size、敏感字段和审计限长检查。

建议验证命令：

```bash
cargo test -p tiygate-protocols responses
cargo test -p tiygate-server --test capability_routing
cargo test -p tiygate-server --test wiremock_providers
```

#### 阶段交接与回滚

- 交接给阶段 5/6B：CRL requirement/transform provider、错误码和 per-target body 计划契约；`malformed carrier` 必须是 ingress `invalid_request`，合法但无可行计划才是 `no_compatible_target`。
- 回滚：关闭 `gateway.responses.crl_tool_promotion_enabled`，仅保留原生 passthrough；删除或禁用 transform 不得影响普通 Responses 请求和已有 opaque item。

### 阶段 5：Shadow 能力规划

#### 目标

在不改变生产路由的前提下验证 requirement、profile、planner、transform 和证据反馈的正确性与性能。

本阶段的跨协议接入是“统一诊断契约”，不是扩大首期转换范围：所有当前 ingress 都生成
generic plan 和 shape，但只有阶段 4 定义的 Responses/CRL transform 可以被标记为
EnforceEligible；Chat、Messages、Gemini、Embeddings 的未实现转换必须保持
`ShadowEligible` 或 `Disabled`。

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
10. 为 planner、telemetry sink 和聚合器固定事件状态：`compatible`、`incompatible`、`unknown`、`planner_error`；内部异常必须带受限 reason、版本元组和 request/route/shape 关联。
11. 实现能力计划/反馈的可恢复投递：入队返回“已持久化/待重试/gap”三态，gap 记录原因、计数和时间窗；补偿重放按唯一键 upsert，不重复放大分母或分子。
12. 按真实 `missing`/`unknown` 结果计算 Target × capability 判定对和请求级分母；禁止用 required capability 数量或进程内计数器替代实际样本。

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
- planner 内部异常均以 `planner_error` 记录并触发告警，业务请求仍沿用旧数据路径；不存在被伪装成 Unknown、Unsupported 或 `no_compatible_target` 的内部错误。
- 队列背压、数据库/序列化/脱敏失败和进程重启都会产生可查询的 `telemetry_gap`；gap 窗口不能通过准入，补偿重放后指标分母/分子保持幂等。
- profile resolution coverage、compatible shape coverage、unknown/error rate 的分母与第 9.5 节定义一致，低流量和无样本明确区分。
- 通过验收组 `AC-SHADOW`、`AC-ROUTING` 和适用的 `AC-QUALITY`。

#### 可启用模式

允许 `off` 和 `shadow`；禁止 `enforce`。未达到准入门槛的 Route 必须保持 shadow。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S5-1` planner 接线 | Chat、Messages、Responses、Gemini、Embeddings 在策略排序前调用同一 generic planner；协议差异仅由 `protocols` provider 提供；Responses opaque requirement expression 必须保留 typed constraint | 每种 ingress/egress 组合的 requirement、plan、missing/unknown 和 transform snapshot；普通文本与 CRL shape 分开统计；同 ID 不同 constraint 生成不同 shape |
| `S5-2` deterministic plan | Target 按配置顺序生成诊断，再交给既有 weighted/priority/cooldown/latency 仅做 shadow 排序；同输入/profile 输出必须稳定 | property/replay 测试证明排序、去重、constraint 和错误原因稳定；无 DB/网络调用的 planner benchmark |
| `S5-3` telemetry persistence | 记录实际目标、shadow 目标、capability shape、missing/unknown、evidence、transform、planning latency、planner/probe error 和 outcome；聚合必须可跨进程、重启和时间窗口查询；关键事件背压写入可恢复 outbox，无法落盘时写入 `telemetry_gap` | OLTP/request-attempt schema 与聚合查询测试；进程重启后指标连续，不能只依赖原子内存计数；无样本分母明确返回无样本；存在 gap 时 gate 不通过 |
| `S5-4` passive feedback | 仅可验证 tool call/continuation 写 SuccessfulTraffic；明确 capability 4xx 标 stale/reprobe；普通 200、未调用工具和缺少 reasoning 不产生负证据 | 正/负/不可归因响应 fixture；feedback 不改 HealthRegistry、quota、EWMA 或业务 fallback 状态 |
| `S5-5` shadow report | 以 Route × capability shape 输出第 9.5 节覆盖率、Unknown、disagreement、probe error、planner error 和 p95，并限制 cardinality/采样；shape 必须包含规范化 typed constraint | 固定观察窗口报告、带 namespace/range 约束的 shape hash、告警和低流量例外报告；指标分母、时间窗和脱敏字段有契约测试 |
| `S5-6` planner error 与 gap | requirement/profile/transform 异常输出 `planner_error`；队列、数据库、序列化/脱敏失败输出带时间窗的 gap；补偿按唯一键重放 | 故障注入、outbox 重放、重复/乱序事件、窗口 gate 阻断与补偿后恢复测试 |

Shadow 阶段的“兼容”只表示 planner 结论，不代表改变实际选择；任何 planner 内部错误都必须保留旧数据路径并产生可检索告警。指标不能从 request hot path 同步写数据库，使用现有异步 telemetry/OLTP 管道或等价的批量缓冲。

建议验证命令：

```bash
cargo test -p tiygate-core capability
cargo test -p tiygate-server --test capability_routing
cargo test -p tiygate-store log_sink::oltp::tests::write_capability_plan_persists_target_diagnostics
cargo test -p tiygate-server --test wiremock_providers
```

#### 阶段交接与回滚

- 交接给阶段 6A/6B：planner 输入输出版本、shape hash 版本、异步 telemetry schema、准入报告和观察窗口基线；报告必须能重放并解释每个 Target 的排除原因。
- 回滚：把受影响 Route 的 mode 降为 `off`，停止写入新的 enforce 计划但保留 shadow telemetry；planner 出错时无条件走旧数据路径。

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
8. 配置导入的 override 先按 route/target 定位器唯一匹配并重新计算 TargetKey，再执行 registry、typed value、baseline Forbidden、TTL 和 scope 校验；未知 capability ID 只保留往返，不参与 enforce。
9. admission 报告、shape hash、gate policy、snapshot epoch 和 revision 全部由服务端重算/核对；客户端提交的 hash、指标和“通过”标记不得直接授权 enforce。
10. 将 mutation 幂等记录、审计事件、admission/profile/job 状态和 capability epoch 放入同一事务或可证明的持久 outbox，避免“响应成功但状态/审计缺失”。

控制面必须固定以下状态转换和请求契约：

```text
off → shadow                         允许
shadow → enforce                    仅当 shape admission 通过且未过期
enforce → shadow|off                允许，立即生效
off → enforce                       拒绝，必须先建立 shadow 观察窗口
```

Route mode 只表达上限，shape admission 才表达实际放行范围。写入 admission 至少携带
`route_id`、`capability_shape_hash`、`required_capabilities`、`gate_policy_version`、
`report`、`expires_at` 和 `expected_revision`；低流量例外/门槛下调的 `reason`、
`exception_type` 和批准信息写入 `report_json` 与审计记录。服务端重新计算 shape hash、校验
报告与当前 snapshot/指标的一致性，并使用条件更新避免并发覆盖。所有 mutation 支持
`Idempotency-Key`（或等价的请求指纹），重复请求返回原结果而不重复创建 job 或 audit。
幂等键及请求摘要必须与 mutation/audit 原子落库；同一键提交不同 payload 返回
`409 idempotency_conflict`，幂等记录按审计保留期清理，不把原始凭证或请求体写入记录。
这里的 mutation 不仅包括 probe、override、admission 和 worker pause/resume，也包括通过
通用 settings/route API 修改 capability routing mode、probe budget/concurrency 或 CRL
promotion 开关的请求；若复用通用接口，必须在同一入口执行 capability mutation 的幂等、
条件更新和审计事务，不能以“普通设置更新”绕过门禁。
低流量例外和门槛下调必须额外携带 `exception_type`、`expires_at`、批准人和补充证据，
不得创建永久例外；到期或 gate policy 版本变化时自动撤销 enforce。

查询接口统一返回 `{items, total, limit, offset, next_cursor}`，服务端限制最大 `limit`；
`entries` 仅作为现有 WebUI 的兼容别名，不能替代 `items`；写入
接口按操作返回 `202 Accepted`（异步 job）、`200`（幂等重复）或 `409 revision_conflict`。
能力存储未配置、migration 未完成或 snapshot 不可用时返回受限的
`capability_unavailable`（管理端可见原因），不返回空的“全部 Unknown”报告来诱导
管理员批准 enforce。

#### 交付物

- 完整 Admin API、审计事件和服务端准入校验器。
- 命令/API 可执行的 probe、override、mode、回滚和诊断运维流程。
- Admin 权限、并发更新、审计、脱敏和非法状态转换测试。

#### 退出标准

- 未达到准入门槛的 Route 无法通过 API 进入 enforce。
- override 不能越过 baseline Forbidden，重探测不覆盖 override，过期 override 自动退出 resolved profile。
- manual probe、暂停/恢复和 mode 更新在多副本下最终一致。
- 所有写操作均有完整审计，所有敏感字段在 API 和 audit 中不可见。
- override 导入在唯一目标匹配、身份不一致、重复目标、未知 capability、Forbidden 或无效 typed value 时均有确定的 imported/skipped 结果，不会部分套用或静默扩大作用域。
- admission 只接受服务端重新计算且与当前 snapshot/指标/版本一致的报告；幂等重放返回首次响应，不重复写 job、admission 或 audit。
- 仅使用 Admin API 即可完成 profile 检查、重探测、shadow 准入、enforce 和回滚，不依赖数据库手工操作。
- 通过验收组 `AC-ADMIN`、`AC-PERSISTENCE` 和适用的 `AC-QUALITY`。

#### 可启用模式

允许 `off` 和 `shadow`；API 中存在 enforce 枚举，但在阶段 6B 发布前由 feature gate 禁止实际启用。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S6A-1` 查询契约 | registry/profile/job/report 列表统一分页、排序、最大 page size 和脱敏；detail 返回 schema/probe/evidence/TTL 摘要 | OpenAPI/JSON fixture、分页边界、未知 capability 往返、敏感字段扫描测试 |
| `S6A-2` 写入契约 | probe、override、pause/resume、mode mutation 使用幂等请求；override 校验 descriptor/value/baseline Forbidden | 重复请求不产生重复 job/audit；非法 state/value、过期 override 和 Forbidden override 返回稳定错误 |
| `S6A-3` 并发与授权 | 所有写接口检查 Admin 权限、scope、revision/ETag（或等价条件更新），记录 actor/reason/before/after | 并发更新只接受一个版本；越权、重放、错误 TargetKey 和跨 Route 操作均被拒绝 |
| `S6A-4` 准入门禁 | 增加 `capability_route_admissions`（SQLite/PostgreSQL）；服务端基于持久 shadow report 计算 Route × shape 门槛，并持久化 `required_requirements_json`；CRL shape 可授权完整转换，其他 shape 只能执行其已注册的通用过滤/转换；低流量例外和门槛下调必须二次确认且有期限 | 不满足门槛无法启用 enforce；带不同 namespace/range constraint 的 shape 不会复用同一 admission；未注册 transform 的 shape 保持 shadow；批准、撤销、过期、条件更新和审计回滚均可重复测试 |
| `S6A-5` 运维闭环 | 提供仅靠 Admin API 完成诊断、重探测、暂停/恢复、shadow 准入和回滚的顺序操作；不依赖 SQL | 从空 profile 到 ready/stale/error 的端到端集成 fixture；API 故障时旧模式保持不变 |
| `S6A-6` 导入与版本核验 | override 按 route/target 定位器重绑定；服务端重算 typed requirement shape/hash/report 并在同一事务写 mutation、审计和 epoch | 跨安装成功/跳过、未知 ID 往返、Forbidden/typed value 拒绝、不同约束 shape 隔离、幂等重放/冲突、版本失配和部分失败回滚测试 |

阶段 6A 只能通过 `capability_route_admissions` 授权“某 Route 的某能力形状”进入 enforce，不能以全局 mode 写入替代逐项准入。Admin 返回的逐 Target 报告可包含 TargetKey，但不得包含 API Base 敏感 query、账号、凭证材料或原始上游错误。

建议验证命令：

```bash
cargo test -p tiygate-admin --test integration target_capability
cargo test -p tiygate-server --test capability_routing
cargo test -p tiygate-store capabilities
npm --prefix webui run lint
```

#### 阶段交接与回滚

- 交接给阶段 6B/6C：已版本化的 Admin API/JSON schema、审计事件、gate policy、条件更新语义和 admission fixture；6B 只能消费服务端计算出的 admission，不能在 executor 内自行放宽门槛。
- 回滚：撤销或过期指定 Route × shape admission，Route mode 回到 `shadow`/`off`；profile、job、审计和历史报告继续保留。

### 阶段 6B：Enforce 数据路径

#### 目标

只对已通过 Shadow 门禁的 Route/能力形状启用能力过滤和 Target 独立转换。

#### 进入条件

- 阶段 6A 的服务端准入、审计和回滚 API 已通过退出标准。
- 目标 Route/能力形状满足第 9.5 节门槛并由管理员显式批准。
- `capability_route_admissions` 中存在未过期、revision 匹配且 gate policy 版本有效的 enforce 记录。
- 至少一个 compatible Target 具有 fresh 或 stale-valid profile，且端到端 continuation 已验证。
- 若兼容路径依赖 `responses.promote_crl_additional_tools`，运行时
  `gateway.responses.crl_tool_promotion_enabled` 必须已显式开启，且目标 egress dialect
  允许顶层 `tools`；仅有能力画像不等于允许提升。

#### 实施内容

1. 在路由策略排序前过滤 incompatible Target；Unknown 对首期 Required capability 视为不满足。
2. 对剩余 PlannedTarget 应用现有 weighted/priority/cooldown/latency 策略和 HealthRegistry。
3. Executor 按当前 PlannedTarget 选择 raw passthrough 或 promotion body，不复用其他 Target 的 body。
4. 无 compatible Target 时返回协议原生、受限 `no_compatible_target`；完整报告留在内部。
5. 明确 target-specific capability 4xx 在未发送客户端字节时跳到下一个已规划 Target；普通 BadRequest 保持失败。
6. 实现 per-route/per-shape 自动降回 shadow、告警和 feature flag 紧急回退。
7. 对 non-stream、SSE、fallback、断流和 client disconnect 保持现有一次发送与计费语义。
8. 将“客户端是否已提交字节”作为 executor/fallback 的显式状态传递，而不是使用固定值或错误文本推断；首字节后 capability error、断流和取消均禁止切换 Target。
9. 在 fallback、HealthRegistry、capability feedback 和 stale/reprobe 入队之间使用明确的 TargetInstanceId/TargetKey 适配；能力失败不得污染健康、熔断、延迟 EWMA 或业务 quota。
10. planner、snapshot、admission 或 transform 内部错误统一走旧数据路径并触发受限告警；仅结构化且首字节前的 capability rejection 才能消耗既有 fallback 预算。

请求执行顺序固定为：

```text
读取 RuntimeTunables 与 Route mode
  → 提取 ExchangeRequirements、shape hash
  → 读取 CapabilitySnapshot 与 admission（只读内存/已发布准入）
  → 为每个 Target 生成 CompatibilityReport 与 PlannedTransform
  → enforce 时先过滤 Required 不满足者，再进入既有策略排序
  → 发送当前 PlannedTarget 的 raw/materialized body
  → 仅在首字节前且错误满足结构化 capability-rejection 契约时尝试下一个已规划 Target
```

`shadow` 与 `off` 的实际目标列表、请求体和 fallback 必须与旧路径一致；`enforce` 的
过滤结果只能来自当前 Route × shape admission。若 snapshot、admission 或 planner 发生
内部错误，保留旧路径并将该 shape 降为 `shadow`，不得把内部错误转换为“不支持”。

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
- 首字节状态由实际 response body/stream 驱动，非流和流式路径在错误、取消、重试时均不会误切换或重复计费。
- planner/transform/snapshot 失败不会产生 `no_compatible_target` 或 Unsupported；旧路径可用且受影响 shape 会被记录为 shadow/告警。
- stale/reprobe 的后台任务在 shutdown、重启和多副本场景下可恢复，且不阻塞业务响应。
- canary 中第 9.5 节指标持续满足一个完整观察窗口，且现有 fallback、health、quota、SSE 和 telemetry 回归全部通过。
- 通过验收组 `AC-ROUTING`、`AC-CRL`、`AC-SHADOW` 和适用的 `AC-QUALITY`。

#### 可启用模式

允许 `off`、`shadow` 和通过门禁的 per-route/per-shape `enforce`；全局 enforce 必须在所有纳入范围的 Route 分别通过门禁后启用。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S6B-1` 准入过滤 | 先读取 Route mode 与 `capability_route_admissions` 的 shape gate，再在既有权重、优先级、cooldown、latency 和 HealthRegistry 排序前过滤 Required 不满足的 Target；Unknown 的处理只按注册表/门禁策略决定 | 第一目标 incompatible 时零业务出站；第二目标 compatible 时直接使用其计划；未获 shape 准入时保持 shadow/off；普通请求仍与旧排序一致 |
| `S6B-2` per-target 执行 | executor 使用 `PlannedTarget` 自己的 raw/materialized body、dialect 和模型，不共享 Route 级缓存 | native/promotion/cross-protocol 混合 Route 的 wiremock 请求体、header、stream/non-stream 快照 |
| `S6B-3` 错误与 fallback | 只依据结构化 provider code、明确 status/header 和 codec 语义将 capability rejection 分类；禁止仅凭模糊错误文本；客户端已收到字节后不得切换 | capability-specific 4xx、普通 400、401/403、429、5xx、首字节前/后断流的 fallback 矩阵测试 |
| `S6B-4` 外部错误 | 无兼容 Target 返回协议原生 `no_compatible_target`，只输出限长 required/unknown/request_id；完整报告写内部 telemetry/Admin | 各协议 error encoder 一致；响应与日志中无 TargetKey、API Base、account、credential 或拓扑泄露 |
| `S6B-5` 运行时回退 | 高置信冲突、planner error、唯一 Target 画像失效等触发器只降级受影响 Route/shape 到 shadow；紧急开关可立即回到 off | canary 期间自动降级和恢复、其他 Route 隔离、SSE 已发送首字节不重试/不换目标 |
| `S6B-6` 计费与容量 | 探针与 shadow telemetry 不计入业务 quota；enforce 过滤不扩大重试预算、并发和请求体限制 | quota、fallback budget、request log、TTFB/latency、client disconnect 与现有回归集保持一致 |
| `S6B-7` 提交状态与后台恢复 | executor 显式维护首字节/客户端提交状态；stale/reprobe 和 feedback 走可恢复句柄；TargetKey/TargetInstanceId 只在边界转换 | 首字节前后错误矩阵、流取消、重复计费、健康隔离、shutdown/restart、planner error 旧路径回归测试 |

结构化 capability rejection 至少需要：上游 HTTP status、协议错误 code/type、被拒绝字段或 capability 的明确指示、请求是否已向客户端发送字节；无法满足这些条件时保持原错误，不进行 target-specific fallback。

建议验证命令：

```bash
cargo test -p tiygate-server --test capability_routing
cargo test -p tiygate-server --test wiremock_providers
cargo test --workspace --all-features
```

#### 阶段交接与回滚

- 交接给阶段 6C/运营：可观测的 enforce 结果、fallback 分类、canary 观察窗口和自动降级事件；每次降级都必须能定位到 Route、shape 和 gate policy 版本。
- 回滚：先把单个 shape 的 admission 设为 `shadow`，若仍有风险再把 Route 或全局 mode 设为 `off`；流式响应已发送首字节后不得尝试回滚到另一 Target。

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
4. 提供 manual probe、override、worker pause/resume、shadow/enforce 和回滚操作；准入表单支持 `required_requirements` typed constraint JSON，并在提交前校验为数组。
5. Unsupported→Supported override、门槛下调、低流量例外和 enforce 使用二次确认，并要求 reason；没有现有 Shadow shape admission 时禁用 Enforce 选项。
6. CompatibilityReport、job 和 audit 列表分页，长值折叠且不渲染未转义上游内容。
7. 所有异步操作显示 job 状态、revision/ETag 和重试入口；提交成功后按服务端返回的 snapshot epoch 刷新，不用本地乐观状态绕过门禁。
8. UI 以 registry/API 返回的状态为唯一事实来源；未知字段保留展示，未知版本、`capability_unavailable` 和过期游标进入只读/可恢复状态。
9. 将高风险操作、准入报告和回滚 runbook 的作用域、过期时间、批准人和审计链接完整展示，避免把“HTTP 200”或“探针完成”显示为能力已支持。

#### 交付物

- Target 能力详情、Shadow 准入、probe/override/mode 操作和审计查看界面。
- 加载、空状态、错误、权限、并发更新和敏感内容渲染测试。

#### 退出标准

- 运维人员无需数据库或 CLI 即可完成阶段 6A 的全部日常操作。
- UI 明确区分 Unknown、Inconclusive、ProbeError、Unsupported 和 stale，不将 HTTP 200 展示为能力成功。
- enforce 前显示作用域、门槛结果、受影响 Target 和回滚方式；高风险操作均有二次确认与审计 reason。
- 分页、错误恢复、并发更新冲突和 HTML/JSON 转义通过前端测试。
- capability store 不可用时展示 `capability_unavailable` 和只读降级状态，不提供可提交的 enforce 控件。
- 异步 job、revision 冲突、幂等重放、过期游标和 snapshot epoch 变化均有可恢复交互；UI 不依赖本地缓存决定 enforce 是否可用。
- 高风险操作完成后可从 UI 追溯 actor、reason、作用域、版本和回滚入口，且未知上游文本不会被当作 HTML 执行。
- WebUI TypeScript、lint 和构建通过，后端 API 契约测试保持通过。
- 通过验收组 `AC-ADMIN` 和 `AC-QUALITY`。

#### 可启用模式

不改变数据路径模式；UI 仅调用阶段 6A/6B 已授权的服务端能力。

#### 可执行工作包与验证

| 工作包 | 实施边界 | 必须产出的验证 |
|--------|----------|----------------|
| `S6C-1` 数据模型 | `webui/src/api/types.ts` 与 API schema 对齐 profile/job/report、mode、shape、audit 和分页；未知 capability 保留为可展示数据；列表必须按 `next_cursor`/受限 page size 分页拉取，不得用无限制全量请求 | fixture 版本兼容、未知字段往返、分页/排序、游标失效和错误码映射测试 |
| `S6C-2` Target 详情 | Route Target 行与详情页展示 dialect、profile 状态、fresh/stale、证据、TTL、job lease、missing capability 和 transform | Supported/Unsupported/Constrained/Unknown/Inconclusive/ProbeError/stale 每种状态均有 UI fixture |
| `S6C-3` 受控操作 | probe、override、pause/resume、shadow/enforce、回滚、低流量例外和门槛下调全部调用 Admin API；准入表单支持 `required_requirements` typed constraint JSON；高风险操作统一二次确认和 reason | 权限失败、typed constraint JSON 校验、并发 revision 冲突、重复提交、超时重试和成功后刷新快照测试 |
| `S6C-4` 安全与可用性 | 长文本折叠、上游内容转义、敏感字段不渲染；加载/空/`capability_unavailable`/错误/无权限/旧 API 状态可恢复 | XSS/敏感字段扫描、键盘可达性、响应式布局和错误恢复测试 |
| `S6C-5` 发布验证 | UI 构建产物与 Admin 路由集成；不具备 6A 准入时隐藏或禁用 enforce | TypeScript、lint、生产构建及浏览器级关键流程通过；UI 不可绕过服务端门禁 |
| `S6C-6` 异步状态与运营证据 | 轮询 job/epoch、处理幂等/ETag/游标错误；展示 gate policy、批准、过期和回滚证据 | API/浏览器 fixture 覆盖 pending→ready/stale/error、重放、冲突、不可用和未知版本；页面不以本地状态开启 enforce |

建议验证命令：

```bash
npm --prefix webui run lint
npm --prefix webui run build
cargo test -p tiygate-admin --test integration
```

#### 阶段交接与回滚

- 交付给运营：与 Admin API schema 同版本的 UI、关键流程浏览器 fixture 和无障碍/安全扫描结果；UI 不持有数据库连接或绕过服务端准入。
- 回滚：撤回 UI 静态资源或隐藏未授权操作即可；不撤销后端 profile、admission、审计数据，也不改变当前数据路径模式。

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
- [ ] 首期 Enforce 白名单与 gate policy 版本化；白名单外 capability 即使 descriptor 标记为 Implemented，也只能用于诊断或 Shadow。
- [ ] registry/baseline、profile、probe/judge、shape hash、gate policy 和 telemetry 版本元组均有机器可读来源；主版本失配会阻止 enforce。

### 13.3 `AC-IDENTITY`：Target 身份

- [ ] 相同实际 Target 的等价 API Base 形式生成相同 TargetKey。
- [ ] 相同 Provider/model 但不同 API Base、endpoint、dialect、account 或 credential scope 生成不同 TargetKey。
- [ ] API Base 规范化覆盖 scheme/host 大小写、默认端口、尾斜杠、path、userinfo 和 query 安全规则。
- [ ] API Key 使用 keyed HMAC；OAuth token 刷新不改变稳定 grant fingerprint；主密钥轮换不改变 fingerprint。
- [ ] 相同实际 Target 被多个 Route 引用时复用 profile；weight、enabled 和 Route 顺序不改变身份。
- [ ] RouteTarget 未配置 dialect 时保持 auto，显式 dialect 参与 TargetKey 和 baseline 解析。
- [ ] 明文 API Key、OAuth token、IAM session token 和 fingerprint secret 不进入 TargetKey、profile、日志或错误体。
- [ ] TargetKey、TargetInstanceId、HealthKey 只能通过显式边界适配互转；能力画像与 HealthRegistry 不会因裸字符串复用而相互污染。

### 13.4 `AC-PERSISTENCE`：持久化、任务与快照

- [ ] SQLite 和 PostgreSQL migration、profile/override/job CRUD、TTL 和 fresh/stale 行为一致。
- [ ] Route 配置和初始 probe job 在同一事务提交，创建响应不等待完整探测。
- [ ] Provider credential、API Base、endpoint、model、dialect、account/scope 等身份变更与 profile/job 入队具有同一事务保证；若使用 outbox/reconciliation，则可证明配置 epoch 与任务最终一致且无孤儿状态。
- [ ] 相同 TargetKey/probe set 的并发 claim 在多副本下只有一个有效 lease。
- [ ] 进程在任务创建、claim、执行和提交阶段退出后，任务均能恢复且结果提交幂等。
- [ ] 应用优雅关闭停止新任务 claim、等待或安全释放当前 worker；重启后 lease 到期任务可被接管，不存在无法回收的 detached worker。
- [ ] Target 删除、禁用或身份变化后旧任务取消，旧 observation 不进入新身份 profile。
- [ ] fresh 到期后在 stale grace 内继续提供最后已验证结果，超过 stale_until 后不再参与 enforce。
- [ ] profile/override 更新通过 capability epoch 最终发布到所有副本，且不触发完整配置重载。
- [ ] 正常请求只读 CapabilitySnapshot，不执行 SQL、不等待 watcher。
- [ ] snapshot 重建失败或数据库短暂不可用时保留最后可用快照并产生告警；profile/旧 job 的延迟清理不影响读取。
- [ ] `capability_route_admissions` 的条件更新、revision、过期和 capability epoch 发布在两种数据库中一致；缺失或 stale admission 不会进入 enforce。
- [ ] admission 同时保存 `required_capabilities_json` 与 `required_requirements_json`；旧空 requirement 列可按 ID 兼容读取，带 namespace/range constraint 的 shape hash 不会退化为无约束 hash。
- [ ] `request_capability_plans`/`request_capability_feedback` 的迁移、幂等键、乱序重放和保留期在两种数据库中一致；异步 sink 重启后不会丢失已确认的 gate 样本。
- [ ] profile/job 不随配置导出，override 可选择导出并按 route/target 定位器安全重绑定；无法唯一匹配时计数跳过，fingerprint secret 永不导出。
- [ ] Profile、probe job、admission 状态转换只通过条件更新；版本失配、过期、取消、重启和清理均保留原因和诊断窗口。
- [ ] snapshot reloader、ProbeWorker、admission guard、stale/reprobe 反馈和 telemetry outbox 均有可停止/可接管句柄，不存在 detached task 或永久 running job。

### 13.5 `AC-PROBE`：探测正确性与安全

- [ ] 强制 function 探针只有收到正确 call、name 和 nonce 才产生 Positive。
- [ ] function output continuation 成功后才标记 continuation Supported。
- [ ] control 未成功或模型未按提示执行时产生 Inconclusive/Error，不误判 Unsupported。
- [ ] 明确 schema/field 4xx 只有在点名具体能力且作用域匹配时产生 ExplicitNegative。
- [ ] `401/403`、`429`、`5xx`、timeout 不生成负面 observation，也不清空已有结论。
- [ ] CRL A/B 只有 control 成功、实验变量唯一且结果稳定时产生 ExplicitNegative。
- [ ] 探针与业务 egress 使用相同 AuthApplier、URL、TLS、代理和 redirect 安全规则。
- [ ] probe_id 只能来自受审计 allow-list；地址校验、请求/响应限长、最大 token、全局/provider/account 并发和预算在执行器统一生效。
- [ ] 探测请求不修改 HealthRegistry、EWMA、下游 quota 和业务 request log。
- [ ] 默认探针不执行 hosted search、shell、computer use、MCP、Multi-agent 或其他外部副作用。
- [ ] 达到预算、worker 暂停或发生瞬时错误时任务正确延期，最后已验证 profile 不被清空。
- [ ] 未实现 continuation 的 Anthropic/Gemini 或其他协议探针返回 Inconclusive，不进入 EnforceEligible 或产生 Unsupported。
- [ ] ProbeOutcome 的 Positive、ExplicitNegative、Inconclusive 和 Error 均有 wiremock 测试。
- [ ] 同一 probe 提交键的重复/乱序结果只保留一个 observation、一次费用/审计记录；stale/reprobe 入队可在队列背压和重启后补偿。

### 13.6 `AC-CRL`：CRL 建模与转换

- [ ] 原生 CRL Target 收到 wire 等价的 additional_tools 请求及其他未知字段。
- [ ] 标准 Responses Target 在依赖满足时收到提升后的顶层 tools，carrier 已移除。
- [ ] 已有顶层工具、多个 carrier、嵌套 namespace 和其他 opaque item 按稳定顺序合并。
- [ ] 普通工具和 namespace path 使用稳定 identity；canonical JSON 等价定义去重，冲突定义返回 400。
- [ ] namespace/custom/function 等实际载体必需能力任一缺失时不执行提升。
- [ ] additional_tools 复用现有 responses_opaque_input_items，不建立平行 carrier。
- [ ] raw passthrough 与 promotion body 按 PlannedTarget 决定，不受同 Route 其他 Target 影响。
- [ ] transform 失败、body-size 超限或未知字段无法证明保留时 fail-closed，不删除字段后继续发送。
- [ ] non-stream/SSE 工具响应均识别为 FinishReason::ToolCalls，而不是 Stop。
- [ ] tool result continuation 能返回最终 assistant message。
- [ ] 关闭 promotion 后恢复原生 passthrough，不影响普通 Responses 请求。

### 13.7 `AC-SHADOW`：Shadow 验证与准入

- [ ] capability shape 只由 Required capability/constraint 生成，普通文本不进入 CRL 指标分母。
- [ ] shadow 同时记录实际选择与规划选择，但不改变 Target 顺序、请求体、fallback 或客户端响应。
- [ ] profile coverage、compatible shape coverage、success disagreement、Unknown、probe error、planner error 和 planning latency 均按第 9.5 节定义计算。
- [ ] Shadow 指标从异步 telemetry/OLTP 或等价持久聚合读取，跨进程、重启和观察窗口连续；不得只依赖进程内原子计数。
- [ ] Shadow 聚合显式区分无样本、低流量样本和达到门槛的样本；`planner_error`、probe 终结错误、evidence 摘要和时间窗口都能追溯到原始脱敏事件。
- [ ] 能力计划/反馈事件发生背压或落盘缺口时产生 `telemetry_gap`，相关 Route/shape 不得通过 enforce 门禁，补偿完成后才能重新计算窗口。
- [ ] 每条申请 enforce 的 Route/shape 满足连续 24 小时、至少 100 个相关请求和全部默认门槛，或具有合规低流量例外。
- [ ] 高置信 verified success disagreement 为零，Unknown rate 为零，每种能力形状至少有一个 compatible Target。
- [ ] planner 对相同 request/profile 输入输出稳定，每个排除结果包含 capability 和 evidence 原因。
- [ ] SuccessfulTraffic 只从可验证工具语义产生，普通 200 message 不作为工具能力证明。
- [ ] 发布基准中 planning latency p95 不超过 1 ms，异步 telemetry 不阻塞请求。
- [ ] 系统不自动从 shadow 晋级 enforce；门槛下调和低流量例外均有作用域、reason 和审计。
- [ ] requirement/profile/transform 内部错误均记录为 `planner_error` 并保留旧数据路径，不被计为 Unsupported、Unknown 已解决或 `no_compatible_target`。
- [ ] telemetry 背压、数据库/序列化/脱敏失败和进程重启会产生可查询 `telemetry_gap`；存在 gap 的窗口无法通过准入，补偿后按唯一键恢复。
- [ ] coverage 与 error 指标使用实际 Target × capability 和请求级分母，明确区分无样本、低流量和完整窗口。

### 13.8 `AC-ROUTING`：Enforce 路由与转换

- [ ] incompatible Target 在 weighted/priority/cooldown/latency 排序前被过滤。
- [ ] 第一目标 incompatible、第二目标 compatible 时，不向第一目标发送业务请求。
- [ ] 不同 Target 获得并执行各自的 transform 和请求体计划。
- [ ] Unknown 对首期 Required capability 在 enforce 中视为不满足。
- [ ] 无 compatible Target 时返回受限 no_compatible_target，完整逐 Target 报告只进入内部 telemetry/Admin。
- [ ] target-specific capability 4xx 仅在明确分类且未发送客户端字节时 fallback；普通 BadRequest 不被误分类。
- [ ] capability rejection 分类至少同时具备 status、结构化 code/type、被拒绝字段/能力和“客户端是否已收到字节”证据；模糊错误文本不触发 fallback。
- [ ] 流式响应发送首字节后不切换 Target，现有断流、计费和客户端取消语义保持不变。
- [ ] 协议 Forbidden 不能被探测或 override 越权扩展。
- [ ] 准入指标恶化时只把受影响 Route/shape 自动降为 shadow，不影响其他 Route。
- [ ] 客户端提交状态来自实际非流响应/流式首帧；首字节前后 capability error、取消和重试分别符合 fallback 预算，不重复计费。
- [ ] stale/reprobe、feedback 和自动降级任务在 shutdown/restart 后可恢复，planner/snapshot 失败不会改变其他 Route。
- [ ] 现有 fallback、health、quota、telemetry 和跨协议 lossy conversion 测试全部通过。

### 13.9 `AC-ADMIN`：管理接口、WebUI 与可观测性

- [ ] Admin API 支持分页查询 registry/profile/job/report，支持 manual probe、override、worker 和 mode 控制。
- [ ] 不满足准入门槛的 Route 无法进入 enforce；低流量例外和门槛下调要求二次确认与审计。
- [ ] 首期 gate policy 对 Responses CRL `additional_tools` shape 授权完整 native/promotion 转换；其他 shape 即使可做通用能力过滤，也不能使用 CRL transform 或隐式丢弃字段。
- [ ] Route 级 enforce 只能在至少一个有效 shape admission 存在时写入；shape admission 是实际执行准入的最小作用域。
- [ ] Route mode 与 `capability_route_admissions` 的上限关系、shape hash、policy version、过期和撤销语义经过 SQLite/PostgreSQL/API 一致性测试。
- [ ] 重探测不覆盖人工 override，Forbidden 不能被 Supported override 覆盖。
- [ ] 写操作具备幂等键或等价条件更新；并发 revision/ETag 冲突不会覆盖他人变更，pause/resume 和 mode 更新跨副本最终一致。
- [ ] capability 专用接口以及通过通用 settings/route 接口修改 capability 字段的请求都支持 `Idempotency-Key`；同键同载荷重放原响应，同键异载荷返回 `409 idempotency_conflict`，且 mutation、审计和状态提交不可部分成功。
- [ ] 所有 mode、override、probe 和例外写操作记录 actor、reason、scope 与 before/after。
- [ ] override 导入按 route/target 定位器唯一重绑定后，重新执行 registry、typed value、baseline、TTL 和 scope 校验；未知 ID 只往返，冲突/跳过均计数且不部分写入。
- [ ] admission 的 hash、report、gate policy、snapshot epoch 和 revision 均由服务端核验；客户端不能直接提交“已通过”标记。
- [ ] Admin 可提交 `required_requirements` 的 typed value；服务端校验 value kind/matcher、Required strength、ID 集合一致性并重新计算 shape hash。
- [ ] Admin API 和 audit 可以使用 TargetKey 关联记录，但不暴露 credential、fingerprint secret、API Base 敏感 query 或其他可恢复身份材料。
- [ ] 普通客户端错误不暴露 TargetKey、account、API Base 或完整路由拓扑。
- [ ] UI 区分 Supported、Unsupported、Constrained、Unknown、Inconclusive、ProbeError 和 stale。
- [ ] UI 展示 mode 继承、Shadow 指标、准入未满足项、证据、TTL、job 和回滚方式。
- [ ] 高风险操作具有二次确认、reason、并发更新冲突处理和明确作用域。
- [ ] request attempt 能解释 Target 被排除的 capability、evidence 和 transform 计划。
- [ ] Target 详情不返回 credential scope fingerprint、canonical API Base、URL override 或任何可恢复凭证；只显示 TargetKey 和脱敏状态摘要。
- [ ] 运维人员无需直接操作数据库即可完成日常诊断、重探测、override、准入、enforce 和回滚。

### 13.10 `AC-QUALITY`：工程质量门禁

- [ ] `cargo check --workspace --all-targets`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] `scripts/verify-deps.sh`
- [ ] `npm --prefix webui run lint`
- [ ] `npm --prefix webui run build`
- [ ] WebUI TypeScript 与生产构建通过。
- [ ] `git diff --check`
- [ ] SQLite/PostgreSQL 均有 migration、job lease、幂等提交和 capability epoch 测试。
- [ ] CI 使用固定 PostgreSQL service container 执行迁移、事务回滚、lease/幂等和遥测聚合用例；未连接 PostgreSQL 的本地运行只作为补充，不得替代 CI 门禁。
- [ ] `V-TELEMETRY-GAP` 与 `V-LIFECYCLE` 的故障注入、outbox 重放、shutdown/restart 和 lease 接管在 CI 或受控脚本中可重复执行。
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

1. 所有能力感知路由由 `gateway.capabilities.routing_mode=off|shadow|enforce` 控制，紧急回退设置为 `off`。
2. CRL 提升由独立设置 `gateway.responses.crl_tool_promotion_enabled` 控制。
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
