# 协议能力矩阵（Protocol Capability Matrix）

> 字段级无损/有损/不支持判定表。作为 `lossy_default_reject` 跨协议有损转换拒绝的唯一判定来源。

## 判定符号

| 符号 | 含义 |
| ------ | ------ |
| ✅ | 无损（双向可逆） |
| ⚠️ | 有损（`lossy_default_reject` 拒绝） |
| ❌ | 不支持（目标协议无此能力，拒绝） |
| N/A | 不适用 |

## 1. Tool Calling（工具调用）

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
| `function_calling` | ✅ | ✅ | ✅ | ✅ | N/A |
| `parallel_tool_calls` | ✅ | ✅ 禁用并行映射为 `disable_parallel_tool_use:true`；显式启用仍按既有契约拒绝 | ✅ | ⚠️ 禁用并行无等价 carrier，拒绝 | N/A |
| `tool_choice=required` | ✅ | ✅ (via `{type:"any"}`) | ✅ | ✅ (via `toolConfig.functionCallingConfig.mode=ANY`) | N/A |
| `tool_choice=具体函数` | ✅ | ✅ (via `{type:"tool", name:"x"}`) | ✅ | ✅ (via `mode=ANY` + `allowedFunctionNames`) | N/A |
| `tool_result` 引用 | ✅ | ✅ | ✅ | ✅ | N/A |

**有损组合（阶段 1-3 已知）**：

- `chat_completions → messages` 且请求包含 `parallel_tool_calls=true` → **拒绝**
- `messages → gemini` tool_use 块结构 → **有损**（Gemini 用 `functionCall`/`functionResponse` parts，语义不完全等价）

## 2. 多模态（Multimodal）

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
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
| ------ | :---: | :---: | :---: | :---: | :---: |
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

### 3.1 DeepSeek Responses profile

DeepSeek Provider 仅将 `deepseek-chat` 与 `deepseek-reasoner` 发送到
`POST /v1/chat/completions`；其余模型统一使用原生 `POST /responses` 出站。DeepSeek
Responses 的 reasoning item 使用
`content: [{"type":"reasoning_text","text":"..."}]`，TiyGate 在该 provider profile 下会把
OpenAI 风格的 `summary` 回放转换为 `reasoning_text`。DeepSeek 无法解密 OpenAI 风格加密推理，
因此当客户端回传带 `encrypted_content` 的 reasoning 项时，TiyGate 会**剥离该加密字段并保留明文
`summary`/`reasoning_text`**；若该项仅有密文、无任何明文可回放（加密-only shell），则**丢弃整个
reasoning 项**，而不是拒绝整单请求——与 DeepSeek 对其它“接受但忽略”非语义控制项的处理方式一致。

DeepSeek 会静默忽略部分 OpenAI Responses 能力。为维持 `lossy_default_reject` 契约，
TiyGate 对有语义影响的不支持项返回 `400 LossyOrCapability`，包括
`previous_response_id`、conversation/store/background 状态、自动 truncation、禁用并行工具
调用、未支持的 input item，
以及 `file_search`、`code_interpreter`、`computer_use`、`mcp` 等工具。允许的工具为
function、web search，以及名为 `apply_patch` 的 custom tool。

`prompt_cache_key`、`prompt_cache_retention`、`prompt_cache_options`、`metadata`、`include`、
`text.verbosity`、`reasoning.summary`
属于例外：DeepSeek 自动管理上下文缓存并默认返回 reasoning/tool items，且从不解析
这些非语义控制项（`text.verbosity` 与 `reasoning.summary` 是“接受但忽略”），因此
TiyGate 在 DeepSeek Responses 出站前显式移除这些字段，不将其视为影响生成语义的有损
转换，也不会因此拒绝 Codex 客户端请求。DeepSeek 不支持加密 reasoning content，
客户端请求 `include: ["reasoning.encrypted_content"]` 时收到的仍是明文 `reasoning_text`
——这是 DeepSeek 固有限制，而非剥离所致；同理，回传的加密 blob 在出站前被剥离，
不构成有语义影响的有损转换。

来源：[DeepSeek Responses API 指南](https://api-docs.deepseek.com/guides/responses_api/)、
[DeepSeek Responses API Reference](https://api-docs.deepseek.com/api/create-response/)。

## 4. 确定性/种子

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `deterministic_seed` | ✅ | ❌ | ❌ | ❌ | N/A |

- `chat_completions → 其他协议` 且请求含 `seed` → **丢弃 seed（有损但不拒绝，seed 丢弃不影响语义正确性）**

## 5. 诊断用 N×N 跨协议组合矩阵

| Ingress ↓ / Egress → | chat_completions | messages | responses | gemini |
| ---------------------- | :---: | :---: | :---: | :---: |
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
| ------ | :---: | :---: | :---: | :---: | :---: |
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
| ------ | :---: | :---: | :---: | :---: | :---: |
| function tools | ✅ | ✅ | ✅ | ✅ | N/A |
| custom tools (`type: "custom"`) | ✅ | ❌ 跨协议拒绝 (`CustomTools`) | ✅ | ❌ 跨协议拒绝 (`CustomTools`) | N/A |
| Responses hosted tools (`web_search` / `file_search` / `code_interpreter` / `computer_use_preview` 等) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅（`Tool.tool_type` + `config` 往返） | ❌ 跨协议拒绝 | N/A |
| Gemini native tools (`codeExecution` / `googleSearch` 等) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ 同协议保留配置 | N/A |
| Programmatic Tool Calling (`programmatic_tool_calling` / `allowed_callers` / `program` / `caller` / `program_output`) | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ 稳定版有序往返 | ❌ 跨协议拒绝 | N/A |

**跨协议策略**：Responses 保留 hosted/function tool 的完整配置，并建模 PTC 的 program、caller 与 program_output 关系。目标协议不能表达 hosted tool 或 PTC 时由 lossy guard 明确拒绝，不再静默过滤。Hosted tool 的 provider-specific 输出 item（`web_search_call` / `file_search_call` / `code_interpreter_call` / `computer_call` 等）在同协议 Convert/re-encode 路径通过有序 `extensions["responses_opaque_output_items"]` 保活；跨协议仍丢弃（客户端不会消费这些 wire item）。raw PassThrough 路径保留原生 JSON 字段；入口 JSON 解析、模型改写和 Provider profile mutation 不承诺客户端原始空白、键顺序或数字文本的字节保真。

## 6.2 Explicit Prompt Caching

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
| `prompt_cache_key` | ✅（`openai_extra` 透传） | N/A | ✅（`responses_extra` 透传） | N/A | N/A |
| `prompt_cache_retention` | ✅（`openai_extra` 透传） | N/A | ✅（`responses_extra` 透传） | N/A | N/A |
| `prompt_cache_options` | ✅（Chat ↔ Responses 重放） | N/A | ✅（Chat ↔ Responses 重放） | N/A | N/A |
| per-item `prompt_cache_breakpoint` | ✅（有序 content block） | ❌ 跨协议拒绝 | ✅（有序 input content block） | ❌ 跨协议拒绝 | N/A |
| `cache_write_tokens` usage | ✅（non-stream/stream） | ✅（`cache_creation_input_tokens`） | ✅（non-stream/stream） | N/A | N/A |

**跨协议策略**：Chat 与 Responses 通过 canonical content block 保持显式 breakpoint 的精确位置，顶层 options 使用统一 OpenAI extension 重放；目标协议无等价 carrier 时明确拒绝。

## 6.3 GPT-5.6 Text Controls 与 Beta 边界

| 维度 | chat_completions | messages | responses | gemini |
| ------ | :---: | :---: | :---: | :---: |
| `verbosity` | ✅ 顶层 `verbosity` | ❌ 跨协议拒绝 | ✅ `text.verbosity` | ❌ 跨协议拒绝 |
| `safety_identifier` | ✅ | N/A | ✅ | N/A |
| image `detail: "original"` | ✅ | ⚠️ 无等价语义 | ✅ | ⚠️ 无等价语义 |
| Multi-agent Beta | ❌ 跨协议拒绝 | ❌ 跨协议拒绝 | ✅ 同协议透传 / re-encode 保活（见 §13） | ❌ 跨协议拒绝 |

Multi-agent 仍要求客户端显式提供 `OpenAI-Beta: responses_multi_agent=v1`。同协议路径保活顶层 `multi_agent` 与 `multi_agent_call/output` items；跨协议由 `LossyDimension::MultiAgent` 硬拒绝。不建模 agent 事件类型，也不宣称 typed multi-agent 完整支持——详见 §13。

## 7. Metadata

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
| `metadata` KV 对 | ✅ | ⚠️ → 仅保留 `user_id` | ✅ | ✅ (`labels`) | N/A |
| `user_id` | ✅ | ✅ | ✅ | ✅ | N/A |

**跨协议策略**：Anthropic 只支持 `user_id` 键，其他键静默丢弃（与官方 API 一致）。公开 OpenAI Responses 支持顶层 `metadata`；但 `openai_codex` OAuth egress 面向 ChatGPT/Codex 私有后端，会在发送前丢弃不兼容字段，并保留网关内部审计数据。Codex egress 还会清洗嵌套 cache breakpoint、校验 reasoning encrypted content，并按 Claude Code session/agent 派生 prompt-cache identity。

## 8. Annotations / Citations

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
| URL citation | ✅ (`annotations[]`) | ⚠️ → 丢弃 | ✅ (`annotations[]`) | ✅ (`groundingMetadata`) | N/A |
| File citation | ✅ | ⚠️ → 丢弃 | ✅ | ⚠️ → 丢弃 | N/A |

**跨协议策略**：annotations 跨协议时允许丢弃（annotations 是展示层数据，不影响模型推理）。

## 9. Refusal

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
| refusal 文本 | ✅ (`message.refusal`) | ⚠️ → 作为 text 输出 | ✅ (`refusal` output item) | ⚠️ → 作为 text 输出 | N/A |
| refusal stop_reason | ✅ → `content_filter` | ✅ (`stop_reason:"refusal"`) | ✅ → `incomplete` | ✅ → `SAFETY` | N/A |

**跨协议策略**：refusal 文本跨协议时保留为 `Content::Refusal`，目标协议不支持独立 refusal 字段时作为 text 输出。

## 10. Encrypted Reasoning Content

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `encrypted_content` | ⚠️ → 丢弃 | ✅ (`redacted_thinking.data`) | ✅ (`reasoning.encrypted_content`) | ⚠️ → 丢弃 | N/A |

**跨协议策略**：一般跨协议时丢弃 encrypted_content（加密数据是协议特定的）；但 `openai_codex` OAuth egress 对 Anthropic thinking signature 做 GPT/Codex 外层格式校验，只有有效 signature 才转换为 Responses `reasoning.encrypted_content`，非法或其他供应商 signature 仍会丢弃。

## 11. Stop Details

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `stop_details` (structured) | ⚠️ → 仅 `finish_reason` | ✅ (`stop_details` object) | ⚠️ → 仅 `status` | ⚠️ → 仅 `finishReason` | N/A |

**跨协议策略**：stop_details 跨协议时映射到目标协议的 stop reason 字段，结构化 details（type/category/explanation）可能丢失。

## 12. Codex 扩展兼容性

Codex 客户端在 OpenAI Responses 协议上扩展了若干 item 类型和字段。同协议 Passthrough（Responses→Responses）保留原生字段，模型改写和 Provider profile mutation 另有边界；以下行为仅适用于跨协议转换（Convert 模式）。

### Codex Input Item 类型

| Item 类型 | 跨协议行为 |
| ----------- | ----------- |
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
| ----------- | ----------- |
| `local_shell_call` | ✅ 映射为 IR `Content::ToolCall`，计入 `FinishReason::ToolCalls` 判断 |
| `custom_tool_call` | ✅ 映射为 IR `Content::ToolCall` |
| `tool_search_call` / `agent_message` / `compaction` 等 | ⚠️ 静默丢弃（响应中的这些 item 对跨协议客户端无意义） |

### Codex 扩展字段

| 字段 | 跨协议行为 |
| ------ | ----------- |
| `reasoning.summary` | ✅ 解析到 IR `ThinkingConfig.summary`，Responses egress 时回写；跨协议到 Anthropic/Gemini 时丢弃（不拒绝） |
| `text.verbosity` | ✅ 解析到 IR `params.verbosity`；Responses 同协议还通过 `extensions["text"]` 保留完整 `text` 对象；跨协议到非 OpenAI egress 时由 `LossyDimension::Verbosity` **拒绝**（不是静默丢弃） |
| `client_metadata` | ✅ 加入 `responses_extra` 透传列表，同协议 egress 自动回写；跨协议时丢弃 |

### Codex 自定义请求头

| 头 | 跨协议行为 |
| ---- | ----------- |
| `x-codex-*` | ✅ 不在 `DEFAULT_REQUEST_DENY` / `DEFAULT_RESPONSE_DENY` 中，C→G→P 和 P→G→C 方向均自动转发 |
| `x-openai-subagent` | ✅ 同上 |
| `x-codex-turn-state` | ✅ 响应头，不在 `DEFAULT_RESPONSE_DENY` 中，自动转发回客户端 |
| `OpenAI-Beta` | ✅ 通用客户端头，自动转发 |

## 13. Multi-agent Beta（GPT-5.6 / Responses）

OpenAI Responses Multi-agent Beta（`OpenAI-Beta: responses_multi_agent=v1`）仅在 **Responses 同协议**路径上支持透传；跨协议一律拒绝，不做 IR 类型化或转换。

| 维度 | chat_completions | messages | responses | gemini | embeddings |
| ------ | :---: | :---: | :---: | :---: | :---: |
| 顶层 `multi_agent` | ❌ 拒绝 | ❌ 拒绝 | ✅ 同协议透传 / re-encode 保活 | ❌ 拒绝 | N/A |
| `multi_agent_call` / `multi_agent_call_output` input items | ❌ 拒绝 | ❌ 拒绝 | ✅ 存入有序 `responses_opaque_input_items` + 内容袋 `multi_agent_items`，同协议按原顺序回放 | ❌ 拒绝 | N/A |
| 跨协议 Convert | ❌ | ❌ | N/A（同协议） | ❌ | N/A |

**运行时行为**：

- 同协议（Responses→Responses）：raw passthrough 与 IR re-encode 均保留 `multi_agent` 与 multi-agent input items；re-encode 通过 `responses_opaque_input_items` 的原始 index 保持与 user/assistant 消息的交错顺序；`OpenAI-Beta` 头按现有 denylist 策略转发。
- 跨协议：`check_lossy_conversion` 检测到 `responses_extra.multi_agent` 或非空 `multi_agent_items` 时，以 `LossyDimension::MultiAgent` **拒绝**（HTTP 400），不静默丢弃。
- 不支持 WebSocket multi-agent 长连接；本网关 Responses 面仅为 HTTP + SSE。
- 不建模 agent 调度语义；不做跨协议转换。


## 14. Protocol Review 回归契约（2026-10-02）

- **SSE 字节与事件**：跨协议流在 server 层按完整事件 framing；支持 UTF-8 跨网络分片、CRLF/CR、多个 `data:` 行以及冒号后可选空格。单事件缓冲上限 16 MiB，超限/非法 UTF-8/损坏 JSON 显式错误并结束转换，不以成功终止掩盖。
- **工具流关联**：Messages 输出工具块按 call ID/index 维护，交错参数不写入最近工具块；各块在 Finish/ResponseCompleted 时成对关闭。Chat 数组工具结果保持消息级 `tool_call_id`。
- **终止**：Gemini `usageMetadata` 不是终止信号，必须等待 `finishReason`；EOF 缺真实终态不生成成功完成。Chat 错误后的 `[DONE]` 不生成 Responses 成功 `response.completed`。Responses encoder 等待 `ResponseCompleted` 后才封存最终累计 usage；长度/过滤终态输出原生 `response.incomplete` 和 `incomplete_details`，item 状态保持一致。完成/错误后不输出新的语义内容。
- **用量**：IR `completion_tokens` 包含 reasoning；Gemini decode 将 `candidatesTokenCount + thoughtsTokenCount` 合并，encode 再拆分，缓存 input 约定不变。
- **Gemini 请求/回放**：读取 `systemInstruction`（兼容历史 `system_instruction`），出站使用 camelCase；保留 functionCall/functionResponse 原生 ID。无 ID 的同名调用按出现次数分配独立 ID，历史 name-only result 按调用顺序匹配；无法匹配时拒绝。真实 thoughtSignature 按 call ID 原样回放，不能被 sentinel 覆盖。多个 candidate 的 IR 转换明确拒绝，正常同协议 raw 不承诺 IR 多候选支持。
- **Schema**：Gemini `responseJsonSchema` 进入 canonical JsonSchema；需要 `additionalProperties`/`$ref`/`$defs`/`prefixItems` 时使用原生 JSON Schema carrier，其余保留 Schema 支持的边界约束。不能安全等价的组合/校验关键字（如 multipleOf、oneOf、allOf、not、条件 schema）明确拒绝，绝不降为 prompt 或 JSON-object 简写。
- **工具选择**：Gemini ANY + allowedFunctionNames 保留 required 和全部允许名称，跨协议只提供允许的声明；允许集合与声明交集为空时拒绝。
- **工具失败**：IR ToolResult 保留 `is_error`；Messages 回放原 flag，Gemini 用 `response.error` 承载。目标 Chat/Responses 没有原生 error flag 时，包含 `is_error=true` 的请求明确拒绝（`ToolResultError`），不默默视为成功。
- **Reasoning/refusal**：Messages `redacted_thinking.data` 在同协议 re-encode 使用密文 carrier 原样回放；跨供应商加密数据仍非通用可验证载体。Responses 原生 `message.content[].refusal` 保留到 canonical refusal。
- **媒体**：loss guard 除 source 形式也检查 MIME，audio→Chat/Messages、video→非 Gemini 明确拒绝，避免编码为伪 image。

### 14.1 Follow-up 修复契约（TG-PROTO-023～036）

- Chat 字符串 `stop` 归一为一个停止序列，数组保留全部序列。
- OpenAI 函数工具的 `strict:true/false` 双向保留；Chat→Responses 缺省时显式发送 `strict:false`，Responses→Chat 缺省时显式发送 `strict:true`，避免协议默认值差异。Messages 支持 native `strict`；严格工具转换到无 carrier 的 Gemini 时拒绝。
- `parallel_tool_calls:false` ↔ Messages `tool_choice.disable_parallel_tool_use:true`；未知/未指定与 false 分开。向 Gemini 转换禁用并行时拒绝。
- Responses `conversation` 在同协议 IR 重编码保活；带 `previous_response_id` 或 `conversation` 的跨协议请求明确拒绝，网关不下载或猜测历史上下文。同协议账号/模型作用域仍由目标 Provider 管理，不承诺跨账号 fallback 可续接。
- 当前 IR 只支持一个候选。跨协议 `n>1`、多 Chat choices 或非零 choice index 明确拒绝；同协议 raw 响应可保留多个候选，IR 不合并答案。
- 非流 Chat/Responses 工具参数损坏时返回 codec 错误，不替换为 `{}`。Responses 完整 arguments.done/output_item.done 根据 item/call ID 补交付缺失后缀；重复完整 payload 不重复参数，不一致或损坏则报错。
- `RefusalDelta` 独立于答案文本：Chat 输出 delta.refusal，Responses 输出原生 refusal 生命周期，Messages/Gemini 以文本承载；完整 refusal 与 delta 去重。
- Messages `document` 的 base64/URL carrier 进入媒体 IR；不支持的 document source 显式拒绝，不能入口丢弃。文档到 Chat 无等价输入 carrier 时拒绝，Responses/Gemini 保留文件语义，Messages 重编码使用 document 类型。
- HTTP 200 Responses `status:failed` 即使带 `output:[]` 仍是上游失败；客户端获得原生错误 envelope 和非成功状态，不能转换为空答案/stop。

回归语料为本项目自行编写的官方 wire 合成输入，见 `crates/protocols/tests/review_regressions.rs`、`crates/server/tests/protocol_review.rs`；不引入参考项目代码或 AGPL fixture。以上是有限已验证输入的契约，不代表全部模型/账号/生产上游兼容。


### 14.2 Final review 修复契约（TG-PROTO-029、037～050）

- **指定工具选择**：Chat 的 `tool_choice.function.name` 与 Responses 的 `tool_choice.name` 在入口归一，出站恢复目标原生 carrier；缺名称明确 codec 错误，不索引缺失键而 panic。函数工具和 custom 选择的类型分别保留。
- **工具历史**：Gemini 名称查找使用 canonical `call_id.unwrap_or(id)`，不从合成 ID 前缀猜名称。Responses 请求历史中损坏的 function arguments 返回入口错误，不替换为 `{}`。含文本与工具结果的有序内容转换到 Chat 时按出现顺序拆成 user/tool 消息。
- **Gemini Schema**：`responseSchema` 与 function declaration `parameters` 属于原生 Schema dialect；递归归一 `OBJECT`/`STRING` 等类型，`nullable:true` 使用包含 null 的 `anyOf`，同时保留 enum 等约束。`responseJsonSchema` / `parametersJsonSchema` 已是 JSON Schema，原样进入 IR；不转换 enum/default 中的实例值。
- **Gemini hosted 工具**：`codeExecution`、`googleSearch` 等非函数 carrier 保留类型与配置。同协议重编码回放，跨协议目标明确以 `HostedTools` 拒绝；不能在入口删除后绕过 guard，也不能与 Responses hosted 工具名称混用。
- **Messages 工具结果媒体**：文本数组继续使用文本 IR；包含 image/document 或其他非文本块时保留原始有序数组供同协议回放。当前 ToolResult IR 没有通用多模态内容载体，因此跨协议以 `ToolResultContent` 明确拒绝，不能将图片 JSON 当作文本工具结果发送。重复的非文本 result ID 明确拒绝。
- **Custom tools**：Chat 定义使用 `tools[].custom`，Responses 使用扁平定义；Chat 兼容历史扁平输入，输出使用原生嵌套结构。流式调用按 `custom.name/input` 解码，向 Chat/Responses 输出原生 custom 增量与完整 free-form input，不按函数 JSON 校验。
- **Chat 工具流身份**：按 index 保存调用身份；重复相同 ID/name 幂等，冲突拒绝，迟到身份有界缓冲到 ID/name 齐全再创建一次工具块。不合成工具名或 ID；成功结束时缺身份或函数参数 JSON 损坏明确错误，Length/ContentFilter 保留截断语义。
- **流式资源上界**：Chat tool index 为 0～255，Messages content index 为 0～1023；非法类型、负值和超限 index 返回 codec 错误。Chat 单个 ID/name 上限 1 KiB，总工具参数缓存上限 16 MiB，防止巨大或稀疏 index 驱动无界扩容。上限是网关资源契约，不宣称协议官方 index 上限相同。
- **Refusal**：Gemini 非流响应以 text 保留 refusal 文本；Messages 流/非流使用原生 `stop_reason:"refusal"`，映射为 canonical ContentFilter，不能变成自然 end_turn/stop。
- **Gemini URL**：出站 base 可为无版本 proxy root 或以 `/v1beta`（也兼容 `/v1`）结尾的 versioned base；只添加一次版本，stream/nonstream 分别拼原生 method。模型/账号 profile 的实际能力仍需独立验证。

原始语料与独立断言见 `crates/protocols/tests/review_final.rs` 和 `crates/server/tests/protocol_review_final.rs`。CI 缺口 TG-PROTO-020 不由本轮 codec 修复解决；真实账号、全部 profile 和大内存压力不属于这些离线用例的证明范围。
