# `/v1/models` 端点协议规范

> tiygate 对外暴露的 OpenAI 兼容 `GET /v1/models` 与 `GET /v1/models/{model_id}` 端点的标准协议。
> 本文档是字段层级的契约来源，所有实现必须按此规范返回数据。

## 1. 设计目标与原则

1. **OpenAI 优先兼容**：直接复用 OpenAI `List models` / `Retrieve model` 的 `object`/`data`/`id`/`created`/`owned_by` 形状，保证 `openai-python`、`openai-node`、`langchain`、`llama-index`、Dify、FastGPT 等主流客户端零修改接入。
2. **多厂商字段统一抽象**：在 OpenAI 形状之上，将 Anthropic `display_name`、Google Gemini `displayName` + `supportedGenerationMethods` + `inputTokenLimit/outputTokenLimit`、Hugging Face `pipeline_tag`、Ollama `details.{family, parameter_size, quantization_level}`、vLLM `max_model_len` 等事实标准字段纳入 `Model` 对象的可选扩展区。客户端可选用熟悉的字段集。
3. **可演进**：必填字段为各厂商共识；扩展字段按"未知即不返回"原则允许后续追加，旧客户端的解析不破。
4. **类型用 OpenAI 习惯**：`created` 用 Unix 秒（int），不用 ISO8601；字符串 ID；金额/字节用 int。

## 2. 端点形态

| 路径 | 方法 | 鉴权 | 用途 |
|------|------|------|------|
| `/v1/models` | `GET` | 必填，Bearer Token（OpenAI 兼容） | 列出当前租户可见的全部模型 |
| `/v1/models/{model_id}` | `GET` | 必填，Bearer Token | 拉取单个模型的元数据卡片（ModelCard） |

### 2.1 通用请求约定

- `Authorization: Bearer <token>` 必填。token 解析、租户隔离、配额、限流等中间件逻辑由 `tiygate` 现有鉴权层提供。
- `Content-Type: application/json`。
- 错误响应遵循 OpenAI 错误体（`{"error": {"message", "type", "param", "code"}}`）。鉴权失败 → `401`、权限不足 → `403`、模型不存在 → `404`、限流 → `429`、上游不可用 → `502/503`、参数错 → `400`。

### 2.2 `/v1/models` 查询参数

参考 OpenAI 实现 + Gemini/多厂商分页惯例：

| 参数 | 类型 | 必填 | 含义 |
|------|------|------|------|
| `limit` | integer `1..1000` | 否 | 单页最大模型数，默认 100，最大 1000 |
| `after` | string | 否 | 游标分页锚点（取上一次响应 `last_id` 的值） |
| `order` | string `asc`\|`desc` | 否 | 列表排序方式（按 `created`），默认 `desc` |
| `owned_by` | string | 否 | 按 `owned_by` 过滤，例如 `openai`、`anthropic`、`google`、自建 `self`、合作方 ID |

> 注：OpenAI 官方 `/v1/models` 当前未提供 `limit` / `after` / `order`（列表很短、官方不分页）。`tiygate` 为多租户/多模型路由场景引入上述扩展分页，但**默认值必须**与 OpenAI 一致（即客户端不带 `limit` 时返回**全部**可见模型），避免破坏简单用例。

### 2.3 `/v1/models/{model_id}` 路径参数

| 参数 | 类型 | 必填 | 含义 |
|------|------|------|------|
| `model_id` | string | 是 | 路径段，模型唯一 ID。建议做 URL 解码，原始 ID 允许 `:`、`/` 等字符（与 OpenAI `ft:gpt-4o-mini:org:suffix:abc` 兼容） |

`model_id` 命名约定（用于租户内分发路由）：
- 官方预置 ID（如 `gpt-4o`、`claude-opus-4-6`、`gemini-2.0-flash`）保持原样。
- 租户内私有模型加 `tenant_` 前缀（可选），例如 `tenant_abc_finetuned_llama`。
- 删除字符后统一 lowercase；查询时大小写不敏感。

## 3. 列表响应

```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "object": "list",
  "data": [ /* Model objects */ ],
  "has_more": false,
  "first_id": "claude-haiku-4-5",
  "last_id": "gpt-4o"
}
```

### 3.1 `ListModelsResponse` 字段表

| 字段 | 类型 | 必填 | 含义 |
|------|------|:---:|------|
| `object` | string | ✅ | 固定 `"list"`。OpenAI 兼容值 |
| `data` | `Model[]` | ✅ | 模型卡片数组。空数组时也必须返回 `[]` 而非 `null` |
| `has_more` | boolean | 否 | 是否还有更多页。`false` 或缺省时表示已到末页 |
| `first_id` | string | 否 | 当前页第一条模型的 `id`，配合 `after` 游标分页使用 |
| `last_id` | string | 否 | 当前页最后一条模型的 `id`，配合 `after` 游标分页使用 |

## 4. 模型对象 `Model`（核心契约）

```json
{
  "id": "claude-opus-4-6",
  "object": "model",
  "created": 1770085800,
  "owned_by": "anthropic",

  "display_name": "Claude Opus 4.6",
  "version": "20260204",
  "status": "active",
  "deprecated": false,

  "type": "chat",
  "family": "claude",
  "languages": ["en", "zh", "ja"],
  "modalities": { "input": ["text", "image"], "output": ["text"] },

  "context_window": 200000,
  "max_input_tokens": 200000,
  "max_output_tokens": 128000,

  "capabilities": {
    "tools": true,
    "tool_choice": true,
    "vision": true,
    "audio_input": false,
    "audio_output": false,
    "pdf_input": true,
    "video_input": false,
    "reasoning": true,
    "thinking": { "supported": true, "types": ["enabled", "adaptive"] },
    "structured_outputs": true,
    "json_mode": true,
    "function_calling": true,
    "parallel_tool_calls": true,
    "streaming": true,
    "system_messages": true,
    "image_generation": false,
    "embeddings": false,
    "fine_tuning": false,
    "web_search": true,
    "code_execution": true,
    "computer_use": false,
    "prompt_caching": true,
    "logprobs": true
  },

  "supported_generation_methods": ["generateContent", "streamGenerateContent"],
  "input_token_limit": 200000,
  "output_token_limit": 128000,
  "temperature_range": { "min": 0.0, "max": 1.0, "default": 1.0 },
  "top_p_range": { "min": 0.0, "max": 1.0, "default": 1.0 },
  "top_k_range": { "min": 0, "max": 200, "default": null },

  "pricing": {
    "currency": "USD",
    "input_token_usd_per_million": 15.0,
    "output_token_usd_per_million": 75.0,
    "cached_input_token_usd_per_million": 1.5
  },

  "metadata": {
    "pipeline_tag": "text-generation",
    "family_hf": "llama",
    "parameter_size": "8B",
    "quantization_level": "Q4_K_M",
    "context_length": 131072,
    "format": "gguf",
    "max_model_len": 131072,
    "parent_model": "ft:gpt-4o-mini:acme:custom:abc",
    "base_model": "gpt-4o-mini"
  }
}
```

### 4.1 `Model` 字段表（字段级契约）

| 字段 | 类型 | 必填 | 兼容性来源 | 含义 |
|------|------|:---:|------|------|
| `id` | string | ✅ | OpenAI 必填 | 模型唯一 ID |
| `object` | string | ✅ | OpenAI 必填 | 固定 `"model"` |
| `created` | integer (Unix 秒) | ✅ | OpenAI 必填 | 模型注册时间 |
| `owned_by` | string | ✅ | OpenAI 必填 | 模型归属方：`openai` / `anthropic` / `google` / `meta` / `mistralai` / `self` / `<partner_id>` 等 |
| `display_name` | string | 否 | Anthropic / Gemini | 人类可读的展示名（支持 i18n） |
| `version` | string | 否 | Anthropic | 版本号或日期快照（`20250514`、`claude-opus-4-6`） |
| `status` | string | 否 | 自定义扩展 | `active` / `deprecated` / `preview` / `beta` |
| `deprecated` | boolean | 否 | 自定义扩展 | 是否已弃用。`true` 时客户端应给出迁移提示 |
| `type` | string | 否 | 抽象 | 模型能力大类：`chat` / `embedding` / `image` / `audio` / `video` / `moderation` / `realtime` |
| `family` | string | 否 | Ollama `details.family` | 家族名（`claude` / `gpt` / `gemini` / `llama`） |
| `languages` | string[] | 否 | HF `language` | ISO 639-1 列表（`["en","zh"]`） |
| `modalities` | object | 否 | 抽象 | 输入/输出支持：`{ "input": ["text","image"], "output": ["text"] }` |
| `context_window` | integer | 否 | 抽象 | 总上下文窗口（`max_input_tokens + max_output_tokens` 的常见取值） |
| `max_input_tokens` | integer | 否 | 抽象 | 最大输入 token 数 |
| `max_output_tokens` | integer | 否 | Anthropic `max_tokens` | 最大输出 token 数 |
| `capabilities` | object | 否 | 抽象 | 见 §4.2 |
| `supported_generation_methods` | string[] | 否 | Gemini | `generateContent` / `streamGenerateContent` / `predict` / `embedContent` |
| `input_token_limit` | integer | 否 | Gemini | 同 `max_input_tokens`，保留以兼容 Gemini 客户端 |
| `output_token_limit` | integer | 否 | Gemini | 同 `max_output_tokens` |
| `temperature_range` | object | 否 | Gemini | 温度参数范围 `{ min, max, default }` |
| `top_p_range` | object | 否 | Gemini | top_p 范围 |
| `top_k_range` | object | 否 | Gemini | top_k 范围 |
| `pricing` | object | 否 | 自定义扩展 | 见 §4.3 |
| `metadata` | object | 否 | HF / Ollama | 厂商无关的元数据，见 §4.4 |
| `permissions` | object[] | 否 | OpenAI | OpenAI 兼容权限声明；多数实现可省略 |
| `root` | string | 否 | vLLM | 基础模型 ID（合并/微调场景） |
| `parent` | string | 否 | vLLM | 父模型 ID（LoRA / adapter 场景） |

> **重要**：`max_input_tokens` / `input_token_limit` / `context_window` 在不同厂商命名不同，按"互不冲突、并存可读"原则同时存在。客户端优先用 `max_input_tokens` 与 `max_output_tokens`。

### 4.2 `capabilities` 字段表

所有 `capabilities` 子字段默认 `false`。客户端应做"未声明即不支持"判断。

| 字段 | 类型 | 含义 |
|------|------|------|
| `tools` | bool | 是否支持工具/函数调用 |
| `tool_choice` | bool | 是否支持 `tool_choice=auto/none/required/<具体函数>` |
| `parallel_tool_calls` | bool | 是否支持单次响应中并行多个工具调用 |
| `vision` | bool | 是否接受图像输入（OpenAI vision、Claude vision、Gemini vision） |
| `audio_input` | bool | 是否接受音频输入（whisper、gpt-4o-audio） |
| `audio_output` | bool | 是否支持 TTS 语音输出 |
| `pdf_input` | bool | 是否接受 PDF 输入（Claude、Gemini） |
| `video_input` | bool | 是否接受视频输入（Gemini） |
| `image_generation` | bool | 是否为图像生成模型（dall-e、gpt-image） |
| `video_generation` | bool | 是否为视频生成模型（sora） |
| `embeddings` | bool | 是否为 embedding 模型（text-embedding-3） |
| `reasoning` | bool | 是否带内置推理（o-series、Gemini thinking） |
| `thinking` | object | Anthropic 风格细粒度：`{ supported, types: ["enabled","adaptive"] }` |
| `structured_outputs` | bool | 是否支持 `response_format=json_schema`（OpenAI） |
| `json_mode` | bool | 是否支持 `response_format={"type":"json_object"}`（旧 JSON 模式） |
| `function_calling` | bool | 是否支持 OpenAI 风格 function tool（旧字段，与 `tools` 同步） |
| `streaming` | bool | 是否支持 SSE 流式响应 |
| `system_messages` | bool | 是否接受 `role: system` 消息 |
| `developer_messages` | bool | 是否接受 `role: developer` 消息（OpenAI o-series+） |
| `web_search` | bool | 是否内置 web search 工具 |
| `code_execution` | bool | 是否内置 code interpreter 工具 |
| `computer_use` | bool | 是否支持 computer use 工具（Claude） |
| `file_search` | bool | 是否支持 file_search/向量检索工具 |
| `prompt_caching` | bool | 是否支持 prompt cache |
| `logprobs` | bool | 是否可返回 `logprobs` |
| `fine_tuning` | bool | 是否支持在该模型上做微调 |
| `moderation` | bool | 是否为内容审核模型（omni-moderation） |
| `realtime` | bool | 是否支持 Realtime API 双向流 |

### 4.3 `pricing` 字段表

| 字段 | 类型 | 必填 | 含义 |
|------|------|:---:|------|
| `currency` | string | 否 | ISO 4217 货币代码（默认 `USD`） |
| `input_token_usd_per_million` | number | 否 | 输入 token 单价（USD / 1M tokens） |
| `output_token_usd_per_million` | number | 否 | 输出 token 单价（USD / 1M tokens） |
| `cached_input_token_usd_per_million` | number | 否 | 命中缓存的输入 token 单价 |
| `cached_input_token_usd_per_million_5min` | number | 否 | 5 分钟缓存单价（Anthropic 区分） |
| `cached_input_token_usd_per_million_1hr` | number | 否 | 1 小时缓存单价 |
| `image_usd_per_unit` | number | 否 | 图像生成单价（per image），维度如 `1024x1024` 见 `metadata` |
| `audio_usd_per_minute` | number | 否 | 音频处理单价 |
| `video_usd_per_second` | number | 否 | 视频生成单价 |
| `request_usd_per_call` | number | 否 | 某些模型按请求计费（如 embeddings、moderation） |
| `tier` | string | 否 | 处理通道分层（`standard` / `batch` / `flex` / `priority`），与按 **上下文阶梯** 的分段正交。阶梯分段见 §4.5 |

> 单价为 `null` 或字段缺省 = 不提供，客户端按"未知"处理，**不要**回退到 0。
> 按上下文长度分级的模型，使用 §4.5 的 `segments` 字段承载各档价格。

### 4.4 `metadata` 字段表（厂商扩展区）

`metadata` 是开放扩展容器，键名建议遵循以下约定（不强制但推荐）：

| 字段 | 含义 | 兼容来源 |
|------|------|------|
| `pipeline_tag` | 任务类型（`text-generation`、`text-to-image`、`automatic-speech-recognition`） | HF `pipeline_tag` |
| `family_hf` | Hugging Face 家族（`llama`、`qwen`） | HF |
| `parameter_size` | 参数规模（`7B`、`8B`、`70B`） | Ollama |
| `quantization_level` | 量化等级（`Q4_K_M`、`FP16`） | Ollama |
| `format` | 权重格式（`gguf`、`safetensors`） | Ollama |
| `context_length` | 训练/支持的总上下文长度 | Ollama `model_info.<family>.context_length` |
| `max_model_len` | vLLM 等运行时视角的上下文上限 | vLLM |
| `parent_model` | 父模型（微调来源） | OpenAI `ft:` ID 模式 |
| `base_model` | 基础模型（合并/微调的根） | HF `base_model` |
| `deprecation_date` | 计划下线日期（ISO 8601） | 自定义 |
| `replacement_model` | 推荐替代模型 ID | 自定义 |
| `provider` | 真实上游（`openai` / `anthropic` / `azure` / `bedrock`） | 自定义 |
| `region` | 部署区域 | 自定义 |
| `tier` | 部署层级（`prod` / `staging` / `dev`） | 自定义 |
| `tags` | 任意标签数组 | HF `tags` |

`metadata` 内的**自定义字段**必须以**小写 snake_case** 命名，禁止使用驼峰或全大写；预置字段见上表。

### 4.5 `pricing.segments` —— 上下文长度阶梯定价

> 主流厂商（OpenAI `<272K` 角标、Google Gemini `prompts <= 200k / > 200k` 同表分段、Anthropic 显式不收长上下文溢价）对"上下文阶梯"的披露是 **在模型行后挂阈值注释**，而非新增 `tier` 枚举。本协议沿用该做法：阶梯作为 `pricing.segments[]` 数组承载，**与 `tier`（处理通道）正交**。

#### 4.5.1 适用判定

- 全部窗口同价（如 Anthropic Claude 4.6+、多数开源兼容、o1、o3-mini）→ **不返回 `segments`**，只填 §4.3 平铺字段。
- 按上下文长度分段（如 OpenAI GPT-5.5 `<272K`、Gemini Pro `≤200k / >200k`、Gemini Flash Live）→ 返回 `segments`，每段覆盖一个上下文区间。
- 阶梯段数 **2..4** 为宜。超过 4 段建议在 UI/文档拆分而非扩展协议。

#### 4.5.2 字段表

| 字段 | 类型 | 必填 | 含义 |
|------|------|:---:|------|
| `pricing.segments` | `Segment[]` | 否 | 上下文长度阶梯数组，按 `context_threshold_tokens` **升序**排列 |
| `Segment.context_threshold_tokens` | integer | ✅ | 本段的**下界**（含），单位 token。第 0 段固定为 0 |
| `Segment.context_upper_bound_tokens` | integer | 否 | 本段的**上界**（不含）。缺省 = 无限（最后一档）。**禁止**设 `0` |
| `Segment.input_token_usd_per_million` | number | 否 | 该段输入 token 单价 |
| `Segment.output_token_usd_per_million` | number | 否 | 该段输出 token 单价 |
| `Segment.cached_input_token_usd_per_million` | number | 否 | 该段缓存命中单价 |
| `Segment.cached_input_token_usd_per_million_5min` | number | 否 | 5min 缓存单价（Anthropic 区分） |
| `Segment.cached_input_token_usd_per_million_1hr` | number | 否 | 1h 缓存单价 |
| `Segment.threshold_dimension` | string | 否 | 默认 `"context_window"`（按"输入+输出 token 总和"判定）。可选：`"input_tokens"`（仅按输入 token）、`"prompt_tokens"`、`"request_tokens"` |
| `Segment.note` | string | 否 | 厂商附加说明（"prompts > 200k" 等），i18n 友好 |

#### 4.5.3 示例

**OpenAI 风格（`gpt-5.5` 占位示例）**：

```json
"pricing": {
  "currency": "USD",
  "input_token_usd_per_million": 5.0,
  "output_token_usd_per_million": 30.0,
  "cached_input_token_usd_per_million": 0.50,
  "segments": [
    {
      "context_threshold_tokens": 0,
      "context_upper_bound_tokens": 272000,
      "input_token_usd_per_million": 5.0,
      "output_token_usd_per_million": 30.0,
      "cached_input_token_usd_per_million": 0.50,
      "note": "Standard rate for prompts < 272K tokens"
    },
    {
      "context_threshold_tokens": 272000,
      "input_token_usd_per_million": 10.0,
      "output_token_usd_per_million": 60.0,
      "cached_input_token_usd_per_million": 1.0,
      "note": "Standard rate for prompts >= 272K tokens"
    }
  ]
}
```

**Google Gemini 风格**（同结构，只把单位换成 cache storage）：

```json
"pricing": {
  "currency": "USD",
  "input_token_usd_per_million": 2.0,
  "output_token_usd_per_million": 12.0,
  "cached_input_token_usd_per_million": 0.20,
  "segments": [
    {
      "context_threshold_tokens": 0,
      "context_upper_bound_tokens": 200000,
      "input_token_usd_per_million": 2.0,
      "output_token_usd_per_million": 12.0,
      "cached_input_token_usd_per_million": 0.20,
      "note": "prompts <= 200k tokens"
    },
    {
      "context_threshold_tokens": 200000,
      "input_token_usd_per_million": 4.0,
      "output_token_usd_per_million": 18.0,
      "cached_input_token_usd_per_million": 0.40,
      "note": "prompts > 200k tokens"
    }
  ]
}
```

**全窗口同价（不返回 `segments`）**：

```json
"pricing": {
  "currency": "USD",
  "input_token_usd_per_million": 5.0,
  "output_token_usd_per_million": 25.0,
  "cached_input_token_usd_per_million": 0.5
}
```

#### 4.5.4 段内一致性约束（实现端必读）

1. **平铺字段 = 最低档快照**：当 `segments` 存在时，`pricing.input_token_usd_per_million` / `output_token_usd_per_million` / `cached_input_*_usd_per_million` 必须等于 `segments[0]` 的对应值（即 `context_threshold_tokens = 0` 那一档）。客户端对 `segments` 不感知时仍能拿到正确的基础价。
2. **升序不重叠**：`segments[i].context_threshold_tokens < segments[i+1].context_threshold_tokens`，且 `segments[i].context_upper_bound_tokens <= segments[i+1].context_threshold_tokens`。
3. **最后一档缺省上限 = 无限**：`segments[last].context_upper_bound_tokens` 缺省时表示无上限（等于该模型 `max_input_tokens + max_output_tokens` 的运行时上限）。
4. **缺失档位 = 不支持**：客户端按 `prompt_tokens` 落在 `segments[i].context_threshold_tokens` 起的最高档位计费；如 `prompt_tokens > 任意段下界但 < 某段下界`，回退到不超其上界的那一档。
5. **批/flex 不复制阶梯**：`tier=batch` / `flex` / `priority` 是**乘法**关系（OpenAI Batch `-50%`、Priority `~2.5x`），不进入 `segments`；同一模型在 `tier=standard` 下的 `segments` 作为基准。
6. **缓存单价的阶梯**：当模型对缓存读写也按上下文阶梯（如 Gemini `cached: $0.20 / $0.40`），必须在**每段**同时给出 `cached_input_*_usd_per_million`，不允许"全局缓存价 + 阶梯输入价"混搭。
7. **`note` 不可解析**：客户端必须**不**解析 `note` 内容做逻辑分支；它只供 UI 直接显示或 i18n 覆盖。

#### 4.5.5 与 `tier` 字段的边界

| 维度 | `tier` 字段 | `pricing.segments` |
|------|------------|--------------------|
| 正交于 | 模型本身 | `tier` 字段 |
| 取值数量 | 1（每个响应） | 1..N（每个 `pricing` 最多 4 段） |
| 判定依据 | 处理通道（同步/异步/优先级） | 上下文长度（token 计数） |
| 关系 | 整段 `pricing` 共享同一 `tier` | 同一 `tier` 下可有多段 |
| 缺失语义 | `tier` 缺省 = `standard` | `segments` 缺省 = 全窗口同价 |

例：同一模型 `tier=standard` 和 `tier=batch` 应**各自**返回独立 `pricing` 对象（由客户端在请求时按 `tier` 选择），不在 `segments` 内做乘法。

## 5. 单模型响应

```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "id": "claude-opus-4-6",
  "object": "model",
  "created": 1770085800,
  "owned_by": "anthropic",
  ...
}
```

`GET /v1/models/{model_id}` 返回结构等同于 `Model` 对象的全部字段。
找不到时返回 `404`：

```json
{
  "error": {
    "message": "Model 'foo-bar' not found",
    "type": "invalid_request_error",
    "param": "model_id",
    "code": "model_not_found"
  }
}
```

## 6. 厂商字段映射参考

实现端在把上游模型元数据映射为 `Model` 时，按下表统一字段命名：

| 概念 | OpenAI | Anthropic | Gemini | Ollama | vLLM | HF | 本协议 |
|------|--------|-----------|--------|--------|------|------|--------|
| 唯一 ID | `id` | `id` | `name` (去掉 `models/` 前缀) | `name` | `id` | `modelId` | `id` |
| 展示名 | — | `display_name` | `displayName` | `name` (同名) | — | — | `display_name` |
| 所有者 | `owned_by` | — | — | — | `owned_by` | `author` | `owned_by` |
| 创建时间 | `created` (Unix 秒) | `created_at` (RFC 3339) | `version` (字符串) | `modified_at` (RFC 3339) | `created` (Unix 秒) | `createdAt` | `created` (Unix 秒) |
| 输入 token 上限 | — | `max_input_tokens` | `inputTokenLimit` | `model_info.<family>.context_length` | `max_model_len` | `maxPositionEmbeddings` | `max_input_tokens` + `input_token_limit` |
| 输出 token 上限 | — | `max_tokens` | `outputTokenLimit` | — | `max_model_len - prompt` | — | `max_output_tokens` + `output_token_limit` |
| 多模态 | — | `capabilities.image_input/pdf_input/...` | `supportedGenerationMethods` 间接表达 | `capabilities` | — | `tags` | `modalities` + `capabilities` |
| 家族/版本 | — | `display_name` 内嵌 | `displayName` / `version` | `details.family` | — | `pipeline_tag` | `family` + `version` + `metadata.pipeline_tag` |
| 推理能力 | — | `capabilities.thinking` | — | — | — | `tags: reasoning` | `capabilities.reasoning` + `capabilities.thinking` |
| 函数调用 | — | `capabilities.function_calling` | `supportedGenerationMethods` 含 `generateContent` + tool 字段 | `capabilities: ["tools"]` | — | `tags: tool-use` | `capabilities.function_calling` + `capabilities.tools` |
| 价格 | — | `pricing` (in `capabilities` 或文档) | — | — | — | — | `pricing` |

## 7. 错误响应

通用错误体：

```json
{
  "error": {
    "message": "Human-readable description",
    "type": "invalid_request_error | authentication_error | permission_error | not_found_error | rate_limit_error | server_error",
    "param": "field name or null",
    "code": "stable_machine_readable_code"
  }
}
```

| HTTP | `type` | `code` 示例 | 场景 |
|------|--------|-----------|------|
| 400 | `invalid_request_error` | `invalid_param` | 查询参数非法 |
| 401 | `authentication_error` | `invalid_api_key` | 缺/错 token |
| 403 | `permission_error` | `insufficient_scope` | token 有效但无权访问该模型 |
| 404 | `not_found_error` | `model_not_found` | `model_id` 不存在 |
| 429 | `rate_limit_error` | `rate_limit_exceeded` | QPS/RPM 触发限流 |
| 500 | `server_error` | `internal_error` | 内部错误 |
| 502 | `upstream_error` | `upstream_unavailable` | 上游厂商服务异常 |
| 503 | `server_error` | `service_unavailable` | 本服务降级 |

`code` 字段对客户端是**机器可读契约**，不应随意变更；变更需要发版说明。

## 8. 反向兼容与扩展规则

1. **新增必填字段 = 破坏性变更**，必须大版本号变更。
2. **新增可选字段**：直接添加，老客户端忽略未知字段。`metadata` 是首选扩展区。
3. **删除字段**：先标记 `deprecated`，至少保留 6 个月再下线。
4. **不修改字段名/类型**：永远视为破坏性。
5. **值缺省/降级**：上游未提供时：
   - 必填字段：回退到合理默认（如 `owned_by` 默认为 `self`），不允许 `null`。
   - 可选字段：直接**不返回**该字段，不要写 `null`（除 `error.param` 等明确 nullable 字段）。
6. **字符串 ID 大小写**：存储原始大小写；查询路径不区分大小写但返回原始 ID。

## 9. 模型目录（Model Catalog）

`/v1/models` 端点返回的模型元数据由 `crates/store/src/model_catalog.rs` 中的模型目录模块提供。该模块从 [models.dev](https://models.dev/api.json) 拉取多厂商模型清单，经过规范化处理后作为 `/v1/models` 响应的数据源。

### 9.1 双层架构

模型目录采用两层设计，保证启动可用性与数据新鲜度：

- **嵌入式基线快照**：在编译期通过 `build.rs` 将 models.dev JSON 快照嵌入二进制（`OUT_DIR/models_dev_api.generated.json`），进程启动时立即可用，无需网络请求。同时嵌入一份精简摘要（`models_catalog.generated.json`，schema `tiygate.model_catalog.summary.v1`），用于 Admin 控制台快速展示 lab/provider 列表。
- **运行时刷新**：`ModelCatalogStore` 在启动后立即尝试从 `https://models.dev/api.json` 拉取最新数据，成功则原子替换当前快照；失败时保留嵌入式基线继续服务，并在后续周期重试。

读取侧使用 `arc_swap::ArcSwap` 实现无锁快照读取，刷新侧由 `tokio::sync::Mutex` 保护，确保启动预热、定时周期、手动 Admin 刷新三者不会并发重建。

### 9.2 刷新机制

| 参数 | 值 | 说明 |
|------|------|------|
| 默认数据源 | `https://models.dev/api.json` | 可通过 `new_with_source_url` 覆盖 |
| 默认刷新间隔 | 24 小时 | `DEFAULT_REFRESH_INTERVAL` |
| 启动行为 | 立即预热一次 | 失败仅 warn 日志，不阻塞启动 |
| 刷新失败 | 保留前一份快照 | 不降级为空列表 |
| 并发控制 | `async Mutex` | 同一时刻仅一个刷新任务在执行 |

`spawn_refresh` 返回 `ModelCatalogRefreshHandle`，调用 `stop()` 可优雅终止后台刷新任务。

### 9.3 数据模型

目录的核心数据结构：

| 结构体 | 说明 |
|------|------|
| `ModelCatalog` | 不可变快照，包含 `version`、`labs`（`BTreeMap<String, LabCatalog>`）、`models`（`BTreeMap<String, ModelMetadata>`） |
| `CatalogVersion` | 版本与来源信息：`source`（如 `embedded:models.dev/api.json` 或实际 URL）、`checksum`（原始 JSON 的 SHA-256）、`generated_at_unix`、`provider_count`、`model_count` |
| `LabCatalog` | 按规范化的 lab 分组的模型集合：`id`、`display_name`、`official_provider_aliases`、`canonical_models` |
| `ModelMetadata` | 单个模型的规范化元数据，见下表 |

`ModelMetadata` 字段及到 `/v1/models` 扩展字段的映射：

| `ModelMetadata` 字段 | 对应 `/v1/models` 扩展字段 | 说明 |
|------|------|------|
| `id` | `id` | 规范化后的模型 ID（优先使用 official provider 的原始 ID） |
| `lab_id` | `owned_by` | 规范化后的 lab ID（如 `anthropic`、`openai`、`zhipuai`） |
| `display_name` | `display_name` | 人类可读展示名 |
| `family` | `family` | 模型家族（`claude`、`gpt`、`glm` 等） |
| `context_window` | `context_window` | 总上下文窗口 |
| `max_input_tokens` | `max_input_tokens` + `input_token_limit` | 同时写入两个字段以兼容 Gemini 客��端 |
| `max_output_tokens` | `max_output_tokens` + `output_token_limit` | 同时写入两个字段以兼容 Gemini 客户端 |
| `capabilities` | `capabilities` | 能力位图对象，见 §9.4 |
| `modalities` | `modalities` | 输入/输出模态（`{ "input": [...], "output": [...] }`） |
| `pricing` | `pricing` | 定价信息，见 §9.5 |
| `metadata` | `metadata` | 附加元数据：`knowledge_cutoff`、`release_date`、`last_updated`、`open_weights` |

`to_model_extensions()` 方法将上述字段转换为 `/v1/models` 扩展区 JSON，遵循"未知即不返回"原则——空值字段不会被序列化为 `null`，而是直接省略。`status` 固定写入 `"active"`。

### 9.4 能力位图（capabilities）

`capabilities_from_model()` 将 models.dev 的原始布尔标志映射为 §4.2 中定义的 `capabilities` 对象：

| models.dev 原始字段 | 映射到的 capabilities 字段 |
|------|------|
| `tool_call` | `tools`、`function_calling`、`tool_choice` |
| `reasoning` | `reasoning` |
| `structured_output` | `structured_outputs`、`json_mode` |
| `temperature` | `temperature` |
| `attachment` | `file_search` |
| `modalities.input` 含 `image` | `vision` |
| `modalities.input` 含 `audio` | `audio_input` |
| `modalities.input` 含 `video` | `video_input` |
| `modalities.output` 含 `audio` | `audio_output` |
| `modalities.output` 含 `image` | `image_generation` |
| `modalities.output` 含 `video` | `video_generation` |
| `modalities.output` 含 `embedding` | `embeddings` |
| （固定） | `streaming: true`、`system_messages: true` |

未在 models.dev 中出现的 capabilities 字段（如 `pdf_input`、`parallel_tool_calls`、`thinking`、`web_search`、`computer_use`、`prompt_caching`、`logprobs` 等）不会被 catalog 填充，由其他数据源或运行时推断补充。

### 9.5 定价来源优先级

当同一模型在多个 provider 侧出现时，定价信息按以下优先级选取：

1. **Official**（`PricingSourceKind::Official`）：模型所属 lab 的官方 provider（如 `anthropic`、`openai`、`zhipuai`、`tencent-tokenhub` 等）。
2. **OpenRouter fallback**（`PricingSourceKind::OpenRouterFallback`）：官方无定价时，使用 OpenRouter 的定价。
3. **Aggregator fallback**（`PricingSourceKind::AggregatorFallback`）：以上均无时，使用任意带 cost 数据的聚合器定价。

`ModelPricing` 结构体映射到 §4.3 的 `pricing` 字段，额外包含 `source_provider`（定价来源 provider ID）和 `source_kind`（定价来源类型枚举，序列化为 `snake_case`）。`cached_write_token_usd_per_million` 对应 models.dev 的 `cost.cache_write`。

> **注意**：当前 catalog 的定价模型为平铺单价，不支持 §4.5 的 `segments` 阶梯定价。阶梯定价由其他数据源或手动配置补充。

### 9.6 模型 ID 匹配

`get_model(id)` 按 4 级策略依次匹配，命中即返回：

1. **精确匹配**：对查询 ID 做 `canonical_model_id_str` 规范化后，在 `models` map 中精确查找（大小写不敏感回退）。
2. **去前缀匹配**：若查询 ID 含 `/`（如 `openai/gpt-image-2`），取最后一段在 map 中查找。
3. **指纹匹配**：对查询 ID 和候选 ID 计算 `model_fingerprint`，做模糊匹配。

`model_fingerprint` 的规范化规则：

- 取最后一段路径（`a/b/c` → `c`）
- 去掉 `:tag` 后缀（`kimi-k2:thinking` → `kimi-k2`）
- 去掉装饰性后缀（`-free`、`-latest`、`-tee`、`-fp8`、`-6bit` 等）。注意：`-turbo`、`-fast`、`-thinking`、`-preview`、`-highspeed`、`-her`、`-lightning`、`-cheaper` 等属于正常模型命名，**不**会被剥离
- 转小写
- `.` 与 `-` 互换（`glm-5.2` ↔ `glm-5-2`）
- 折叠字母与数字之间的 `-`（`kimi-k-2-6` → `kimi-k26`，`kimi-k2.6` → `kimi-k26`）

这保证了 `minimax-m3` ↔ `MiniMax-M3`、`glm-5-2` ↔ `glm-5.2`、`deepseek-v4-flash-free` ↔ `deepseek-v4-flash` 等变体能命中同一模型。

### 9.7 Lab / Provider 规范化

`normalized_lab_id()` 将 models.dev 的 provider 别名归一为标准 lab ID：

| models.dev provider ID | 规范化 lab ID |
|------|------|
| `zhipuai`、`zai`、`zhipu`、`zhipuai-coding-plan` | `zhipuai` |
| `minimax`、`minimax-cn`、`minimax-coding-plan` | `minimax` |
| `tencent-tokenhub`、`tencent-coding-plan`、`tencent` | `tencent` |
| `google-vertex` | `google` |
| `google-vertex-anthropic` | `anthropic` |
| `*-coding-plan`（其他） | 去掉 `-coding-plan` 后缀 |

`is_official_provider_alias()` 判断一个 provider 是否为官方直连源（而非聚合器），当前包括 `anthropic`、`deepseek`、`google`、`google-vertex`、`minimax`、`minimax-cn`、`moonshotai`、`openai`、`tencent-tokenhub`、`xai`、`zhipuai`。

当 models.dev 中未收录某 provider 但模型 ID 遵循可识别的命名前缀时（如 `doubao-` → `bytedance`、`glm-` → `zhipuai`、`kimi-` → `moonshotai`），`infer_lab_from_model_id()` 会从模型 ID 前缀推断 lab 归属。

### 9.8 构建流程

`build_catalog()` 将 models.dev 原始 JSON 转换为 `ModelCatalog` 的步骤：

1. 解析 JSON 根对象为 `SourceProvider` 集合。
2. 对每个 provider 的每个模型，计算 `canonical_model_id` 和 `model_fingerprint`，以 fingerprint 为分组键聚合到 `candidates` map 中（使跨 provider 的同名模型变体合并为同一候选组）。
3. 对每个候选组，调用 `build_model_metadata()` 选择最佳元数据候选和最佳定价候选：
   - 元数据候选优先级：official（score 3）> 非 official 非 openrouter 聚合器（score 2）> openrouter（score 1）；同级内按 `metadata_score`（name/family/limit/cost/modalities 各 +1）排序。
   - 定价候选优先级：official > openrouter > any-with-cost。
   - 模型 ID 优先使用 official provider 的原始 ID（保留人类可读形式如 `claude-sonnet-4-6`，而非指纹 `claude-sonnet-46`）。
4. 按 lab 分组模型，构建 `LabCatalog`。
5. 计算原始 JSON 的 SHA-256 作为 `checksum`，生成 `CatalogVersion`。

### 9.9 错误处理

`CatalogError` 枚举覆盖三种失败场景：

| 变体 | 触发条件 | 影响 |
|------|------|------|
| `Json` | models.dev JSON 解析失败 | 嵌入式基线加载失败会 panic（编译期数据损坏）；运行时刷新失败保留旧快照 |
| `Http` | HTTP 请求失败或非 2xx 状态码 | 保留旧快照，24h 后重试 |
| `InvalidRoot` | JSON 根不是对象 | 同上 |

## 10. 不在本协议范围内的事项

- 鉴权机制（OAuth/JWT/Service Key 等）的具体格式：本协议只要求 `Authorization: Bearer <token>`。
- 配额与计费策略：归属到 `quota.md` / `admin-api.md`。
- 上游 `chat/completions`、`messages`、`responses`、`embeddings` 等推理端点的字段兼容：归属到 `protocol-capability-matrix.md`。
- 模型注册/编辑/删除：管理面另行设计，不在 `GET /v1/models` 路径上。
- 模型权重下载、文件下载：非本协议职责。
