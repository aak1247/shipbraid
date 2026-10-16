# AutoTask 生产发布接入

Shipbraid 的 `autotask-prod` 通道协调一次完整生产发布：内网 Kubernetes `prod/autotask`、node166 的 `autotask-worker-prod.service`，以及 `prod/autotask-website`。三个组件为 `app,website,worker`。启用通道不会自动发布当前 HEAD；首次真实发布仍需提交明确的目标 SHA。

## 发布入口

在 AutoTask 仓库运行：

```bash
pnpm release:prod --param database_backup=skip
pnpm deploy:prod <完整SHA> --urgent --param database_backup=include
pnpm deploy:coordinated --environment prod --param database_backup=skip --no-wait
pnpm deploy:coordinated --environment prod --request-id <request-id>
```

默认目标是生产受信镜像中已发布的 `vibe` 提交。开发 WIP、未推送本地提交、请求中的命令、主机覆盖和跳过检查参数不进入生产执行。协调请求不能只发布三个组件的一部分。生产项目声明 `database_backup` 为必填枚举参数，未提供或值非法时在入账前拒绝。参数被冻结到 run 信封，不同参数不会合并执行或复用对方的成功回执。

生产令牌独立保存在主机的 `~/.config/auto-deliver/production-operator.token`；客户端也支持 `AUTO_DELIVER_PROD_TOKEN` / `AUTO_DELIVER_PROD_TOKEN_FILE`。开发令牌无生产 lane 权限，生产令牌也无开发 lane 权限。现有安装保留旧 unit 和目录名，避免另开协调器或丢失账本身份。凭据不提交到 Git。

Dashboard 使用原管理身份查看两个环境，地址为 `http://172.168.3.70:8787`。请求、执行和日志沿用同一状态模型。网络断连或客户端超时只结束本地等待，不代表部署停止；凭 request-id 恢复查询，不能改用 direct 入口盲目重发。

## 执行与验收

生产观察器与开发适配器分开配置，执行配置固定管理员审查的完整 Git SHA。生产镜像从 GitHub 的已发布分支导入独立 bare mirror；发布进程还会再次核对目标与审查提交均在 `origin/vibe` 历史内。

持久监督器启动前先记录 request。每个 run/generation 只交接一个独立 systemd user unit，任务在协调器重启后继续运行；已写入 intent 而监督器身份丢失时绝不盲重放。

独立进程在自己的审查工具 worktree 中运行：

```text
release.sh prod all --from-git --ref <完整SHA> (--include-db-backup | --skip-db-backup)
```

核心发布脚本、传递调用及生产清单的 blob 必须与审查提交一致；漂移检查在独立 job 内完成，失败有持久结果和可核验的停止身份。应用源码由既有 release.sh 创建另一个干净 worktree 构建，不修改共享工作区 WIP。数据库是否备份由请求者必填的 `parameters.database_backup=include|skip` 决定，没有默认值或自动判断。生产快照、CAS、静态资源保留与原上线校验全部保留；不自动回滚已完成的部分组件。

原始 direct 维护入口保留为 `pnpm release:prod:direct` / `pnpm deploy:prod:direct`。协调执行与这些入口共用 `/tmp/autotask-release-prod.lock`。使用 direct 前仍应 drain 并核对所有权，防止已有排队意图随后重复发布。锁等待超时不证明另一个发布停止。

成功 receipt 需要新鲜、独立的三组件观察：

- 主应用：固定集群身份、不可变镜像 digest、全部副本更新且 Ready、镜像 OCI 源 SHA/tree、每副本 build-info 与健康检查。
- 独立 Worker：node166 systemd 身份、实际运行进程与二进制哈希匹配该发布的镜像内 Worker；发布后确认其启动身份发生变化。
- 官网：不可变镜像、全部 Ready 副本及实际内容与目标版本一致，同时检查现有网站入口。

observe 的 completed 不满足请求。verify 必须重新观察，并与本次 job 保存的证明一致后，才生成带三个组件 digest 的 receipt。部分失败、漂移或未知状态不能记成功。失败只在系统监督器与完整 cgroup 子树证明 job 已停止后释放所有权，随后暂停 lane 并将请求标记需处理；人工检查后显式重试。

## 接入验收与边界

生产接入验收使用 mock/隔离任务测试完整交接及验收协议，并只读检查真实集群、Worker、注册表和现有网站。接入过程不发布代码、不重启三套生产工作负载，也不将当前线上状态伪造成某个新请求的成功 receipt。

本轮没有关闭原高并发性能目标。隔离复测 `50 requests/s × 60s` 的 3000 个请求全部入账，p95 约 8.8 秒，仍未达到 250ms；见 [负载记录](acceptance/production-load-20261017.json)。生产接入按人工触发的低频发布使用，不能据此宣称达到该吞吐量的延迟指标。

通用参数声明、HTTP/CLI 格式与脚本读取方式见[项目请求参数](request-parameters.md)。

本机启用记录、必填参数拒绝用例与只读生产健康检查见[生产接入验收](acceptance/production-integration-20261017.json)。已启用生产 lane，尚未提交首个真实三组件发布。
