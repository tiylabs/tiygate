# RHEL 系发行版主机层调优方案

适用范围：**基于 RHEL、使用 `dnf`（或兼容的 `yum`）作为包管理器的发行版**，包括 Rocky Linux、AlmaLinux、RHEL 本身、CentOS Stream、Oracle Linux 等。下文命令以 `dnf` 为主，若目标系统只有 `yum`，直接替换命令名即可（参数完全兼容）。

针对 TiyGate 的部署特性——Rust 异步 AI 网关，承载大量入向 SSE 长连接、大量出向到海外 LLM 供应商的连接（高 RTT/易 bufferbloat），且全量请求日志落库 Postgres，单机 `docker-compose` 部署（tiygate + postgres + redis）——整理的**主机系统层**配置方案。

本方案只覆盖主机系统层。TiyGate 自身的路由策略、入口限流、上游超时等运行时参数走 Admin 控制台 `/admin/ui/settings`（见 `docs/deployment-operations.md`），两者互补、不重叠：系统层管资源上限，应用层管业务策略。

**推荐按步骤逐项手动执行**，每一步都可以先验证效果、确认无误后再进入下一步，避免一次性改动过多、出问题不好定位排查。

## 步骤总览

| 步骤 | 内容 |
| --- | --- |
| 1 | 检查 BBR 是否可用，不可用则安装 |
| 2 | 加载 BBR 模块并临时启用验证 |
| 3 | 持久化 sysctl（网络调优 + BBR） |
| 4 | 主机级文件描述符 / 进程数限制 |
| 5 | Docker daemon 配置（默认 ulimits + 可靠性参数） |
| 6 | Redis 关闭透明大页（THP） |
| 7 | 整体验收 |

> 容器重启恢复无需额外托管：三个服务在 `docker-compose.yml` 里已配置 `restart: unless-stopped`，配合 `sudo systemctl enable docker`（宿主机重启后 dockerd 自启），dockerd 启动时会自动按策略拉起已存在的容器，链路已闭环，不需要额外的 systemd unit 包一层 `docker compose up -d`。

---

## 步骤 1：检查 BBR 是否可用

```bash
# 当前拥塞控制算法（默认通常是 cubic）
sysctl net.ipv4.tcp_congestion_control

# 内核已编译、可直接启用的算法列表
sysctl net.ipv4.tcp_available_congestion_control

# bbr 内核模块是否存在（主流 RHEL 系发行版的 5.14+ 内核一般自带）
modinfo tcp_bbr

# 当前内核版本（BBR 需要 >= 4.9）
uname -r
```

- 若 `tcp_available_congestion_control` 已包含 `bbr` → 跳到步骤 2。
- 若 `modinfo tcp_bbr` 报 `Module tcp_bbr not found` → 继续下面的安装小节。

**BBR 不可用时如何安装：**

```bash
# 1. 确认当前内核包
rpm -q kernel kernel-core

# 2. 更新到仓库最新内核（各 RHEL 系发行版官方内核基本都带 tcp_bbr，缺失多是精简云镜像）
sudo dnf update -y kernel kernel-core kernel-modules
sudo reboot

# 3. 重启后再检查
uname -r
modinfo tcp_bbr
```

若更新官方内核后仍无该模块（极少见，通常是云厂商定制精简内核），才考虑装 elrepo mainline 内核（以下以 RHEL 9 系为例，需按发行版大版本选择对应的 `elrepo-release` 包）：

```bash
sudo dnf install -y https://www.elrepo.org/elrepo-release-9.el9.elrepo.noarch.rpm
sudo dnf --enablerepo=elrepo-kernel install -y kernel-ml
sudo grubby --set-default /boot/vmlinuz-$(rpm -q kernel-ml --queryformat '%{VERSION}-%{RELEASE}.%{ARCH}')
sudo reboot
```

> 装 mainline 内核会脱离发行版官方内核维护轨道，属于最后手段，非必要不建议。

---

## 步骤 2：加载 BBR 模块并临时启用验证

```bash
sudo modprobe tcp_bbr
lsmod | grep bbr                                  # 确认已加载

sudo sysctl -w net.core.default_qdisc=fq
sudo sysctl -w net.ipv4.tcp_congestion_control=bbr

sysctl net.ipv4.tcp_congestion_control            # 期望: bbr
sysctl net.core.default_qdisc                     # 期望: fq
```

这一步是临时生效（重启会丢失），先观察一段时间网络状况正常，再进入下一步做持久化。

---

## 步骤 3：持久化 sysctl 配置

创建 `/etc/sysctl.d/99-tiygate.conf`：

```ini
# ------------------------------------------------------------------------------
# TiyGate host-level kernel tuning (RHEL-family: RHEL / Rocky / AlmaLinux /
# CentOS Stream / Oracle Linux, using dnf or compatible yum)
# Target: /etc/sysctl.d/99-tiygate.conf
# Apply: sudo sysctl --system
# ------------------------------------------------------------------------------
# Rationale: TiyGate is a Rust async AI gateway that (a) terminates many
# inbound client connections including long-lived SSE streams, and (b) opens
# many outbound connections to overseas LLM providers (high RTT, prone to
# bufferbloat). All request/response payloads are logged to Postgres.
# ------------------------------------------------------------------------------

# ---- Connection backlog: absorb bursts of new inbound connections ----------
net.core.somaxconn = 65535
net.core.netdev_max_backlog = 65535
net.ipv4.tcp_max_syn_backlog = 65535

# ---- Outbound port pressure: many concurrent connections to LLM providers --
net.ipv4.ip_local_port_range = 1024 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fin_timeout = 15

# ---- TCP keepalive: network-layer safety net for long idle SSE streams -----
# (App layer already enforces idle/TTFB timeouts; this is a fallback so dead
# peers/NAT sessions get reclaimed even if the app-layer timer is disabled.)
net.ipv4.tcp_keepalive_time = 60
net.ipv4.tcp_keepalive_intvl = 10
net.ipv4.tcp_keepalive_probes = 6

# ---- Conntrack: if traffic passes through firewalld/NAT, long streaming ----
# connections can silently exhaust the default conntrack table.
net.netfilter.nf_conntrack_max = 262144

# ---- Socket buffers: throughput headroom for concurrent streams -----------
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216

# ---- Memory / writeback: Postgres does heavy fsync for request logging ----
vm.swappiness = 10
vm.overcommit_memory = 1
vm.dirty_ratio = 10
vm.dirty_background_ratio = 5

# ---- File descriptor ceiling (system-wide) ---------------------------------
fs.file-max = 2097152

# ---- BBR congestion control -------------------------------------------------
# Better throughput / lower latency jitter on high-RTT, bufferbloat-prone
# links (outbound to overseas LLM providers) and for long-lived SSE streams
# (inbound direction, gateway -> client). Requires fq qdisc to fully benefit
# from BBR's pacing. Mainstream RHEL-family kernels (5.14+) ship tcp_bbr as a
# loadable module.
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
```

应用：

```bash
echo "tcp_bbr" | sudo tee /etc/modules-load.d/bbr.conf
sudo sysctl --system
```

验证：

```bash
sysctl net.ipv4.tcp_congestion_control   # bbr
sysctl net.core.default_qdisc             # fq
```

回退：`sudo rm /etc/sysctl.d/99-tiygate.conf && sudo sysctl --system`

---

## 步骤 4：主机级文件描述符 / 进程数限制

`docker-compose.yml` 已为 tiygate 容器设置 `nofile: 1048576`，但主机默认值（通常 1024）和 dockerd 自身也要跟上，否则容器级限制形同虚设。

创建 `/etc/security/limits.d/99-tiygate.conf`：

```
# ------------------------------------------------------------------------------
# TiyGate host-level ulimits (RHEL-family)
# Target: /etc/security/limits.d/99-tiygate.conf
# ------------------------------------------------------------------------------
*    soft nofile 1048576
*    hard nofile 1048576
*    soft nproc  65535
*    hard nproc  65535
```

> **需要重新登录 SSH 会话**（或新开 shell）才生效，不影响已存在的会话/进程。

验证（新开 shell）：

```bash
ulimit -n    # 期望 1048576
ulimit -u    # 期望 65535
```

回退：`sudo rm /etc/security/limits.d/99-tiygate.conf`

---

## 步骤 5：Docker daemon 配置

```bash
# 先检查是否已有配置，避免覆盖已有的私有仓库/代理等设置
cat /etc/docker/daemon.json 2>/dev/null || echo "文件不存在，可新建"
```

目标内容（若已有配置，手动合并以下字段，不要整体覆盖）：

```json
{
  "default-ulimits": {
    "nofile": { "Name": "nofile", "Soft": 1048576, "Hard": 1048576 }
  },
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "50m",
    "max-file": "5"
  },
  "live-restore": true,
  "storage-driver": "overlay2"
}
```

说明：
- `default-ulimits` 让 postgres/redis 容器也受益，无需在 compose 里逐个重复配置。
- `live-restore: true` 呼应项目"reliability first"原则——之后 dockerd 升级/重启时容器不中断（本次首次开启除外）。
- `log-opts` 是全局兜底，避免以后新增容器没配日志上限把磁盘打满。

**这一步改完必须重启 dockerd 才生效，会导致运行中的容器短暂中断一次**：

```bash
sudo systemctl restart docker
docker info | grep -i "default runtime\|live restore\|storage driver"
```

回退：`sudo rm /etc/docker/daemon.json && sudo systemctl restart docker`

---

## 步骤 6：Redis 关闭透明大页（THP）

Redis 后台 `bgsave`/AOF rewrite 走 fork+COW，THP 开启会导致明显延迟毛刺，Redis 官方强烈建议关闭。

```bash
cat /sys/kernel/mm/transparent_hugepage/enabled     # 查看当前状态
echo never | sudo tee /sys/kernel/mm/transparent_hugepage/enabled   # 立即生效
```

持久化（新建 systemd service，在 docker 启动前执行一次）：

```bash
sudo tee /etc/systemd/system/disable-thp.service <<'EOF'
[Unit]
Description=Disable Transparent Huge Pages (THP) for Redis
Before=docker.service

[Service]
Type=oneshot
ExecStart=/bin/sh -c "echo never > /sys/kernel/mm/transparent_hugepage/enabled"
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now disable-thp.service
systemctl status disable-thp.service
```

回退：`sudo systemctl disable --now disable-thp.service && sudo rm /etc/systemd/system/disable-thp.service`

---

## 步骤 7：整体验收

```bash
sysctl net.ipv4.tcp_congestion_control          # bbr
sysctl net.core.default_qdisc                    # fq
lsmod | grep bbr                                  # tcp_bbr 已加载
ulimit -n                                          # 1048576（新 shell）
docker info | grep -i "live restore"               # true
cat /sys/kernel/mm/transparent_hugepage/enabled     # [never]
systemctl is-enabled docker                         # enabled
docker compose ps                                   # 三个服务都 healthy，RESTART POLICY 均为 unless-stopped
```

---

## 补充：其他可选优化项

以下几项未在上面的分步方案中展开，视情况按需处理：

- **磁盘与文件系统**：`/var/lib/docker` 建议独立分区，XFS + `ftype=1`（overlay2 存储驱动强制要求），mount 选项加 `noatime`。Postgres 的 `postgres_data` volume 承载全量请求日志写入，是 I/O 压力最大的组件，建议单独挂盘或用 SSD/NVMe，避免 WAL fsync 被其他容器 I/O 拖慢。
- **时间同步**：`sudo systemctl enable --now chronyd`。项目有分布式追踪（`traceparent`/`tracestate`）和 TLS 证书校验依赖准确时钟，时间漂移会导致 trace 时间线错乱或上游 TLS 握手失败。
- **安全与最小化**：firewalld 只放行数据面端口（如 `3000`）；SELinux 保持 `enforcing`；关闭不需要的服务（`avahi-daemon`、`cups` 等）；可选装 `fail2ban` 保护 SSHD，与应用层 Admin API 防护互补。
- **可观测性**：`journalctl` 限制日志占用（`/etc/systemd/journald.conf` 设 `SystemMaxUse=500M`）；可选部署 `node_exporter` + `cAdvisor` 采集主机/容器指标，与 TiyGate 自带的 Admin 控制台请求分析互补。

## 注意事项

- 本方案只覆盖**主机系统层**，与 TiyGate 应用层的运行时可调参数（Admin 控制台热加载）是互补关系，不重叠。
- BBR 随时可无损切回：`sudo sysctl -w net.ipv4.tcp_congestion_control=cubic`，不需要重启。
- 步骤 5（docker daemon 重启）和步骤 4（重新登录 shell）是唯二两个有短暂中断/需要重新登录的步骤，建议安排在维护窗口内执行。
- 不同 RHEL 系发行版在包名、SELinux 策略细节、`elrepo-release` 版本号上可能略有差异（例如 CentOS Stream/Oracle Linux 与 Rocky/AlmaLinux 的内核 rpm 命名基本一致，但第三方仓库地址需按大版本号核对），执行前建议先用 `cat /etc/os-release` 确认目标系统的具体发行版和大版本号。
