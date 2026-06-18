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

**有损组合（阶段 1-3 已知）**：
- URL 承载 → `messages`（Anthropic 需要 inline base64，无法传递 URL）→ **拒绝**
- inline audio → `chat_completions`/`messages` → **拒绝**
- inline video → 任何非 Gemini → **拒绝**
- file_id → 非 `responses` → **拒绝**

## 3. Reasoning / 结构化输出

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `reasoning` | ✅ | ✅ | ✅ | ✅ | N/A |
| `extended_reasoning` | ❌ | ✅ | ✅ | ✅ | N/A |
| `structured_output` | ✅ | ❌ | ✅ | ✅ | N/A |
| `response_format json_schema` | ✅ | ❌ | ✅ | ✅ | N/A |
| `response_format json_object` | ✅ | ❌ | ✅ | ✅ | N/A |

**有损组合（阶段 1-3 已知）**：
- 任意协议 → `messages` 且请求含 `response_format` → **拒绝**（Anthropic 不支持结构化输出）
- `chat_completions` → 任意 且请求含 `extended_reasoning` → OpenAI 不产生 reasoning，但也不报错，所以 **⚠️ 方向单向有损**

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
| **responses** | ⚠️ file_id 丢失 | ⚠️ file_id + structured_output 拒绝 | PassThrough ✅ | ⚠️ file_id+audio 拒绝 |
| **gemini** | ⚠️ inline video/audio 拒绝 | ⚠️ inline video/audio 拒绝 | ⚠️ inline video/audio 拒绝 | PassThrough ✅ |

## 维护策略

- 每次新增协议 codec 或修改 IR 时，**必须同步更新本矩阵**
- N×N 组合中有损判定必须对应一条集成测试（见 `crates/protocols/tests/`）
- `lossy_default_reject` 的拒绝消息应明确指出被拒绝的维度（如 "tool_choice=required not supported by target protocol gemini"）

## 6. Thinking / Reasoning 配置

| 维度 | chat_completions | messages | responses | gemini | embeddings |
|------|:---:|:---:|:---:|:---:|:---:|
| `effort` (minimal/low/medium/high/xhigh/max) | ✅ (`reasoning_effort`) | ✅ (`thinking.output_config.effort`，adaptive 类型) | ✅ (`reasoning.effort`) | ✅ (Gemini 3+ `thinkingConfig.thinkingLevel`；2.5 → 推导 `thinkingBudget`) | N/A |
| `budget_tokens` | ✅ → 推导 effort（`budget_to_effort`） | ✅ (`thinking.budget_tokens`，enabled 类型) | ✅ → 推导 effort（`budget_to_effort`） | ✅ (Gemini 2.5 `thinkingConfig.thinkingBudget`；3+ → 推导 `thinkingLevel`) | N/A |
| `display` (summarized/omitted) | ⚠️ → 丢弃 | ✅ (`thinking.display`) | ⚠️ → 丢弃 | ✅ → 推导 `includeThoughts` | N/A |
| `include_thoughts` | ⚠️ → 丢弃 | ✅ → 推导 `display`（需同时有 effort 或 budget_tokens） | ⚠️ → 丢弃 | ✅ (`thinkingConfig.includeThoughts`) | N/A |

**跨协议策略**：thinking 配置跨协议时映射或丢弃，不拒绝（thinking 配置不影响语义正确性，只影响模型行为质量）。

**effort 级别映射**：IR 使用 6 级枚举（Minimal/Low/Medium/High/XHigh/Max）。各协议支持级别不同，超出部分 clamp：
- OpenAI: minimal/low/medium/high/xhigh（Max → xhigh）
- Anthropic: low/medium/high/xhigh/max（Minimal → low，使用 adaptive thinking + `output_config.effort`）
- Gemini: 3+ 使用 minimal/low/medium/high（XHigh/Max → high）并只输出 `thinkingLevel`；2.5 使用 `thinkingBudget`。官方协议不允许同一请求同时包含 `thinkingLevel` 和 `thinkingBudget`。

**effort ↔ budget_tokens 双向映射**：`ThinkingConfig::effort_to_budget` / `budget_to_effort` 提供数值映射，各协议 encode 时自动推导缺失字段。

**display ↔ include_thoughts 映射**：Summarized ↔ true，Omitted ↔ false。Anthropic encode 时从 `include_thoughts` 推导 `display`；Gemini encode 时从 `display` 推导 `includeThoughts`。注意 Anthropic 的 `enabled` thinking 类型必须有 `budget_tokens`，仅 `include_thoughts` 无法单独表达。

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
