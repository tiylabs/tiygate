# TiyGate WebUI UI/UX Design System

> 状态：v0.1 · 适用于 `webui/` React + TypeScript + Vite 控制台
> 关联文档：[`docs/webui-requirements.md`](webui-requirements.md)、[`docs/admin-api.md`](admin-api.md)

## 1. 设计定位

TiyGate WebUI 是面向运维、开发者与网关管理员的 **AI Gateway 控制台**。界面应优先服务高频监控、配置管理、故障定位与安全操作，不追求营销化装饰。

设计关键词：**专业、冷静、数据密集、可扫描、低干扰、操作可恢复**。

推荐模式：**Data-Dense Operations Dashboard**。

- 页面以侧边导航 + 主内容区为核心骨架。
- 视觉以中性色、蓝色主色和明确状态色为主。
- 信息层级通过字号、字重、间距、边框和表格密度表达，避免过度卡片化。
- 每个页面必须覆盖 loading、empty、error、success 四类反馈。
- 所有危险操作必须二次确认，优先提供可撤销或明确后果说明。

## 2. 设计原则

1. **可读性优先**：表格、日志、JSON、密钥脱敏信息必须清晰可扫描，默认不使用低对比灰字承载关键数据。
2. **操作安全优先**：删除 Provider、Route、API Key、重置密钥、OAuth 刷新等操作必须有明确确认与结果反馈。
3. **状态透明**：请求失败、token 失效、后端未配置、日志截断、响应脱敏、熔断不可用等状态必须显式展示。
4. **同源控制台感**：产品感应接近云控制台和开发者工具，而不是 ToC 应用或营销站。
5. **无隐藏交互**：核心操作不可只依赖 hover；图标按钮必须有文本、tooltip 或 `aria-label`。
6. **Token 驱动主题**：颜色、圆角、阴影、间距、动效均通过语义 token 管理，组件中避免散落 raw hex。
7. **Radix 负责行为，项目负责外观**：弹窗、菜单、tabs、tooltip、popover、select 等复杂交互优先用 Radix UI primitives 保证可访问性。

## 3. 色彩系统

### 3.1 语义颜色

| Token | Light | Dark | 用途 |
| --- | --- | --- | --- |
| `--bg` | `#F8FAFC` | `#020617` | 页面背景 |
| `--surface` | `#FFFFFF` | `#0F172A` | 卡片、表格、弹窗 |
| `--surface-muted` | `#F1F5F9` | `#1E293B` | 次级区域、hover 背景 |
| `--border` | `#E2E8F0` | `#334155` | 边框、分割线 |
| `--text` | `#0F172A` | `#F8FAFC` | 主文本 |
| `--text-muted` | `#475569` | `#CBD5E1` | 次级文本 |
| `--text-subtle` | `#64748B` | `#94A3B8` | 说明文本 |
| `--primary` | `#2563EB` | `#60A5FA` | 主操作、当前导航、链接 |
| `--primary-strong` | `#1D4ED8` | `#93C5FD` | hover / active |
| `--accent` | `#F97316` | `#FB923C` | 需要突出但非默认主操作的 CTA |
| `--success` | `#16A34A` | `#4ADE80` | healthy、enabled、ok |
| `--warning` | `#D97706` | `#FBBF24` | pending、quota approaching、degraded |
| `--danger` | `#DC2626` | `#F87171` | error、delete、auth failure |
| `--info` | `#0284C7` | `#38BDF8` | informational、help |

### 3.2 使用规则

- 主色蓝用于默认主按钮、链接、选中导航、聚焦状态，不用于错误或成功状态。
- 橙色只用于次要强调，例如“复制一次性 secret”“查看重放”等需要注意但不危险的操作。
- 状态色必须搭配文本或图标，不得只靠颜色传达状态。
- 表格 hover 使用 `surface-muted`，选中行使用浅蓝底 + 左侧 2px 指示线。
- 深色模式不能简单反相，必须独立校验文字对比度。

## 4. 字体与排版

### 4.1 字体栈

默认优先使用系统字体，避免额外网络字体影响嵌入式控制台加载速度。

```css
--font-sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
--font-mono: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
```

如未来允许引入 Google Fonts，可选 `Fira Sans` + `Fira Code`，但必须评估离线部署和构建产物体积。

### 4.2 字号层级

| Token | Size / Line Height | 用途 |
| --- | --- | --- |
| `text-xs` | 12 / 16 | Badge、辅助说明、表格次要元信息 |
| `text-sm` | 14 / 20 | 表格、表单、按钮、导航 |
| `text-base` | 16 / 24 | 正文、弹窗内容、重要说明 |
| `text-lg` | 18 / 28 | 页面副标题、登录标题 |
| `text-xl` | 20 / 28 | 页面标题 |
| `text-2xl` | 24 / 32 | Dashboard 核心指标 |

规则：

- 页面标题使用 20px / 600；卡片标题使用 14px / 600。
- 表格正文使用 14px，代码、ID、hash、token 前缀使用等宽字体。
- 数字指标使用 tabular numbers：`font-variant-numeric: tabular-nums;`。
- 长文本默认换行；只有 ID、hash、URL 可省略，并通过 tooltip 或详情面板查看完整值。

## 5. 间距、圆角、阴影与布局

### 5.1 间距

采用 4px 基础步进，页面布局以 8px 节奏组织。

| Token | Value | 用途 |
| --- | --- | --- |
| `space-1` | 4px | 图标与文字微间距 |
| `space-2` | 8px | 表单字段内部、按钮 gap |
| `space-3` | 12px | 紧凑卡片 padding、导航 item |
| `space-4` | 16px | 默认卡片内容 padding |
| `space-6` | 24px | 页面主 padding、区块间距 |
| `space-8` | 32px | 大区块间距 |

### 5.2 布局

- 桌面端默认：左侧固定侧边栏 `224px`，主内容区 `24px` padding。
- 小屏端应切换为顶部栏 + 抽屉式导航，避免 224px 侧边栏挤压内容。
- 主内容最大宽度不强制限制；配置表单可限制在 `720px`，日志/表格可全宽。
- 表格区域必须支持横向滚动，但页面整体不能在 375px 宽度下出现不可控横向溢出。
- 固定或 sticky 区域必须预留滚动内容底部空间。

### 5.3 圆角与阴影

| Token | Value | 用途 |
| --- | --- | --- |
| `radius-sm` | 6px | Badge、小按钮、输入框 |
| `radius-md` | 8px | 卡片、普通按钮、Select |
| `radius-lg` | 12px | Dialog、Popover、Toast |
| `shadow-sm` | `0 1px 2px rgb(15 23 42 / 0.06)` | 默认卡片 |
| `shadow-md` | `0 8px 24px rgb(15 23 42 / 0.12)` | Popover、Dropdown |
| `shadow-lg` | `0 20px 48px rgb(15 23 42 / 0.18)` | Dialog |

规则：卡片阴影要克制，主要靠边框建立层级；弹窗与浮层才使用明显阴影。

## 6. 组件规范

### 6.1 Button

变体：

- `primary`：页面主操作，例如 Create Provider、Save changes。
- `secondary`：普通操作，例如 Refresh、Cancel。
- `ghost`：低强调操作，例如表格行内查看。
- `danger`：破坏性操作，例如 Delete。
- `accent`：一次性重要操作，例如 Copy secret。

规则：

- 默认高度不低于 36px；触控目标不低于 44px，可通过外层 padding 扩大。
- disabled 必须同时有语义属性和视觉弱化。
- loading 状态应禁用按钮并显示 spinner 或 “Saving…” 文案。
- 每个页面只保留一个视觉最强主操作。

### 6.2 Form

- 表单字段必须有可见 label，不使用 placeholder 替代 label。
- 错误信息显示在字段下方，并说明原因与修复方式。
- JSON 输入字段必须提供格式校验、等宽字体和示例提示。
- 密钥字段永不回显原值；编辑时使用“留空表示不修改”。
- 提交失败保留用户输入，不清空表单。
- 复杂表单按基础信息、认证、元数据、启用状态分组。

### 6.3 Table

- 表头固定使用 12–14px 中等字重，正文 14px。
- 行高建议 44–52px，日志表可用紧凑模式 40px。
- ID、hash、provider、model 支持复制；复制后显示 toast。
- 空态必须说明没有数据的原因和下一步操作。
- 错误态必须提供重试按钮。
- 分页、过滤、排序状态应反映在 URL query，便于刷新和分享。

### 6.4 Card / Metric

- 卡片用于分组，不用于装饰性堆叠。
- Dashboard 指标卡必须包含标题、数值、时间范围或统计口径。
- 成本字段在后端未接入定价时不得显示 `0`，应显示“未配置定价”。
- 健康状态卡必须明确 healthy / degraded / unavailable。

### 6.5 Badge

推荐 tone：`success`、`warning`、`danger`、`info`、`neutral`。

- `enabled` / `healthy` / `ok` 使用 success。
- `disabled` / `cancelled` 使用 neutral。
- `rate_limited` / `degraded` / `truncated` 使用 warning。
- `error` / `auth failure` 使用 danger。
- Badge 文案必须可翻译，不直接展示后端枚举给最终用户；可在 tooltip 中展示原始枚举。

### 6.6 Dialog / Modal

所有弹窗优先使用 Radix `Dialog`。

必须满足：

- 包含 `Dialog.Title`，必要时包含 `Dialog.Description`。
- 打开后 focus 进入弹窗，关闭后 focus 回到触发器。
- Escape 可关闭非破坏性弹窗。
- 破坏性确认需要清晰说明对象名称、影响范围和不可恢复性。
- secret 一次性展示弹窗关闭后不可再次恢复，需提示用户立即复制。

### 6.7 Dropdown / Menu

优先使用 Radix `DropdownMenu`。

- 触发器使用 `asChild`，避免嵌套 button。
- 图标按钮必须有 `aria-label`。
- 破坏性菜单项与普通项用 separator 分隔。
- 菜单项必须支持键盘方向键、Enter、Escape。

### 6.8 Tooltip / Popover

- Tooltip 只放辅助解释，不放关键操作。
- 错误详情、字段帮助、截断说明可使用 Popover。
- Tooltip 延迟建议 200ms；内容必须可被键盘触发。

### 6.9 Toast / Alert

- 成功 toast 自动消失 3–5 秒。
- 错误 toast 不应过快消失，或提供错误区域持久展示。
- Toast 使用 `aria-live="polite"`；严重错误用页面内 Alert。
- 不在 toast 中展示完整 token、secret、Authorization header。

## 7. Radix UI 设计系统接入规范

当前 `webui/package.json` 尚未包含 Radix 依赖。后续重构复杂交互组件时，建议按需安装单个 primitive，而不是一次性引入整套库。

推荐 primitives：

| 组件 | Radix primitive | 使用场景 |
| --- | --- | --- |
| Dialog | `@radix-ui/react-dialog` | 创建/编辑、删除确认、一次性 secret 展示 |
| DropdownMenu | `@radix-ui/react-dropdown-menu` | 表格行操作、用户菜单 |
| Tooltip | `@radix-ui/react-tooltip` | ID 截断说明、按钮解释 |
| Popover | `@radix-ui/react-popover` | 过滤器、字段帮助、日期范围选择 |
| Tabs | `@radix-ui/react-tabs` | 详情页分区、Provider auth mode 配置 |
| Select | `@radix-ui/react-select` | auth mode、provider、status 选择 |
| Switch | `@radix-ui/react-switch` | enabled / disabled 开关 |

### 7.1 组件封装模式

- 所有 Radix primitive 必须在 `webui/src/components/ui/` 或现有 `components/ui.tsx` 的后续拆分文件中二次封装。
- 封装组件暴露项目自己的 variant、size、tone，不把 Radix 内部结构泄露到业务页面。
- 使用 `asChild` 支持 `NavLink`、`button`、自定义 Button，避免无语义 wrapper。
- 受控状态用于需要同步 URL、表单或外部状态的组件；否则优先非受控。

示例约定：

```tsx
<Dialog.Root open={open} onOpenChange={setOpen}>
  <Dialog.Trigger asChild>
    <Button variant="primary">Create provider</Button>
  </Dialog.Trigger>
  <Dialog.Portal>
    <Dialog.Overlay className="fixed inset-0 bg-black/50" />
    <Dialog.Content className="...tokenized classes...">
      <Dialog.Title>Create provider</Dialog.Title>
      <Dialog.Description>Configure an upstream model provider.</Dialog.Description>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
```

### 7.2 可访问性红线

- 不得移除 Radix 默认 focus management。
- 不得阻止 Escape、Tab、Arrow key 的默认可访问行为，除非有明确替代方案。
- Dialog 不允许缺少 title；视觉隐藏也必须保留 screen reader 可读标题。
- Dropdown item 不允许只用颜色区分危险操作。
- Select、Switch、Tabs 必须有可读 label 或 `aria-label`。

## 8. 页面级 UX 规范

### 8.1 登录页

- 保持单任务界面，只要求输入 admin token。
- 503 明确提示服务端未配置 `TIYGATE_ADMIN_TOKEN`，不要提示 token 错误。
- “记住本机”必须说明会存入本地浏览器。
- 登录页仍需保留语言切换。

### 8.2 Dashboard

- 首屏展示请求数、错误率、token 总量、熔断状态四类核心信息。
- 时间范围选择应明显，默认最近 24h。
- 图表加载失败时不要留下空坐标轴，应展示错误和重试。
- 数据为 0 时展示真实 0，不使用空态替代。

### 8.3 Provider / Route / API Key

- 列表页上方放筛选与创建按钮，表格行内放次要操作。
- 创建/编辑使用 Dialog 或独立详情页；字段较多时优先分组。
- 删除必须二次确认，确认文案包含对象名称。
- API Key secret 创建后只展示一次，并提供复制按钮。

### 8.4 OAuth

- OAuth 流程应设计为向导：选择 provider → 发起授权 → 新窗口完成 → 刷新状态。
- 明确提示 redirect URI、state 与浏览器可达性要求。
- OAuth 错误信封与主 Admin API 不同，错误文案必须兼容解析。

### 8.5 请求日志与重放

- 日志页以过滤器 + 高密度表格 + 详情抽屉为主。
- `truncated`、`[REDACTED]`、inline media 元信息必须以 Badge 明确标识。
- 请求/响应 JSON 使用等宽字体、可复制、可折叠。
- 重放内容是快照，不应暗示会重新向上游发起请求，除非后端提供真实重放端点。

### 8.6 审计日志

- 审计日志优先展示 actor、resource、action、timestamp、details。
- `details` JSON 默认折叠，避免页面噪音。
- 写操作需要强调不可篡改的审计属性，避免提供“编辑审计日志”等误导操作。

## 9. 动效与反馈

- 微交互动效持续时间 150–250ms。
- 弹窗进入可使用 fade + scale，退出比进入略快。
- 不动画 width、height、top、left；优先使用 opacity 和 transform。
- 尊重 `prefers-reduced-motion: reduce`，禁用非必要动画。
- 加载超过 300ms 显示 spinner 或 skeleton；超过 1s 的列表加载优先 skeleton。
- 点击、保存、复制、删除、刷新必须在 100ms 内有视觉反馈。

## 10. 响应式规范

| 断点 | 行为 |
| --- | --- |
| `375px` | 单列布局，侧边栏收起，表格横向滚动，主操作全宽可选 |
| `768px` | 两列卡片布局，过滤器可横向排列 |
| `1024px` | 固定侧边栏，Dashboard 多列卡片 |
| `1440px` | 表格与图表展示更多列，但不拉伸长文本 |

规则：

- 任何页面不得依赖桌面 hover 才能完成核心操作。
- 表单在小屏下单列；按钮组垂直堆叠或主按钮全宽。
- 表格在小屏下允许横向滚动，但首列关键字段应尽量可见。

## 11. 国际化规范

- 文案集中在 i18n 资源中，组件不得硬编码中英文混排文案。
- Badge、错误、空态、按钮、表单 label 都必须可翻译。
- 日期时间显示本地化，并标注 UTC / Local 模式。
- 数字使用 `Intl.NumberFormat`，时间使用 `Intl.DateTimeFormat`。
- 技术字段名可保留原始英文，例如 `provider_id`、`virtual_model`，但需要说明文案。

## 12. 安全与隐私 UX

- token、secret、Authorization header 不得进入 URL、日志、toast、错误详情或控制台。
- 密钥输入框默认 password 类型，并提供短暂显示/隐藏切换。
- 复制敏感值后提示用户妥善保存，不在页面长期保留明文。
- 所有脱敏值以 `[REDACTED]`、`[encrypted:…]` 或 Badge 明确标识。
- destructive action 与 logout 在视觉和空间上与普通导航/操作分离。

## 13. CSS Token 建议

建议在 `webui/src/index.css` 建立语义变量，再用 Tailwind v4 的 token 或工具类引用。

```css
:root {
  color-scheme: light;
  --bg: #f8fafc;
  --surface: #ffffff;
  --surface-muted: #f1f5f9;
  --border: #e2e8f0;
  --text: #0f172a;
  --text-muted: #475569;
  --text-subtle: #64748b;
  --primary: #2563eb;
  --primary-strong: #1d4ed8;
  --accent: #f97316;
  --success: #16a34a;
  --warning: #d97706;
  --danger: #dc2626;
  --info: #0284c7;
  --radius-sm: 6px;
  --radius-md: 8px;
  --radius-lg: 12px;
  --shadow-sm: 0 1px 2px rgb(15 23 42 / 0.06);
  --shadow-md: 0 8px 24px rgb(15 23 42 / 0.12);
  --shadow-lg: 0 20px 48px rgb(15 23 42 / 0.18);
}

[data-theme="dark"] {
  color-scheme: dark;
  --bg: #020617;
  --surface: #0f172a;
  --surface-muted: #1e293b;
  --border: #334155;
  --text: #f8fafc;
  --text-muted: #cbd5e1;
  --text-subtle: #94a3b8;
  --primary: #60a5fa;
  --primary-strong: #93c5fd;
  --accent: #fb923c;
  --success: #4ade80;
  --warning: #fbbf24;
  --danger: #f87171;
  --info: #38bdf8;
}

@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 0.01ms !important;
    animation-iteration-count: 1 !important;
    scroll-behavior: auto !important;
    transition-duration: 0.01ms !important;
  }
}
```

## 14. 迁移建议

1. 将现有 `components/ui.tsx` 拆分为 Button、Field、Card、Badge、Table、Dialog 等独立文件。
2. 先引入 CSS 语义 token，逐步替换组件中的 raw `slate-*`、`red-*` 组合。
3. 将自实现 `Modal` 替换为 Radix Dialog，补齐 focus trap、focus restore、aria title/description。
4. 为表格行操作引入 Radix DropdownMenu，为说明文字引入 Tooltip。
5. 对登录页、Provider 表单、API Key secret 弹窗做第一批视觉与安全 UX 重构。
6. 再处理 Dashboard 图表、请求日志详情抽屉和响应式导航。

## 15. 验收清单

- [ ] 所有页面在 375px、768px、1024px、1440px 下可用。
- [ ] 所有交互元素有可见 focus ring，并可通过键盘操作。
- [ ] 普通文本对比度不低于 4.5:1，UI 图形对比度不低于 3:1。
- [ ] 所有图标来自同一 SVG 图标体系，禁止使用 emoji 作为结构性图标。
- [ ] 所有表单字段有 label、错误信息和提交中状态。
- [ ] 所有 destructive action 有二次确认。
- [ ] 所有 loading、empty、error、success 状态都有明确 UI。
- [ ] token、secret、header 不出现在 URL、日志、toast、console。
- [ ] Dialog、Dropdown、Tooltip、Select 等复杂交互使用 Radix 或达到同等可访问性标准。
- [ ] `prefers-reduced-motion` 下无非必要动画。
- [ ] 中英双语文案完整，缺失时有可接受 fallback。
