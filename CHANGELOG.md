# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.7] - 2026-06-26
### :sparkles: New Features
- [`db90c48`](https://github.com/tiylabs/tiygate/commit/db90c48fef0e15a8be809165eed8eaffbe169570) - **auth**: ✨ Implement OAuth 2.0 support for Codex/Claude/xAI providers *(PR [#11](https://github.com/tiylabs/tiygate/pull/11) by [@HayWolf](https://github.com/HayWolf))*


## [0.1.6] - 2026-06-24
### :sparkles: New Features
- [`9704b38`](https://github.com/tiylabs/tiygate/commit/9704b3843d82280f4106b3f16feb52d3c42e03e7) - **store**: ✨ add token stats export/import and fix Tauri desktop download *(PR [#6](https://github.com/tiylabs/tiygate/pull/6) by [@jorben](https://github.com/jorben))*

### :bug: Bug Fixes
- [`8e56a1c`](https://github.com/tiylabs/tiygate/commit/8e56a1cb00f56ef75a658d8c28179a6bc29cd7ed) - **ingress**: 🐛 inject end frame after stream error and improve admin console UX *(PR [#9](https://github.com/tiylabs/tiygate/pull/9) by [@jorben](https://github.com/jorben))*


## [0.1.5] - 2026-06-23
### :sparkles: New Features
- [`d2accf6`](https://github.com/tiylabs/tiygate/commit/d2accf63c2d1097afe23fd815eb423e5e5ef2ece) - **ui**: ✨ show enabled state in target badges *(commit by [@jorben](https://github.com/jorben))*
- [`982b5c0`](https://github.com/tiylabs/tiygate/commit/982b5c0eb14aad65d3b00d1d57b9d43266a63a91) - **protocol**: ✨ preserve image_url.detail across protocol translation *(commit by [@jorben](https://github.com/jorben))*

### :bug: Bug Fixes
- [`0bb6b29`](https://github.com/tiylabs/tiygate/commit/0bb6b29bda1aebce4301b247e7755e3d1c4bbb48) - **ingress**: 🐛 fix passthrough forwarding media-stripped body upstream *(commit by [@jorben](https://github.com/jorben))*


## [0.1.4] - 2026-06-22
### :sparkles: New Features
- [`b4d06e4`](https://github.com/tiylabs/tiygate/commit/b4d06e4b265f0286c92190bcecdd8ddd51739c02) - **ingress**: ✨ detect and log downstream client disconnects during SSE streaming *(commit by [@jorben](https://github.com/jorben))*
- [`84c3874`](https://github.com/tiylabs/tiygate/commit/84c387418f04721fc597a5d89d6f49839014f375) - **webui**: ✨ add cache hit ratio display and conditionally hide fields in request logs *(commit by [@jorben](https://github.com/jorben))*

### :recycle: Refactors
- [`58a2912`](https://github.com/tiylabs/tiygate/commit/58a29123143b1e7f5ff9a417705e27d7c64f6164) - **telemetry**: ♻️ normalise request log status and error_class into typed enums *(commit by [@jorben](https://github.com/jorben))*


## [0.1.3] - 2026-06-22
### :sparkles: New Features
- [`aaeb050`](https://github.com/tiylabs/tiygate/commit/aaeb050d1ccfe4f5e2619a7fcbb731fb4c0a6f7e) - **routes**: ✨ enhance target reorder with pointer and keyboard support *(commit by [@jorben](https://github.com/jorben))*

### :bug: Bug Fixes
- [`212d6f4`](https://github.com/tiylabs/tiygate/commit/212d6f459cdaaf0d7e37b14e5d8c0218bdf5bb8f) - **routes**: ✅ improve target drag-and-drop cancellation and state reset *(commit by [@jorben](https://github.com/jorben))*


## [0.1.2] - 2026-06-22
### :bug: Bug Fixes
- [`8979b0b`](https://github.com/tiylabs/tiygate/commit/8979b0b3a3eec1bb673eacd0e39c4195cbc25442) - **auth**: 🐛 allow remote instances to bypass local first-run setup *(commit by [@jorben](https://github.com/jorben))*


## [0.1.1] - 2026-06-21
### :sparkles: New Features
- [`db91200`](https://github.com/tiylabs/tiygate/commit/db9120082502897fb79e24dc5c0b56530a1aabdd) - **store**: ✨ add SQLite local database maintenance *(commit by [@jorben](https://github.com/jorben))*


## [0.1.0] - 2026-06-20
### :boom: BREAKING CHANGES
- due to [`9ffb875`](https://github.com/tiylabs/tiygate/commit/9ffb875a921c64485af0b4d45fac4dc0a1361758) - 🐛 correct admin UI routing basename and login redirect *(commit by [@jorben](https://github.com/jorben))*:

  Admin console deep links now resolve under `/admin/ui/` with a trailing slash

- due to [`3c976e1`](https://github.com/tiylabs/tiygate/commit/3c976e14bfdd3eb839a3f2a33239c4415a390da6) - ✨ unify cross-protocol thinking config with 6-level effort enum and bidirectional budget mapping *(commit by [@jorben](https://github.com/jorben))*:

  ThinkingEffort enum now includes Minimal and XHigh variants; consumers pattern-matching on the enum must handle the new arms

- due to [`e944b2f`](https://github.com/tiylabs/tiygate/commit/e944b2f46b41ef7a5f9fb9fda561f5c65e385479) - ✨ introduce database-backed runtime settings with hot-reload *(commit by [@jorben](https://github.com/jorben))*:

  environment variables for runtime tuning (body size limits,  
  routing strategy, upstream timeouts, payload archive config, retention,  
  epoch poll interval, token stats) are now only read on first start as  
  seed values; the settings table is the single source of truth thereafter

- due to [`70fe0be`](https://github.com/tiylabs/tiygate/commit/70fe0be7fca4d60a166662100f2b397c90f036a0) - ✨ add require_api_key settings toggle for data-plane authentication *(commit by [@jorben](https://github.com/jorben))*:

  require_api_key defaults to true — existing  
  deployments running in anonymous mode must set  
  TIYGATE_REQUIRE_API_KEY=false or seed API keys to restore  
  pass-through behavior


### :sparkles: New Features
- [`0ac7de0`](https://github.com/tiylabs/tiygate/commit/0ac7de0c67686031df9fa1607059919f797839ae) - ✨ Ship complete TiyGate AI gateway — multi-protocol transcoding, admin console, and CI/CD *(commit by [@jorben](https://github.com/jorben))*
- [`accbd30`](https://github.com/tiylabs/tiygate/commit/accbd30a3bf4722a81f59c1b176d9647a29c4063) - **protocols**: ✨ extend IR with thinking, refusal, metadata, and annotations *(commit by [@jorben](https://github.com/jorben))*
- [`3c976e1`](https://github.com/tiylabs/tiygate/commit/3c976e14bfdd3eb839a3f2a33239c4415a390da6) - **thinking**: ✨ unify cross-protocol thinking config with 6-level effort enum and bidirectional budget mapping *(commit by [@jorben](https://github.com/jorben))*
- [`56019de`](https://github.com/tiylabs/tiygate/commit/56019de0d99fb47278ff342b525b585fa082ea1d) - **store**: ✨ tune autovacuum for payload-heavy log tables *(commit by [@jorben](https://github.com/jorben))*
- [`c19a09f`](https://github.com/tiylabs/tiygate/commit/c19a09ff6c17c10a9de3707d5efb8a5c20a71b55) - **providers**: ✨ add OpenRouter provider integration *(commit by [@jorben](https://github.com/jorben))*
- [`bc3a558`](https://github.com/tiylabs/tiygate/commit/bc3a5581fcf60d6b53dd80f8131277b18e2a55ee) - **providers**: ✨ add OpenCode providers *(commit by [@jorben](https://github.com/jorben))*
- [`ec10f7b`](https://github.com/tiylabs/tiygate/commit/ec10f7b2ab11a63e70cfcf520757ffbaab5fc86d) - **webui**: ✨ add collapsible JSON tree viewer for request log details *(commit by [@jorben](https://github.com/jorben))*
- [`2abe00b`](https://github.com/tiylabs/tiygate/commit/2abe00bba8a98fea7e1833186e35e9b42b7870a7) - **config**: ✨ add config export and import for backup & restore *(commit by [@jorben](https://github.com/jorben))*
- [`e944b2f`](https://github.com/tiylabs/tiygate/commit/e944b2f46b41ef7a5f9fb9fda561f5c65e385479) - **config**: ✨ introduce database-backed runtime settings with hot-reload *(commit by [@jorben](https://github.com/jorben))*
- [`6fbeabf`](https://github.com/tiylabs/tiygate/commit/6fbeabf630b41777ceba4736bb33cb4bb85d7516) - **config**: ✨ extend backup/restore to include settings and selective import *(commit by [@jorben](https://github.com/jorben))*
- [`ef29075`](https://github.com/tiylabs/tiygate/commit/ef290753788fad33f477a8a2041cfad285c67640) - **integration-guide**: ✨ use current origin as default base URL in production *(commit by [@jorben](https://github.com/jorben))*
- [`1dedbce`](https://github.com/tiylabs/tiygate/commit/1dedbcec504e6dec9f78df85632741067c37bd77) - **auth**: ✨ add admin brute-force protection and tag-based version display *(commit by [@jorben](https://github.com/jorben))*
- [`5f94449`](https://github.com/tiylabs/tiygate/commit/5f94449c7eadd6c1e80760ed3d4366ac5f5ed825) - **admin**: ✨ record field-level before/after diff in settings audit *(commit by [@jorben](https://github.com/jorben))*
- [`e999957`](https://github.com/tiylabs/tiygate/commit/e999957458b6dc9099e4b22797f84470c790d2f7) - ✨ add OpenAI Images endpoint passthrough support *(commit by [@jorben](https://github.com/jorben))*
- [`70fe0be`](https://github.com/tiylabs/tiygate/commit/70fe0be7fca4d60a166662100f2b397c90f036a0) - **auth**: ✨ add require_api_key settings toggle for data-plane authentication *(commit by [@jorben](https://github.com/jorben))*
- [`c711214`](https://github.com/tiylabs/tiygate/commit/c7112147d853ad84445218c3ec64527fd00b6c91) - **desktop**: ✨ add Tauri desktop client with sidecar architecture *(commit by [@jorben](https://github.com/jorben))*
- [`35469a4`](https://github.com/tiylabs/tiygate/commit/35469a47611e0f3842c784defec55cec55dbaee0) - **auth**: ✨ add passwordless mode to hide logout button *(commit by [@jorben](https://github.com/jorben))*
- [`1e8556c`](https://github.com/tiylabs/tiygate/commit/1e8556c8ced5d3e48c6876baac5d6f5eb0451b80) - **tray**: ✨ add system tray icon with show/hide/quit controls *(commit by [@jorben](https://github.com/jorben))*
- [`2c2a2cd`](https://github.com/tiylabs/tiygate/commit/2c2a2cd9e228849518adb21c2617576f8a3de63c) - **tauri**: 🎉 run as menu-bar accessory app on macOS *(commit by [@jorben](https://github.com/jorben))*
- [`ce86918`](https://github.com/tiylabs/tiygate/commit/ce8691870ba7a8342d3acc6e0d8ffca3b770a331) - **ui**: ✨ add custom macOS tray icon with template support *(commit by [@jorben](https://github.com/jorben))*
- [`d3c9e51`](https://github.com/tiylabs/tiygate/commit/d3c9e511dbdf194e48c8fc5e50df34c51148d6e3) - **desktop**: ✨ add remote instance management and switching *(commit by [@jorben](https://github.com/jorben))*
- [`2da2e08`](https://github.com/tiylabs/tiygate/commit/2da2e0896cdee070d732e1965249e09ea5cb4bf0) - **auth**: ✨ scope tokens per-instance to support remote instance auto-login *(commit by [@jorben](https://github.com/jorben))*
- [`7bb8062`](https://github.com/tiylabs/tiygate/commit/7bb8062d317c2764a826743a41bc1b86fe797d8f) - **ui**: ✨ show instance indicator on login page and enable logout in Tauri *(commit by [@jorben](https://github.com/jorben))*

### :bug: Bug Fixes
- [`9ffb875`](https://github.com/tiylabs/tiygate/commit/9ffb875a921c64485af0b4d45fac4dc0a1361758) - **webui**: 🐛 correct admin UI routing basename and login redirect *(commit by [@jorben](https://github.com/jorben))*
- [`8610ef6`](https://github.com/tiylabs/tiygate/commit/8610ef6ba64b30785099cda74f68af2a093993f6) - **protocols**: 🐛 preserve tool call args and usage in OpenAI→Messages streaming *(commit by [@jorben](https://github.com/jorben))*
- [`9508d6a`](https://github.com/tiylabs/tiygate/commit/9508d6a044de9b2fadba14a0ff72bca678dc14c5) - **oltp**: 🐛 let Anthropic message_delta usage override message_start *(commit by [@jorben](https://github.com/jorben))*
- [`b678e9c`](https://github.com/tiylabs/tiygate/commit/b678e9cee5a51cdc3b762f37938398a3f9fae36b) - **gemini**: 🐛 resolve thinking field conflict and missing tool result names *(commit by [@jorben](https://github.com/jorben))*
- [`4316769`](https://github.com/tiylabs/tiygate/commit/4316769e7b0e336c809922c0155536376109b802) - **protocol**: ✅ defer terminal delta until usage arrives when finish precedes usage *(commit by [@jorben](https://github.com/jorben))*
- [`8a2885d`](https://github.com/tiylabs/tiygate/commit/8a2885d46c860d1892f2a571c3de1b55a198d7e8) - **protocols**: 🐛 accumulate usage across stream events to preserve cache read tokens *(commit by [@jorben](https://github.com/jorben))*
- [`85f99b2`](https://github.com/tiylabs/tiygate/commit/85f99b295a36bcbf3b5db0e73b84ea7e2d411cc0) - **protocols**: 🐛 fix gemini streaming tool calls incorrectly mapped to stop on traffic-type metadata *(commit by [@jorben](https://github.com/jorben))*
- [`e2caab1`](https://github.com/tiylabs/tiygate/commit/e2caab1df77efc191c29517baca5564abcbd9af5) - **protocols**: 🐛 preserve tool call arguments in gemini to messages transcode *(commit by [@jorben](https://github.com/jorben))*
- [`99dc2b1`](https://github.com/tiylabs/tiygate/commit/99dc2b15d98b09f0c58aa4b1bd3b008db41ba5e2) - **webui**: 🐛 align pagination colors with active theme properties *(commit by [@jorben](https://github.com/jorben))*
- [`7cc3580`](https://github.com/tiylabs/tiygate/commit/7cc3580b811b6fc25e38e8b7dea761b22948afee) - **ui**: 🐛 handle null and undefined values in JsonViewer *(commit by [@jorben](https://github.com/jorben))*
- [`6d5d26a`](https://github.com/tiylabs/tiygate/commit/6d5d26a39efc31746f211e489f98028d4963e9bb) - **store**: 🐛 qualify column name in epoch increment SQL query *(commit by [@jorben](https://github.com/jorben))*
- [`29b6693`](https://github.com/tiylabs/tiygate/commit/29b6693c5a5e86582615229b81446e5d90fcf648) - **ui**: 🐛 auto-migrate stale localhost cache in production *(commit by [@jorben](https://github.com/jorben))*
- [`96fbc3b`](https://github.com/tiylabs/tiygate/commit/96fbc3bcc0d28c611a347d76c992c17c2a2f701e) - **store**: 🐛 cast DATE(ts) to TEXT in token stats aggregation query *(commit by [@jorben](https://github.com/jorben))*
- [`c29928c`](https://github.com/tiylabs/tiygate/commit/c29928c7a731f368cb72217579c393042e86f75b) - **store**: 🐛 cast average metrics to double precision in SQL aggregation *(commit by [@jorben](https://github.com/jorben))*
- [`f93ac8d`](https://github.com/tiylabs/tiygate/commit/f93ac8d9f956daae46af8a143d5f2134dc2a6040) - **store**: 🔧 embed migration SQL files using rust-embed for packaged builds *(commit by [@jorben](https://github.com/jorben))*
- [`9cc1f74`](https://github.com/tiylabs/tiygate/commit/9cc1f74795feed63f927e2e9f0f86df0a89b801f) - **webui**: 🐛 prevent theme flash on initial load *(commit by [@jorben](https://github.com/jorben))*
- [`d2103bc`](https://github.com/tiylabs/tiygate/commit/d2103bce5e562f0df9f1d3eea67b1118fb7e6ea1) - **images**: 🐛 Use JSON serialization for deterministic snapshot *(commit by [@jorben](https://github.com/jorben))*
- [`c196fa2`](https://github.com/tiylabs/tiygate/commit/c196fa228c1f592010643dbc346b5ed018f89011) - **desktop**: 🐛 resolve Windows build and runtime issues *(commit by [@jorben](https://github.com/jorben))*

### :recycle: Refactors
- [`26d30df`](https://github.com/tiylabs/tiygate/commit/26d30dff8a4b7712577361700cb52e7142d5629b) - **auth**: ♻️ consolidate AuthApplier implementations into crates/auth *(commit by [@jorben](https://github.com/jorben))*
- [`6a77a51`](https://github.com/tiylabs/tiygate/commit/6a77a5103844b07bfe245d8f6dd9a1a112e73ca8) - **ui**: ♻️ extract useStickyTableScroll hook for sticky table columns *(commit by [@jorben](https://github.com/jorben))*

### :white_check_mark: Tests
- [`56a891b`](https://github.com/tiylabs/tiygate/commit/56a891bcf6089b59bc7a34c5e72b6881531aa4a8) - **images**: ✅ add snapshot for image generations decode request *(commit by [@jorben](https://github.com/jorben))*

### :wrench: Chores
- [`2a8d283`](https://github.com/tiylabs/tiygate/commit/2a8d2839339fe300087bc5d09b1ff23e66261c2e) - **ci**: 🔧 add Docker, GitHub Actions workflows and project config *(commit by [@jorben](https://github.com/jorben))*
- [`bc924f5`](https://github.com/tiylabs/tiygate/commit/bc924f52d52a20fbacedd68b5d66fdf224285393) - 🔧 use env_file for docker-compose service config *(commit by [@jorben](https://github.com/jorben))*
- [`cca153f`](https://github.com/tiylabs/tiygate/commit/cca153fed6d0b89242d835dd617af65524bc18fb) - **build**: 🔧 make webui dependency installation idempotent *(commit by [@jorben](https://github.com/jorben))*
- [`6ab5ac5`](https://github.com/tiylabs/tiygate/commit/6ab5ac554c6fd175fe82ea7ef0ecf9b26ae61052) - **build**: 🔧 remove updater artifacts and simplify build pipeline *(commit by [@jorben](https://github.com/jorben))*
- [`7301840`](https://github.com/tiylabs/tiygate/commit/73018405abd6004ac3f5a19d610937ac0553b472) - **ci**: 🔧 bump macOS runner to macos-26 *(commit by [@jorben](https://github.com/jorben))*

[0.1.0]: https://github.com/tiylabs/tiygate/compare/0.0.1...0.1.0
[0.1.1]: https://github.com/tiylabs/tiygate/compare/0.1.0...0.1.1
[0.1.2]: https://github.com/tiylabs/tiygate/compare/0.1.1...0.1.2
[0.1.3]: https://github.com/tiylabs/tiygate/compare/0.1.2...0.1.3
[0.1.4]: https://github.com/tiylabs/tiygate/compare/0.1.3...0.1.4
[0.1.5]: https://github.com/tiylabs/tiygate/compare/0.1.4...0.1.5
[0.1.6]: https://github.com/tiylabs/tiygate/compare/0.1.5...0.1.6
[0.1.7]: https://github.com/tiylabs/tiygate/compare/0.1.6...0.1.7
