# 验收记录：可配置执行器与 AutoTask 开发环境接入

日期：2026-10-16。状态：ACTIVE。代码、隔离验收、CI、user service 安装和真实请求发起均已完成；该真实 run 尚未取得最终 receipt，T-104 保持进行中。

## 已通过

- Auto-deliver Rust：38 个测试、`cargo fmt --all --check`、`cargo clippy --locked --all-targets -- -D warnings` 和 debug build 通过。GitHub Actions [run 38897050033](https://github.com/aak1247/auto-deliver/actions/runs/38897050033) 对 commit `880c1de` 报告 SUCCESS。
- Dashboard：10 个前端测试、typecheck 和 build 通过。1440px 浏览器已验证真实服务鉴权、过滤器、详情和本次真实 run 的日志；最终 receipt 展示待运行完成。
- M1 regression：[m1-regression.json](m1-regression.json) 通过，覆盖账本/权限/并发共享、抢占、unknown 恢复、drain、备份恢复与显式 retry。该脚本用 ScriptedExecutor，`real_deployments` 为 0。
- 三类 hooks 协议：[hooks-acceptance.json](hooks-acceptance.json) 通过，验证 command/script/webhook、跨 lane 权限、空 HTTP 202 不等于 Started、重启后独立只读验证、错误输出/非零退出/超时不产生成功 receipt。所有执行均为 synthetic，`real_deployments` 为 0。
- AutoTask bridge 的 6 个 Python tests、AutoTask wrapper 的 4 个 HTTP tests，以及 deploy lock/fence Bash suite 均通过；入口与 bridge 代码已完成独立复核。Bridge 测试文件：[test_bridge.py](../../../integration/autotask/test_bridge.py)。
- 独立设计 gate、Rust hooks bounded-scope review、bridge/installer fix review 均 PASS。

## 真实开发请求进度

- `auto-deliver.service` 已安装、运行并在 `127.0.0.1:8787` ready；production 仍 disabled，远程管理使用 SSH tunnel。
- HTTP request `f6315628-c0cc-434b-96ae-d3d0651b74eb` 与 CLI request `2496d753-3ef9-45c1-966e-aad5082a11f7` 已合并到同一个实际物理 run：`b2924ad8-1949-45ab-b712-21dbd07628bc`，generation `1`，目标 SHA `7454e798bff72cf43175e8b565edf7270b98a246`。真实 systemd executor 已启动。
- 当前执行正在等待既有 AutoTask deploy lock。该锁由另一个 Codex Python 验证脚本持有，脚本在获取两把锁后明确保留 900 秒；没有尝试中断或绕过该锁。待锁释放后，仍需独立验证 source/process hash 与健康状态并取得匹配 receipt。
- Dashboard 浏览器已能显示该真实 run 的日志；receipt 与其最终状态尚未可见/记录。Root 将在验收完成后追加 receipt 和最终浏览器证据。

## 已推送的 AutoTask 改动

AutoTask core patch 已推送到 `origin/vibe`。远端提交为 [`8c1dc7536`](https://github.com/aak1247/autotask/commit/8c1dc7536)、[`353ec5c9b`](https://github.com/aak1247/autotask/commit/353ec5c9b)、[`2ff1110fc`](https://github.com/aak1247/autotask/commit/2ff1110fc)、[`cf08f7673`](https://github.com/aak1247/autotask/commit/cf08f7673)，对应本地开发提交 `c4299122a`、`d6325ce07`、`88c22988d`、`21f7b0861`。主共享工作区存在其他未提交改动且分支历史分叉，因此通过隔离 worktree 推送；未覆盖或回滚这些外来文件。

## 部署边界与待验收项

- 首次只针对本机 `autotask-dev`。协调器监听 `127.0.0.1:8787`，远程访问走 SSH tunnel；production 保持 disabled，旧 opaque hooks 不可抢占。
- AutoTask 主工作区 HEAD 已分叉于当前部署基线。初始可信 mirror 必须从实际部署 checkout 的当前版本（SHA 前缀 `7454e798`）建立，避免把较旧共享 HEAD 部署回去；其后 mirror 更新只接受 fast-forward。
- [service-installation.json](service-installation.json) 保留目标、release 路径和 production-disabled 配置；当前 service ready 状态及请求/run 身份以上述现场检查记录为准。
- T-104 尚未完成：当前真实 run 必须在 lock 正常释放后完成；随后独立检查源码、实际进程哈希和健康状态，保存匹配 receipt，并在 Dashboard 中复核最终 receipt。不能把当前活跃 run、HTTP/CLI 接收或 synthetic fixture 作为发布成功。
- 上一轮原始 M1 NFR-001（50 requests/s 持续 60s、p95 < 250ms）仍为 **NOT PASSED**；本次正确性 regression 和 hooks fixture 不关闭该性能验收，也不代表生产可用。
