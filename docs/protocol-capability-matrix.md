# 协议能力矩阵（Protocol Capability Matrix）

> 字段级无损/有损/不支持判定表。作为 `lossy_default_reject` 跨协议有损转换拒绝的唯一判定来源。

## 判定符号

| 符号 | 含义 |
|------|------|
| ✅ | 无损（双向可逆） |
| ⚠️ | 有损（`lossy_default_reject` 拒绝） |
| ❌ | 不支持（目标协议无此能力，拒绝） |
| N/A | 不适用 |

## 1. Tool Calling（工具调用）

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `function_calling` | ✅ | ✅ | ✅ | ✅ | N/A |
| `parallel_tool_calls` | ✅ | ⚠️ → chat→msg: 并行工具调用无法在 Anthropic 表达 | ✅ | ⚠️ | N/A |
| `tool_choice=required` | ✅ | ✅ (via `{type:"any"}`) | ✅ | ✅ (via `toolConfig.functionCallingConfig.mode=ANY`) | N/A |
| `tool_choice=具体函数` | ✅ | ✅ (via `{type:"tool", name:"x"}`) | ✅ | ✅ (via `mode=ANY` + `allowedFunctionNames`) | N/A |
| `tool_result` 引用 | ✅ | ✅ | ✅ | ✅ | N/A |

**有损组合（阶段 1-3 已知）**：
- `chat_completions → messages` 且请求包含 `parallel_tool_calls=true` → **拒绝**
- `messages → gemini` tool_use 块结构 → **有损**（Gemini 用 `functionCall`/`functionResponse` parts，语义不完全等价）

## 2. 多模态（Multimodal）

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `multimodal` | ✅ | ✅ | ✅ | ✅ | N/A |
| inline base64 | ✅（image） | ✅（image, document） | ✅ | ✅（image, audio, video, pdf） | N/A |
| URL 引用 | ✅ | ⚠️ → 需要先下载转 inline | ✅ | ✅ | N/A |
| file_id 引用 | ❌ | ❌ | ✅ | ❌ | N/A |
| audio inline | ❌ | ❌ | ✅ | ✅ | N/A |
| video inline | ❌ | ❌ | ❌ | ✅ | N/A |
| `image_url.detail` | ✅ | ❌（lossy：字段丢弃） | ✅ | ❌（lossy：字段丢弃） | N/A |

**有损组合（阶段 1-3 已知）**：
- URL 承载 → `messages`（Anthropic 需要 inline base64，无法传递 URL）→ **拒绝**
- inline audio → `chat_completions`/`messages` → **拒绝**
- inline video → 任何非 Gemini → **拒绝**
- file_id → 非 `responses` → **拒绝**
- `image_url.detail` → `messages`/`gemini` → **有损**（该字段在 IR `Content::Media.metadata` 中保留，但 messages/gemini 编解码器不读取，静默丢弃）

## 3. Reasoning / 结构化输出

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `reasoning` | ✅ | ✅ | ✅ | ✅ | N/A |
| `extended_reasoning` | ❌ | ✅ | ✅ | ✅ | N/A |
| `structured_output` | ✅ | ✅ | ✅ | ✅ | N/A |
| `response_format json_schema` | ✅ | ✅² | ✅ | ✅ | N/A |
| `response_format json_object` | ✅ | ✅¹ | ✅ | ✅ | N/A |

**有损组合（阶段 1-3 已知）**：
- `chat_completions` → 任意 且请求含 `extended_reasoning` → OpenAI 不产生 reasoning，但也不报错，所以 **⚠️ 方向单向有损**

> ¹ Anthropic Messages 以 `output_config.format: {type: "json_schema"}` 表达结构化输出；
> `json_object` 映射为根类型为 `object` 的 JSON Schema。Anthropic 原生不含 OpenAI 的
> `json_object` 简写。
>
> ² Anthropic Structured Outputs 只接受其 JSON Schema 子集。跨协议转换会递归拒绝已知
> 不支持的数值/字符串约束（`minimum`、`maximum`、`exclusiveMinimum`、
> `exclusiveMaximum`、`multipleOf`、`minLength`、`maxLength`），以避免静默弱化
> 原始 response contract；拒绝错误携带 JSON Pointer。完整来源和 profile 基线见
> `protocol-specs/structured-output/anthropic.toml`。

## 4. 确定性/种子

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `deterministic_seed` | ✅ | ❌ | ❌ | ❌ | N/A |

- `chat_completions → 其他协议` 且请求含 `seed` → **丢弃 seed（有损但不拒绝，seed 丢弃不影响语义正确性）**

## 5. 诊断用 N×N 跨协议组合矩阵

| Ingress ↓ / Egress → | chat_completions | messages | responses | gemini |
|----------------------|:---:|:---:|:---:|:---:|
| **chat_completions** | PassThrough ✅ | ⚠️ parallel_tc 可能拒绝 | ✅ | ✅ |
| **messages** | ✅ | PassThrough ✅ | ✅ | ⚠️ tool_use→functionCall 有损 |
| **responses** | ⚠️ file_id 丢失 | ⚠️ file_id | PassThrough ✅ | ⚠️ file_id+audio 拒绝 |
| **gemini** | ⚠️ inline video/audio 拒绝 | ⚠️ inline video/audio 拒绝 | ⚠️ inline video/audio 拒绝 | PassThrough ✅ |

## 维护策略

- 每次新增协议 codec 或修改 IR 时，**必须同步更新本矩阵**
- N×N 组合中有损判定必须对应一条集成测试（见 `crates/protocols/tests/`）
- `lossy_default_reject` 的拒绝消息应明确指出被拒绝的维度（如 "tool_choice=required not supported by target protocol gemini"）

## 6. Thinking / Reasoning 配置

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `effort` (none/minimal/low/medium/high/xhigh/max) | ✅ (`reasoning_effort`，含 `none`/`max`) | ✅（`none` 表示不下发 thinking；其余使用 `output_config.effort`） | ✅ (`reasoning.effort`，含 `none`/`max`) | ✅（2.5 的 `none` → `thinkingBudget: 0`；3+ 近似为 `minimal`） | N/A |
| `budget_tokens` | ✅ → 推导 effort（`budget_to_effort`） | ✅ (`thinking.budget_tokens`，enabled 类型) | ✅ → 推导 effort（`budget_to_effort`） | ✅ (Gemini 2.5 `thinkingConfig.thinkingBudget`；3+ → 推导 `thinkingLevel`) | N/A |
| `display` (summarized/omitted) | ⚠️ → 丢弃 | ✅ (`thinking.display`) | ⚠️ → 丢弃 | ✅ → 推导 `includeThoughts` | N/A |
| `include_thoughts` | ⚠️ → 丢弃 | ✅ → 推导 `display`（需同时有 effort 或 budget_tokens） | ⚠️ → 丢弃 | ✅ (`thinkingConfig.includeThoughts`) | N/A |
| `mode` (e.g. `pro`) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ (`reasoning.mode`) | ❌ 跨协议拒绝 | N/A |
| `context` (persisted reasoning) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ (`reasoning.context`) | ❌ 跨协议拒绝 | N/A |

**跨协议策略**：普通 thinking 配置跨协议时映射或丢弃，不拒绝（thinking 配置不影响语义正确性，只影响模型行为质量）。`mode` / `context` 是 Responses-only 的持久化推理控制；向其他协议转换会以 `LossyDimension::ExtendedReasoning` 明确拒绝，避免静默改变请求行为。

**effort 级别映射**：IR 使用 7 级枚举（None/Minimal/Low/Medium/High/XHigh/Max）。各协议支持级别不同：
- OpenAI Chat/Responses: none/minimal/low/medium/high/xhigh/**max**；server 按真实 upstream model 判定，仅 GPT-5.6 系列保留 max，旧模型降为 xhigh。
- Anthropic: low/medium/high/xhigh/max；None 不下发 thinking，Minimal → low。
- Gemini: 3+ 使用 minimal/low/medium/high（None → minimal 近似，XHigh/Max → high）；2.5 使用 `thinkingBudget`，None → 0。官方协议不允许同一请求同时包含 `thinkingLevel` 和 `thinkingBudget`。

**effort ↔ budget_tokens 双向映射**：`ThinkingConfig::effort_to_budget` / `budget_to_effort` 提供数值映射，各协议 encode 时自动推导缺失字段。

**display ↔ include_thoughts 映射**：Summarized ↔ true，Omitted ↔ false。Anthropic encode 时从 `include_thoughts` 推导 `display`；Gemini encode 时从 `display` 推导 `includeThoughts`。注意 Anthropic 的 `enabled` thinking 类型必须有 `budget_tokens`，仅 `include_thoughts` 无法单独表达。

## 6.1 Hosted Tools（Responses 托管工具）

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| function tools | ✅ | ✅ | ✅ | ✅ | N/A |
| custom tools (`type: "custom"`) | ✅ | ❌ 跨协议拒绝 (`CustomTools`) | ✅ | ❌ 跨协议拒绝 (`CustomTools`) | N/A |
| hosted tools (`web_search` / `file_search` / `code_interpreter` / `computer_use_preview` 等) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅（`Tool.tool_type` + `config` 往返） | ❌ 跨协议拒绝 | N/A |
| Programmatic Tool Calling (`programmatic_tool_calling` / `allowed_callers` / `program` / `caller` / `program_output`) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ 稳定版有序往返 | ❌ 跨协议拒绝 | N/A |

**跨协议策略**：Responses 保留 hosted/function tool 的完整配置，并建模 PTC 的 program、caller 与 program_output 关系。目标协议不能表达 hosted tool 或 PTC 时由 lossy guard 明确拒绝，不再静默过滤。Hosted tool 的 provider-specific 输出 item（`web_search_call` / `file_search_call` / `code_interpreter_call` / `computer_call` 等）在同协议 Convert/re-encode 路径通过有序 `extensions["responses_opaque_output_items"]` 保活；跨协议仍丢弃（客户端不会消费这些 wire item）。raw PassThrough 路径始终字节级无损。

## 6.2 Explicit Prompt Caching

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `prompt_cache_key` | ✅（`openai_extra` 透传） | N/A | ✅（`responses_extra` 透传） | N/A | N/A |
| `prompt_cache_retention` | ✅（`openai_extra` 透传） | N/A | ✅（`responses_extra` 透传） | N/A | N/A |
| `prompt_cache_options` | ✅（Chat ↔ Responses 重放） | N/A | ✅（Chat ↔ Responses 重放） | N/A | N/A |
| per-item `prompt_cache_breakpoint` | ✅（有序 content block） | ❌ 跨协议拒绝 | ✅（有序 input content block） | ❌ 跨协议拒绝 | N/A |
| `cache_write_tokens` usage | ✅（non-stream/stream） | ✅（`cache_creation_input_tokens`） | ✅（non-stream/stream） | N/A | N/A |

**跨协议策略**：Chat 与 Responses 通过 canonical content block 保持显式 breakpoint 的精确位置，顶层 options 使用统一 OpenAI extension 重放；目标协议无等价 carrier 时明确拒绝。

## 6.3 GPT-5.6 Text Controls 与 Beta 边界

| 维度 | chat_completions | messages | responses | gemini |
|------|:---:|:---:|:---:|:---:|
| `verbosity` | ✅ 顶层 `verbosity` | ❌ 跨协议拒绝 | ✅ `text.verbosity` | ❌ 跨协议拒绝 |
| `safety_identifier` | ✅ | N/A | ✅ | N/A |
| image `detail: "original"` | ✅ | ⚠️ 无等价语义 | ✅ | ⚠️ 无等价语义 |
| Multi-agent Beta | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ 同协议透传 / re-encode 保活（见 §13） | ❌ 跨协议拒绝 |

Multi-agent 仍要求客户端显式提供 `OpenAI-Beta: responses_multi_agent=v1`。同协议路径保活顶层 `multi_agent` 与 `multi_agent_call/output` items；跨协议由 `LossyDimension::MultiAgent` 硬拒绝。不建模 agent 事件类型，也不宣称 typed multi-agent 完整支持——详见 §13。

## 7. Metadata

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `metadata` KV 对 | ✅ | ⚠️ → 仅保留 `user_id` | ✅ | ✅ (`labels`) | N/A |
| `user_id` | ✅ | ✅ | ✅ | ✅ | N/A |

**跨协议策略**：Anthropic 只支持 `user_id` 键，其他键静默丢弃（与官方 API 一致）。

## 8. Annotations / Citations

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| URL citation | ✅ (`annotations[]`) | ⚠️ → 丢弃 | ✅ (`annotations[]`) | ✅ (`groundingMetadata`) | N/A |
| File citation | ✅ | ⚠️ → 丢弃 | ✅ | ⚠️ → 丢弃 | N/A |

**跨协议策略**：annotations 跨协议时允许丢弃（annotations 是展示层数据，不影响模型推理）。

## 9. Refusal

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| refusal 文本 | ✅ (`message.refusal`) | ⚠️ → 作为 text 输出 | ✅ (`refusal` output item) | ⚠️ → 作为 text 输出 | N/A |
| refusal stop_reason | ✅ → `content_filter` | ✅ (`stop_reason:"refusal"`) | ✅ → `incomplete` | ✅ → `SAFETY` | N/A |

**跨协议策略**：refusal 文本跨协议时保留为 `Content::Refusal`，目标协议不支持独立 refusal 字段时作为 text 输出。

## 10. Encrypted Reasoning Content

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `encrypted_content` | ⚠️ → 丢弃 | ✅ (`redacted_thinking.data`) | ✅ (`reasoning.encrypted_content`) | ⚠️ → 丢弃 | N/A |

**跨协议策略**：encrypted_content 仅在同协议往返时保留（Responses ↔ Responses, Anthropic ↔ Anthropic），跨协议时丢弃（加密数据是协议特定的）。

## 11. Stop Details

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `stop_details` (structured) | ⚠️ → 仅 `finish_reason` | ✅ (`stop_details` object) | ⚠️ → 仅 `status` | ⚠️ → 仅 `finishReason` | N/A |

**跨协议策略**：stop_details 跨协议时映射到目标协议的 stop reason 字段，结构化 details（type/category/explanation）可能丢失。

## 12. Codex 扩展兼容性

Codex 客户端在 OpenAI Responses 协议上扩展了若干 item 类型和字段。同协议 Passthrough（Responses→Responses）时原始字节无损通过；以下行为仅适用于跨协议转换（Convert 模式）。

### Codex Input Item 类型

| Item 类型 | 跨协议行为 |
|-----------|-----------|
| `local_shell_call` | ✅ 映射为 IR `Content::ToolCall { name: "local_shell" }`，跨协议可转换 |
| `local_shell_call_output` | ✅ 映射为 IR `Content::ToolResult`，跨协议可转换 |
| `custom_tool_call` | ✅ 映射为 IR `Content::ToolCall`（`wire_type=custom_tool_call`，input 文本包装为 JSON arguments）；同协议 re-encode 恢复 `custom_tool_call` |
| `custom_tool_call_output` | ✅ 映射为 IR `Content::ToolResult`（`wire_type=custom_tool_call_output`）；同协议 re-encode 恢复原 wire type |
| `tool_search_call` | ⚠️ 原始 JSON 存入有序 `extensions["responses_opaque_input_items"]`（兼容旧 `codex_opaque_items`），同协议 egress 按原 index 还原，跨协议丢弃 |
| `tool_search_output` | ⚠️ 同上 |
| `agent_message` | ⚠️ 同上 |
| `compaction` | ⚠️ 同上 |
| `compaction_trigger` | ⚠️ 同上 |
| `context_compaction` | ⚠️ 同上 |

**注意**：`local_shell_call` 映射为 `Content::ToolCall` 时 tool name 设为 `local_shell`，跨协议到 Chat Completions 后上游可能不识别此工具名——这是固有的语义有损，但不触发 lossy rejection。

### Codex Response Output Item 类型

| Item 类型 | 跨协议行为 |
|-----------|-----------|
| `local_shell_call` | ✅ 映射为 IR `Content::ToolCall`，计入 `FinishReason::ToolCalls` 判断 |
| `custom_tool_call` | ✅ 映射为 IR `Content::ToolCall` |
| `tool_search_call` / `agent_message` / `compaction` 等 | ⚠️ 静默丢弃（响应中的这些 item 对跨协议客户端无意义） |

### Codex 扩展字段

| 字段 | 跨协议行为 |
|------|-----------|
| `reasoning.summary` | ✅ 解析到 IR `ThinkingConfig.summary`，Responses egress 时回写；跨协议到 Anthropic/Gemini 时丢弃（不拒绝） |
| `text.verbosity` | ✅ 解析到 IR `params.verbosity`；Responses 同协议还通过 `extensions["text"]` 保留完整 `text` 对象；跨协议到非 OpenAI egress 时由 `LossyDimension::Verbosity` **拒绝**（不是静默丢弃） |
| `client_metadata` | ✅ 加入 `responses_extra` 透传列表，同协议 egress 自动回写；跨协议时丢弃 |

### Codex 自定义请求头

| 头 | 跨协议行为 |
|----|-----------|
| `x-codex-*` | ✅ 不在 `DEFAULT_REQUEST_DENY` / `DEFAULT_RESPONSE_DENY` 中，C→G→P 和 P→G→C 方向均自动转发 |
| `x-openai-subagent` | ✅ 同上 |
| `x-codex-turn-state` | ✅ 响应头，不在 `DEFAULT_RESPONSE_DENY` 中，自动转发回客户端 |
| `OpenAI-Beta` | ✅ 通用客户端头，自动转发 |

## 13. Multi-agent Beta（GPT-5.6 / Responses）

OpenAI Responses Multi-agent Beta（`OpenAI-Beta: responses_multi_agent=v1`）仅在 **Responses 同协议**路径上支持透传；跨协议一律拒绝，不做 IR 类型化或转换。

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| 顶层 `multi_agent` | ❌ 拒绝 | ❌ 拒绝 | ✅ 同协议透传 / re-encode 保活 | ❌ 拒绝 | N/A |
| `multi_agent_call` / `multi_agent_call_output` input items | ❌ 拒绝 | ❌ 拒绝 | ✅ 存入有序 `responses_opaque_input_items` + 内容袋 `multi_agent_items`，同协议按原顺序回放 | ❌ 拒绝 | N/A |
| 跨协议 Convert | ❌ | ❌ | N/A（同协议） | ❌ | N/A |

**运行时行为**：
- 同协议（Responses→Responses）：raw passthrough 与 IR re-encode 均保留 `multi_agent` 与 multi-agent input items；re-encode 通过 `responses_opaque_input_items` 的原始 index 保持与 user/assistant 消息的交错顺序；`OpenAI-Beta` 头按现有 denylist 策略转发。
- 跨协议：`check_lossy_conversion` 检测到 `responses_extra.multi_agent` 或非空 `multi_agent_items` 时，以 `LossyDimension::MultiAgent` **拒绝**（HTTP 400），不静默丢弃。
- 不支持 WebSocket multi-agent 长连接；本网关 Responses 面仅为 HTTP + SSE。
- 不建模 agent 调度语义；不做跨协议转换。
