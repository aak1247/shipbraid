# 可配置执行器与 AutoTask 开发环境接入

## 1. Document Control
- Version: 0.1；2026-10-16；Owner: Codex。
- Status: APPROVED；Review gate: PASS（独立复核已确认三项必要修改和权限测试要求）。
- Plan: [execution-plan.md](execution-plan.md)。用户已授权部署服务并接入 AutoTask；首次真实执行环境已明确为本机开发环境。

## 2. Context and Problem
M1 只执行本地模拟。用户需要部署一个可常驻使用的协调器，并使 AutoTask 的发布通过它提交；执行机制应与调度分离，可配置命令、脚本和 webhook，通过 HTTP/CLI 触发，同项目支持开发与上线环境。

## 3. Goals and Non-Goals
交付可常驻协调器、同源 Dashboard、三个执行传输、真实 AutoTask 开发发布及独立验收。保留共享请求、幂等和 unknown 所有权规则。此迭代不接入或变更生产环境、不实现通用 DAG，不承诺对任意脚本安全强杀，不关闭 M1 尚未通过的性能/完整故障矩阵。

## 4. Scope
auto-deliver 的配置、执行器、服务、API/CLI、Dashboard、systemd 部署文件和 AutoTask 桥接器。AutoTask 仅增加发布提交入口/说明并复用既有 deploy-local 脚本。用户其他未提交改动不暂存、不回退。单实例、本地 SQLite 持久卷；实际首发使用现有本机用户 systemd，不引入 K8s 平台资源。

## 5. Users and Use Cases
开发者/Agent 通过 HTTP 或 CLI 提交 immutable SHA 和环境；同环境兼容请求共享执行。开发和上线使用不同 lane，权限、仓库发布线、配置、执行器和所有权独立。环境隔离令牌按 lane 授权，不使用会覆盖项目全部环境的 project-wide scope。生产示例默认禁用；仅配置支持不意味着生产授权。

## 6. Requirements
### 6.1 Functional Requirements
- FR-101: 每条 lane 配置 scripted 或 hooks 执行器；hooks 的 dispatch/observe/verify 分别可为 command/script/webhook；v1 不暴露 cancel hook。
- FR-102: HTTP/CLI 支持 project/environment 选择；唯一解析到已授权 lane，不猜测歧义；保留 lane_id 兼容入口。
- FR-103: quick dispatch 交给独立持久监督器，返回稳定 executor_ref；observe 查询真实状态；verify 返回独立证据，退出码 0/HTTP 202 不是验收。
- FR-104: hooks 首版强制不可抢占，并在调用 dispatch 发生任何副作用前把 Deploying 区间持久化；旧脚本内部可能直接修改环境，不能假称支持阶段停止证明。模拟器继续原有安全抢占测试。
- FR-105: 重启和不明响应仅 observe，绝不自动重发 dispatch；有可信身份/证据才能恢复或结算。所有回执按 run/generation/ref fencing。Delivered Verify 在重启后必须重新独立观察并取得新的匹配回执；verify 是只读操作，可以重新验证，但不因它丢响应重新 dispatch。
- FR-106: 部署常驻服务和同源只读 Dashboard，显示真实执行器/项目/环境和有界日志。
- FR-107: AutoTask 增加 HTTP 提交+等待入口；复用明确 SHA 的既有发布脚本，仅发布 committed 源码；成功后验证实际进程、源码及健康。
### 6.2 Non-Functional Requirements
- NFR-101: 不接收客户端自定义命令/URL/环境变量。hook 只从管理员配置解析，无 shell 字符串插值。webhook bearer 从环境读取，不跟随重定向；请求/响应和进程输出有界、调用有超时。
- NFR-102: hook 调用在 SQLite 事务和 writer 锁外；真实任务由独立 systemd 单元监督，协调器重启不停止 AutoTask 发布。
- NFR-103: 保持 WAL/FULL/独占锁；记录真实目标、证据和运行结果；性能仅声明实测，不降低原 NFR-001。

## 7. Proposed Solution
保留 Lane(project,environment) 作为写入边界；不新增 Project/Environment 数据库表。新增 tagged HookConfig，command 用固定 program+args，script 用显式 interpreter+path+args，两者 stdin JSON；webhook POST 同一 JSON 信封。共同协议包括版本、operation、command/run/lane，返回 accepted/started/running/completed/failed/unknown/verified 和稳定身份；verified 含完整 DeploymentReceipt，任何传输成功本身都不完成请求。真实 hook 不依赖模拟计时。新增 effective profile digest 绑定管理员 profile 与序列化执行器配置（不含秘密值）；活动 run 与新配置不匹配时不调用新 hook，保留 unknown/所有权，需恢复原配置后独立观察。

hooks 调用 dispatch 前先事务提交非抢占 Deploying 区间；dispatch 只做快速交接；运行中 observe，完成后进入 verifying，再通过持久 verify 命令调用独立验证器。首版 hooks 一律禁止 preemption，粗粒度 deploy 阶段覆盖旧脚本的整个操作；脚本不可用取消响应来提前解锁。重启后必须再 observe，无法证明则保持 unknown。

AutoTask 桥接器放在 auto-deliver 项目 integration 目录：dispatch 使用 deterministic run/generation 的 systemd user unit；任务用冻结 SHA 调用 AutoTask 现有 deploy-local；单元结果和任务证明保存在私有运行目录。verify 重新核对 deploy checkout SHA、backend/worker 实际 PID 的 executable hash 与构建产物、frontend 进程工作目录及健康端点，生成匹配目标回执。不把共享工作区 HEAD 或 HTTP 健康单独当版本证明。现有旧入口作为运维回退，新增 coordinated 入口使用 HTTP；不递归调用自己。

Dashboard 静态生产包由同一个 Axum 服务提供，API 同源且继续 bearer 鉴权；令牌文件位于仓库外 mode0600。配置只引用其环境变量名。服务用用户 systemd 常驻，绑定开发主机 server1 的 127.0.0.1:8787（同源 UI/API，API bearer 鉴权；远程用 SSH 转发或已配置的 HTTPS 入口）；不创建公网 DNS/Ingress。

### 本机开发目标合同
- lane `autotask-dev`，project `autotask`，environment `dev`，components `["app"]`；prod 示例 disabled 且无生产命令/凭据。
- 固定共享源码 `/home/yahui/repos/autotask`，受信 bare mirror 位于 `~/.local/share/auto-deliver/mirrors/autotask.git`，发布线 `refs/heads/vibe`；部署 checkout `/home/yahui/repos/autotask-head`，只允许 frozen SHA，无镜像容器发布。
- 执行脚本从受信 AutoTask 已提交对象导出到不可变协调器 release，记录来源 commit 与脚本 SHA256，再以该固定脚本 `<full-sha>` 执行；既有脚本锁保持生效。在既有锁内额外拒绝非快进历史替换；桥接器拒绝包含未跟踪源码的脏部署 checkout（不触发 WIP 自动清理），强制重建 backend/worker 并覆盖跳过构建的环境变量。
- 服务 `autotask-backend.service` 对应 `server/bin/autotask`；`autotask-worker.service` 对应 `packages/worker-rust/target/debug/autotask-worker`；`autotask-frontend.service` 主进程 cwd 为部署 checkout。
- 健康：`http://127.0.0.1:8011/live`、`http://127.0.0.1:8011/ready`、`http://127.0.0.1:8012/`。receipt.source_revision=冻结 SHA；component_digests.app=源码树+实际 backend/worker 哈希+前端进程身份的证据摘要；profile_digest=被冻结的有效执行配置；run_id/generation/executor_ref、观察 UTC 和独立验证报告必须匹配。
- 常驻协调器 `auto-deliver.service`，端口8787；配置/令牌 `~/.config/auto-deliver/`，数据库/运行证据 `~/.local/share/auto-deliver/`，发布二进制及 UI 使用独立不可变 release 目录。

## 8. Alternatives and Tradeoffs
不把各语言脚本嵌入调度器或复制编译系统。相同 JSON hook 协议复用三个传输；真实任务监督由 AutoTask 的现有 systemd 环境承担。暂不支持外部安全抢占，换取可验证的真实上线闭环。未来可按明确阶段协议扩展合作式停止能力。

## 9. Risk Assessment
丢 dispatch 响应：持有 unknown，不重发；桥接器可独立观察 deterministic 单元。发布脚本与其他会话竞争：保留现有 flock，遇冲突报告/阻塞，不杀其他部署。任务失败且环境状态不明：保留所有权并要求独立观察。生产误用：生产 lane 默认 disabled，无生产命令或凭据。回退协调入口不取消已经在运行的发布；先 drain/核对任务，再改回旧入口。

## 10. Validation Strategy
真实临时进程验证 command/script；本地 HTTP fixture 验证 webhook 鉴权、响应丢失/错误、日志与 verify；三传输均需状态/证据 fencing、非零退出和超时不误报成功。多环境测试验证互不共享、选择歧义失败，以及只授权开发 lane 的令牌不能读取或提交上线 lane。回归 M1 进程测试；Rust fmt/clippy/test/build，前端 typecheck/test/build。最终使用常驻服务提交并等待一次 AutoTask 本机开发真实发布，再查询回执、服务健康和 Dashboard；不以 fixture 替代该验收。

## 11. Rollout Plan
先完成代码、配置样例与隔离测试；安装常驻协调器但不自动触发任务；提交/push 两仓库代码，首次从开发环境实际部署的已提交版本初始化受信 Git bare mirror 的 refs/heads/vibe；后续仅从固定本地仓库快进更新，冻结该线上具体完整 SHA，发起该 SHA 的开发请求，验证所有目标服务及回执。生产只保留禁用配置示例。回退停用提交包装器并 drain 协调器，已运行任务由 systemd 独立完成/核对。

## 12. Open Questions
Q-101 首个目标已由用户选择本机开发环境；生产配置只作禁用示例。Q-102 webhook 接收端由部署管理员提供，本次用本地 fixture 验证协议，AutoTask 首次使用脚本适配器。

## 13. Review Feedback Tracking
技术自审 PASS：① 不引入 DAG/新数据库实体，三种传输复用一个协议；② 一轮范围只包括本机开发闭环，生产禁用；③ 已明确不明投递、配置指纹、独立验收、日志/响应上限、回退和真实运行验收。hooks 首版强制不可抢占，避免给旧脚本添加无法兑现的停止保证。独立复核提出三项必要修改已落实到设计：dispatch 前持久非抢占区间；Delivered Verify 的独立重新验收；具体源码/服务/端口/回执映射，并补充跨 lane 权限测试。独立限定复核已确认全部四项修改，结论 PASS；实现及真实运行仍须下述验证。

## 14. Definition of Design Complete
明确真实环境、用户授权、协议、独立验证、重启边界、回退与验收；技术 gate PASS 后执行。只有所有本迭代必要任务与真实部署验收通过才关闭计划。
