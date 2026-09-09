# ADR-0001: 声明式 Provider Capability Profile

- 状态：已接受（为实施而细化）
- 日期：2025-09-09
- 决策人：网关维护者
- 相关：`docs/protocol-capability-matrix.md`、`crates/server/src/ingress/headers.rs`、`crates/core/src/protocol/`

## 背景

TiyGate 在把请求转发到上游 provider 前，需要对请求体做「能力裁剪」：某些字段 provider
静默忽略（strip）、某些字段不支持（reject）、某种形态需要转换（convert）。当前实现把
DeepSeek 的这一整套逻辑硬编码在 `crates/server/src/ingress/headers.rs`，用 `if/else` 逐字段
罗列支持度（`prepare_deepseek_responses_request`、`prepare_deepseek_reasoning_item`、
`strip_ignored_deepseek_control`、`validate_deepseek_message_content`、
`validate_deepseek_tool_output`、`is_official_deepseek_responses_target`）。

这带来两个问题：

1. **打地鼠**：每冒出一个新的能力缺口（字段/工具/条件），就要再添一个 `if` 分支。
2. **不可观测**：strip/reject/convert 以散文错误串（`DeepSeek Responses capability rejected: ...`）
   形式出现，无法在 Request Logs 或错误体中结构化呈现，运营难以解释「为什么我的请求被改了」。

目标：以**数据驱动**的能力 profile 取代硬编码，使「新增一个能力缺口」= 在 provider 的声明表里
新增一条字段规则，而不是加一个 if 分支；并让每次能力决策变成可观测的结构化记录。

## 决策

1. **声明模型边界：混合（字段声明 + 结构钩子）。** 顶层/嵌套字段用声明表表达
   `support | strip | reject | convert`；工具白名单、input 内容类型校验、条件性拒绝
   （如 `store=true`、`parallel_tool_calls=false`）这类难以用纯字段规则表达的，留给每个
   profile 一个可选的 `structure` 验证钩子。
2. **未声明 profile 的 provider：无默认，纯透传。** 当 `(目标协议的端点, provider)` 没有
   profile 时，**不运行能力过滤**，body 原样转发；上游自行决定如何处理。重点是：这条仅指
   **能力过滤**，不含通用 egress 变换（见「关键推论」）。
3. **内建 provider 的 profile 也可被覆盖。** 内建（DeepSeek/OpenAI/...）profile 以代码为
   baseline，允许通过 DB 覆盖层（`capabilities_json`）局部覆盖，以满足自有网关的长尾能力。
   覆盖需校验，避免把不支持的字段改标为「支持」。
4. **能力拒绝的 400 错误体：改为纯结构化 `capability` 对象。** 移除对
   `DeepSeek Responses capability rejected:` 字符串前缀的依赖。错误体改为机器可读结构，
   人类可读文案保留在 `detail` 字段内。

## 关键推论

- **拆分「能力过滤」与「通用 egress 变换」。** 现 `prepare_provider_request_body` 混了两类：
  - 能力过滤（DeepSeek 的 strip/reject/convert）→ 由 profile 驱动，仅在声明 profile 时运行。
  - 通用 egress 变换（`maybe_inject_prompt_cache_key`、`normalize_openai_reasoning_for_target`
    的 `max`→`xhigh` 降级）→ 是正确性修正而非能力缺口，**保留常开**，不受「纯透传」影响。
    否则会退化 prompt-cache 命中和 reasoning-effort 归一。
- **错误体变更是 breaking change。** 依赖旧字符串前缀的客户端需同步适配；`detail` 承载人类
  可读文案，但不再有稳定的前缀。
- **`mutated` 透传优化必须保留。** 现在 4 个调用点（`executors.rs` 539/1134/1975/2810）用
  sanitizer 返回的 `mutated: bool` 决定 body 是透传原 bytes 还是重序列化，避免无谓序列化。
  新 sanitizer 必须同时返回 `mutated` 与 `Vec<CapabilityDecision>`。

## 设计

### 核心类型（`crates/core/src/protocol/capability.rs`，零 I/O）

```rust
pub enum FieldAction { Support, Strip, Reject, Convert }
pub enum ConvertKind { SummariesToReasoningText, DropItem }
pub enum MatchCond { Present, Meaningful, EqBool(bool) }

pub struct FieldRule {
    pub path: &'static str,          // "reasoning.encrypted_content"、"input[].reasoning.encrypted_content"
    pub action: FieldAction,
    pub convert: Option<ConvertKind>, // 仅 action==Convert
    pub cond: MatchCond,             // 默认 Meaningful（镜像现 is_meaningful）
    pub reason: &'static str,        // 人类可读原因，进日志/错误体的 detail
    pub extra: Option<&'static str>, // 复用 detail 后缀，如 "only apply_patch"
}

/// 结构校验钩子：覆盖纯字段规则表达不了的部分。
/// 返回 `Result<(), CapabilityReject>`；可见当前是否有能力决定记录，需回传 mutations。
pub trait StructureValidator: Send + Sync {
    fn validate(&self, body: &mut serde_json::Value) -> Result<bool, CapabilityReject>;
}

pub struct CapabilityProfile {
    pub endpoint: ProtocolEndpoint, // profile 的关键维度：suite + name
    pub fields: Vec<FieldRule>,     // 有序，首个匹配生效
    pub structure: Option<Box<dyn StructureValidator>>,
    pub allow_overrides: bool,      // 内建 baseline 是否允许 DB 覆盖
}
```

### 分层与数据流

| 层 | 职责 |
| --- | --- |
| `crates/core` | 类型 + 通用 sanitizer 骨架（纯函数：`sanitize_with_profile(body, profile) -> SanitizeOutcome`） |
| `crates/server` (ingress) | 内建 provider profile（如 DeepSeek Responses）——放在 ingress 层而非 `crates/providers`，因为 `tiygate-providers` 是可选的 `providers` feature，而 egress sanitizer 运行在常开的数据通路上 |
| `crates/store` | 用户自定义/覆盖层的 profile 存储（`Provider.capabilities_json`），`snapshot_to_routing_table` 时注入解析后的 target |
| `crates/server`(executors) | 4 个调用点改走通用 sanitizer；`decisions` 挂进 `ExchangeCapture`；reject 映射为 `LossyOrCapability` 400 |

`SanitizeOutcome`：`{ mutated: bool, decisions: Vec<CapabilityDecision>, reject: Option<CapabilityReject> }`。
`CapabilityDecision = { field, action, reason, profile_endpoint }`；`CapabilityReject = { field, action, reason }`。

### 迁移路径（增量、无色开关）

1. `core` 类型 + registry + 通用 sanitizer 骨架（无行为变更）。
2. 把 DeepSeek 规则灌成声明表 + structure 钩子；删除 `headers.rs` 上述 6 个函数；4 个调用点切到
   新 sanitizer。**用黄金/差分测试保证 strips/rejects/converts 行为与现状完全一致。**
3. 其他有缺口的 provider 按需补声明（不预设）。
4. 可观测性接线：`decisions` → 日志 + 400 错误体（结构化 `capability`）。
5. Store/UI：自定义 provider 声明入口 + 内建能力只读展示 + 覆盖层编辑。

### 测试策略

- `FieldRule` 单测（strip/reject/convert、MatchCond、路径/首个匹配、`input[].` 数组路径）。
- **黄金/差分测试**：先快照当前 DeepSeek 行为（输入 → 输出/错误），重构后逐条比对一致。
- wiremock 集成测试保持绿色（等价比对）。
- `proptest` 测路径匹配与 MatchCond。
- structure 钩子单测（工具白名单、input 内容类型、条件性拒绝）。

### 可观测性

- `CapabilityDecision` → Request Logs 新增「Capabilities」facet/详情，显示「stripped
  reasoning.encrypted_content（DeepSeek 无法解密）」。
- 400 错误体：`{ "type": "invalid_request_error", "capability": { "field", "action", "reason",
  "detail": "<人类可读文案>" } }`。

## 风险与权衡

- **convert 转换（`summary`→`reasoning_text`）与 input 内容校验**是最难声明化的，混合方案已作为
  兜底（structure 钩子）。
- **错误体 breaking change**：需同步客户端；ADR 决策 4 已明确接受。
- **内建可覆盖**：需校验覆盖层，防止把不支持的字段改标为「支持」。
- **无默认透传**：未声明的自定义 provider 不再有保护性过滤，由上游自行决定。

## 遗留问题

- ~~覆盖层（`capabilities_json`）的 schema 校验与冲突解决（内建 baseline vs 覆盖）的具体规则。~~
  **已解决（2026-02 实现）**：覆盖存储为每 provider 单列 JSON（`providers.capabilities_json`，
  自带 `endpoint` 声明适用协议端点）；语义为**整表替换** —— 端点匹配时覆盖的 field rules 整体
  取代内建规则，内建 structure 钩子（如 DeepSeek 工具白名单）保留（覆盖只携带规则、永不携带
  钩子）；Admin API 保存时校验（非法 JSON 拒绝 400），路由表构建时容忍并忽略非法行。
- structure 钩子是否需要针对非 DeepSeek provider 的通用形态（如 Anthropic 的工具校验收敛到同一套）。
- 是否在 `EndpointCapabilities` 与 `CapabilityProfile` 之间做派生/归一，避免两处能力信息重复。
