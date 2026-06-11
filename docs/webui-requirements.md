# TiyGate WebUI 需求文档

> 状态：草案 v0.1 · 对应阶段四「产品化」可选交付物 `webui/`
> 目标读者：前端实施者、控制面后端维护者
> 关联文档：[`docs/admin-api.md`](admin-api.md)、[`docs/log-schema.md`](log-schema.md)、[`docs/quota.md`](quota.md)、[`docs/ai-gateway-architecture-design.md`](ai-gateway-architecture-design.md)

## 1. 背景与定位

TiyGate 的阶段一至四已完成实施。阶段四交付了完整的控制面：`tiygate-admin` 暴露 `/admin/v1/*` REST API（Provider / Route / API Key 的 CRUD、OAuth 交互、统计聚合、请求下钻与重放、审计日志、熔断状态），日志子系统按 `docs/log-schema.md` 落 OLTP 分区表，配额按 `docs/quota.md` 在热路径按 API Key 阻断。

阶段四中 WebUI 被列为**可选交付物**，至今未实施。本文档为 WebUI 的开发提供完整需求指引。WebUI 是控制面的可视化操作面，**不引入任何新业务逻辑**，所有数据与操作都通过既有 `/admin/v1/*` API 完成。

## 2. 关键决策（已与干系人确认）

| 决策项 | 结论 |
| --- | --- |
| 集成/部署方式 | **嵌入单二进制托管**：由 `tiygate-server` 通过 **`rust-embed` 编译期嵌入**前端构建产物，单端口、单进程、真正单文件部署。 |
| 挂载前缀 | **`/admin/ui`**（Vite `base = /admin/ui/`），与控制面 `/admin/v1` 同层，语义清晰。 |
| 前端技术栈 | **React + TypeScript + Vite**。推荐搭配 TanStack Query（数据获取与缓存）+ Tailwind CSS + 一套成熟组件库（如 shadcn/ui）。 |
| 登录鉴权 | **沿用单一 `TIYGATE_ADMIN_TOKEN`**：登录页让运维粘贴 token，前端保存在内存 / `sessionStorage`，每次请求附 `Authorization: Bearer <token>`。不引入会话 / 密码 / RBAC（与设计文档「不做复杂 RBAC」一致）。 |
| 首版功能范围 | **管理 + 分析全覆盖**：覆盖现有 Admin API 全部端点。 |
| 配额端点 | **补后端端点**：新增独立的配额更新端点与单 key `GET`（含实时用量），WebUI 提供完整配额管理。 |
| 国际化 | **首版中英双语**，与主仓库 README 双语惯例一致。 |

## 3. 系统集成约束

这些约束直接来自已实施的后端代码（`crates/server/src/app.rs`、`crates/admin/src/`），实施时必须遵守。

- **同端口同进程**：数据面（`/v1/*`、`/healthz`、`/readyz`）与控制面（`/admin/v1/*`）共享同一个 axum `TcpListener`，监听地址由 `TIYGATE_LISTEN_ADDR`（默认 `0.0.0.0:3000`）决定。WebUI 静态资源将挂载在同一 listener 上。
- **控制面仅在 `all` / `admin` 模式挂载**：`App::router()` 仅当 `DeployMode` 为 `All` 或 `Admin` 且 `TIYGATE_DATABASE_URL` 已设置时才 merge admin router。`proxy` 模式不挂载控制面，**WebUI 也应只在 `all` / `admin` 模式可用**。
- **路由优先级**：`/v1/*` 与 `/admin/v1/*` 必须优先于 WebUI 的 SPA fallback。静态资源建议挂在独立前缀（如 `/ui` 或 `/admin/ui`），SPA fallback 仅在该前缀下生效，避免吞掉数据面与 API 路由。
- **鉴权中间件覆盖全部 admin 路由**：`build_router_with_auth` 对所有 `/admin/v1/*`（含 `/admin/v1/health`）套用 `require_admin_token`。token 缺失返回 `401`；环境变量 `TIYGATE_ADMIN_TOKEN` 未设置时返回 `503`。前端需区分这两种错误并给出对应提示。
- **无 CORS 需求**：因采用同源嵌入托管，无需配置 `CorsLayer`。若未来改为独立部署，需后端补 CORS。
- **统一错误信封**：所有 admin 错误形如 `{ "error": { "message", "type", "source" } }`（HTTP 状态码为权威来源）。OAuth 子路由的错误形如 `{ "error": "<message>" }`（较简，无 type/source），前端解析时需兼容两种形态。

## 4. 鉴权与会话需求

- **R-AUTH-1 登录页**：无 token 时展示登录页，提供 token 输入框。提交后调用一个轻量探活请求（如 `GET /admin/v1/audit?limit=1`）校验 token 有效性后再进入主界面。
- **R-AUTH-2 token 存储**：默认存 `sessionStorage`（关闭标签页即清除）；可提供「记住本机」选项存 `localStorage`。token 绝不写入 URL、日志或可被第三方读取的位置。
- **R-AUTH-3 全局 401 处理**：任意请求返回 `401` 时清除本地 token 并跳回登录页，提示「token 无效或已变更」。
- **R-AUTH-4 503 提示**：收到 `503` 且消息指向 admin token 未配置时，提示运维在服务端设置 `TIYGATE_ADMIN_TOKEN`，而非提示 token 输入错误。
- **R-AUTH-5 登出**：提供登出操作，清除本地 token 并返回登录页。

## 5. 功能需求（按页面/模块）

每个模块标注其依赖的 Admin API 端点。端点定义见 `docs/admin-api.md` 与 `crates/admin/src/handlers.rs`。

### 5.1 概览仪表盘（Dashboard）

- **R-DASH-1**：默认展示最近 24h（与后端默认时间窗一致）的关键指标，支持自定义 `since` / `until`（RFC-3339）时间范围选择。
- **R-DASH-2**：汇总卡片展示总请求数、错误数 / 错误率、总 token（prompt / completion / total）。数据来源 `GET /admin/v1/stats/by-model`、`by-provider`、`by-api-key`，对返回的 `buckets` 做客户端聚合。
- **R-DASH-3**：图表分别按 model、provider、api-key 维度展示请求量与 token 分布（柱状 / 饼图）。
- **R-DASH-4**：展示熔断状态面板，来源 `GET /admin/v1/health/circuit-breakers`，列出每个 target 的 `healthy` / `status`。当返回 `note: "health registry not available"` 时给出明确占位提示。
- **说明**：`cost` 字段在未接 `PriceProvider` 前恒为 `NULL`，UI 应隐藏或标注「未配置定价」，不展示误导性的 0。

### 5.2 Provider 管理

- **R-PROV-1 列表**：`GET /admin/v1/providers`，支持 `?enabled=true|false` 过滤。表格列出 id、name、vendor、api_base、auth_mode、enabled、updated_at。
- **R-PROV-2 创建 / 编辑**：表单字段对应 `ProviderRequest`：`id`（创建可留空，后端用 UUIDv7 生成）、`name`、`vendor`、`api_base`、`api_key`、`auth_mode`（`api_key` / `oauth` / `iam` / `none`）、`oauth_meta`（JSON 字符串）、`metadata`（JSON 对象）、`enabled`。`tenant_scope` 为预留字段，UI 可隐藏或只读。
- **R-PROV-3 密钥脱敏**：`GET` 响应中 `encrypted_api_key` / `encrypted_oauth_meta` 是 `[encrypted:…]` 形态的脱敏标记，**永不回传明文**。编辑时密钥输入框应为「留空表示不修改」语义，不回显原值。
- **R-PROV-4 删除**：`DELETE /admin/v1/providers/:id`，二次确认。
- **R-PROV-5 OAuth 引导**（仅 `auth_mode=oauth` 的 provider）：见 5.5。

### 5.3 Route 管理

- **R-ROUTE-1 列表**：`GET /admin/v1/routes`，展示 `virtual_model`、targets 数量、enabled。
- **R-ROUTE-2 创建 / 编辑**：表单对应 `RouteRequest`：`virtual_model`、`targets[]`（每项含 `provider_id`、`model_id`、`weight`、`account_label`）、`enabled`、`tenant_scope`（预留，可隐藏）。`provider_id` 应从 Provider 列表中下拉选择以减少输入错误。
- **R-ROUTE-3 权重提示**：当路由策略为 `weighted` 时，UI 可展示各 target 的相对权重占比（仅前端可视化，不改变后端语义）。
- **R-ROUTE-4 删除**：`DELETE /admin/v1/routes/:id`，二次确认。

### 5.4 API Key 管理

- **R-KEY-1 列表**：`GET /admin/v1/api-keys`，展示 id、name、`key_hash`、status、quota、tenant_id、created_at。**明文 secret 永不返回**。
- **R-KEY-2 创建**：`POST /admin/v1/api-keys`，字段 `name`、`secret`（可留空由后端生成 `tg-<hex>`）、`quota`、`tenant_id`（预留）。**创建响应中的明文 `secret` 仅返回一次**，UI 必须用醒目的一次性弹窗展示并提供「复制」，关闭后不可再获取。
- **R-KEY-3 配额编辑**：通过新增的**配额更新端点**（后端补充，见 §7）提交 `quota` 对象，字段为 `requests_per_minute`、`requests_per_day`、`tokens_per_minute`、`tokens_per_day`（均可选，空对象表示无限制，见 `docs/quota.md`）。配额表单支持四个限额的独立设置与「无限制」清空。
  - **实时用量**：通过新增的单 key `GET /admin/v1/api-keys/:id` 读取 `QuotaCounter::current_usage` 暴露的当前用量，展示「已用 / 限额」进度。该端点为本期后端协同项（见 §7 第 6 条）。
- **R-KEY-4 禁用 / 删除**：禁用走 `PUT /admin/v1/api-keys/:id`（置 `disabled`），删除走 `DELETE /admin/v1/api-keys/:id`，删除即失效，需二次确认。注意禁用与配额更新需用不同端点（禁用沿用现有 `PUT`，配额更新走 §7 新增端点）。

### 5.5 OAuth 交互向导

- **R-OAUTH-1 发起**：对 `oauth` 类型 provider，调用 `POST /admin/v1/oauth/start`（body `{ provider_id }`），后端返回 `{ url, state }`。UI 引导用户在新窗口打开 `url` 完成授权。
- **R-OAUTH-2 回调**：授权完成后 provider 重定向到 `GET /admin/v1/oauth/callback?code=&state=`，后端完成 token 交换并持久化加密的 refresh-token 元信息。UI 需向运维说明：回调地址必须可被浏览器访问且与 provider 配置的 redirect URI 一致。
- **R-OAUTH-3 刷新**：提供「刷新 token」操作，调用 `POST /admin/v1/oauth/refresh`（body `{ provider_id }`），展示返回的 `expires_in_s`。
- **R-OAUTH-4 错误兼容**：OAuth 子路由错误信封为 `{ "error": "<message>" }`，与主 admin 信封不同，需单独解析。

### 5.6 请求日志下钻与重放

- **R-LOG-1 列表 / 筛选**：`GET /admin/v1/requests`，支持筛选参数 `since`、`until`、`model`、`provider`、`status`（`ok`/`error`/`cancelled`）、`error_class`（`transient`/`rate_limited`/`auth`/`bad_request`/`lossy`）、`min_latency_ms`、`max_latency_ms`，分页 `limit`/`offset`。响应含 `total` 用于分页。
- **R-LOG-2 列展示**：表格列建议含 `ts`、`virtual_model`、`resolved_provider`、`status`、`http_status`、`error_class`、`total_latency_ms`、`ttfb_ms`、`total_tokens`、`cache_hit`（字段定义见 `docs/log-schema.md`）。
- **R-LOG-3 详情 / 重放**：`GET /admin/v1/requests/:id/replay` 返回 `RawEnvelope` 快照。UI 以可读方式展示请求/响应体与脱敏后的 headers。
- **R-LOG-4 脱敏与截断提示**：`raw_envelope.headers` 已被后端脱敏（敏感头替换为 `[REDACTED]`）；超过 `TIYGATE_RAW_ENVELOPE_MAX_BYTES` 的 body 带 `truncated` 标记，inline 媒体默认仅存元信息。UI 须明确展示「已脱敏」「已截断」标识，不得暗示展示的是完整原文。

### 5.7 审计日志

- **R-AUDIT-1**：`GET /admin/v1/audit?limit=N`（默认 50，上限 500），按时间倒序展示对 providers / routes / api_keys 的写操作记录，含 `details`（提交的 JSON 载荷）。

## 6. 非功能需求

- **R-NFR-1 响应式**：适配桌面浏览器为主，主流分辨率下表格 / 图表正常可用。
- **R-NFR-2 错误反馈**：所有 API 失败以统一 toast / 横幅展示后端 `error.message`，区分 4xx（用户可纠正）与 5xx（服务端问题）。
- **R-NFR-3 加载与空态**：列表、图表、详情均需 loading、空数据、错误三态。
- **R-NFR-4 时区**：日志与统计时间戳为 UTC（RFC-3339），UI 默认按本地时区展示并标注，或提供 UTC/本地切换。
- **R-NFR-5 国际化**：首版提供**中英双语**（English / 简体中文），与主仓库 README 双语惯例一致。采用 i18n 框架（如 `react-i18next`），文案集中管理，提供语言切换并记忆用户选择。
- **R-NFR-6 构建产物体积**：作为嵌入资源，控制产物体积，路由级代码分割。
- **R-NFR-7 安全**：token 不落 URL / 不打印；避免在控制台输出敏感响应；对 `metadata` / `oauth_meta` 等 JSON 输入做前端格式校验。

## 7. 嵌入托管集成方案（后端侧）

供后端实施者参考的集成点，不属于前端工作但需协同：

1. **`crates/server/Cargo.toml`**：新增 `rust-embed` 依赖与 `webui` feature 门控（`webui = ["dep:rust-embed"]`），`webui` 是否进入 `default` 待团队定（建议进 default，与 `admin` 对齐）。无需为 `tower-http` 加 `fs` feature（采用编译期嵌入而非运行时目录）。
2. **编译期嵌入**：用 `#[derive(rust_embed::RustEmbed)] #[folder = "webui/dist"]` 将前端产物嵌入二进制；提供一个 axum handler 从嵌入资源按路径返回，未命中静态资源时回退 `index.html`（SPA fallback）。这样保证「单二进制」承诺，部署无需携带静态目录。
3. **`crates/server/src/app.rs` 的 `App::router()`**：在 merge admin router 之后、且仅在 `All`/`Admin` 模式下（`webui` feature 开启时），将上述 handler 挂载到前缀 **`/admin/ui`**（含 `/admin/ui/*path` SPA fallback），确保不与 `/v1/*`、`/admin/v1/*` 冲突。
4. **base path**：Vite 配置 `base: "/admin/ui/"`，保证打包后资源相对路径与挂载前缀一致。
5. **构建流程**：约定前端源码目录（如 `webui/`），产物输出到 `webui/dist`；CI/构建脚本在 `cargo build` 前先 `npm run build`（或文档说明手动构建步骤），`rust-embed` 在编译期读取 `webui/dist`。
6. **配额端点补充（后端开发项）**：
   - 新增单 key `GET /admin/v1/api-keys/:id`：返回 key 元信息 + 经 `QuotaCounter::current_usage` 得到的实时用量。
   - 新增配额更新端点（建议 `PATCH /admin/v1/api-keys/:id` 或专用 `PUT /admin/v1/api-keys/:id/quota`），避免与现有 `PUT`（禁用语义）冲突；接收 `{ "quota": {...} }` 并落 `api_keys.quota_json`。
   - 同步更新 `docs/admin-api.md` 与 `docs/quota.md`，消除现有文档与实现的不一致。
7. **健康/探活**：WebUI 探活复用 `/admin/v1/*` 受保护端点；若需无鉴权探活，需后端单独放行（当前 `/admin/v1/health` 也受 token 保护）。

## 8. 范围外（首版不做）

- 复杂 RBAC、多用户账号体系、密码登录（与设计文档一致）。
- 多租户隔离 UI（`tenant_id`/`tenant_scope` 为预留字段，初期留空）。
- 成本 / 计费展示的真实数据（未接 `PriceProvider` 前 `cost` 恒为空）。
- 独立部署 / CORS 模式（本期采用同源嵌入）。
- 实时流式日志推送、WebSocket 实时监控。
- 配置 epoch 回滚的 UI 操作（如后端未暴露对应 Admin API）。

## 9. 决策记录与残留风险

**已确认决策**（本次澄清）：
- 配额端点缺口 → 后端补独立配额更新端点 + 单 key `GET`（含实时用量），见 §7 第 6 条。
- 挂载前缀 → `/admin/ui`，Vite `base = /admin/ui/`。
- 嵌入方式 → `rust-embed` 编译期嵌入，真正单二进制。
- i18n → 首版中英双语。

**残留风险**：
- **前端构建耦合进 Rust 编译**：`rust-embed` 编译期读取 `webui/dist`，若构建顺序未先跑 `npm run build`，会嵌入空目录或旧产物。需在构建脚本 / CI 中固化「先前端后后端」顺序，并在 dist 缺失时给出清晰编译错误。
- **后端配额端点为新开发项**：WebUI 的配额编辑与实时用量依赖后端先落地新端点；若后端进度滞后，前端可先以 `/admin/v1/stats/by-api-key` 累计值作为用量的降级展示。
- **token 明文驻留浏览器**：单 token 鉴权下 admin token 等同超级权限，存于 `sessionStorage`/`localStorage` 有 XSS 泄露风险。需严格 CSP、避免引入不可信第三方脚本，并在文档提示运维风险。
- **i18n 双语维护成本**：双语文案需与功能同步维护，避免漏翻；建议以英文为基准 key，缺失时回退英文。
