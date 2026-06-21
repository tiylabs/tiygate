<div align="center">

# TiyGate

**致力于提供高可用 LLM 服务的轻量级网关。**

通过一个控制面接入 OpenAI-Compatible、Responses、Messages、Gemini 等协议。用虚拟模型按策略路由多服务商 / 多模型，捕获完整请求响应日志，并支持个人桌面端零配置启用与企业容器化部署。

[![许可证: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Edition: 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![版本: 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey.svg)](Cargo.toml)
[![Workspace: 8 crates](https://img.shields.io/badge/workspace-8%20crates-blueviolet.svg)](Cargo.toml)

[English](README.md) | 简体中文

</div>

---

<div align="center">
  <img width="1546" height="1079" alt="TiyGate Dashboard 截图" src="https://github.com/user-attachments/assets/c95d421a-9274-4804-a160-a7c9cee7e36c" />
</div>

## TiyGate 是什么?

TiyGate 是一款用 **Rust** 编写的**开源 AI 网关**，适合同时使用多个 LLM 服务商、订阅套餐、协议或 API Key 的个人与团队。它位于应用与上游服务商（OpenAI、Anthropic、Bedrock、Gemini 以及 OpenAI-Compatible 服务）之间，把分散的模型接入、路由策略、日志与统计统一到一个稳定控制面。

如果你遇到这些问题，TiyGate 可以直接解决：

1. **还在多个订阅套餐间来回切换** —— 接入多个服务商后，通过统一网关按策略使用。
2. **还在因服务商不稳定频繁改模型配置** —— 虚拟模型可按优先级、权重、吞吐、延迟等策略在多服务商 / 多模型间自动容灾切换与恢复。
3. **还在为不明原因请求失败而挠头定位** —— 实时捕获客户端 ↔ 网关 ↔ 服务商之间的详细请求、响应日志，便于回放与排查。
4. **还在为不同服务商、API Key 用量统计烦恼** —— 聚合服务商、模型、API Key 等多维度统计，统一查看用量数据。

## 为什么选择 TiyGate？

| 能力 | 你能获得什么 |
|---|---|
| **统一接入** | 支持 OpenAI-Compatible、Responses、Messages、Gemini、Embeddings 等协议，并通过 canonical IR 支撑可扩展的 N×N 协议转换。 |
| **策略容灾** | 基于虚拟模型路由，将后端多服务商 / 多模型按优先级、权重、吞吐、延迟等策略自动容灾切换与恢复。 |
| **数据沉淀** | 实时沉淀客户端 → 网关 → 服务商链路上的请求、响应详情，支持策略清理与 S3 兼容对象存储投递。 |
| **轻量应用** | 支持个人场景的 macOS / Windows 桌面端零配置启用，也支持企业场景容器化规模部署；桌面端可管理本地与云端多实例。 |
| **安全加密** | 服务商 API Key 使用 `TIYGATE_MASTER_KEY` 静态加密存储，请求 / 响应日志中的敏感字段支持脱敏。 |
| **备份恢复** | 配置支持加密导出与导入，便于实例迁移、备份与恢复。 |
| **数据统计** | 汇聚多服务商数据，支持按服务商、模型、API Key 等维度查看统计。 |

## 快速开始

按使用场景选择入口：

- **桌面版：** 🖥️ 从 [Releases](https://github.com/tiylabs/tiygate/releases) 下载最新 macOS 或 Windows 安装包，启动后在管理控制台配置服务商与虚拟模型。
- **Docker：** 🐳 运行 `docker run -d -p 3000:3000 jorbenzhu/tiygate:latest`，生产配置参考[部署与运维文档](docs/deployment-operations_zh.md)。
- **源码启动：** 🦀 需要 Rust 1.88+ 和 Node.js 20+。

```bash
git clone https://github.com/tiylabs/tiygate.git
cd tiygate
cp .env.example .env
make dev
```

在 `.env` 中至少设置 `TIYGATE_DATABASE_URL` 和 `TIYGATE_ADMIN_TOKEN`；如需加密存储服务商 Key、OAuth token 与 S3 凭证，请设置 `TIYGATE_MASTER_KEY`。启动后打开 **`http://localhost:3000/admin/ui`**，粘贴 admin token 后即可管理服务商、路由、API Key、运行时设置、日志与统计。

## 文档

- [部署与运维](docs/deployment-operations_zh.md)：部署模式、健康探针、配置、优雅排空、缓存、S3 payload 归档与追踪。
- [协议能力矩阵](docs/protocol-capability-matrix.md)：协议转换行为与有损字段处理。
- [请求日志](docs/request-logging.md)：请求/响应捕获与回放细节。

## 开发

```bash
make check        # cargo check --workspace --all-targets --all-features
make test         # cargo test --workspace --all-features
make lint         # rustfmt check, clippy -D warnings, and webui tsc
make fmt          # 格式化 Rust 与 WebUI 代码
```

贡献规则、分层约束与编码规范见 [AGENTS.md](AGENTS.md)。管理控制台开发见 [webui/README.md](webui/README.md)。

## 贡献

欢迎 Issue 与 PR。设计是有立场的,与分层对抗的贡献(例如给 `core` 加具体 provider 依赖、引入 `allow_lossy`)会被拒。

## 许可证

[Apache-2.0](LICENSE)

---

<div align="center">
<sub>由 <a href="https://github.com/tiylabs">tiylabs</a> 构建 · <a href="docs/protocol-capability-matrix.md">能力矩阵</a></sub>
</div>
