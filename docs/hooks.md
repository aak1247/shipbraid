# 外部执行器协议 v1

调度器只管理请求、所有权与验收，具体执行通过管理员配置的 hook 交接。每条 lane 选择 `scripted` 或 `hooks`；同一个项目的 dev/prod 使用不同 lane、令牌权限和配置指纹。

## 配置与三个传输

```toml
[lanes.executor]
kind = "hooks"
[lanes.executor.dispatch]
kind = "command"
program = "/usr/local/bin/my-ci-bridge"
args = ["dispatch"]
cwd = "/srv/ci-bridge"
timeout_ms = 10000
[lanes.executor.observe]
kind = "script"
path = "/srv/ci-bridge/observe.py"
interpreter = "/usr/bin/python3"
args = []
cwd = "/srv/ci-bridge"
timeout_ms = 10000
[lanes.executor.verify]
kind = "webhook"
url = "https://ci.example.com/hooks/verify"
token_env = "CI_VERIFY_TOKEN"
timeout_ms = 10000
```

这是协议配置示意，不是已经部署的 CI 地址。程序、脚本、参数和 URL 只由服务器配置确定，调用方不能在请求中提供命令。命令/脚本使用固定 argv，没有 shell 字符串插值；JSON 从 stdin 输入、stdout 输出。Webhook POST 相同 JSON，bearer 从环境读取，不跟随重定向；HTTP 只允许 loopback，其余地址要求 HTTPS。

所有 hooks lane 的 `allow_preemption` 必须为 false；v1 不提供 cancel hook。旧发布脚本整个任务按不可抢占处理，不能以 kill 或超时作为停止证明。需要执行任意 shell 脚本时用显式 interpreter + 固定 script path，仍由管理员审查该脚本。

## 信封与回执

请求：

```json
{"version":1,"operation":"dispatch","command":{},"run":{},"lane":{}}
```

`command` 是持久命令（observe 为 null），`run` 为被冻结的执行实体，包含 id、generation、lane_id、target、executor_ref；`lane` 为已授权配置快照。完整字段见 Rust [domain.rs](../src/domain.rs)、[hooks.rs](../src/hooks.rs)。

响应：

```json
{"version":1,"run_id":"<uuid>","generation":1,"executor_ref":"<stable-id>","status":"started","message":"handoff complete","logs":[{"sequence":1,"level":"info","message":"external job started"}]}
```

status 可为 started/running/completed/failed/unknown/verified。started/running 必须有稳定身份；所有响应须匹配 run_id/generation，已有 executor_ref 不得替换。logs 使用该执行内稳定单调的源序号，服务端持久去重并有界暴露。不要在协议消息或日志中返回凭据。

- dispatch 只做快速交接，长构建由独立的持久监督器执行。HTTP 202/传输成功不等于任务已经开始或成功；不明响应会保留通道所有权。
- observe 根据稳定任务身份查询真实状态；completed 仅允许进入 verifying，不满足任何请求。
- verify 是只读、独立、可重新执行的环境观察。只有它的 verified + receipt 才可能满足请求；receipt 的观察时间必须处于这次调用窗口内，远端验证器需时钟同步；receipt 含 id、run_id、generation、observed_at、source_revision、component_digests、profile_digest、verification_report、result。
- 非零退出、超时、错误 JSON、不匹配身份或证据都不能被转换为成功。环境已变化或无法证明停止时保持 unknown。

调用 dispatch 前服务端先持久化不可抢占区间和命令投递意图；重启只观察，绝不盲重发 dispatch。Verify 投递后丢响应可重新只读验收，但仍需新观察与匹配回执。配置指纹变化时不会用新 hook 处理旧 run，需恢复被冻结配置并核对状态。

## 失败后的停止证明与恢复

`failed` 本身不证明部署进程及子任务已经停止。observe/verify 可额外返回：

```json
{"version":1,"run_id":"<uuid>","generation":1,"executor_ref":"<stable-id>","status":"failed","failure_proof":{"observed_at":"<fresh UTC timestamp>","evidence":{"supervisor":"stopped","process_tree":"empty"}}}
```

上例只展示字段形状，不能用静态文本替代真实证据。管理员配置的观察器必须核对完整冻结目标、run/generation、稳定执行器身份及监督器调用身份，独立确认终态和整个进程子树已停止。`observed_at` 必须落在此次调用窗口内；evidence 必须是非空对象，最多 8 KiB。dispatch、不匹配身份、过期证明或带 receipt 的失败证明均被拒绝。完整证据追加到 `failure_stop_verified` 审计事件。

确认后执行变为 `failed`、释放该执行所有权，通道进入 `failure_paused`，相关请求进入 `needs_attention`，由操作者显式重试。它不产生成功 receipt，不满足请求，也不承诺环境没有变化；缺少证明时继续 `unknown` 并保留所有权。

旧冻结观察器无法生成该字段时，可在同一 lane 添加独立的只读失败观察器：

```toml
[lanes.failure_observe]
kind = "script"
path = "/srv/ci-bridge/failure-observe.py"
interpreter = "/usr/bin/python3"
args = []
timeout_ms = 10000
```

该配置支持与其他 hook 相同的 command/script/webhook 传输，必须由管理员审查为只读，输入仍为 `operation=observe`。协调器只接受其 `failed + failure_proof`，忽略其他结果；它不能 dispatch、接纳 running 或验收成功。该补充观察器不改变原执行 profile，原 dispatch/observe/verify 路径和参数必须保持冻结值，目标、generation、所有权检查仍然生效。审计证据记录补充观察器配置的 SHA-256。显式 `confirm_stopped` 也要求重新获取同样的新鲜证据，人工备注不能代替证明。

AutoTask bridge 核对持久 request/result、systemd Id/InvocationID、非零退出状态及前后稳定的监督器快照，并递归检查 cgroup 子树及 `populated=0`；不存在的 unit、活动后代、不可读证据或检查期间变化均不生成证明。旧 result 不被改写，额外要求监督器 ExecStart 精确绑定该 request 路径、退出时间与持久结果相差不超过 10 秒。

相同的观察摘要与 unknown 原因只记录一次，去重标记随账本持久化；新状态或原因仍会记录。带新源序号的真实日志即使文字相同也会保存，历史重复审计行不会删除。

## HTTP 与 CLI

```bash
shipbraid --token-file ~/.config/shipbraid/operator.token submit \
  --project autotask --environment dev --revision <full-sha> --components app \
  --idempotency-key <stable-intent-key>
```

HTTP `POST /v1/requests` 同样接受 project + environment，或显式 lane_id（二者互斥）。先解析为唯一已授权 lane，再计算幂等正文；同项目不同环境不会共享执行。存在多个匹配 lane 时必须显式选择 lane_id。完整接口见 [api.md](api.md)。

## AutoTask 本机安装

以下使用统一后的 `shipbraid` 标识；新安装与已有旧安装的区别见[改名与升级说明](naming.md)。2026-10-16 的现场验收保留当时的服务名与路径，不表示现有运行实例已完成改名。

在本机开发主机运行以下命令只安装服务，不自动提交发布：

```bash
cargo build --release --locked
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
python3 deploy/install-local.py
```

安装器创建常驻服务 `shipbraid.service`，UI/API 同源 `http://127.0.0.1:8787`；配置/凭据位于 `~/.config/shipbraid/`，数据库、执行证据和不可变程序发布目录位于 `~/.local/share/shipbraid/`。令牌文件 mode0600；Dashboard 令牌仍只保存于当前页面内存中。

远程查看开发 Dashboard 可用 SSH 转发：`ssh -L 8787:127.0.0.1:8787 yahui@172.168.3.70`，再打开本机地址。公网入口必须另行配置 HTTPS；本次不创建公网 DNS/Ingress。

局域网入口样例地址为 `http://172.168.3.70:8787`。独立 user socket 仅绑定该 IPv4 地址，使用系统自带的 `systemd-socket-proxyd` 转发到 `127.0.0.1:8787`，沿用 API 的 Bearer 鉴权；协调器仍监听 loopback，启用入口不需要重启协调器或正在运行的部署任务。登录令牌从主机 `~/.config/shipbraid/admin.token` 读取，不随页面分发。该 HTTP 入口用于局域网 Dashboard；CLI 保留 loopback HTTP / HTTPS 的地址校验。

局域网入口的固定主机 unit 在 [shipbraid-lan.socket](../deploy/shipbraid-lan.socket) 和 [shipbraid-lan.service](../deploy/shipbraid-lan.service)，安装与停用方式如下：

```bash
install -m 644 deploy/shipbraid-lan.socket deploy/shipbraid-lan.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now shipbraid-lan.socket
# 停用局域网入口，协调器和发布任务继续运行：
systemctl --user disable --now shipbraid-lan.socket
systemctl --user stop shipbraid-lan.service
```

AutoTask 的 `pnpm deploy:local` 通过 HTTP 提交并等待，具体 worker 在独立 systemd user unit 中执行安装时固定的已提交部署脚本。它拒绝脏 checkout、强制重建 backend/worker，并独立检查源码、实际进程、构建产物及健康。生产 lane 默认 disabled，桥接器本身也拒绝 prod 目标。真实本机验收记录见对应 feature 的 [执行计划](features/iter-20261016-executor-integration/execution-plan.md)。

首次安装从开发部署 checkout 的实际提交初始化受信发布线；后续本机客户端仅快进更新固定镜像引用。共享分支与当前开发环境分叉时，需先合并历史，不能让协调入口偷偷覆盖其他会话的发布。

升级安装器拒绝覆盖仍 active 的协调器。先 drain，等活动任务结算并核对所有权，再停止服务、安装新 release。停止协调器不会停止独立执行的发布任务。

Dashboard 的独立静态资源发布目录为 `~/.local/share/shipbraid/dashboard-releases/<commit>-<digest>/web`。仅前端更新时，构建 `web/dist`，复制到新的不可变目录，切换私有配置中的 `dashboard_dir` 后重启协调器；执行器路径、lane 配置和 profile 指纹保持原值，启动后核对现有任务所有权。完整程序/执行器升级仍遵循上面的排空流程。宽详情与 tail 日志的浏览器及发布证据见 [dashboard-tail-20261016.json](acceptance/dashboard-tail-20261016.json)。
