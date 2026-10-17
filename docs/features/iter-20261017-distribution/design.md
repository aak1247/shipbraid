# 黑箱发行与项目接入设计

## 1. Document Control

- Feature: distribution / project adapter / GitHub push ingress
- Version: 1; Author / Reviewer: Codex（同会话自审，不代表独立审计）
- Updated: 2026-10-17
- Status: APPROVED; Review gate: PASS（见 §13）
- Execution: [execution-plan.md](execution-plan.md)

## 2. Context and Problem

目前首次使用要求 Rust/Node 构建，安装器绑定 AutoTask。已有 hooks 协议，但普通 deploy.sh 无法直接提供任务持久身份、查询和独立验收。用户希望下载后作为黑箱使用，管理员完成首次接入，开发者仅提交和等待。

## 3. Goals and Non-Goals

目标：提供可打包发行的软件、不会启动发布的新安装流程、独立于 AutoTask 的 Linux 本机适配器，以及可选 GitHub push → 请求转换入口。

非目标：SaaS、账号系统、多实例、自动 rollback、通用 DAG、取消 hooks、自动修改集群权限、生产接入验收、结果通知订阅。

## 4. Scope

新增 release tooling/workflow、standalone installer、integration/local、integration/github、独立验收和用户文档；旧 examples/local.toml 仅修正环境变量前缀。保持一个 Cargo package，不修改协调器数据库/协议。发行首个平台为 Linux x86_64 GNU；Python 3.11+、Git、systemd user manager / cgroup v2 为本机适配器运行依赖。CLI 本身不要求 Python 或 systemd。生产须独立配置及明确授权，安装器只初始化 dev/test。

## 5. Users and Use Cases

维护者下载发行包，指定受信本地镜像和经过审查的脚本包，生成私有配置；检查后启动。日常 Agent 使用已有 CLI/API。已有 HTTP 执行器直接配置 hooks webhook，不经本机适配器。可选 GitHub 接收器验证事件后用独立、lane-scoped token 提交。

边界场景：重复交接、交接丢响应、协调器重启、任务进程树未停止、验收不匹配、镜像尚未同步、重复 GitHub 事件、安装路径冲突、发行包损坏。

## 6. Requirements

### 6.1 Functional

- FR-001: 固定版本发行包包含二进制、Dashboard、适配器、安装器、示例、协议/升级文档及 SHA256 校验；tag 与 Cargo 版本一致。
- FR-002: 新安装只写新目录，拒绝覆盖；生成 loopback 配置、独立 admin/operator token、固定版本执行器路径、systemd unit，不自动启动/部署/更新镜像。
- FR-003: 本机适配器实现 dispatch/observe/verify；固定 argv、JSON stdin/stdout；systemd 独立托管长任务；持久意图后不盲重发，同 run/generation 必须找回同一任务。
- FR-004: verify 调用用户只读验证脚本，核对新观察时间、源码 SHA、全部组件 digest、profile 和健康；脚本退出 0 本身不产生验收。
- FR-005: GitHub 接收器仅接收签名验证通过的 push/ping；固定 repository numeric id + ref 映射 lane/components；重复 delivery 重用协调器幂等键；无自动 fetch、无自由命令/URL。
- FR-006: 提供隔离的完整安装 → 提交 → 独立任务 → 验收 → receipt 示例，区分合成示例与真实项目验收。

### 6.2 Non-Functional

- NFR-001 Reliability: 至少一次交付；持久文件 fsync + rename + directory fsync；无法证明的状态保持 unknown，不因超时释放所有权。
- NFR-002 Security: 私有目录/文件；秘密留在环境或私有 env 文件，部署原始日志不回传 API；公网 GitHub 入口通过受控 HTTPS 代理，内置接收器仅监听 loopback。
- NFR-003 Observability: 结果绑定 run/generation/target/适配配置摘要/systemd InvocationID；失败证明必须由新鲜稳定监督器和空 cgroup 共同证明。
- NFR-004 Compatibility: 不更改已有 schema/hooks；升级保留数据库、旧不可变适配器路径与证据。新安装器不执行存量升级。

## 7. Proposed Solution

发行脚本从已构建二进制和 web/dist 组装 tar.gz，记录目标、版本和每文件摘要。GitHub tag workflow 在质量检查和包验收后生成 draft Release；公开发布由维护者检查后完成。二进制最低 libc 取决于构建平台，官方首版使用 Ubuntu 22.04 构建并烟测。

安装器校验内容清单、版本和平台，将发行内容与显式选择的项目脚本包复制到不可变 release 目录。配置中保留受信镜像绝对路径，不 clone/fetch。示例脚本包包含 deploy.sh/verify.py；复制后脚本必须通过自身目录定位依赖，依赖的部署凭据由适配器专属 EnvironmentFile 提供，避免将协调器 token 传给任务。

本机适配器的配置固定 lane/project/environment/repository/components、state_dir、cwd、deploy argv、verify argv 和私有任务 env 文件。对每次输入校验边界。dispatch 在锁内持久保存输入与配置摘要后，最多尝试一次 systemd-run，unit 由 run UUID/generation 确定。worker 不持有协调器生命周期。observe 结合持久结果、InvocationID、unit 终态及 cgroup v2 populated=0（前后稳定快照）判断；结果或 unit 丢失保持 unknown。失败只在完整停止证明后返回 failed + failure_proof。没有自动强杀、超时取消或重启。

verify 只在任务停止且成功时运行，用户脚本接收 JSON 信封，返回实际 source_revision、component_digests、profile_digest、observed_at、healthy、verification_report。适配器拒绝不匹配或非本次窗口观察，并包装 receipt。用户验证脚本是受信边界，通用适配器不能自动证明任意应用的实际版本。验收脚本必须只读，其超时/异常保持 unknown。

GitHub 接收器不成为新调度器，无自建请求库：使用已持久化的服务端 Idempotency-Key，按接收配置 route 和 delivery ID 派生键；重投相同事件正文不变，不同正文同 delivery 被服务端拒绝。签名、大小和形状校验后固定映射，仅提交 normal/exact。HTTP 超时返回非成功，可重投；不向 GitHub 返回 token、内部配置或部署日志。镜像同步通过已有维护流程在重投前完成。

## 8. Alternatives and Tradeoffs

不选择容器中直接运行宿主部署脚本：会引入宿主权限和挂载复杂度。首版不做 DEB/RPM/Homebrew 多套打包。保留现有 command/script/webhook 三种传输。本机适配器采用已存在的 systemd，避免自建后台任务守护进程；代价是首版仅支持 Linux。

## 9. Risks

持久意图后崩溃可能没有实际任务：保持 unknown，由人核对，不补发。丢失 unit/账本或更改旧配置可能使任务无法独立观察：保留旧目录，不清理 unit。后台子进程会阻塞完成：要求部署脚本等待全部写操作，cgroup 非空不结算。只读验证器由管理员审查；通用适配器不推断业务语义。停止协调器不等于停止任务。升级先 drain 并等待结算，保留 DB 和冻结路径；数据库迁移后的二进制回退须检查兼容性并使用在线备份，不能盲换空库。

## 10. Validation Strategy

必需：Rust fmt/clippy/test；Python 单测验证幂等/错误边界/停止证明/安装拒绝覆盖/包篡改/GitHub 签名和路由；隔离临时环境真实 systemd user unit 与真实协调器端到端验收，证明同 run 只部署一次、重启后仍能查询、真实环境观察不匹配不能满足请求。包内运行无需源码/Rust/Node。所有部署限于临时合成目录，不访问 AutoTask/集群/生产。

## 11. Rollout Plan

本地先生成并验收包；提交 release workflow 后由新版本 tag 生成草稿。不得把本地打包等同于 GitHub 已发布。先配置 dev/test、验收后再交给 Agent；生产后续单独授权。已有安装仅使用文档中的保留数据升级步骤。

## 12. Open Questions

无阻塞问题。暂定首个平台 Linux x86_64 GNU；真实 GitHub delivery/HTTPS 代理及具体项目环境验收由后续使用者配置，不宣称已验证。

## 13. Review Feedback Tracking

1. Review verdict: PASS，信心 MEDIUM；发行路径可在一次迭代完成，外部生产/公开发布不计本地实现验收。
2. Overdesign: MINOR OD-001，多平台/安装包矩阵扩张。已收敛至单平台 tar.gz 和 stdlib Python，无新服务框架。
3. Feasibility: YES；复用 hooks 和 HTTP API，GitHub ingress 独立可选，不修改调度内核。
4. Completeness: MAJOR DG-001，不能将 deploy.sh 成功当环境证据；FR-004 + T-002/005 强制新鲜独立验收。MAJOR DG-002，安装/升级可能覆盖旧所有权；FR-002 + T-003 拒绝已有目录并保留旧路径。MAJOR DG-003，任务秘密不能继承协调器身份；NFR-002 + T-002 使用独立 env 文件和环境 allowlist。
5. Clarifying questions: 均已明确默认值，无需阻塞用户；若扩大到生产、多平台、任意 CI 平台则重新评审。

实现反馈：包级真实临时 systemd 验收已通过；支持终态空 ControlGroup（完整 fence 前后检查）、持锁防重叠只读验证、有界日志流而不限制产物大小。原有 `--param` 文档/测试与当前核心不一致另列于执行计划 §8，不并入本轮；未实现参数文档不随包提供。

## 14. Definition of Design Complete

需求可追溯；范围/失败模式/回退/验收明确；review PASS；开放外部接入不误标为本轮完成。
