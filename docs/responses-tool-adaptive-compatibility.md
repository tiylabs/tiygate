# Responses 工具方言的被动自适应兼容方案

> 状态：方案设计，尚未实现
>
> 适用范围：Codex/OpenAI Responses ingress → Responses egress，尤其是
> `namespace`、`tool_search`、`defer_loading` 与带 `namespace` 的
> `function_call`。

## 1. 结论

`type: "namespace"`、`tool_search` 和 `defer_loading` 是合法的 Responses
工具能力。OpenAI 官方文档展示了 namespace 工具、服务端 tool search，以及
带 `namespace` 的 `function_call` 回应；因此客户端发送嵌套工具定义本身不是
请求错误。

问题在于，名称为 Responses-compatible 的中转服务未必实现同一版工具方言。
TiyGate 不应为每个 target 发送独立 capability probe，也不应按 Provider 名称
全局推断能力。推荐采用：

```text
首次真实请求：优先发送原生 Responses
    ├─ 成功：按该 target 继续使用 native
    └─ 明确的工具 schema 拒绝：同一 target 只做一次 flat 降级重试
          ├─ 成功：短期记忆该 target 使用 flat_tools
          └─ 失败：标记 target capability mismatch，并允许路由尝试下一个 target
```

这是“被动学习”，不是主动探测。只有用户真实请求包含相关工具能力时，才会
产生额外的一次请求；不发送人工 probe，不增加上游 token 消耗。

官方参考：

- [OpenAI Tool Search：Hosted tool search](https://developers.openai.com/api/docs/guides/tools-tool-search#hosted-tool-search)
- [OpenAI API：Using tools](https://developers.openai.com/api/docs/guides/tools)

## 2. 背景与当前实现差距

### 2.1 合法的 Responses 工具形态

典型请求如下：

```json
{
  "tools": [
    {
      "type": "namespace",
      "name": "collaboration",
      "tools": [
        {
          "type": "function",
          "name": "spawn_agent",
          "parameters": {"type": "object"}
        }
      ]
    },
    {"type": "tool_search"}
  ]
}
```

对应的 Responses output item 可以是：

```json
{
  "type": "function_call",
  "name": "spawn_agent",
  "namespace": "collaboration",
  "call_id": "call_123",
  "arguments": "{}"
}
```

### 2.2 TiyGate 当前行为

- `ResponsesCodec` 将 Responses endpoint 的 `hosted_tools` 设为 `true`，但没有
  独立建模 namespace/tool-search 能力：
  [responses.rs](/Users/jorben/.codex/worktrees/945f/tiygate/crates/protocols/src/responses.rs:360)
- 未识别的 tool type（包括 `namespace`）会进入 hosted-tool 通用分支，嵌套内容
  保存在 `Tool.config`，不会按 target 方言转换：
  [responses.rs](/Users/jorben/.codex/worktrees/945f/tiygate/crates/protocols/src/responses.rs:811)
- Responses→Responses 同协议默认 raw passthrough：
  [responses.rs](/Users/jorben/.codex/worktrees/945f/tiygate/crates/protocols/src/responses.rs:2119)
- `RoutingTarget` 没有 target-level 的工具能力信息：
  [routing/mod.rs](/Users/jorben/.codex/worktrees/945f/tiygate/crates/core/src/routing/mod.rs:17)
- IR `Content::ToolCall` 没有 namespace 字段，强制进入 IR re-encode 时可能丢失
  function-call identity：
  [ir/mod.rs](/Users/jorben/.codex/worktrees/945f/tiygate/crates/core/src/ir/mod.rs:210)
- 当前 fallback 将普通 400/422 视为不可重试错误：
  [routing/mod.rs](/Users/jorben/.codex/worktrees/945f/tiygate/crates/core/src/routing/mod.rs:781)

因此，当前同协议路径可以保证“原样转发”，但不能保证“目标 Provider 接受该
Responses 方言”。

## 3. 目标与非目标

### 3.1 目标

1. 原生支持 namespace/tool_search 的 target 保持字节级或语义级无损路径。
2. 对只支持顶层 function/custom tools 的 Responses-compatible target，自动完成
   一次受控降级，而不要求管理员预先探测。
3. 降级后恢复 Responses 客户端可识别的 `namespace + name`，同时支持非流式和 SSE。
4. 将已从真实请求学习到的结果按具体 target 短期缓存，避免后续请求重复失败。
5. 不把普通请求错误、认证错误、限流错误或生成中断误判为工具能力不兼容。

### 3.2 非目标

- 不实现主动 capability probe 或虚构的 `/capabilities` 上游协议。
- 不把同一 Provider 下的所有模型、账号、API base 视为同一能力。
- 不静默删除 client-executed tool search 或动态产生的工具集合。
- 不在已经向下游输出 SSE 内容后重放请求。
- 不尝试用 flat function 名称重写任意自然语言 developer prompt；这类语义差异
  必须作为降级风险记录。

## 4. 核心设计

### 4.1 Target identity

被动学习的 key 必须绑定真实 egress target：

```text
provider_id
account_label
effective_api_base
egress endpoint
exact model_id
transport（HTTP/SSE 或 WebSocket）
```

Provider vendor 只能作为默认 hint，不能作为最终 capability 结论。模型目录的
能力描述也不能证明某个代理 endpoint 接受某个 wire field。

### 4.2 被动学习状态

第一阶段只需要进程内状态，不新增数据库表和管理页面：

```rust
enum ResponsesToolMode {
    Unknown,
    Native,
    FlatTools,
}

struct ResponsesToolCompatibilityKey {
    provider_id: String,
    account_label: Option<String>,
    api_base: String,
    endpoint: String,
    model_id: String,
    transport: UpstreamTransport,
}
```

建议的默认行为：

- `Unknown`：原生发送。
- 仅当请求实际包含 namespace/tool search 能力且原生请求成功时，记录 `Native`。
- 原生请求因明确工具 schema 拒绝、flat 重试成功时，记录 `FlatTools`。
- 记忆 TTL 默认 1 小时，可通过 runtime tunable 调整。
- target 的 API base、endpoint、model 或 config epoch 发生变化时，key 自然变化
  或缓存失效。
- 不因没有工具的普通请求记录 `Native`，避免错误推断。

可选优化是对同一 key 使用 single-flight，防止冷启动并发请求同时触发多次原生
失败；不应把该优化变成正确性前提。

### 4.3 Tool adapter

新增 Responses 专用 adapter，建议放在 server egress 层，而不是把 Provider 逻辑
塞进 `crates/core`：

```rust
struct ResponsesToolAdaptation {
    body: serde_json::Value,
    mode: ResponsesToolMode,
    changed: bool,
    lossy: bool,
    reverse_map: NamespaceReverseMap,
}
```

adapter 必须是幂等的：同一个 body 再次执行同一 mode 不应继续改变名称或重复删除
字段。

## 5. FlatTools 转换规则

### 5.1 工具声明

将 namespace 子工具提升为顶层 function/custom tool：

```text
collaboration + spawn_agent
→ collaboration__spawn_agent
```

处理要求：

- 扫描顶层 `tools`。
- 扫描 `input` 中的 `additional_tools.tools`，如果该形态存在。
- 保留 description、parameters、strict 和目标协议仍理解的 schema 字段。
- 删除 `defer_loading`，因为 flat target 不提供 deferred tool loading。
- 工具名超过 64 bytes 时使用 UTF-8 安全截断和稳定 hash 后缀。
- 顶层工具与 namespace 展平名冲突、或两个 namespace 展平后冲突时，立即返回
  本地明确错误，不发送含歧义的请求。

### 5.2 tool_search

对于 hosted tool search：

- 删除顶层 `type: "tool_search"`。
- 将 namespace 子工具全部 eager 展开。
- 删除历史中的 `tool_search_call` 和 `tool_search_output` orchestration items，
  因为 flat target 无法理解这些 item。

对于 client-executed tool search、动态 `additional_tools` 或无法确定完整工具集合
的请求：

- 不做静默删除。
- 返回 `TargetCapabilityMismatch`，让路由选择其他兼容 target，或向客户端返回
  明确的 capability 错误。

### 5.3 历史 input 与 tool choice

只改写明确的工具调用 item：

- `function_call`
- `custom_tool_call`
- `tool_call`
- `mcp_tool_call`

转换：

```json
{"type":"function_call","name":"spawn_agent","namespace":"collaboration"}
```

变成：

```json
{"type":"function_call","name":"collaboration__spawn_agent"}
```

不得递归删除普通 message/content 中的任意 `namespace` 字段。

`tool_choice` 处理：

- 具体 namespace child：改成扁平名称。
- namespace group choice：标准 flat Responses 无等价表达，降级为 `auto`，并记
  录 lossy reason。
- 无法安全解析的 tool choice：不猜测，返回本地 capability mismatch。

### 5.4 响应恢复

FlatTools 请求必须为当前 attempt 保存反向映射：

```text
collaboration__spawn_agent
→ { namespace: "collaboration", name: "spawn_agent" }
```

恢复范围：

- 非流式 `response.output[*]`。
- SSE `response.output_item.added`。
- SSE `response.output_item.done`。
- SSE `response.completed.response.output[*]`。

mapping 必须是 request/attempt scoped，不能放在全局 Provider 状态中；路由切换到
另一个 target 时必须清空并重新建立 mapping。

## 6. 错误识别与重试边界

### 6.1 可触发自适应的错误

只有以下条件全部满足才允许 native→flat retry：

1. HTTP 状态为 400 或 422，且尚未向客户端输出响应内容。
2. 原始请求实际包含 namespace、tool_search 或 defer_loading。
3. 上游 code/param/message 明确指向工具 schema，例如：
   - `unknown_parameter`
   - `unsupported_parameter`
   - 明确的 `invalid_value`
   - `tools[N].type`、`tools[N].tools`、`tools[N].defer_loading`
   - `input[N].namespace`
   - message 明确出现 `namespace` 或 `tool_search`
4. adapter 生成的新 body 与旧 body 不同，且没有执行过相同变换。

不能仅凭 `invalid_request_error`、普通 400 或一段模糊错误文本触发转换。

### 6.2 重试预算

- 同一个 target 的 namespace/tool-search 自适应最多一次。
- 同一用户请求跨多个 target 时，共享一个小的全局兼容预算，建议最多 2 次变换尝试。
- 记录 body hash，禁止相同 body 循环重试。
- 400 schema rejection 不应触发 credential rotation；只有被归类为
  `TargetCapabilityMismatch` 后才允许路由尝试下一 target。
- timeout、429、401/403、5xx 不进入 flat adapter；沿用现有冷却、认证和重试策略。

### 6.3 新错误分类

建议在 core/server 错误边界增加独立的：

```text
TargetCapabilityMismatch
```

语义：请求对 Responses 标准合法，但当前具体 target 不支持所需工具方言。

与 `BadRequest` 的区别：

| 错误 | 是否同 target 改写 | 是否尝试下一 target |
|------|-------------------|--------------------|
| 普通客户端 400 | 否 | 否 |
| namespace/tool_search schema mismatch | 是，最多一次 | flat 仍失败时是 |
| 认证/限流/传输错误 | 否 | 按现有策略 |

## 7. 请求执行流程

### 7.1 非流式

```text
route target
  ↓
读取 target learned mode
  ↓
native 或 flat 生成 egress body
  ↓
发送 Responses 请求
  ↓
HTTP 400/422？
  ├─ 否：按 native/flat 处理响应
  └─ 是：读取结构化错误
          ├─ 非工具 schema 错误：原样结束
          └─ 明确工具 schema 错误：生成 flat body，重试一次
                         ↓
                  成功则恢复 namespace 并更新 learned mode
```

### 7.2 SSE

第一阶段只对“HTTP status 400/422、尚未建立有效 SSE 输出”的情况做自适应。后续
如需处理 HTTP 200 后发送 `response.failed` 的 Provider，应增加首帧缓冲：

- 在第一个完整 SSE 事件确认前暂不向客户端输出。
- 若首个事件是明确的 request/schema failure 且没有 output item，允许一次重试。
- 一旦输出 `response.output_item.*`、文本 delta 或任何有效模型内容，禁止重试。

## 8. 与现有 TiyGate 代码的接入点

实施时保持 `core` 零 I/O 和 Provider 无关：

1. 在 server egress 层新增 `responses_tool_adapter`，负责 JSON 变换和反向映射。
2. 在 `prepare_codex_egress_body` 附近调用 target-specific Responses adapter；不要把
   flatten 逻辑写进 `ResponsesCodec` 的全局 `pass_through_policy`。
3. `compute_pass_through` 仍可先提供原始 body；一旦 adapter 产生变更，当前 target
   的 `pass_through_verbatim` 必须为 false。
4. 在非流式响应处理和 SSE 转发链路分别接入 response restore hook。
5. 在 `RoutingTarget` 的运行时上下文旁挂载 learned mode；第一阶段不必改变持久化
   `RouteTarget` schema。
6. 将工具 schema mismatch 映射为 `TargetCapabilityMismatch`，让 fallback 能区分
   “target 不支持”与“客户端请求错误”。
7. 将 attempt mode、触发参数、转换 reason、retry count 和最终 target 写入结构化
   telemetry；请求体仍按现有 redaction 策略处理。

后续若需要强制运维配置，再给 `RouteTarget` 增加显式 override：

```text
responses_tool_mode = auto | native | flat_tools | reject
```

`auto` 是默认值；显式 `native`/`flat_tools` 只用于已验证的特殊 upstream。

## 9. 实施步骤

### Phase 1：边界类型与纯函数 adapter

- 增加 `TargetCapabilityMismatch` 错误语义及序列化/日志映射。
- 实现 namespace descriptor、flatten name、collision check、reverse map。
- 实现 `native` 和 `flat_tools` 两种 request transform。
- 实现 JSON response restore。
- 为 adapter 增加纯单元测试，不接入网络。

交付物：可对任意 JSON request 做确定性转换，重复执行结果稳定。

### Phase 2：Responses egress 接入

- 在 Responses target egress 生成 body 后调用 adapter。
- 保留原始 body 和变换后的 body capture，但不记录敏感工具参数的明文日志。
- 接入非流式 restore。
- 接入 SSE added/done/completed restore。

交付物：native target 完全不变；flat target 的请求和回程均可闭环。

### Phase 3：被动学习与有界 retry

- 增加 per-target in-memory learned-mode cache。
- 识别明确的 400/422 工具 schema rejection。
- 实现 native→flat 一次重试和 body-hash loop guard。
- 成功后按 TTL 记录 `FlatTools`；无相关工具请求不更新能力。
- 处理并发 single-flight 或等价的冷启动抑制。

交付物：无需 probe，第一次真实不兼容请求可自适应，后续请求不重复失败。

### Phase 4：路由 fallback 与可观测性

- 让 `TargetCapabilityMismatch` 可安全切换下一 target。
- 保持普通 400 不切换 target。
- 增加 metrics：
  - `responses_tool_adaptation_attempts_total`
  - `responses_tool_adaptation_success_total`
  - `responses_tool_adaptation_failures_total`
  - `responses_tool_adaptation_cache_hits_total`
  - `responses_tool_capability_mismatch_total`
- 在 request log detail 中显示 target mode、trigger param 和 retry reason。

交付物：可以区分客户端错误、Provider 方言不兼容和真实上游故障。

### Phase 5：文档与可选运维 override

- 更新 [protocol-capability-matrix.md](/Users/jorben/.codex/worktrees/945f/tiygate/docs/protocol-capability-matrix.md)，将 namespace/tool_search 从笼统 hosted tools 中单独列出。
- 记录 flat conversion 的语义损失和 client-executed tool search 的拒绝边界。
- 如运营确有需要，再增加 per-target override 和 Admin UI；默认仍使用 auto。

## 10. 测试计划

### 10.1 Adapter 单元测试

- 无 namespace 的 body 保持字节/语义不变。
- 单 namespace 展平、多个 namespace 展平。
- 顶层工具冲突和 namespace 间冲突。
- 64-byte ASCII 名称上限。
- 多字节 UTF-8 名称截断不破坏 UTF-8。
- `tool_choice` 具体函数和 namespace group。
- function_call、custom_tool_call、tool_call 历史改写。
- `additional_tools` 展开。
- `tool_search`、`tool_search_call/output` eager 降级。
- client-executed/dynamic search 明确拒绝。
- response JSON 还原。
- SSE added/done/completed 逐事件还原。
- adapter 幂等性和 body hash 防循环。

### 10.2 Server wiremock 测试

- native target：上游只收到一次原始 namespace body。
- 第一次返回明确 `unknown_parameter`，第二次收到 flat body 并成功。
- flat 响应被恢复为 namespace function_call。
- SSE 失败发生在首个输出前时可以重试。
- 已发送文本/tool output 后的失败不重试。
- 普通 400、429、401、500 不触发 flat adapter。
- target A mismatch 后切换 target B；普通 400 不切换。
- learned mode 命中后第一次请求直接使用 flat body。
- TTL 到期后重新 native-first。
- 同一请求不同 target 的 reverse map 不串用。

### 10.3 回归命令

实现后至少执行：

```bash
cargo test -p tiygate-protocols --test cross_protocol --test lossy_conversion
cargo test -p tiygate-server --test wiremock_providers
make check
make test
```

如果修改 WebUI 或 Admin override，再执行 `make lint` 并验证 route/provider 配置
向后兼容。

## 11. 验收标准

### 功能验收

- [x] 官方 Responses namespace 请求在支持该能力的 target 上不发生转换、不产生额外
      请求。
- [x] 不支持 namespace 的 target 在首次真实请求收到明确 schema rejection 后，最多
      自动 flat 重试一次并成功完成。
- [x] flat 请求返回的 function call 对客户端仍包含原始 `namespace` 和本地工具名。
- [x] 非流式、SSE added/done/completed 三类响应均能恢复。
- [x] 后续同 target 请求命中 learned `FlatTools`，不再先发送必然失败的 native body。
- [x] learned mode 只作用于具体 target，不污染同 Provider 的其他账号、API base、模型
      或 endpoint。
- [x] client-executed/dynamic tool search 不被静默删除，而是返回明确能力错误或切换
      兼容 target。

### 安全与可靠性验收

- [x] 普通 400/422、认证、限流、超时和 5xx 不会被错误转换为 flat retry。
- [x] 同一 target 的自适应重试最多一次；请求全局 retry budget 有上限；不存在 body
      重复循环。
- [x] 已向客户端发送任何有效 SSE 内容后不会重试。
- [x] namespace collision 在本地被拒绝，不会调用歧义工具。
- [x] reverse map 只存在于当前 request/attempt，fallback target 切换后不会复用旧 map。
- [x] 日志和 telemetry 遵循现有脱敏规则，不记录完整工具参数或认证信息。

### 运维验收

- [x] telemetry 可以区分 native、flat、cache hit、retry success、capability mismatch。
- [x] learned mode TTL 可配置或至少可通过 config epoch 失效。
- [x] 不增加主动 probe 流量，不需要新增数据库迁移即可启用默认 auto 模式。
- [x] 未包含 namespace/tool_search 的既有 Responses 请求，其请求 body、延迟路径和
      fallback 行为不发生回归。

## 12. 风险与处理

| 风险 | 处理 |
|------|------|
| flat 名称改变模型工具寻址习惯 | 保留原始名称映射；记录 lossy reason；支持显式 native/reject override |
| tool_search 依赖动态工具集合 | client-executed/dynamic 模式拒绝 eager 降级 |
| Provider 返回模糊 400 | 必须匹配 allowlist + 参数路径 + 请求实际字段，不猜测 |
| 多请求同时冷启动 | single-flight 或共享 bounded retry budget |
| 上游升级后仍缓存 flat | TTL、target key 变化和 config epoch 失效 |
| 400 后实际已执行模型 | 只接受 schema validation 类错误；禁止对 timeout/5xx 重放 |

## 13. 参考实现对照

### CLIProxyAPI

CLIProxyAPI 没有针对 namespace/tool_search 的独立 target probe。它根据已知 executor
目标格式静态翻译 Responses→Chat，并通过 descriptor/reverse-map 恢复 namespace。可借鉴
其名称身份映射和 SSE 回程恢复，不直接照搬其“目标格式预先已知”的假设。

### sub2api

sub2api 已实现严格的 rejected-field retry：只识别明确的
`unknown_parameter`/`unsupported_parameter`，使用 body hash 防循环，并对 input
namespace 做白名单式修改。TiyGate 应借鉴其错误绑定和 retry budget；namespace 声明本身
则使用完整的 flatten/reverse-map adapter，避免只删除一个历史字段而留下嵌套工具定义。

## 14. 本次实施落地

本方案已接入 `crates/server` 的 Responses egress：

- `responses_tool_adapter` 提供 namespace、`tool_search`、`additional_tools` 的纯
  JSON flatten、64-byte UTF-8 安全命名、collision 检查、历史 item/tool choice 改写、
  非流式和 SSE reverse-map 恢复。
- `AppState` 持有按 provider/account/API base/endpoint/model/transport 绑定的进程内
  learned-mode cache，默认 TTL 为 1 小时；配置 epoch 变化时清空，未增加数据库迁移。
- Responses executor 采用 native-first；只对明确的 400/422 工具 schema rejection 做
  同 target 一次 flat retry，请求级共享预算最多 2 次；普通 400/422、401/403、429、
  timeout、5xx 不进入 adapter。
- `AppError` 增加 target capability mismatch 标记，fallback 只对该标记切换下一
  target，避免把普通客户端错误误判为 provider 不兼容。
- `EventPayload::ResponsesToolAdaptation` 记录 native/flat/cache-hit/retry/mismatch
  标签，不包含工具参数和凭据；OLTP sink 将其作为非持久化 pipeline 辅助事件处理。
- Codex Responses WebSocket 与普通 HTTP/SSE 共用同一 reverse-map 恢复路径；其他协议
  仍保持原有 IR 转码路径。

新增验证覆盖：

- adapter 单元测试：namespace flatten/restore、collision、长 UTF-8 名称、动态 search
  拒绝、nested `additional_tools`、TTL。
- WireMock：首次 schema rejection 后 flat retry、learned flat 命中、非流式恢复、SSE
  `added/done/completed` 恢复、普通 400 不重试、target mismatch fallback。
