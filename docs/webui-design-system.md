# TiyGate WebUI UI/UX Design System

> 状态：v0.2 「静谧精密 Quiet Precision」 · 适用于 `webui/` React 18 + TypeScript + Vite + Tailwind v4 控制台
> 关联文档：[`docs/webui-requirements.md`](webui-requirements.md)、[`docs/admin-api.md`](admin-api.md)
> 实现入口：`webui/src/index.css`（token）、`webui/src/components/ui/`（组件）、`webui/src/lib/theme.tsx`（主题切换，6 色相 × 明暗共 12 套，见 §3.6）

## 1. 设计定位与设计语言

TiyGate 是一款 Rust 编写的 AI Gateway，WebUI 面向运维、开发者与网关管理员，定位为 **数据密集型运维控制台（Data-Dense Operations Dashboard）**。界面服务于高频监控、配置管理、故障定位与安全操作，不做营销化装饰。

### 1.1 设计语言：静谧精密（Quiet Precision）

v0.2 在 v0.1「专业、冷静」的基础上，将视觉气质升级为 **优雅、细腻、克制的工程美学**，对标 Linear、Vercel Dashboard、Datadog 一类专业工具的质感。核心特征：

1. **墨色中性底（Ink Neutrals）**：文本与中性色从泛蓝 slate 收敛为更深、更稳的墨色系，信息密度高时依然干净。
2. **发丝级层级（Hairline Hierarchy）**：层级优先靠 1px hairline 边框与背景微差表达，阴影只在浮层出现，且使用多层柔和阴影。
3. **低饱和主色（Calm Indigo-Blue）**：主色蓝向靛蓝微偏、降低荧光感，配套 faint/soft/strong 完整梯度，用色彩浓度而非更多颜色表达层级。
4. **精密排版（Precision Typography）**：标题收紧字距、数字强制 tabular、标签用微放大字距的大写小字号，让表格与指标呈现仪表质感。
5. **有目的的动效（Purposeful Motion）**：统一 duration / easing token，所有动效解释因果（展开、进入、反馈），无装饰性动画。
6. **暗色为一等公民**：暗色不是反相，而是独立调校的近中性深色面板体系，长时间盯日志不刺眼。

### 1.2 反模式（必须避免）

- 华丽渐变、玻璃拟态、大面积彩色块等装饰性风格。
- 过度卡片化：用嵌套卡片和重阴影堆出来的"层级"。
- 荧光感状态色、纯色大底 Badge。
- emoji 作为结构性图标；图标混用多个体系。
- 依赖 hover 才能发现的核心操作。

## 2. 设计原则

1. **可读性优先**：表格、日志、JSON、密钥脱敏信息必须清晰可扫描；关键数据不得使用低对比灰字。
2. **操作安全优先**：删除 Provider / Route / API Key、重置密钥、OAuth 刷新等操作必须二次确认并给出结果反馈。
3. **状态透明**：请求失败、token 失效、后端未配置、日志截断、响应脱敏、熔断不可用必须显式展示。
4. **密度服务效率**：列表与日志默认紧凑，但留白节奏化；密度高 ≠ 拥挤。
5. **无隐藏交互**：核心操作不可只依赖 hover；图标按钮必须有 tooltip 或 `aria-label`；hover 才出现的快捷操作（如行内复制）必须有键盘可达的等价路径。
6. **Token 驱动主题**：颜色、圆角、阴影、间距、动效全部经语义 token 引用，组件内禁止 raw hex 与裸 `slate-*` 类。
7. **Radix 负责行为，项目负责外观**：Dialog、DropdownMenu、Select、Switch、Toast、Tooltip 已基于 Radix 封装于 `components/ui/`，业务页面只消费封装层。
8. **细节即专业**：对齐、字距、hairline、数字格式、复制反馈、骨架屏节奏——细腻感来自这些 1px 级决策的一致性。

## 3. 色彩系统

### 3.1 中性色（Ink Neutrals）

> 下表为 **蓝色主题（默认）** 的中性色基准值；其余主题在此基础上按各自主色相微染（见 §3.6）。

| Token | Light | Dark | 用途 |
| --- | --- | --- | --- |
| `--bg` | `#F7F8FA` | `#0A0C14` | 页面背景 |
| `--surface` | `#FFFFFF` | `#11141E` | 卡片、表格、弹窗 |
| `--surface-muted` | `#F1F3F6` | `#191D2A` | 次级区域、表头、hover 背景 |
| `--border` | `#E4E7EC` | `#262C3B` | 默认边框、分割线（hairline） |
| `--border-strong` | `#D0D5DD` | `#343C4E` | 输入框、可交互控件边框 |
| `--text` | `#101828` | `#EDEFF3` | 主文本 |
| `--text-muted` | `#475467` | `#A4ACBB` | 次级文本 |
| `--text-subtle` | `#667085` | `#7C8494` | 说明文本、占位符 |

### 3.2 主色与强调色

| Token | Light | Dark | 用途 |
| --- | --- | --- | --- |
| `--primary` | `#2E5BE6` | `#84A6FF` | 主操作、链接、选中导航、聚焦 |
| `--primary-strong` | `#2348C7` | `#A3BCFF` | hover / active |
| `--primary-soft` | `#E8EEFE` | `#1D2A4A` | 选中行底色、active nav 底色 |
| `--primary-faint` | `#F4F7FE` | `#161E33` | 极轻提示底、表格选中 hover |
| `--on-primary` | `#FFFFFF` | `#0B0D12` | 主色之上的文字 |
| `--accent` | `#B8540B` | `#F08C3D` | 一次性重要操作（复制 secret 等），非危险但需注意 |
| `--accent-strong` | `#9A460A` | `#F6A86A` | accent hover |

### 3.3 状态色（每色三档：default / strong / soft）

| Token | Light | Dark | 用途 |
| --- | --- | --- | --- |
| `--success` | `#0F8043` | `#45C77F` | healthy、enabled、ok |
| `--success-soft` | `#E6F6EC` | `#11291C` | success Badge / Alert 底 |
| `--warning` | `#B45309` | `#F0B254` | pending、degraded、quota approaching |
| `--warning-soft` | `#FBF1E0` | `#2E2410` | warning Badge / Alert 底 |
| `--danger` | `#D92D20` | `#F2756B` | error、delete、auth failure |
| `--danger-strong` | `#B42318` | `#F8958D` | danger hover |
| `--danger-soft` | `#FBEAE8` | `#321714` | danger Badge / Alert 底 |
| `--info` | `#0E7CB8` | `#4FBBF0` | informational、help |
| `--info-soft` | `#E3F3FC` | `#0E2433` | info Badge / Alert 底 |

### 3.4 使用规则

- **主色克制**：一屏内主色实底（primary 按钮、active 指示条）不超过 2~3 处；其余层级用 `primary-soft` / `primary-faint` 表达。
- **Badge 一律 soft 底 + 深色字**：如 `bg-success-soft text-success`，不用纯色大底；暗色下用对应 soft 暗色底。
- **状态色必须伴随文本或图标**，不得只靠颜色传达状态。
- **hover 用状态层而非换色**：列表行、菜单项、ghost 按钮 hover 统一用 `surface-muted` 或 `color-mix(in srgb, var(--text) 4%, transparent)`，保持色相稳定。
- **表格选中行**：`primary-faint` 底 + 左侧 2px `primary` 指示线。
- **聚焦环统一 token**：`--ring: color-mix(in srgb, var(--primary) 45%, transparent)`，所有可交互元素 `focus-visible` 使用 2px ring。
- **暗色独立校验**：暗色不是反相，所有文字/底色组合必须单独验证对比度（正文 ≥ 4.5:1，大字与图形 ≥ 3:1）。

### 3.5 数据可视化色板（预留）

引入图表库时使用以下 6 色分类色板，顺序固定、色盲安全（避免红绿直接相邻）：

```
chart-1: #2E5BE6（蓝 · 默认/主序列）   chart-2: #17914F（绿）
chart-3: #E56910（橙）                 chart-4: #7A5AF8（紫）
chart-5: #0E9BA4（青）                 chart-6: #C11574（玫红）
```

- 趋势 → 折线；对比 → 柱状；占比 ≤ 5 类 → 环形，超过用条形。
- 网格线用 `--border` 低对比；图例必须可见；图表失败显示错误 + 重试，不留空坐标轴。
- 错误率等语义指标固定用语义色（danger/warning/success），不占用分类色板。

### 3.6 多主题体系（6 色相 × 明暗）

控制台提供 **6 个色相主题 × 明暗两种模式 = 12 套主题**，由 `lib/theme.tsx` 的 `THEMES` 元数据驱动，经 `document.documentElement[data-theme]` 切换，并持久化于 `localStorage("tiygate.theme")`。所有主题共享同一套语义 token 名，仅改写 token 值，故业务组件无需任何改动即可适配全部主题。

| 名称（中 / EN） | Light `data-theme` | Dark `data-theme` | 主色相 | Light primary | Dark primary |
| --- | --- | --- | --- | --- | --- |
| 蓝色 / Blue | `light` | `dark` | 靛蓝 | `#2E5BE6` | `#84A6FF` |
| 琥珀 / Amber | `light-warm` | `dark-dim` | 琥珀橙 | `#B4530E` | `#F0915A` |
| 石板 / Slate | `light-slate` | `dark-oled` | 中性灰 | `#475569` | `#94A3B8` |
| 青柠 / Lime | `light-lime` | `dark-lime` | 黄绿 | `#4D7C0F` | `#A3E635` |
| 品红 / Fuchsia | `light-fuchsia` | `dark-fuchsia` | 品红 | `#C026D3` | `#E879F9` |
| 藕荷 / Mauve | `light-mauve` | `dark-mauve` | 柔紫 | `#8B5C9E` | `#C4A7E0` |

**命名规范**：

- 主题标签按 **主色相** 命名（蓝色 / 琥珀 / 石板 / 青柠 / 品红 / 藕荷），不再使用"默认 / 暖色 / 柔和 / 纯黑"这类与色相无关或不一致的词。
- 同一色相的 light 与 dark **共用一个 i18n 标签键**（`app.themeBlue`、`app.themeAmber`、`app.themeSlate`、`app.themeLime`、`app.themeFuchsia`、`app.themeMauve`），切换器内按"浅色 / 深色"分组展示，左右对称。
- `data-theme` 的 id 保留历史值（`light` / `dark` / `light-warm` / `dark-dim` / `light-slate` / `dark-oled`）以兼容已保存的 localStorage，新增 4 组用 `{mode}-{hue}` 规则（`light-lime` / `dark-lime` 等）。

**染色规则**：

- **背景跟随主色相微染**：每套主题的 `--bg` / `--surface` / `--surface-muted` / `--border` 都带各自主色相的极低饱和染色（如蓝色 dark 背景偏冷蓝 `#0A0C14`、琥珀 dark 偏暖 `#1B1812`、青柠 dark 偏绿 `#0C120A`），使整套界面色温统一，而非中性灰底配彩色控件。
- **石板（Slate）为中性主题**：主色本身即中性灰，故 light/dark 背景保持近中性；其 dark 变体（`dark-oled`）采用真黑 `#000000` 背景，专为 OLED 屏省电与极致对比设计，是唯一的纯黑特例。
- **辅助色（accent）取主色的邻近或互补色**：如青柠配蓝绿 teal、品红配蓝、藕荷配粉，用于一次性强调操作。
- **状态色（success / warning / danger / info）跨主题基本统一**，仅个别浅色主题按底色冷暖微调，保证语义色识别稳定。

**切换器（`ThemeSwitcher.tsx`）**：以 `Palette` 图标触发的 Radix DropdownMenu，按 light/dark 两行渲染色块；每个色块为左上=主色、右下=背景的对角分割 chip，当前主题右上角带 `Check` 角标。选择后用 `event.preventDefault()` 保持菜单打开，便于连续对比。

## 4. 字体与排版

### 4.1 字体栈

默认系统字体，保证嵌入式部署零网络字体开销：

```css
--font-sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, "PingFang SC", "Noto Sans SC", Helvetica, Arial, sans-serif;
--font-mono: ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
```

如未来追求更强一致性，可自托管 `Inter`（variable，开启 `cv05/cv11` 替代字形）+ `JetBrains Mono`，禁止运行时拉取 Google Fonts CDN；必须评估离线部署与产物体积。

### 4.2 字号角色（Type Roles）

| 角色 | 规格 | 用途 |
| --- | --- | --- |
| `display` | 28 / 36 · 600 · `-0.02em` | Dashboard 核心大数字（配 tabular） |
| `title-lg` | 20 / 28 · 600 · `-0.01em` | 页面标题 |
| `title` | 16 / 24 · 600 · `-0.01em` | 弹窗标题、区块标题 |
| `body` | 14 / 20 · 400 | 表格、表单、正文、导航 |
| `body-strong` | 14 / 20 · 500 | 卡片标题、表内重点字段 |
| `label` | 12 / 16 · 500 · `+0.04em` · uppercase | 表头、指标卡标题、分组标签 |
| `caption` | 12 / 16 · 400 | 辅助说明、时间戳、元信息 |
| `code` | 13 / 20 · mono | ID、hash、token 前缀、JSON、curl |

### 4.3 排版规则

- 所有数字指标、计数列强制 `font-variant-numeric: tabular-nums`，杜绝跳动。
- ID、hash、model 名、provider key 等技术字段一律等宽字体 + 可复制。
- `label` 角色的大写小字号是表头与指标卡的统一仪表语言，不用于正文。
- 长文本默认换行；仅 ID、hash、URL 可截断省略，且必须配 tooltip 或详情面板展示完整值。
- 中英文混排时英文/数字两侧不手动加空格，交由排版自然处理；文案全部走 i18n。

## 5. 空间、圆角、层级与阴影

### 5.1 间距

4px 基础步进，页面以 8px 节奏组织：

| Token | Value | 用途 |
| --- | --- | --- |
| `space-1` | 4px | 图标与文字微间距 |
| `space-2` | 8px | 按钮 gap、表单字段内部 |
| `space-3` | 12px | 紧凑卡片 padding、导航 item |
| `space-4` | 16px | 默认卡片 padding |
| `space-6` | 24px | 页面主 padding、区块间距 |
| `space-8` | 32px | 大区块间距 |

**节奏规则**：页面级 24 → 区块级 16 → 元素级 8 → 微间距 4。同层级间距必须一致，禁止 12/14/18 等随机值。

### 5.2 布局骨架

- 桌面端：左侧固定侧边栏 `224px` + 主内容区 `24px` padding；小屏切换顶部栏 + 抽屉导航。
- 页面头部统一模式：`title-lg` 标题 + caption 说明（可选）+ 右侧主操作按钮，底部 hairline 分隔。
- 配置表单最大宽 `720px`；日志/表格全宽；表格区域支持横向滚动，页面整体在 375px 不得横向溢出。
- sticky 表头、固定栏必须为滚动内容预留底部空间。

### 5.3 圆角

| Token | Value | 用途 |
| --- | --- | --- |
| `radius-xs` | 4px | Badge、行内 code 块 |
| `radius-sm` | 6px | 小按钮、输入框、菜单项 |
| `radius-md` | 8px | 卡片、按钮、Select、Popover |
| `radius-lg` | 12px | Dialog、Drawer、Toast |
| `radius-full` | 999px | 状态圆点、Switch、头像 |

### 5.4 层级模型（Elevation）

层级靠「背景差 + hairline」表达，阴影仅用于浮层，且为多层柔和阴影：

| 层 | 表达方式 | Token |
| --- | --- | --- |
| L0 页面 | `--bg` | — |
| L1 卡片/表格 | `--surface` + 1px `--border`，可选 `shadow-xs` | `--shadow-xs: 0 1px 2px rgb(16 24 40 / 0.05)` |
| L2 Dropdown/Popover/Tooltip | `--surface` + border + `shadow-md` | `--shadow-md: 0 4px 8px -2px rgb(16 24 40 / 0.10), 0 2px 4px -2px rgb(16 24 40 / 0.06)` |
| L3 Dialog/Drawer | `--surface` + border + `shadow-lg` | `--shadow-lg: 0 20px 24px -4px rgb(16 24 40 / 0.10), 0 8px 8px -4px rgb(16 24 40 / 0.04)` |

- 暗色模式阴影几乎不可见，L2/L3 依靠更亮的 `--surface-muted` 边框与背景差区分。
- 弹窗遮罩统一 `rgb(16 24 40 / 0.5)`（暗色 `rgb(0 0 0 / 0.6)`），保证前景隔离。
- z-index 阶梯固定：`0 / 10(sticky) / 20(侧边栏) / 40(dropdown) / 50(dialog) / 60(toast)`，不得插入随机值。

## 6. 动效系统

### 6.1 Motion Token

```css
--duration-fast: 120ms;   /* hover、按下、行高亮 */
--duration-base: 180ms;   /* 展开、切换、淡入 */
--duration-slow: 240ms;   /* Dialog、Drawer 进入 */
--ease-out: cubic-bezier(0.16, 1, 0.3, 1);   /* 进入 */
--ease-in: cubic-bezier(0.7, 0, 0.84, 0);     /* 退出（时长取进入的 ~70%） */
```

### 6.2 动效配方

| 场景 | 配方 |
| --- | --- |
| 按钮/行 hover | 背景色过渡 `--duration-fast` |
| 按钮按下 | `transform: scale(0.98)`，松开还原 |
| Dialog 进入 | overlay fade + content `opacity 0→1, scale 0.96→1, translateY 8px→0`，`--duration-slow --ease-out` |
| Drawer 进入 | `translateX(16px)→0 + fade`，退出更快 |
| Toast 进入 | `translateY(8px)→0 + fade` |
| 列表骨架 | shimmer 或 pulse，加载 >300ms 显示，>1s 优先 skeleton |
| 数字刷新 | 不滚动跳字，直接替换；可用 `--duration-fast` 淡入 |

### 6.3 红线

- 只动画 `opacity` 与 `transform`，不动画 width/height/top/left。
- 退出永远快于进入；动效必须可被打断，不阻塞输入。
- 严格尊重 `prefers-reduced-motion: reduce`（已在 `index.css` 实现全局降级）。
- 任何点击、保存、复制、删除、刷新必须 100ms 内有视觉反馈。

## 7. 组件规范

实现位置：`webui/src/components/ui/`，全部基于 Radix primitive 二次封装，业务页面不直接触碰 Radix 结构。

### 7.1 Button（`button.tsx`）

| 变体 | 外观 | 用途 |
| --- | --- | --- |
| `primary` | `primary` 实底 + `on-primary` 字 | 页面唯一最强操作 |
| `secondary` | `surface` 底 + `border-strong` 边 | Refresh、Cancel |
| `ghost` | 透明底，hover `surface-muted` | 表格行内、低强调 |
| `danger` | `danger` 实底（或 danger 描边弱化版） | Delete |
| `accent` | `accent` 实底 | Copy secret 等一次性操作 |

- 尺寸：`sm` 高 32px / `md` 高 36px；触控目标不低于 44px，可用外层 padding 扩大。
- 图标与文字间距 `space-2`，图标统一 16px / stroke 1.5（lucide-react）。
- loading 态：禁用 + spinner + 文案（"Saving…"）；disabled：`opacity-50` + 语义属性。
- 每个页面只保留一个 `primary`。

### 7.2 Form（`input.tsx`、`field.tsx`、`select.tsx`、`switch.tsx`）

- 字段必须有可见 label（`text-sm font-medium`），不以 placeholder 代替；必填以 `*`（danger 色）标记。
- 输入框：`border-strong` 边框、`radius-sm`；focus 切换为 `primary` 边框 + 2px `--ring`。
- 错误信息显示在字段下方，说明原因与修复方式；校验在 blur 时触发，不逐键打扰。
- JSON 字段：等宽字体 + 格式校验 + 示例提示。
- 密钥字段永不回显原值；编辑时"留空表示不修改"；默认 password 类型并提供显隐切换。
- 提交失败保留用户输入；复杂表单按"基础信息 / 认证 / 元数据 / 启用状态"分组，组间 `space-6`。

### 7.3 Table（`table.tsx`）

- 表头：`label` 角色（12px / 500 / uppercase / +0.04em）+ `surface-muted` 底 + 底部 hairline。
- 行高：默认 48px，日志紧凑模式 40px；行 hover `surface-muted`（`--duration-fast` 过渡）。
- 行分隔用 hairline `--border`，不用斑马纹。
- 数字列右对齐 + tabular；ID/hash/model 等宽字体 + 截断 + tooltip + 复制（复制后 toast 确认）。
- 空态说明原因与下一步操作；错误态必须有重试按钮；分页/过滤/排序状态反映在 URL query。
- 50+ 行的长列表考虑虚拟滚动。

### 7.4 Card / Metric（`card.tsx`)

- 卡片：`surface` + hairline + `radius-md` + `shadow-xs`，padding `space-4`；卡片用于分组，不嵌套堆叠。
- 指标卡结构：`label` 标题 → `display` 数值（tabular）→ caption 统计口径/时间范围；可选趋势 delta（▲▼ + success/danger 色 + 文字）。
- 成本字段后端未接入定价时显示"未配置定价"，不显示 `0`。
- 健康状态卡：状态圆点（`radius-full`，8px）+ 文字，明确 healthy / degraded / unavailable。

### 7.5 Badge（`badge.tsx`）

- tone：`success / warning / danger / info / neutral / primary`，统一 soft 底 + 同色系深字 + `radius-xs`，`text-xs font-medium`。
- 映射：enabled/healthy/ok → success；disabled/cancelled → neutral；rate_limited/degraded/truncated → warning；error/auth failure → danger。
- 文案可翻译，不直出后端枚举；原始枚举可放 tooltip。

### 7.6 Dialog / Drawer（`dialog.tsx`、`confirm-dialog.tsx`）

- 必含 `Dialog.Title`（视觉隐藏也要保留给读屏器）；focus 进入弹窗、关闭后回到触发器；Escape 关闭非破坏性弹窗。
- 破坏性确认：写明对象名称、影响范围、不可恢复性；确认按钮 danger。
- secret 一次性展示弹窗：accent 复制按钮 + 关闭前明确提示"关闭后不可再次查看"。
- 进入动效按 §6.2；遮罩按 §5.4。

### 7.7 Dropdown / Tooltip / Popover（`dropdown.tsx`、`tooltip.tsx`）

- 触发器使用 `asChild`；图标按钮必须 `aria-label`。
- 菜单 `radius-md` + `shadow-md`；菜单项 hover `surface-muted`；破坏性项 danger 色 + separator 分隔，且不只靠颜色（配图标/文字）。
- Tooltip 延迟 200ms，只放辅助解释；错误详情、字段帮助用 Popover。

### 7.8 Toast / Alert（`toast.tsx`、`feedback.tsx`）

- 成功 4s 自动消失；错误 8s 或持久展示；`aria-live="polite"`，不抢焦点。
- 严重错误用页面内 Alert（soft 底 + 左侧图标 + 标题 + 正文 + 操作）。
- toast 不展示完整 token、secret、Authorization header。

### 7.9 代码与 JSON 展示

- 统一容器：`surface-muted` 底 + hairline + `radius-sm` + `code` 角色字体，行内 padding `space-3`。
- JSON 支持折叠/展开与复制；`truncated`、`[REDACTED]` 以 Badge 标注于容器头部。
- 不引入重型语法高亮库前，至少保证 key 与 value 的字重区分。

## 8. 页面级 UX 规范

### 8.1 登录页

- 单任务界面：居中卡片（max-w 380px）、产品标识、admin token 输入、记住本机、语言切换。
- 503 明确提示服务端未配置 `TIYGATE_ADMIN_TOKEN`，不误导为 token 错误。
- "记住本机"说明会存入本地浏览器。

### 8.2 Dashboard

- 首屏四类核心信息：请求数、错误率、token 总量、熔断状态；指标卡按 §7.4。
- 时间范围选择明显，默认最近 24h；数据为 0 显示真实 0，不用空态替代。
- 图表/统计加载失败显示错误 + 重试，不留空坐标轴。

### 8.3 Provider / Route / API Key

- 列表页：上方筛选 + 创建按钮（页面唯一 primary），行内次要操作走 ghost / RowActions。
- 创建/编辑用 Dialog 或详情页，字段多时分组；删除二次确认且文案包含对象名称。
- API Key secret 创建后只展示一次，accent 复制按钮 + 妥善保存提示。

### 8.4 OAuth

- 向导式流程：选择 provider → 发起授权 → 新窗口完成 → 刷新状态，每步有明确状态指示。
- 明确提示 redirect URI、state 与浏览器可达性要求。
- OAuth 错误信封与主 Admin API 不同，错误文案必须兼容解析。

### 8.5 请求日志与重放

- 过滤器 + 高密度表格（紧凑 40px 行高）+ 详情抽屉（Drawer）。
- `truncated`、`[REDACTED]`、inline media 元信息以 Badge 标识；请求/响应 JSON 按 §7.9。
- 重放内容是快照，不暗示会重新请求上游，除非后端提供真实重放端点。

### 8.6 审计日志

- 优先展示 actor、resource、action、timestamp、details；`details` JSON 默认折叠。
- 强调审计不可篡改属性，不提供任何"编辑审计日志"类操作。

## 9. 响应式规范

| 断点 | 行为 |
| --- | --- |
| `375px` | 单列，侧边栏收为抽屉，表格横向滚动，主操作可全宽 |
| `768px` | 两列卡片，过滤器横向排列 |
| `1024px` | 固定侧边栏，Dashboard 多列指标卡 |
| `1440px` | 表格与图表展示更多列，不拉伸长文本 |

- 任何页面不依赖桌面 hover 完成核心操作。
- 小屏表单单列；按钮组垂直堆叠或主按钮全宽。
- 表格小屏横向滚动时首列关键字段尽量可见（可考虑首列 sticky）。

## 10. 国际化规范

- 文案集中在 i18n 资源（i18next），组件不硬编码中英文。
- Badge、错误、空态、按钮、表单 label 全部可翻译。
- 日期时间本地化并标注 UTC / Local；数字用 `Intl.NumberFormat`，时间用 `Intl.DateTimeFormat`。
- 技术字段名保留英文（`provider_id`、`virtual_model`），配说明文案。

## 11. 安全与隐私 UX

- token、secret、Authorization header 不进入 URL、日志、toast、错误详情、console。
- 密钥输入默认 password 类型 + 显隐切换；复制敏感值后提示妥善保存。
- 脱敏值以 `[REDACTED]`、`[encrypted:…]` 或 Badge 明确标识。
- destructive action 与 logout 在视觉和空间上与普通导航分离。

## 12. CSS Token 实现（v0.2）

落地位置：`webui/src/index.css`。通过 Tailwind v4 `@theme inline` 映射为工具类（`bg-surface`、`text-text-muted` 等），主题经 `[data-theme="…"]` 切换（`lib/theme.tsx`）。下方仅列出 **蓝色主题（默认 light / dark）** 的完整 token 作为基准；其余 10 套主题（§3.6）结构完全相同，只改写 token 值，新增主题块统一插入在基准块之后、`@theme inline` 之前。

```css
:root,
[data-theme="light"] {
  color-scheme: light;
  /* neutrals */
  --bg: #f7f8fa;
  --surface: #ffffff;
  --surface-muted: #f1f3f6;
  --border: #e4e7ec;
  --border-strong: #d0d5dd;
  --text: #101828;
  --text-muted: #475467;
  --text-subtle: #667085;
  /* primary & accent */
  --primary: #2e5be6;
  --primary-strong: #2348c7;
  --primary-soft: #e8eefe;
  --primary-faint: #f4f7fe;
  --on-primary: #ffffff;
  --accent: #b8540b;
  --accent-strong: #9a460a;
  --on-accent: #ffffff;
  /* status */
  --success: #0f8043;
  --success-soft: #e6f6ec;
  --warning: #b45309;
  --warning-soft: #fbf1e0;
  --danger: #d92d20;
  --danger-strong: #b42318;
  --danger-soft: #fbeae8;
  --on-danger: #ffffff;
  --info: #0e7cb8;
  --info-soft: #e3f3fc;
  /* focus */
  --ring: color-mix(in srgb, var(--primary) 45%, transparent);
  /* radius */
  --radius-xs: 4px;
  --radius-sm: 6px;
  --radius-md: 8px;
  --radius-lg: 12px;
  --radius-full: 999px;
  /* elevation */
  --shadow-xs: 0 1px 2px rgb(16 24 40 / 0.05);
  --shadow-md: 0 4px 8px -2px rgb(16 24 40 / 0.10), 0 2px 4px -2px rgb(16 24 40 / 0.06);
  --shadow-lg: 0 20px 24px -4px rgb(16 24 40 / 0.10), 0 8px 8px -4px rgb(16 24 40 / 0.04);
  --overlay: rgb(16 24 40 / 0.5);
  /* motion */
  --duration-fast: 120ms;
  --duration-base: 180ms;
  --duration-slow: 240ms;
  --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
  --ease-in: cubic-bezier(0.7, 0, 0.84, 0);
  /* fonts */
  --font-sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto,
    "PingFang SC", "Noto Sans SC", Helvetica, Arial, sans-serif;
  --font-mono: ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas,
    "Liberation Mono", monospace;
}

[data-theme="dark"] {
  color-scheme: dark;
  --bg: #0a0c14;
  --surface: #11141e;
  --surface-muted: #191d2a;
  --border: #262c3b;
  --border-strong: #343c4e;
  --text: #edeff3;
  --text-muted: #a4acbb;
  --text-subtle: #7c8494;
  --primary: #84a6ff;
  --primary-strong: #a3bcff;
  --primary-soft: #1d2a4a;
  --primary-faint: #161e33;
  --on-primary: #0b0d12;
  --accent: #f08c3d;
  --accent-strong: #f6a86a;
  --on-accent: #0b0d12;
  --success: #45c77f;
  --success-soft: #11291c;
  --warning: #f0b254;
  --warning-soft: #2e2410;
  --danger: #f2756b;
  --danger-strong: #f8958d;
  --danger-soft: #321714;
  --on-danger: #0b0d12;
  --info: #4fbbf0;
  --info-soft: #0e2433;
  --overlay: rgb(0 0 0 / 0.6);
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

新增 token（`--border-strong`、`--primary-faint`、`--ring`、`--radius-xs`、`--radius-full`、`--shadow-xs`、`--overlay`、motion token）需同步加入 `@theme inline` 映射。

## 13. v0.1 → v0.2 迁移指南

当前 `index.css` 已实现 v0.1 token，迁移按以下顺序进行，每步可独立发布：

1. **替换 token 值**：在 `index.css` 用 §12 的值整体替换 `:root` 与 `[data-theme="dark"]`，新增 token 并补 `@theme inline` 映射。组件因走语义类名，多数无需改动即获得新视觉。
2. **新旧映射**：`shadow-sm → shadow-xs`（卡片）；`--info/--success` 等沿用同名；输入框、Select 边框从 `--border` 升级为 `--border-strong`。
3. **排版升级**：表头与指标卡标题切换为 `label` 角色（uppercase + 字距）；页面标题加 `-0.01em` 字距；数字列补 `tabular-nums`。
4. **组件细节**：Badge 改 soft 底方案；按钮补按下 `scale(0.98)`；行 hover、菜单 hover 统一 `--duration-fast` 过渡；focus ring 切换为 `--ring`。
5. **动效统一**：现有 keyframes（`overlay-in`、`content-in`、`drawer-in`、`toast-in`）参数对齐 §6 token。
6. **后续增强**：JSON 视图容器规范化（§7.9）、日志表紧凑密度、图表库引入时采用 §3.5 色板。

### 13.1 多主题扩展记录

在 v0.2 基础上将主题从「明暗 2 套」扩展为 **6 色相 × 明暗 = 12 套**（§3.6），落地要点：

1. **CSS（`index.css`）**：每套主题一个 `[data-theme="…"]` 块，改写全部语义 token 值；`:root` 与 `[data-theme="light"]` 共用蓝色 light 基准。新增主题块统一插在 `dark` 基准块之后、`@theme inline` 之前。
2. **元数据（`lib/theme.tsx`）**：`Theme` 联合类型登记全部 12 个 id；`THEMES: ThemeMeta[]` 为每套提供 `id` / `mode` / `labelKey` / `swatchBg` / `swatchColor`，驱动切换器渲染与持久化。
3. **命名统一**：标签键收敛为 6 个色相键（`themeBlue` / `themeAmber` / `themeSlate` / `themeLime` / `themeFuchsia` / `themeMauve`），light/dark 同色相复用同一键，移除旧的 `themeLightDefault` / `themeDarkDim` / `themeDarkOled` 等不一致命名。
4. **i18n（`locales/en.ts`、`locales/zh.ts`）**：同步只保留 6 个色相键，中文用「蓝色 / 琥珀 / 石板 / 青柠 / 品红 / 藕荷」。
5. **兼容性**：保留历史 `data-theme` id，旧 localStorage 值仍可命中；非法值回落到系统 `prefers-color-scheme` 对应的默认 `light` / `dark`。

## 14. 验收清单

### 基础质量

- [ ] 所有页面在 375 / 768 / 1024 / 1440px 下可用。
- [ ] 所有交互元素有可见 focus ring（`--ring`），可全键盘操作。
- [ ] 正文对比度 ≥ 4.5:1，UI 图形 ≥ 3:1；暗色独立验证。
- [ ] 图标全部来自 lucide-react，16px / stroke 1.5，禁止 emoji 结构图标。
- [ ] 所有表单字段有 label、错误信息、提交中状态；destructive action 有二次确认。
- [ ] loading / empty / error / success 四态完整。
- [ ] token、secret、header 不出现在 URL、日志、toast、console。
- [ ] 复杂交互全部经 Radix 封装层；`prefers-reduced-motion` 下无非必要动画。
- [ ] 中英双语完整，缺失有 fallback。

### 细腻度（v0.2 新增）

- [ ] 一屏主色实底不超过 2~3 处；Badge 全部为 soft 底方案。
- [ ] 表头、指标卡标题统一 `label` 角色（uppercase + `+0.04em` 字距）。
- [ ] 所有数字列与指标使用 `tabular-nums`，右对齐。
- [ ] 层级靠 hairline + 背景差表达，卡片无重阴影；浮层阴影使用多层柔和 token。
- [ ] hover/按下/进入动效统一使用 motion token，退出快于进入。
- [ ] ID/hash 等技术字段等宽 + 截断 + tooltip + 复制反馈三件套齐全。
- [ ] 间距遵守 24/16/8/4 节奏，无随机间距值。
- [ ] z-index 遵守固定阶梯，无 `z-[999]` 类魔法值。
