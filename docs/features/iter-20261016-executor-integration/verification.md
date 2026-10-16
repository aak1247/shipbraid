# 验收记录：可配置执行器与 AutoTask 开发环境接入

日期：2026-10-16。状态：DONE。本迭代代码、质量检查、常驻部署、真实开发发布与 Dashboard 验收均已完成。生产未启用；原 M1 未通过的性能验收保持开放。

## 真实开发发布

[现场证据](real-development-deployment.json) 记录一次真实发布，两项独立提交共享同一个物理任务：

- HTTP（AutoTask `pnpm deploy:local`）：`f6315628-c0cc-434b-96ae-d3d0651b74eb`。
- Rust CLI：`2496d753-3ef9-45c1-966e-aad5082a11f7`。
- Run：`b2924ad8-1949-45ab-b712-21dbd07628bc`，generation 1，state `succeeded`。
- 冻结源码：`7454e798bff72cf43175e8b565edf7270b98a246`，重新发布当时实际运行的开发版本。
- 独立 systemd unit：`auto-deliver-autotask-b2924ad8-1949-45ab-b712-21dbd07628bc-1.service`，退出码 0，保留 `active/exited` 的稳定身份。
- Receipt：`e31d69a1-fb32-44fa-b776-c3c05552279f`，独立观察时间 `2026-10-16T12:18:23.692546Z`。两请求均为 `satisfied` 且引用该相同 run/receipt；HTTP wrapper 恢复等待与 CLI wait 均退出 0。

执行先等待其他会话的临时验收锁正常释放，随后完整构建 Worker、docker-proxy、后端及匹配 chat runtime，重启后端、前端与 Worker。Worker 构建日志用时 4m53s；合并请求节省重复构建，未宣称缩短单轮构建时间。

发布结果和随后独立只读 verify 的环境证明一致：部署 checkout 的 SHA/tree 匹配且干净；实际 backend PID 2160080、frontend PID 2160328、worker PID 2167226；backend/worker 的 `/proc/PID/exe` 路径、文件哈希与构建产物一致；进程启动身份稳定且三个服务均已重启。`/live`、`/ready`（ready=true）与前端入口均 HTTP 200。完整哈希、脚本指纹和 profile/component digest 保存在现场证据中。

## 常驻服务与 Dashboard

`auto-deliver.service` 已安装、enabled、active，监听 `127.0.0.1:8787`，版本 0.2.0；`/ready` 通过，静态入口 200，未认证 API 401。[安装目标](service-installation.json) 指向不可变 binary/web/bridge/发布脚本目录；本地凭据 mode0600，仅保留在主机私有配置中。

[浏览器验收](browser-verification.json) 覆盖鉴权、项目/环境过滤、禁用生产通道、真实日志、成功 run、两个已满足请求、成功 receipt 和展开的独立环境报告。1440px 和 390px 的 document scrollWidth 分别等于视口宽度，展开报告无横向溢出。localStorage 与 sessionStorage 均无条目。最终截图也包含随后另行提交的下一项开发请求；本记录只验收上述两个请求及其单个成功 run，不将后续任务计入本次成功。桌面/手机截图保存在本机 `.impeccable/review/auto-deliver-success-{desktop,mobile}.png`，未包含凭据。

远程管理使用 SSH tunnel：`ssh -L 8787:127.0.0.1:8787 yahui@172.168.3.70`，随后打开本机 `http://127.0.0.1:8787`。

## 检查与评审

- Rust：38 个测试、fmt、clippy（warnings denied）、debug/release build 通过。
- Dashboard：10 个测试、typecheck、生产 build 通过。
- Bridge 6 个 Python tests、AutoTask wrapper 4 个真实 HTTP fixture tests、deploy lock/fence Bash suite 通过。
- [M1 regression](m1-regression.json) 与 [三传输 hooks 验收](hooks-acceptance.json) 通过。它们属于 synthetic fixture，分别保留 `real_deployments: 0`；仅上述独立现场记录的 `real_deployments: 1` 代表真实部署。
- Hooks 验收涵盖 command/script/webhook、HTTP/CLI 环境选择、权限、空 HTTP 202 不等于 Started、Delivered Verify 后 SIGKILL 恢复且只读再验收、错误输出/非零退出/超时不产生成功回执。
- 独立设计 gate、Rust hooks bounded-scope review、bridge/installer 修订复核均 PASS。
- GitHub CI [run 38898166993](https://github.com/aak1247/auto-deliver/actions/runs/38898166993)（`5533271`）与 [run 38897050033](https://github.com/aak1247/auto-deliver/actions/runs/38897050033)（安装版本 `880c1de`）均 SUCCESS。

## 代码交付

Auto-deliver `main` 已推送至 [aak1247/auto-deliver](https://github.com/aak1247/auto-deliver)。AutoTask core patch 已推送 `origin/vibe`：[`8c1dc7536`](https://github.com/aak1247/autotask/commit/8c1dc7536)、[`353ec5c9b`](https://github.com/aak1247/autotask/commit/353ec5c9b)、[`2ff1110fc`](https://github.com/aak1247/autotask/commit/2ff1110fc)、[`cf08f7673`](https://github.com/aak1247/autotask/commit/cf08f7673)。由于共享工作区存在其他会话的改动及分叉提交，通过隔离 worktree 推送本任务补丁，保留共享区所有其他改动。

## 运行边界

- 首次仅启用 `autotask-dev`；`autotask-prod` disabled，桥接器本身拒绝 prod。外部 hooks v1 整体不可抢占；urgent 提高排队优先级，不强杀真实脚本。
- 本次重新发布实际开发版本。共享本地 HEAD 与其历史分叉；默认发布该 HEAD 前需要先合并历史。受信 mirror 从实际部署版本开始，只接受快进；协调发布拒绝脏 checkout、倒退与分叉替换。
- 单实例、本地 SQLite 持久卷；未知状态保留所有权，重启不会盲重发真实 dispatch。公网入口和生产接入仍需独立配置。
- 原 M1 NFR-001（50 requests/s × 60s、p95 < 250ms）仍为 **NOT PASSED**。本迭代未重新执行该负载指标，不关闭原计划的性能、完整故障矩阵及后续能力任务；本机成功不代表生产可用。
