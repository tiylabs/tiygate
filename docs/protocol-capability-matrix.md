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
| `tool_choice=required` | ✅ | ✅ | ✅ | ⚠️ | N/A |
| `tool_choice=具体函数` | ✅ | ⚠️ → 仅 Anthropic 支持 `tool_choice: {type:"tool", name:"x"}` | ✅ | ❌ | N/A |
| `tool_result` 引用 | ✅ | ✅ | ✅ | ✅ | N/A |

**有损组合（阶段 1-3 已知）**：
- `chat_completions → messages` 且请求包含 `parallel_tool_calls=true` → **拒绝**
- `chat_completions → gemini` 且 `tool_choice=required` → **拒绝**
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
| **chat_completions** | PassThrough ✅ | ⚠️ parallel_tc/tool_choice 可能拒绝 | ✅ | ⚠️ tool_choice=required 拒绝 |
| **messages** | ✅ | PassThrough ✅ | ✅ | ⚠️ tool_use→functionCall 有损 |
| **responses** | ⚠️ file_id 丢失 | ⚠️ file_id + structured_output 拒绝 | PassThrough ✅ | ⚠️ file_id+audio 拒绝 |
| **gemini** | ⚠️ inline video/audio 拒绝 | ⚠️ inline video/audio 拒绝 | ⚠️ inline video/audio 拒绝 | PassThrough ✅ |

## 维护策略

- 每次新增协议 codec 或修改 IR 时，**必须同步更新本矩阵**
- N×N 组合中有损判定必须对应一条集成测试（见 `crates/protocols/tests/`）
- `lossy_default_reject` 的拒绝消息应明确指出被拒绝的维度（如 "tool_choice=required not supported by target protocol gemini"）
