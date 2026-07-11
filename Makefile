# tiygate — top-level Makefile
# 用法: make <target>
# 约定: 所有命令均可在仓库根目录直接执行;前端产物在 webui/dist,Rust 端用
# rust-embed 在编译期嵌入。

SHELL := /bin/sh

# ---------- 可配置项(可通过环境变量覆盖) ----------
CARGO        ?= cargo
NPM          ?= npm
NODE         ?= node
WEBUI_DIR    ?= webui
CRATES_DIR   ?= crates
TARGET_DIR   ?= target
BIN_NAME     ?= tiygate

# Rust 二进制在哪个 crate(若 workspace 只有一个 bin,这里直接用包名即可)
SERVER_CRATE ?= tiygate-server

# dev 模式默认开启的 cargo features。`webui` 会通过 rust-embed 在编译期
# 嵌入 webui/dist,因此 dev 前会先做一次 webui-build(避免二进制里只有空壳)。
SERVER_FEATURES ?= webui

# dev/build 的并行度;留空让 cargo 自己决定
JOBS         ?=

# ---------- 覆盖率工具(llvm-cov / llvm-profdata)----------
# rustup 用户通常无需配置(自带 llvm-tools-preview 组件)。
# Homebrew 安装的 Rust 缺少该组件,自动检测 brew llvm 路径;
# 也可通过环境变量手动覆盖,例如:
#   make test-cov LLVM_COV="$(brew --prefix llvm@21)/bin/llvm-cov"
LLVM_COV     ?= $(shell command -v llvm-cov 2>/dev/null || ls /opt/homebrew/opt/llvm*/bin/llvm-cov 2>/dev/null | head -1)
LLVM_PROFDATA ?= $(shell command -v llvm-profdata 2>/dev/null || ls /opt/homebrew/opt/llvm*/bin/llvm-profdata 2>/dev/null | head -1)

# ---------- 内部辅助变量 ----------
CARGO_BUILD  := $(CARGO) build $(if $(JOBS),-j$(JOBS),)
CARGO_TEST   := $(CARGO) test $(if $(JOBS),-j$(JOBS),)
CARGO_RUN    := $(CARGO) run
CARGO_CLIPPY := $(CARGO) clippy --all-targets --all-features -- -D warnings
CARGO_FMT    := $(CARGO) fmt --all -- --check
CARGO_FMT_W  := $(CARGO) fmt --all

# 默认目标:打印帮助
.DEFAULT_GOAL := help

.PHONY: help
help: ## 显示所有可用目标
	@awk 'BEGIN {FS = ":.*##"; printf "可用目标:\n"} \
		/^[a-zA-Z_-]+:.*##/ { printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

# =========================
# 前端(WebUI)
# =========================
# `webui/node_modules/.package-lock.json` 是 npm install 成功后写入的标志文件,
# 用来幂等判断"依赖是否就绪",避免每次都跑 install。
WEBUI_INSTALLED := $(WEBUI_DIR)/node_modules/.package-lock.json

.PHONY: webui-install
webui-install: ## Install webui dependencies (skip if already installed, so first make dev/build succeeds in one go)
	@if [ ! -f "$(WEBUI_INSTALLED)" ]; then \
		echo ">> webui deps not ready, running: $(NPM) install"; \
		cd $(WEBUI_DIR) && $(NPM) install; \
	else \
		echo ">> webui deps already installed ($(WEBUI_INSTALLED)), skipping install"; \
	fi

.PHONY: webui-lint
webui-lint: ## 前端类型检查(tsc --noEmit)
	cd $(WEBUI_DIR) && $(NPM) run lint

.PHONY: webui-fmt
webui-fmt: ## 前端格式化(prettier --write,若未安装可改为 npm run format)
	cd $(WEBUI_DIR) && $(NPM) run -s format -- --write . || true

.PHONY: webui-build
webui-build: ## 构建前端生产产物到 webui/dist
	cd $(WEBUI_DIR) && $(NPM) run build

.PHONY: webui-clean
webui-clean: ## 清理前端构建产物
	rm -rf $(WEBUI_DIR)/dist $(WEBUI_DIR)/node_modules/.cache

# =========================
# Rust 端
# =========================
.PHONY: fmt
fmt: webui-fmt ## 格式化 Rust + 前端代码
	$(CARGO_FMT_W)

.PHONY: lint
lint: webui-lint ## 静态检查:cargo fmt --check + clippy + 前端类型检查
	$(CARGO_FMT)
	$(CARGO_CLIPPY)

.PHONY: build
build: webui-install webui-build ## 构建 Rust release 版本(会先安装前端依赖并构建产物以供 rust-embed 嵌入)
	$(CARGO_BUILD) --release

.PHONY: build-debug
build-debug: webui-install webui-build ## 构建 Rust debug 版本
	$(CARGO_BUILD)

.PHONY: test
test: ## 运行 Rust 测试(workspace 全量)
	$(CARGO_TEST) --workspace --all-features

.PHONY: test-cov
test-cov: ## 运行测试并生成覆盖率(需要 cargo-llvm-cov)
	$(if $(LLVM_COV),LLVM_COV="$(LLVM_COV)") $(if $(LLVM_PROFDATA),LLVM_PROFDATA="$(LLVM_PROFDATA)") cargo llvm-cov --workspace --all-features --html --open

.PHONY: clean
clean: webui-clean ## 清理 Rust + 前端构建产物
	$(CARGO) clean
	rm -rf $(WEBUI_DIR)/dist

# =========================
# 开发体验
# =========================
.PHONY: dev
dev: webui-install webui-build ## 本地开发:先安装/校验前端依赖,再构建 webui 供 Rust 嵌入,然后 cargo run 启动服务(带 $(SERVER_FEATURES) feature)
	$(CARGO_RUN) -p $(SERVER_CRATE) --features "$(SERVER_FEATURES)"

.PHONY: dev-server
dev-server: ## 仅启动 Rust 服务端(走默认 features,跳过 webui 嵌入构建)
	$(CARGO_RUN) -p $(SERVER_CRATE)

.PHONY: dev-web
dev-web: webui-install ## 仅启动 WebUI 开发服务器(在 webui/ 下跑 npm run dev)
	cd $(WEBUI_DIR) && $(NPM) run dev

.PHONY: watch
watch: ## 监听 Rust 代码变更并自动重新构建/运行(需要 cargo-watch)
	cargo watch -x 'run -p $(SERVER_CRATE)'

# =========================
# 桌面客户端 (Tauri)
# =========================
# Sidecar features — 包含 webui 嵌入,排除 redis-quota/bedrock/dotenv
# (dotenv 排除: Tauri 已显式注入所有 env,dotenvy 会搜索 CWD 下的 .env,
#  若 workspace 在 ~/Documents 下会触发 macOS 文稿权限弹窗)
SIDECAR_FEATURES ?= webui,admin,cache,providers,control-plane,tracing
SIDECAR_SCRIPT   := scripts/build-sidecar.sh

.PHONY: build-sidecar
build-sidecar: ## 编译 tiygate-server 为 Tauri sidecar 二进制并复制到 src-tauri/binaries/(release)
	bash $(SIDECAR_SCRIPT)

.PHONY: build-sidecar-debug
build-sidecar-debug: ## 编译 tiygate-server sidecar(debug 模式)
	bash $(SIDECAR_SCRIPT) --debug

.PHONY: dev-desktop
dev-desktop: webui-install webui-build build-sidecar-debug ## 开发模式:编译 sidecar(debug)+ 启动 Tauri dev(webui 热更新)
	cd src-tauri && cargo tauri dev

.PHONY: desktop
desktop: webui-install webui-build build-sidecar ## 构建桌面客户端安装包(release):编译 sidecar + webui + Tauri bundle(自动适配当前平台)
	cd src-tauri && cargo tauri build

.PHONY: desktop-check
desktop-check: webui-install webui-build ## 检查 Tauri 客户端 crate 是否可编译
	cd src-tauri && cargo check

.PHONY: desktop-lint
desktop-lint: webui-install webui-build ## Tauri 客户端 clippy 检查(需先构建 webui 以满足 generate_context!)
	$(CARGO) clippy -p tiygate-desktop --all-targets -- -D warnings

.PHONY: doc
doc: ## 生成并打开 Rust 文档
	$(CARGO) doc --workspace --all-features --no-deps --open

# =========================
# 工具链辅助
# =========================
.PHONY: check
check: ## cargo check(workspace 全量,比 build 快)
	$(CARGO) check --workspace --all-targets --all-features

.PHONY: sync-protocol-specs
sync-protocol-specs: ## 刷新官方 API wire-schema 快照及其锁定摘要
	sh scripts/sync-protocol-specs.sh

.PHONY: check-protocol-specs
check-protocol-specs: ## 检查已提交的 API wire-schema 快照是否仍与官方来源一致
	sh scripts/sync-protocol-specs.sh --check

.PHONY: test-structured-output-profiles
test-structured-output-profiles: ## 验证 Structured Output profile 与核心协议规则一致
	$(CARGO) test -p tiygate-core --test structured_output_profiles

.PHONY: update
update: ## 更新 Cargo 与 npm 依赖
	$(CARGO) update
	cd $(WEBUI_DIR) && $(NPM) update

.PHONY: audit
audit: ## 依赖安全审计
	$(CARGO) audit
	cd $(WEBUI_DIR) && $(NPM) audit --omit=dev || true

# 工具安装策略:若系统用 rustup 就走 rustup;若用 Homebrew/系统 cargo,
# rustfmt/clippy 一般已自带,跳过即可。
.PHONY: install-tools
install-tools: ## 安装常用开发工具(rustfmt/clippy/llvm-cov/watch)
	@if command -v rustup >/dev/null 2>&1; then \
		rustup component add rustfmt clippy; \
	else \
		echo "未检测到 rustup(看起来用的是 Homebrew/系统 cargo)。rustfmt/clippy 通常已随 cargo 自带,跳过。"; \
		echo "如确实缺失,可用: brew install rustfmt clippy"; \
	fi
	-$(CARGO) install cargo-llvm-cov --locked
	-$(CARGO) install cargo-watch --locked
	-$(CARGO) install cargo-audit --locked
	-$(CARGO) install tauri-cli --locked
