# 路由策略详解

> tiygate 的路由引擎在每个请求到达时，根据当前策略对目标 backend 集合进行排序，并按排序结果依次尝试。本文档详细阐述四种内置策略的排序逻辑、健康检查机制、延迟指标收集，以及回退/重试执行循环。

## 1. 架构概览

### 1.1 核心抽象

路由系统由三个核心组件构成：

| 组件 | 位置 | 职责 |
|------|------|------|
| `Strategy` trait | `crates/core/src/routing/mod.rs` | 将 `&[RoutingTarget]` 排序为尝试序列 |
| `HealthRegistry` | `crates/core/src/routing/mod.rs` | 熔断器 + 冷却 + EWMA 延迟指标，per-instance 状态 |
| `execute_with_fallback` | `crates/server/src/ingress/fallback.rs` | 执行循环：排序 → 尝试 → 健康检查 → 回退/重试 |

`Strategy` trait 定义如下：

```rust
pub trait Strategy: Send + Sync {
    /// Sort/select targets from the routing chain.
    /// Returns targets in the order they should be tried.
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget>;
}
```

策略只负责排序，不负责执行。执行循环由 `execute_with_fallback` 统一处理。

### 1.2 策略枚举

```rust
pub enum RoutingStrategyName {
    Weighted,   // 默认：加权随机洗牌
    Priority,   // 按权重降序
    Cooldown,   // 健康优先
    Latency,    // 健康优先 + 最低 EWMA 延迟
}
```

策略选择优先级：**per-route 覆盖 > 网关全局默认**。

```rust
// crates/server/src/ingress/fallback.rs:97-103
let effective_strategy = state
    .current_config()
    .routing_table
    .resolve_strategy(virtual_model)   // per-route 覆盖优先
    .unwrap_or(state.tunables().routing_strategy);  // 否则用网关默认
```

### 1.3 策略工厂

```rust
// crates/server/src/ingress/mod.rs:36-65
fn build_strategy(name, health) -> (Box<dyn Strategy>, &'static str) {
    match name {
        Weighted  => (Box::new(WeightedStrategy),  "WeightedStrategy"),
        Priority  => (Box::new(PriorityStrategy),  "PriorityStrategy"),
        Cooldown  => (Box::new(CooldownStrategy::new(health)), "CooldownStrategy"),
        Latency   => (Box::new(LatencyStrategy::new(health)),  "LatencyStrategy"),
    }
}
```

`Cooldown` 和 `Latency` 需要 `HealthRegistry` 句柄；`Weighted` 和 `Priority` 是无状态的。

### 1.4 热重载

后台任务 `spawn_tunables_reloader` 轮询 config epoch，从 `settings` 表的 `ROUTING_DEFAULT_STRATEGY` 读取策略名，经 `RoutingStrategyName::parse()` 解析（失败回退 `Weighted`），通过 `ArcSwap` 原子发布 `RuntimeTunables`。数据面无锁读取，热重载无需重启。

---

## 2. 健康检查机制

`HealthRegistry` 是所有动态策略的基础设施，维护每个 backend 的三类状态。状态是 per-instance 的，不跨副本共享。

### 2.1 健康状态枚举

```rust
pub enum RoutingTargetHealth {
    Healthy,
    CircuitBroken { until: Instant },  // 连续失败触发熔断
    Cooling { until: Instant },        // 429 限流冷却
}
```

冷却（Cooling）优先于熔断判断——一个目标可能同时处于熔断恢复期和冷却期，冷却截止时间更近则先解除冷却。

### 2.2 熔断器（Circuit Breaker）

**触发条件**：连续失败次数 `consecutive_failures >= failure_threshold`（默认 3）。

**恢复机制**：分层指数退避，默认恢复层级为 `[60s, 180s, 600s, 1800s]`：

```rust
fn recovery_duration_for(&self, consecutive_failures: u32) -> Duration {
    let overflow = consecutive_failures.saturating_sub(self.failure_threshold) as usize;
    let tier_index = overflow.min(self.recovery_tiers.len() - 1);
    self.recovery_tiers[tier_index]
}
```

- 首次熔断（第 3 次连续失败）→ 恢复窗口 60s
- 半开放探针失败 → 升级到 180s
- 再次失败 → 600s → 1800s（上限，后续重复）

**半开放探针**：恢复窗口过后 `is_healthy()` 返回 true，允许一个请求通过。成功 → 重置退避到第一层；失败 → 升级到下一层。

**成功重置**：任何一次成功请求都会将 `consecutive_failures` 归零并清除冷却状态：

```rust
pub fn record_success(&self, target_key: &str) {
    if let Some(state) = states.get_mut(target_key) {
        state.consecutive_failures = 0;
        state.cooling_until = None;
        state.cooling_reason = None;
    }
}
```

### 2.3 冷却（Cooling）

冷却用于处理 429 RateLimited 响应。当上游返回 429 时：

1. 解析 `Retry-After` 头（秒数），无该头则默认 30s
2. 调用 `apply_cooling(target_key, duration, "rate_limited")` 设置冷却截止时间
3. 冷却期内 `is_healthy()` 返回 false

对于 401/403 Auth 错误，施加 300s 冷却并跳过同 `account_label` 的其他目标。

### 2.4 延迟指标收集（EWMA）

每个请求完成后（无论成功或失败），`execute_with_fallback` 调用 `record_latency_ms()` 记录 hop 延时：

```rust
pub fn record_latency_ms(&self, target_key: &str, latency_ms: u64) {
    let entry = latencies.entry(target_key.to_string())
        .or_insert(LatencyEwma { ewma: 0.0, samples: 0 });
    if entry.samples == 0 {
        entry.ewma = latency_ms as f64;       // 第一个样本直接赋值
    } else {
        entry.ewma = 0.3 * (latency_ms as f64) + 0.7 * entry.ewma;
    }
    entry.samples += 1;
}
```

- **α = 0.3**：新观测权重 30%，历史权重 70%
- **半衰期 ≈ 1.94 个样本**：约 2 个请求后旧数据影响力减半
- **有效窗口 ≈ 10 个样本**：第 10 步后单样本权重 < 1%，第 15 步后累计覆盖 99.5%

| 距今步数 | 权重 | 累计权重 |
|---------|------|---------|
| 0（最新） | 30.0% | 30.0% |
| 1 | 21.0% | 51.0% |
| 2 | 14.7% | 65.7% |
| 3 | 10.3% | 76.0% |
| 5 | 5.0% | 88.2% |
| 10 | 0.8% | 98.0% |

---

## 3. 四种策略详解

### 3.1 Weighted（加权随机洗牌）

**结构体**：`WeightedStrategy`（无状态）

**逻辑**：加权随机洗牌（Weighted Random Shuffle），不是简单的"加权随机选一个"。算法逐轮从剩余目标中按权重抽取一个并移除，产出一个完整的有序序列。

```rust
impl Strategy for WeightedStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        // ...
        let mut remaining: Vec<(usize, &RoutingTarget)> = targets.iter().enumerate().collect();
        let mut result = Vec::with_capacity(targets.len());
        let mut rng = rand::thread_rng();
        while !remaining.is_empty() {
            let total: f64 = remaining.iter().map(|(_, t)| t.weight.max(0.0)).sum();
            let mut pick = rand::Rng::gen_range(&mut rng, 0.0..total);
            for (i, (_, t)) in remaining.iter().enumerate() {
                pick -= t.weight.max(0.0);
                if pick <= 0.0 {
                    // 选中此目标，从剩余列表移除
                    let (_, target) = remaining.remove(i);
                    result.push(target);
                    break;
                }
            }
        }
        result
    }
}
```

**行为特征**：
- 每个请求都会重新洗牌，序列是随机的
- 高权重目标更可能排在前面，但不保证总是第一
- 所有权重 ≤ 0 时退化为原始顺序
- 不感知健康状态和延迟——熔断目标仍可能被排到前面，但执行循环会跳过

**示例**：3 个 backend A(weight=3)、B(weight=2)、C(weight=1)，总权重 6：
- A 排第一的概率 ≈ 50%（3/6）
- B 排第一的概率 ≈ 33%（2/6）
- C 排第一的概率 ≈ 17%（1/6）

### 3.2 Priority（优先级排序）

**结构体**：`PriorityStrategy`（无状态）

**逻辑**：按 `weight` 降序稳定排序。weight 值最大的排最前，相同 weight 保持原始顺序。

```rust
impl Strategy for PriorityStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        let mut sorted: Vec<&RoutingTarget> = targets.iter().collect();
        sorted.sort_by(|a, b| {
            b.weight.partial_cmp(&a.weight).unwrap_or(std::cmp::Ordering::Equal)
        });
        sorted
    }
}
```

**行为特征**：
- 确定性排序，无随机性
- weight 在此策略中被当作优先级使用：值越大优先级越高
- 排第一的目标始终是 weight 最大的那个，除非它被熔断跳过
- 不感知健康状态和延迟

**与 Weighted 的区别**：Priority 是确定性的，高权重目标始终排第一；Weighted 是概率性的，高权重目标只是更可能排第一。

### 3.3 Cooldown（健康优先）

**结构体**：`CooldownStrategy`（持有 `Arc<HealthRegistry>`）

**逻辑**：按健康状态分组排序，健康目标（`is_healthy() == true`）排前面，不健康目标排后面。组内保持原始顺序。

```rust
impl Strategy for CooldownStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget>) -> Vec<&'a RoutingTarget> {
        let mut sorted: Vec<&RoutingTarget> = targets.iter().collect();
        sorted.sort_by_key(|t| {
            if self.health.is_healthy(&t.health_key()) { 0u8 } else { 1u8 }
        });
        sorted
    }
}
```

**行为特征**：
- 健康目标组内不区分延迟，保持原始配置顺序
- 熔断/冷却目标排到尾部，执行循环仍可能跳过它们
- 适合不需要延迟感知、只需跳过故障 backend 的场景

### 3.4 Latency（延迟感知）

**结构体**：`LatencyStrategy`（持有 `Arc<HealthRegistry>`）

**逻辑**：排序键为 `(healthy, latency_key)` 组合的 u128 值。健康优先，然后按 EWMA 延迟升序，未观测目标（无样本）排最前。

```rust
impl Strategy for LatencyStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        let mut sorted: Vec<&RoutingTarget> = targets.iter().collect();
        sorted.sort_by_key(|t| {
            let healthy = if self.health.is_healthy(&t.health_key()) { 0u32 } else { 1u32 };
            let latency = self.health.ewma_latency_ms(&t.health_key());
            let latency_key: u128 = match latency {
                Some(ms) => (ms as u128) & 0x0000_FFFF_FFFF_FFFF_FFFF_FFFF_FFFFu128,
                None => 0u128,  // 未观测 → 排最前，先采样
            };
            ((healthy as u128) << 64) | latency_key
        });
        sorted
    }
}
```

**排序优先级**（从高到低）：
1. 健康目标（healthy=0）排在不健康目标（healthy=1）之前
2. 健康目标中，未观测的（latency_key=0）排在已观测的之前
3. 已观测目标中，EWMA 延迟最低的排最前

**"未观测优先"设计**：新加入的 backend 没有延迟样本时 `ewma_latency_ms()` 返回 `None`，`latency_key` 为 0，排在所有已观测目标之前。这确保每个新 backend 都能获得初始请求来采集样本，避免饿死。

**赢家通吃**：一旦所有目标都有了样本，延迟最低的目标会持续独占流量，直到它的延迟上升或被熔断/冷却。

#### 请求分配示例

假设 3 个健康的 backend A、B、C，初始状态均为 None：

| 请求 | A EWMA | B EWMA | C EWMA | 排序 | 命中 | 原因 |
|------|--------|--------|--------|------|------|------|
| #1 | None | None | None | A→B→C | A | 全是 None，稳定排序保持原始顺序 |
| #2 | 100ms | None | None | B→C→A | B | B、C 仍 None，排在已观测的 A 前面 |
| #3 | 100ms | 80ms | None | C→B→A | C | 只有 C 还未观测 |
| #4 | 100ms | 80ms | 120ms | B→A→C | B | 全部已观测，B 延迟最低 |
| #5 | 100ms | 75ms | 120ms | B→A→C | B | B 持续独占 |
| #6 | 100ms | 70ms | 120ms | B→A→C | B | B 持续独占 |

**何时切换**：
- **B 延迟上升**：假设 B 突然变慢，新样本 200ms → `EWMA = 0.3×200 + 0.7×70 = 109ms`，超过 A 的 100ms → 下个请求切到 A。约需 1-2 个慢请求即可完成切换。
- **B 熔断**：连续失败 3 次 → `is_healthy()` 返回 false → 排到尾部 → 立即切换到 A。
- **B 被 429 冷却**：`apply_cooling()` → 健康检查失败 → 跳过 B。

---

## 4. 执行循环（execute_with_fallback）

所有策略的排序结果都交给 `execute_with_fallback` 执行。这是所有 ingress handler 共享的多目标回退执行器。

### 4.1 执行流程

```
请求到达
  ↓
strategy.order(targets)  →  有序目标列表
  ↓
target_index = 0
  ↓
┌─────────────────────────────────────────┐
│  target = ordered_targets[target_index]  │
│  ↓                                       │
│  is_healthy(target)?                     │
│    No → target_index++, continue         │
│    Yes → execute_one(target)             │
│  ↓                                       │
│  Success?                                │
│    Yes → record_success + record_latency │
│           → return Success               │
│    No  → record_failure + record_latency │
│           → classify_error               │
│           → TryNext / Retry / Fail       │
└─────────────────────────────────────────┘
```

### 4.2 默认预算

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `max_total_attempts` | 10 | 最大尝试次数 |
| `deadline` | 120s | 请求总超时 |
| `max_retries` | 2 | 同目标重试次数 |
| `base_delay` | 1s | 重试基础退避 |
| `max_delay` | 30s | 重试退避上限 |

### 4.3 重试退避

同目标重试时采用指数退避 + ±25% jitter：

```rust
pub fn delay_for(&self, retry_num: usize) -> Duration {
    let exp = 2u64.pow(retry_num as u32);
    let delay = self.base_delay * exp as u32;
    let clamped = delay.min(self.max_delay);
    let jitter = rand::random::<f64>() * 0.5 * clamped.as_secs_f64();
    let jittered = clamped.as_secs_f64() * 0.75 + jitter;
    Duration::from_secs_f64(jittered)
}
```

### 4.4 错误分类与决策

错误分类（`classify_error`）决定下一步动作：

| 决策 | 含义 | 触发条件 |
|------|------|---------|
| `TryNext` | 跳到下一个目标 | 不可重试错误（如 404、413） |
| `Retry` | 重试同目标 | 可重试错误（如 500、502、503） |
| `Fail` | 立即失败 | 已开始流式输出（`bytes_emitted > 0`，幂等性保护）或不可恢复错误 |

**特殊处理**：
- **429 RateLimited**：解析 `Retry-After` 头 → `apply_cooling()` → `TryNext`
- **401/403 Auth**：300s 冷却 + 跳过同 `account_label` 的所有目标
- **已流式输出**：`bytes_emitted > 0` 时强制 `Fail`，避免重复输出

### 4.5 延迟记录

无论成功还是失败，每个 hop 结束后都会记录延迟：

```rust
// 成功路径
let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0) as u64;
state.health.record_success(&health_key);
state.health.record_latency_ms(&health_key, hop_elapsed_ms);

// 失败路径
state.health.record_failure(&health_key);
state.health.record_latency_ms(&health_key, hop_elapsed_ms);
```

失败请求的延迟也被记录，这使得 `Latency` 策略能感知到慢响应（即使最终失败）。

---

## 5. 策略对比总览

| 策略 | 排序依据 | 健康感知 | 延迟感知 | 随机性 | 典型场景 |
|------|---------|---------|---------|--------|---------|
| Weighted | weight 概率 | 否 | 否 | 有 | 负载分担、灰度发布 |
| Priority | weight 降序 | 否 | 否 | 无 | 主备切换、优先级路由 |
| Cooldown | 健康状态 | 是 | 否 | 无 | 故障隔离、简单可用性保障 |
| Latency | 健康 + EWMA 延迟 | 是 | 是 | 无 | 性能优化、自动选择最快 backend |

### 决策频率

所有策略在**每个请求到达时**都重新调用 `order()` 进行排序，不存在定时刷新或决策周期。对于 `Latency` 策略，这意味着 EWMA 值的任何变化都会在下一个请求立即生效。

---

## 6. 配置与管理

### 6.1 全局默认策略

通过 Admin Console `/admin/ui/settings` 修改 `ROUTING_DEFAULT_STRATEGY`，或通过 API：

```
PUT /admin/v1/settings
{ "routing_default_strategy": "latency" }
```

### 6.2 Per-route 策略覆盖

对单条路由设置独立策略，优先级高于全局默认：

```
PUT /admin/v1/routes/{route_id}
{ "routing_strategy": "latency" }
```

`routing_strategy` 为 `null` 时继承全局默认。

### 6.3 可选值

| 值 | 策略 |
|----|------|
| `weighted` | 加权随机洗牌（默认） |
| `priority` | 优先级排序 |
| `cooldown` | 健康优先 |
| `latency` | 延迟感知 |

未知值会被 `parse()` 拒绝，回退到全局默认。

---

## 7. 扩展新策略

遵循 `Strategy` trait 扩展即可，符合项目"Extensible by design"原则：

1. 在 `RoutingStrategyName` 增加变体
2. 实现 `Strategy` trait 的 `order()` 方法
3. 在 `build_strategy` 增加匹配分支
4. 在 `RoutingStrategyName::as_str()` / `parse()` 增加序列化映射
5. 如需新指标，在 `HealthRegistry` 增加收集方法，并在 `execute_with_fallback` 的成功/失败回调中记录

---

## 附录：相关源码索引

| 文件 | 内容 |
|------|------|
| `crates/core/src/routing/mod.rs:67-108` | `RoutingStrategyName` 枚举 |
| `crates/core/src/routing/mod.rs:178-420` | `HealthRegistry`（熔断器、冷却、EWMA） |
| `crates/core/src/routing/mod.rs:570-668` | `Strategy` trait + 四种策略实现 |
| `crates/server/src/ingress/mod.rs:36-65` | `build_strategy` 策略工厂 |
| `crates/server/src/ingress/fallback.rs:67-300` | `execute_with_fallback` 执行循环 |
| `crates/server/src/ingress/mod.rs:405-460` | `spawn_tunables_reloader` 热重载 |
| `crates/server/src/config.rs:165,250-254` | 配置解析 |
| `crates/store/src/models.rs:112` | 持久化模型 |
| `crates/admin/src/handlers.rs:494,504` | Admin API |
