# TiyGate 请求日志：c→g→p→g→c 全链路记录规范

请求日志详情视图记录单次请求在网关内的完整四段链路。本文定义每段
记录什么、在哪里捕获、如何脱敏与截断，确保字段齐全、客户端自定义
header 可见、且仅对敏感值脱敏。

## 四段链路

| 段 | 含义 | header 字段 | body 字段 | 数据来源 |
|----|------|-------------|-----------|----------|
| c→g | 客户端 → 网关（ingress 请求） | `redacted_headers_json` | `raw_envelope_json` | `RawEnvelope`（`request_logs`） |
| g→p | 网关 → 供应商（egress 请求） | `egress_headers_json` | `egress_body` | `ExchangeCapture`（`request_payloads`） |
| p→g | 供应商 → 网关（upstream 响应） | `upstream_resp_headers_json` | `upstream_resp_body` | `ExchangeCapture` |
| g→c | 网关 → 客户端（client 响应） | `client_resp_headers_json` | `client_resp_body` | `ExchangeCapture` |

数据由两条记录拼成：c→g 来自 `RawEnvelope`，其余三段来自
`ExchangeCapture`，两者经 telemetry bus 异步落库后由
`get_request_replay()` 用 `request_id` LEFT JOIN 拼回，前端
`RequestLogs.tsx` 分四个区块展示。

## 字段齐全约定

记录的目标是**字段完整**：每个实际收发的 header 都应出现，敏感值脱敏
为 `[REDACTED]`，但 header 名与非敏感字段必须保留。

- **c→g**：`build_redacted_envelope()`（`crates/server/src/ingress_phase4.rs`）
  遍历**全部**客户端请求 header（含 `x-request-id`、`x-debug-id` 等自定义
  header），经 `Redactor` 脱敏后写入 `redacted_headers_json`。自定义
  header 明文保留，便于定位问题。
- **g→p**：egress header 以 **reqwest 实际构建的请求为准**。在每个上游
  发送点，请求 builder 经 `inject_trace()` 注入 `traceparent`、
  `.json()`/`.body()` 写入 body 后，调用
  `finalize_egress()`（`ingress_phase4.rs`）执行 `builder.build()` 得到
  最终 `reqwest::Request`，并从 `req.headers()` 快照完整 header 集合，
  再用 `client.execute(req)` 发送。这样 `content-type`、`content-length`、
  `traceparent`、`authorization`/`x-api-key`（Anthropic 还有
  `anthropic-version`）等 reqwest 在 finalize 时补齐的 header 都会被记录。

  > 历史问题：旧实现对**手工构建的 `upstream_headers`** 做快照，而
  > `content-type`/`content-length` 是 reqwest 发送时才补上的、发生在快照
  > 之后，passthrough 路径起点更是空 `HeaderMap`，导致日志里只剩
  > `authorization` 与手动 push 的 `traceparent`。改为 build-then-capture
  > 后字段齐全。

## 脱敏策略

脱敏由 `crates/core/src/redaction.rs` 的 `Redactor` 完成（规则详见
[`redaction.md`](redaction.md)）：

- header 名命中精确名单（`authorization`/`x-api-key`/`cookie` 等）或子串
  名单（`token`/`secret`/`password`/`credential`）时，值替换为
  `[REDACTED]`，**header 名保留**。
- JSON body 命中已知凭证键（`api_key`/`token`/`client_secret` 等）时递归
  替换为 `[REDACTED]`，其余字段（如 `messages[].content`）verbatim 保留。

g→p/p→g/g→c 三段在落库前由 `OltpSink::capture_to_row()`
（`crates/store/src/log_sink/oltp.rs`）统一脱敏；c→g 段在 ingress 热路径
构建 `RawEnvelope` 时即脱敏。捕获到的明文完整 header 只在内存中短暂存在，
落库前一定经过脱敏。

## 截断与媒体剥离

- c→g body 受 `raw_envelope_max_bytes` 限制；当
  `raw_envelope_capture_media` 关闭（默认）时，内联 base64 媒体被替换为
  `{_media_meta: {...}}` 元数据后再截断。
- g→p/p→g/g→c body 受 `payload_max_bytes` 限制，超出则截断并置
  `*_truncated` 标志。
- 流式响应额外尝试把上游 SSE 合并为结构化 JSON 存入
  `sse_parsed_json`。

## 覆盖的上游路径

所有 5 个协议执行函数（chat completions、anthropic messages、embeddings、
responses、gemini）的 stream 与 non-stream 分支共 9 个发送点，统一通过
`finalize_egress()` + `client.execute()` 捕获 egress header，保证规范一致。
