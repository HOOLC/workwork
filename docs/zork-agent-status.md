# zork-agent 实现与验收状态

更新于 2026-09-05，验收主机为 mini1。目标设计以
[`zork-agent-architecture.md`](./zork-agent-architecture.md) 为准；本文只记录实现状态和证据。

## 已完成

- 持久上下文策略：默认 compaction，支持 handoff；策略变化只影响下一次上下文整理，恢复保留进行中的计划。
- Agent HTTP、Gateway、Admin 和桌面端共用 Agent 持久化的上下文配置。Admin 支持自定义保留 token 数，桌面支持常用值及当前自定义值；Gateway 不保存策略副本。
- 工具结果摘录先保留调用参数、实际 offset、退出码等元数据，再截取大正文；摘要指令区分事实、检查结果与计划，并要求纠正旧摘要中的错误。file.read/file.edit 的初始说明包含正确参数。
- Gateway 测试替身和断言已跟随新增 context 字段更新；target-linux 已从 Git 和格式/lint 扫描中排除。
- 启动目录发现最多使用四个元数据读取线程，最终排序和恢复优先级保持不变。
- 向前查询先定位返回范围，再只解码需要返回的事件；解码借用原始 JSON。两次扫描复用同一文件句柄，避免日志轮转替换路径造成读取竞态。
- 所有独立 benchmark 入口接受 Cargo 自动传入的 --bench 参数；启动和查询压测失败也输出指标。

## 回归与实测

- Rust 全工作区 231 项通过（后端 200 项、原生 GUI 31 项）；Agent / testkit 其中 128 项通过，含 compaction、handoff、恢复、取消、迟到工具结果、HTTP/SSE、provider 对端和真实 shell/文件系统合同。
- JS / Admin UI：26 项行为与集成测试通过。DeepSWE adapter 的 66 项单元测试在独立、按路径触发的工作流运行。
- Admin TypeScript、构建、lint 和格式检查通过；会话列表的返回类型显式标注为 SessionRecord。
- 真实 Agent + Gateway 桌面入口测试通过，包括上下文读写、非法值拒绝和显式消息边界。
- 界面实测：Admin 保存 12345 tokens，切换 handoff 并刷新；桌面读取这个自定义值，改为 compaction/8000，Admin 刷新显示一致。

mini1 release 基准（fixture 构造不计入测量）：

| 场景 | 结果 | 门槛 |
| --- | --- | --- |
| 10000 个虚拟 session 完整生命周期 | 1.277 秒，7833 sessions/s，90000 events | 保留 release 基线 |
| 131072-event session，1024 次查询 × 1024 events | 串行 37/s，8 workers 192/s，峰值 RSS 13.5 MiB | ≥20/s，≤256 MiB |
| 32 MiB segment，zstd level 12 | 2.01×，24.6 MiB/s，峰值 RSS 50.8 MiB | ≤256 MiB |
| 100000 session 启动 | 总计 6.511 秒；发现 2.532 秒；后台 3.977 秒；峰值 RSS 101.4 MiB | ≤10 秒，≤512 MiB |
| 100 sessions × 100 fragments × 至少 16 MiB，10000 次随机查询/档 | 10 workers 670.5/s，p95 29.1 ms，p99 36.0 ms；峰值 RSS 238.1 MiB | ≤256 MiB |

启动精确恢复与请求优先恢复均低于毫秒输出精度，并分别只恢复一次。优化前同机总耗时为 10.982 秒，未通过 10 秒门槛。

极限查询压测优化前峰值为 357.7 MiB，超过 256 MiB 门槛；最终在相同默认负载、系统分配器下测得 238.1 MiB。为避免保留大量原文和反复解码废弃记录，before 查询先定位窗口，再从同一打开的文件句柄读取返回范围。代价是额外扫描：同机片段查询从串行 54/s、8 workers 270/s 降至 37/s、192/s，仍高于 20/s 门槛。

分配器替换和固定线程池实验没有提供足够收益，均已撤回；最终验收未连接堆采样工具。

测试精简后，本机后端 200 项测试的执行时间合计约 7.5 秒（不含编译），原生 GUI 31 项约 0.02 秒，JS / Admin UI 26 项约 8.9 秒。默认测试移除重复的 debug 性能跑分、源码字符串/样式常量自测和旧直连 Agent 的 GUI 假服务自测；性能门槛仍由独立 release benchmark 验收。普通 CI 覆盖全部后端和 Admin UI；原生 GUI 按相关路径独立运行。

本地原始日志、截图和跨端验证结果保存在 `artifacts/agent-readiness-20260905/`；本轮测试和 CI 日志在 `artifacts/merge-agent-readiness/`。

## 仍需实证的模型行为

本轮修复了可复现的摘要证据丢失和工具指引问题，但没有重新运行真实模型的 Wasmi 长任务。不能据此宣称已经消除重复探索或提高解题完成率。

下一次长任务验收应固定源码、模型与预算，以实际代码变更、完整检查输出、任务要求的提交和 verifier 结果为完成标准。比较压缩前后保留的硬性要求、工具参数纠正、重复读取比例及未缓存 token 消耗；不以 step/compaction 次数作为任务进展。

## 复现命令

在 mini1 仓库执行，沿用 AGENTS.md 的构建资源约束：

```sh
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=4
cargo build --locked --workspace
cargo test --locked --workspace
npx --yes pnpm@10.33.0 test
npx --yes pnpm@10.33.0 format:check
npx --yes pnpm@10.33.0 lint
npx --yes pnpm@10.33.0 build:js
python3 crates/zork-gui/tests/test_gateway_entry.py
npx --yes pnpm@10.33.0 benchmark:deep-swe:test
cargo bench --locked -p zork-agent-testkit --bench test_world --bench startup_recovery --bench query_api --bench segment_compression
cargo bench --locked -p zork-agent-testkit --bench query_pressure
```

性能基准应独立运行，避免同时编译或执行其他负载测试。
