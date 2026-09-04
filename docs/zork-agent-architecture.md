# zork-agent 架构规范

本文是实现、测试和审查共同使用的现行架构规范，只保留已确认的设计与可验证契约。实现与测试引用本文中的契约编号；验收结果和复现命令见 [`zork-agent-status.md`](./zork-agent-status.md)。

## 1. 目标

zork-agent 是持久、可恢复的 agent runtime。它负责：

- 保存 session mailbox、event、当前可恢复状态和历史；
- 调用 provider，并把 provider 输出转换为持久事实；
- 调度逻辑工具并保存真实结果；
- 在进程退出、工具中断、provider 错误和上下文交接后继续执行；
- 通过 HTTP、history 和 SSE 提供一致的外部契约。

实现必须优先保持以下性质：

1. event 是会话持久事实的唯一来源；
2. 同一 session 只有一个 event 写者；
3. state、Decision 和 provider projection 是纯计算；
4. 已发送给 provider 的 generation 前缀不可改写；
5. 只要仍能构造合法 provider 请求，错误不能禁止 agent 继续；
6. 空闲 session 的内存成本有界，恢复和查询不加载完整历史；
7. 一个语义只保留一个抽象和一份实现。

## 2. 所有权与执行边界

### 2.1 Data root（`PERSIST-01`、`ENV-01`）

一个 data root 同一时间只允许一个 zork-agent 进程写入。进程在启动时取得 data root 排他锁；不同 data root 互不影响。

data root 是 zork-agent 的内部持久数据目录，不是工具运行的 workspace。workspace 只表示相对路径的解析基准和 shell 默认工作目录，不是安全边界。zork-agent 不限制绝对路径、`..` 或进程本身有权访问的其他路径。

### 2.2 单 session 单写者（`PERSIST-02`）

每个 active session 恰有一个 runner。只有该 runner 可以向该 session 追加 event；不同 session 的 runner 可以并行。

外部输入、工具结果、deadline 和取消请求先进入 supervisor，再交给对应 runner。正常 append 不读取或重放历史：runner 基于当前内存 state 产生 event，store 持久化后，runner只对新增 event 做增量 fold。

### 2.3 纯计算与副作用

- `state + event -> next state` 是纯 fold；
- `state + world -> Decision` 是纯计算；
- `state -> provider transcript` 是纯 projection；
- provider 请求和 `tool params -> tool result` 是外部执行；
- `tool state + tool result -> next tool state` 是工具定义的纯 fold。

恢复只重放 event 和纯 fold，不重新执行 provider 或工具副作用。

## 3. 持久目录

每个 session 使用独立目录：

```text
<data-root>/sessions/<session-id>/
  segments/
    <first-event-id>.jsonl.zst
    <first-event-id>.jsonl
```

session ID 使用 ULID，只作为不透明、全局唯一的身份。session ID 不承担事件排序、恢复优先级或 cursor 语义。恢复优先级来自 session 最近活动时间。

本格式不兼容此前的磁盘数据。runtime 不保留 legacy reader、双读 fallback 或 importer；确有旧数据需要保留时，为实际数据单独编写临时迁移脚本。

## 4. Event

### 4.1 Event envelope（`EVENT-01`、`EVENT-02`、`EVENT-04`）

JSONL 每一行是一个 event envelope，包含 `event_id`、`schema_version`、
`batch_index`、`batch_count` 和 `event` 五个字段。`event_id` 必须是下一节定义的
16 位标准文本；这里不写脱离具体 session 的伪造示例，因为最后一位校验码绑定 session。

`batch_index` 从 `0` 开始。同一批 event 连续写入且具有相同 `batch_count`，index 必须依次覆盖 `0` 到 `batch_count - 1`。恢复时，不完整批次整体不参与 fold；压缩时删除这些没有意义的残留 event。完整批次中的 event 原样保留。

每条 event 自带 schema version。读取时先通过纯 migration 在内存中逐级转换为当前格式，再交给 fold；旧 event 不写回。

### 4.2 Event ID（`EVENT-03`）

event ID 只在所属 session 内分配。完整身份是 `(session_id, event_id)`。

标准文本形式固定为 16 位：

- 前 14 位：从 `00000000000001` 开始的补零十进制序号；
- 第 15 位：前 14 位的 Verhoeff 校验位；
- 第 16 位：带固定域标识的 `SHA-256(session_id || sequence) mod 10`；
- 最大序号：`99999999999999`。

SHA-256 的字节输入固定为 `b"zork-agent:event-id:v1\0"`、ULID session ID 的
16 字节大端值、`u64` sequence 的 8 字节大端值依次拼接。`mod 10` 把完整
SHA-256 digest 视为一个大端无符号整数后取模。这个编码属于持久格式，不能随实现变化。

JSONL、segment 文件名、history、SSE cursor 和工具参数只接受这一种表示。不提供短写，不自动补零，不规范化非法输入。

snapshot 是 event，也占用一个 event ID。event ID 不用于去重，不另设重复的数字 sequence 或 stream version。

### 4.3 Event 语义（`EVENT-05`、`EVENT-06`、`RECOVERY-02`）

领域 event 至少覆盖：

- session 创建、选择及上下文策略变化和 mailbox 输入；
- turn 开始、取消请求和结束；
- step 准备、成功、失败和中断；
- 工具取消请求和工具结果；
- 上下文交接应用及文档降级；
- deadline 到期；
- runner/recovery 故障；
- snapshot。

`StepStarted` 是 provider 外部执行前的持久准备事实，冻结 purpose、输入边界和输出上限。ModelGateway 返回 `ModelOutcome` 后，runner 先用 `StepCompleted` 保存 purpose、文字、原始 provider call、usage、provider context 和非敏感输入诊断；普通 step 的调用再交给 ToolExecutor，上下文 step 由 runtime 校验文档。固定 `call` wrapper 或逻辑工具参数不合法不能把这个事实降级成 `StepFailed`；后者只表示没有取得可用 `ModelOutcome`。

普通和 handoff step 的每个原始 provider call 都对应一个领域 ToolInvocation。无法解析时保留原始 call ID、参数和拒绝原因，随后产生失败 ToolResult，供下一次请求按原 call ID 配对。独立且无工具的 compaction 不建立调用协议对：意外工具输出只保存原始事实并判文档尝试失败，不创建或执行 ToolInvocation。runtime 不把 `parameters`、`path` 等错误字段静默改写成合法 `arguments`。

不需要独立 `ToolStarted`。已持久化 ToolInvocation 但没有 ToolResult 时，恢复必须追加“执行被中断，实际结果未知”的错误结果，不得自动重新执行。

任何到达的 ToolResult 都写 event。无法与当前 pending invocation 匹配的结果仍是事实，并在下一次 provider 请求中作为新增通知告诉 agent。

ToolResult 只有一个规范 JSON 载荷 `data`。工具、持久化、fold、provider projection 和 UI 都读取这一份数据；不得再保存平行的展示正文。provider 与 UI 的文本表现由投影层从 `data` 生成，失败原因放在 `data.error`，因此 `file.read`、`history.list` 等大结果在一次 provider 请求中只能出现一份正文。

### 4.4 持久确认（`PERSIST-03`）

一个批次的全部行写入并且 `sync_data` 成功后，才能确认持久化。`write` 或 `flush` 成功不构成持久确认。

进程在持久化后、确认前退出时，外部提交方可以重新提交；runtime 不保存 event ID 去重集合，也不承诺消除这种语义重复。

## 5. Segment、Snapshot 与大结果文件

### 5.1 Segment（`SEGMENT-01`、`SEGMENT-02`）

一个 session 的 event 流由多个 segment 组成。segment 只用首个 event ID 命名，因此文件名的字符串顺序就是 event 顺序。

当前 segment 是未压缩 JSONL。切分阈值按未压缩字节数计算；达到阈值只表示允许切分，不决定最终切分位置。只有之后出现 snapshot 边界时才切分：旧 segment 在 snapshot 之前结束，新 segment 的第一个 event 是 snapshot。

封存的旧 segment 在后台使用 zstd level 12 压缩，不阻塞新 event append 或持久确认。压缩和读取都逐行流式处理；不得把整个 segment 解压或收集到内存。segment target 不能小到持续产生压缩率很差的小文件，也不能大到使恢复、查询或运维的峰值资源失控；默认值必须同时用压缩率和峰值内存基准校验。当前默认 target 为 32 MiB，达到它仍只表示等待后续 snapshot 边界。

store 只负责单写：data root 所有权、完整 commit append、snapshot 边界切段、封存 segment 压缩和 session detach。store 不公开历史、恢复或其它读取 API，recovery 不依赖 store。

持久读取统一属于 SessionQuery。底层先提供从文件末尾开始的完整行 streaming，并忽略 torn tail；其上按 `batch_index/batch_count` 和 event ID 把行组合成反向 commit streaming。恢复从尾部消费 commit，并把经过的 commit 暂存为新到旧；遇到可用 snapshot 后以 snapshot 初始化 state，再反转暂存 commit 并按旧到新 fold。snapshot 不可用时继续反向寻找，暂存后缀继续保留。最后完整 commit、启动发现、history cursor 和 snapshot 恢复都复用同一读取实现；正常恢复只暂存最新 snapshot 后的恢复窗口，不收集完整 session 历史。

压缩不正向解析 event。它先从未压缩 segment 尾部反向检查最后一个完整 envelope；若 `batch_index + 1 != batch_count`，继续反向找到该不完整 batch 的 `batch_index == 0`，把它和后面的残留排除。随后只把确定保留的原始字节前缀流式写入 zstd，不执行 event migration 或重序列化。

正常完整恢复只反向读取最新 JSONL：从最后一个完整 commit 向前寻找最后一个可用 snapshot，并用已经暂存的后续 commit 完成 fold。若文件内后一个 snapshot 损坏、版本不可迁移或不属于当前 session，则继续向前找更早 snapshot；若最新 JSONL 内没有可用 snapshot且它是首个 segment，才把暂存 commit 反转后从 `SessionCreated` 开始 fold。只有最新 JSONL 无法提供恢复起点时，才向前读取更早 segment 进行容错恢复；不能在正常路径遍历已封存历史。

### 5.2 Snapshot（`SNAPSHOT-01`、`SNAPSHOT-02`、`SNAPSHOT-03`）

snapshot 只在上下文交接后产生，compaction 和 handoff 共用此路径。它是由当前 state 生成的派生 event：损坏或缺失只能影响恢复成本，不能改变其他 event 表达的事实。

snapshot 保存“当前仍需恢复”的数据，即只读取 snapshot 和后续 event，就能作出与崩溃前相同的下一步 Decision，并正确处理之后输入、工具结果和 deadline 所需的数据。它包括：

- 当前 session 配置和 generation；
- 当前 generation 的 provider 上下文；
- 未消费输入；
- active turn 和 step；
- pending ToolInvocation；
- agent 已知工具版本；
- 定义了 fold 的工具当前 state；
- token anchor、deadline 和连续 runner 故障状态。

snapshot 不复制仅供历史查询的旧 generation，不保存完整 event ID 集合、永久工具结算表或历次工具 state。

snapshot 保存 `state_schema_version`。恢复时先检查版本，再在内存中执行纯 state migration；旧 snapshot 不写回。snapshot 中的 tool state 由对应工具自己的 migration 处理。

不为 snapshot 或 event 单独增加内容 hash。无法使用最新 snapshot 时，恢复使用更早的可用 snapshot或从 event 恢复。

### 5.3 大结果文件（`TOOL-13`）

ToolResult 中持久化的唯一 JSON 载荷有大小上限，并必须包含工具 fold 和展示所需的全部数据。更大的展示内容由工具写入普通文件，ToolResult 只保存有界预览和文件路径。

文件在引用它的 ToolResult 持久化前完成写入和 flush。fold 可以保存和传递文件路径，但不能读取文件；文件之后被修改或删除不影响 event fold 和恢复的正确性。

持续产生输出的工具直接保留运行期间写入的同一个文件，完成时不复制到另一套存储。`shell.run` 从运行开始写入 `.zork/live-<invocation-id>.log`，ToolResult 返回该路径和有界 tail；运行中和完成后都通过普通 `file.read` 读取同一个文件。

## 6. State、Decision 与恢复

### 6.1 State（`STATE-01`、`STATE-02`）

SessionState 是 event 的纯派生状态，不是第二事实源。它只保存执行和恢复当前 session 所需的数据。

State 不包含：

- `seen_events` 或其他 event ID 去重集合；
- 永久增长的 `credited` 工具结算表；
- 逐次追加的 opaque effects；
- 完整历史 generation。

工具结果到达时，core 从 ToolRegistry 取得对应工具的纯兼容逻辑，将 result 和当前 tool state 迁移到当前 schema，再执行 `fold(state, result) -> next_state`。未定义 fold 的无状态工具不创建 tool state。

core 与各工具分别提供纯 `outstanding` 计算。runtime 合并这些结果，用于文本输出和 `end` 的完成门禁。

### 6.2 Decision（`DECISION-01`）

运行时下一步枚举命名为 `Decision`。Decision 只能根据 SessionState 和显式传入的 world 数据作出，不读取时间、executor、registry 或外部资源的隐藏状态。

runner 执行一个 Decision 后，把执行结果持久化为 event，再重新计算。Decision 不直接调用 provider 或工具。

### 6.3 可恢复错误（`RECOVERY-01`）

格式或顺序异常只形成诊断，不自动停用 session。只要剩余数据仍足以恢复 session 身份、配置、当前状态并构造合法 provider 请求，runtime 就必须继续，并通过持久 event 把异常告诉 agent。

只有确实无法构造下一轮 provider 请求时，agent 才不能继续执行。不存在的 session 与损坏到无法恢复的 session 必须区分。

## 7. Provider 与 Generation

### 7.1 ModelGateway（`PROVIDER-01`、`PROVIDER-03`、`PROVIDER-04`）

session 只依赖一个 ModelGateway，请求、结果和错误类型只定义一次。ModelGateway 内部负责：

- profile 解析、provider 路由、密钥和协议；
- provider 自己的重试；
- 连接、continuation、缓存和资源回收；
- provider wire ToolCall；
- 取消和 transient text delta。

runner 只提交 selection、session/generation/step 身份、完整 transcript、固定 `call` 定义和输出上限。

provider adapter 必须按所选协议发送 profile 解析出的控制项，不得硬编码覆盖，也不得发送该协议不支持的字段。`parallel_tool_calls` 在协议支持时原样发送；`max_output_tokens` 在协议支持时原样发送，在 ChatGPT Codex Responses 协议不支持时只用于请求前的输入预算，不写入 wire request。provider 返回 incomplete/failed 终态时，具体 reason、request 身份、非敏感输入诊断和失败前实际收到的 usage 必须保留到持久错误事实中；没有收到 usage 时保持缺失，不用估算值代替。完整逻辑请求由 `StepStarted` 对应的 generation 前缀、selection、工具目录和输出上限确定，错误 event 不再复制 transcript。

runner 可以向 ModelGateway 提供 session 或稳定 key 的资源释放建议。建议不影响正确性，provider 自己决定何时执行。

### 7.2 Generation 前缀（`PROJECTION-01`、`PROJECTION-02`、`PROJECTION-03`）

同一 generation 内，已经发送给 provider 的 transcript 只能追加，不能删除、替换或重新组织前面的消息。ToolResult 在下一请求前到达时可以直接配对；一个 pending 调用一旦以“仍未完成”的合法占位结果发送，后来的真实结果只能作为新增通知出现。

provider wire call ID 只负责 provider 协议配对。每个 provider call 同时生成 zork-agent ToolInvocation ULID；state、ToolExecutor、ToolResult 和 fold 只使用内部 ULID。provider projection 从已匹配的调用声明中重新取得 provider 原始 ID，不依赖 ModelGateway 内存 continuation。

任何可恢复 event 序列都必须被投影成 provider 接受的消息序列。需要补齐 provider call/result 配对时，补充内容必须说明未完成、中断或结果未知，不能伪造成功。

### 7.3 Token anchor 与统一交接（`PROVIDER-02`、`CONTEXT-01`、`CONTEXT-02`）

一个 generation 的第一个普通请求直接发送，不做本地 token 估算。主对话响应返回的准确 input token 数作为持久 anchor，之后只估算 anchor 以后的新增内容。profile/model 变化和上下文交接使旧 anchor 失效。独立摘要请求的 usage 必须持久化并计入消耗，但成功或失败都不更新主对话 anchor。

`context_window` 和本次 `max_output_tokens` 必须在发送请求前由所选 profile 或 provider/model 默认值确定。无法确定时拒绝该 selection；runner 不提供猜测性的全局兜底值。上下文维护触发比例和增量 token 估计方法可以使用系统默认值。

provider 可见的固定 `call` 定义已经包含在准确 anchor 中；adapter 因无状态协议重复发送相同定义，不表示新增 token 内容。

前置 Decision 按 session 配置选择 conversation、compaction 或 handoff step，默认 compaction。普通 step 只使用当前已准备好的 generation，不分辨其产生方式；不增加第二套策略执行器、provider 循环或重试系统。一次交接开始后冻结 purpose 和保留范围，改配置只影响下一次操作。交接不消费新输入；普通新输入不取消当前操作，cancel 和 delete 可以取消。两种方式都没有跨请求总时限。

两种方式共用 `ContextApplied`：原子提交文档、原文保留范围和 carried tools，创建新 generation、重置主对话 anchor、保存 snapshot、释放旧 provider continuation。移动原文不重新 fold ToolResult 或执行工具，持久历史不删除。

无文档降级是所有交接方式的兜底。文档为空、输出不合法、明确输出上限/内容过滤导致不完整或工具参数不能解码，都消耗有限的文档尝试次数；耗尽后原子写入 `ContextFailed` 和无文档的 `ContextApplied`，继续同一个 turn。provider 明确拒绝交接请求的上下文也走此出口；普通请求超限时，compaction 先尝试独立摘要，handoff 可直接降级。

只有文档可以缺失：未消费输入、未完成工具、未交付真实结果、tool state、session 配置和待处理通知必须保留，并提供 `history.list` 恢复指引。摘要/交接期间看到但未进入保留原文的结果仍须交付；已保留的结果不重复发送。普通网络、鉴权、限流等失败走独立 provider 重试，不能通过丢上下文处理；耗尽后按原规则结束 turn 为 Failed。

### 7.4 独立摘要与最近原文（`COMPACTION-01`）

compaction 使用当前 session 的模型与 thinking，独立请求摘要，不继承主对话 continuation、加密 reasoning 或缓存路由，不提供工具。输入是前次摘要/交接文档及将移出的旧前缀，要求保留任务、约束、决策理由、进展、关键文件命令、阻塞和下一步，不能执行原任务。非空纯文本即合法摘要，不强加格式 schema。

最近原文默认目标为 20,000 个估计 token，由 `keep_recent_tokens` 配置，并限制在当前输入预算的一半内。只在完整 provider call/result 组之间切分，不以用户 turn 为最小单位，支持单个超长 turn；超过目标的原子组整体进入摘要。窗口大小估计不取代 API token anchor。

摘要材料采用有界摘录：工具结果每条约 2,000 字节，其他历史条目约 16,000 字节；优先保留前次文档、最早任务和较新的旧前缀。总文本采用不大于输入 token 预算数值的保守字节上限，另有 1 MiB 内存保护上界，截断必须标记。摘要请求沿用所选 profile/model 的输出上限，不另设固定上限；adapter 只发送协议支持的字段。完整原文仍在历史中。

工具结果摘录先保留有界的调用参数和实际执行元数据，再截取正文，避免源码输出挤掉 offset、退出码等证据。摘要应区分已读、已改、已验证和计划，保留硬性要求与具体下一步，并依据后续证据修正旧摘要；工具调用成功不表示所有附加参数均被使用。当前工具说明和后续观测优先于摘要中的历史描述。

摘要文字及其流式 delta 不作为普通回复发布，原始请求结果、失败诊断和 usage 仍持久化。benchmark 总消耗包含摘要，并单列 compaction 次数和输入/输出 token；主对话上下文峰值不混入摘要请求。

### 7.5 Handoff 文档（`HANDOFF-01`、`HANDOFF-02`、`HANDOFF-03`）

`handoff` 不属于普通 ToolRegistry，不出现在初始工具列表、工具变化通知或 `tool.help` 查询结果中。它只在 handoff step 临时存在：runtime 必须把完整用法作为 user role notification 追加，明确要求不返回解释文本、不调用 `tool.help` 或任何其他逻辑工具，只提交一次 `{"tool":"handoff","arguments":{"document":"..."}}`。为保留同一 generation 的 provider continuation，固定 `call` schema 不变；runtime 直接拒绝且绝不执行该 step 中除唯一合法 `handoff` 外的任何调用。

handoff step 不进入普通 ToolExecutor：runtime 要求恰好一个 `handoff` 调用，读取非空字符串 `document`，忽略未消费的附加字段。合法时原子保存 `StepCompleted` 与 `ContextApplied`，立即进入新 generation；handoff 不保留旧原文窗口。

文档不合法时按共用规则留在原 generation 重试。原始输出由 `StepCompleted` 保存，其中的 provider call 只补失败 ToolResult 以满足协议配对，不执行逻辑工具。

`ContextApplied` 冻结当时未完成调用的 carried tools，包括内部 ID、工具名、参数和开始时间。新 generation 说明这些调用在交接时尚未完成，不伪造原调用的 provider call/result 对；后来的真实结果只追加通知。

## 8. 动态工具

### 8.1 固定 provider call（`TOOL-01`）

provider 只注册一个固定工具 `call`：

```json
{ "tool": "aa.bb", "arguments": {} }
```

`tool` 是逻辑工具完整名称，`arguments` 始终是 JSON object。tool version、provider call ID 和内部 ToolInvocation ULID 都由 runtime 添加，不由 agent填写。固定 schema 不枚举动态工具名。

provider 即使违反该固定 schema，原始响应也仍按 `StepCompleted` 持久化；对应调用只在执行边界被拒绝并返回失败 ToolResult，不触发 provider step 重试。

当前 ToolRegistry 注册的内置逻辑工具为（`handoff` 仅在 §7.5 的 handoff step 临时可用）：

- `end`
- `wait`
- `tool.cancel`
- `tool.help`
- `history.list`
- `file.read`
- `file.write`
- `file.edit`
- `shell.run`

`file.read` 保留一套 offset/limit 读取 API：普通路径读取文件系统，`zork://history/<event-id>` 委托 SessionQuery；runner 不处理 URI 特例。

### 8.2 ToolRegistry（`TOOL-02`、`TOOL-03`、`TOOL-04`、`TOOL-08`、`TOOL-09`）

ToolRegistry 是进程级共享目录。每个完整逻辑工具独立注册、独立版本化，点号前缀只是命名空间。

一个 registry entry 统一保存：

- opaque agent-visible tool version；
- 初始说明、详细说明和参数 schema；
- 当前不可变执行实例；
- 可选 initial state、纯 fold 和纯 outstanding；
- result/state schema migration；
- 工具删除后旧 session 仍需的纯兼容逻辑。

runner 只能比较 tool version 是否相等，不能解释格式或比较大小。tool version 只表示 agent 可见契约；result 和 state schema version 独立。

工具更新通过原子替换当前不可变实例完成。调用只查询一次 registry：版本相等时固定使用该实例完成调用；版本不等时不执行副作用，返回版本已更新的 ToolResult。调用执行期间不持 registry 锁。

删除工具后，当前执行入口不可用，但旧 result/state 的 migration、fold 和 outstanding 继续存在。同名工具以后重新加入时按新增工具处理。

### 8.3 Agent 已知版本与说明（`TOOL-05`、`TOOL-06`、`TOOL-07`）

初始 generation 和上下文交接把当时所有工具的初始说明发给 agent，并把对应版本保存为 agent 已知版本。工具作者决定初始说明是完整说明，还是只说明用途和何时查询详细用法。

`tool.help` 按完整名称返回目标工具当前版本的最新详细说明，并更新该工具的 agent 已知版本。

每次 provider 请求前，runtime 比较当前 registry 与 agent 已知版本。工具新增、更新或删除时，追加简短通知，提示 agent 需要时调用 `tool.help`；通知不附带工具说明。通知一旦发送就认为 agent 已知，立即更新或删除已知版本项，不重复提示。

每次 ToolInvocation 自动携带 state 中该工具的 agent 已知版本。执行前若与 registry 当前版本不匹配，返回的 ToolResult 与版本通知执行相同的已知版本更新，避免下一轮再重复通知。

### 8.4 ToolExecutor（`TOOL-10`、`TOOL-11`、`TOOL-12`、`TOOL-14`）

一批 ToolInvocation 持久化后整体交给 ToolExecutor。并行度、排队和实际启动顺序由 ToolExecutor 管理，auto wait 不参与调度。

所有工具调用都只消费 schema 声明的字段。runtime 在校验和执行前递归丢弃多余字段；已声明字段缺失或类型错误仍返回失败 ToolResult。provider 原始输出保留在 StepCompleted 中用于审计。

内置工具在独立、受监督的 task 中执行。参数错误、返回错误、I/O 错误、超时、取消和 task panic 都转换成正常 ToolResult，不能炸掉 runner。

执行表保留调用直到真实结果进入 runner 队列；runner 先读取执行表快照，再消费结果队列，不能把完成与投递之间的间隙误判为中断。停止 runner 时先关闭结果队列，避免队列背压阻塞工具收尾。

第三方工具不能作为 native plugin 加载；需要第三方工具时使用 WASM 隔离。当前没有已确认的第三方工具 ABI，因此 runtime 不提前实现猜测 ABI。

`tool.cancel` 先持久化取消请求，再通知 ToolExecutor。取消信号不等于取消成功；目标调用最终完成、失败、取消或中断只由真实 ToolResult 表达。

## 9. Turn、Auto Wait 与重试

### 9.1 Turn（`TURN-01`、`TURN-02`）

`end` 是 turn 正常结束的唯一信号。agent 只输出文字时，文字持久化并展示，但 turn 继续。

只有成功执行的 `end` 才能结束 turn；失败结果返回 agent 后继续下一轮。

agent 只输出文字或调用 `end` 时，runtime 都计算全部 outstanding 并把清单告诉 agent。`end` 默认不在 outstanding 存在时结束；agent 可以在看过清单后显式确认带着 outstanding 结束。

### 9.2 Auto wait 与 mailbox（`WAIT-01`、`WAIT-02`、`MAILBOX-01`）

每批工具调用只有一次 auto wait，结束条件为：

1. 整批完成；
2. 超时；
3. 收到新的 mailbox 消息。

任一条件触发时，已完成结果和未完成状态一起进入下一轮。尚未完成的调用以后完成或失败时只追加通知。工具自己的 result 不被当作“新消息”提前结束 auto wait。

provider 请求执行期间，新 mailbox 消息先持久化，但不自动取消请求；只有明确 cancel 才中断。

### 9.3 Provider 重试（`RETRY-01`、`RETRY-02`）

provider 内部重试属于同一个 step。ModelGateway 返回最终错误后，runner 的每次重试都是新的完整 step：先持久结束上一个 step，再写新的 `StepStarted`。

只有百分之百确定重试无法解决的 provider 错误才不重试；其他错误默认退避重试十个完整 step。耗尽后把当前 turn 结束为 Failed，但 session 保持可用，新输入可以开启新 turn。

## 10. Supervisor、Deadline 与恢复

### 10.1 有界队列（`SUPERVISOR-02`、`SUPERVISOR-03`）

supervisor 对尚未持久化的消息同时设置全局和单 session 容量上限。普通外部提交在入队前取得容量：单 session 满返回 HTTP 429，全局满返回 HTTP 503，不等待、不写 event。

内部 ToolResult 可以等待有界队列容量，但不能丢弃，也不能通过隐藏 Vec 或无界 task 堆积绕过容量限制。

channel 发送失败时，只重新入队 channel 明确退回、可以证明 runner 从未收到的消息。发送成功后 runner 在确认前退出，该次提交失败，不自动重投。

### 10.2 Runner 生命周期与故障（`SUPERVISOR-01`、`SUPERVISOR-04`）

runner 只在 session 需要执行时存在。完全空闲后 runner 正常退出，槽位退回轻量状态；新事实到达时重建。

runner 异常退出后，supervisor 重建 runner，并先写入 agent 可见的 RuntimeFault。可预期错误使用固定 `failure_id + stage` 作为指纹；未捕获 panic 使用 `stage + panic payload` 兜底。

只有连续相同指纹才累计。重建后完成至少两个 `StepCompleted` 即清零。默认第五次相同错误熔断，前四次自动重建；上限可配置。

### 10.3 Cancel 与 Delete（`CANCEL-01`、`DELETE-01`）

`/cancel` 只取消当前 turn。runner 先持久化取消请求，再中断当前 provider 请求和本 turn 启动的工具，结束当前 step，并把 turn 记为 Cancelled。取消本身不触发新的 provider 请求；新输入可以开启新 turn。

DELETE 必须经过 supervisor。supervisor 先拒绝新工作，停止 runner、工具、deadline 和 provider 请求，再把整个 session 目录原子移出活动目录；移动成功后确认删除，物理清理可以后台完成。

### 10.4 Deadline scheduler（`DEADLINE-01`）

全部 session 共用一个进程级 deadline scheduler。deadline 的事实来源仍是 session event/state；scheduler 只保存可重建内存索引。

到期时 scheduler 只唤醒 runner。runner 重新读取当前 state 和时间，确认 deadline 仍有效后写到期 event。过期或重复唤醒不能直接改变状态。

### 10.5 启动恢复（`STARTUP-01`、`STARTUP-02`、`STARTUP-03`）

zork-agent 不等待全量恢复完成才提供服务。启动后立即监听 HTTP，同时后台发现全部历史 session，并为每个 session 建立轻量槽位。

恢复按最近活动时间倒序进行。历史和完整 state 一次只加载一个或有界数量的 session；正常结束且当前无工作的 session 只保留轻量完成状态，不持有 runner、channel、provider 资源或完整 state。

后台检查正常结束的 session 时，只从最新未压缩 JSONL 尾部反向读取最后一个完整 batch。若它明确以 `TurnFinished(Finished)` 结束，直接记录轻量完成状态；只有结尾不能明确证明正常结束时才做完整恢复。收到请求的 session 不等待后台顺序，直接读取最新 JSONL 精准恢复。

请求命中尚未检查的 session 时，该 session 进入恢复优先队列。请求和后台扫描汇合到同一次检查，不得并发或先后重复检查同一个 session。

十万 session 是必须通过的规模验收，不通过时不能以增加第二套持久恢复索引代替修正扫描和槽位实现。

## 11. Query、HTTP 与 SSE

### 11.1 SessionQuery（`QUERY-01`、`QUERY-02`）

SessionQuery 是全部持久读取的独立模块。它读取有序 segment 文件名，用首个 event ID 二分定位 segment；反向行、反向 commit、snapshot 定位、启动发现和 history cursor 都由它负责。

公开边界只有一个 `SessionQuery` trait，文件实现为 `FileSessionQuery`。通用数据结构为 `Commit { events }`、`ReadResult<T> { value, diagnostics }`、`ReadSummary { diagnostics }`、`SnapshotWindow { origin, commits }`；窗口起点明确区分 `Snapshot`、`SessionStart` 和 `Unanchored`，上层不再自行猜测读到的数据能否作为恢复起点。

除 `after`、`before`、`event` 外，Query 直接提供 `exists`、`discover_sessions`、`last_commit`、`snapshot_windows` 和 `all_commits_forward`。这些函数只组合日志读取，不迁移 snapshot、不 fold state，因此既避免 supervisor、recovery、工具各自重复拼装，又不吞并领域职责。

`before` 不从 session 起点全扫。它先用 cursor 定位所在 segment，再在该 segment 内正向扫描到 cursor，同时只用环形 buffer 保留最后 `limit` 条可见 event；数量不足时，按 segment 从新到旧回退，但每个 segment 内仍然正向扫描，只保留尚缺的数量，凑满即停。这样工作内存为 `O(limit)`，并且同一实现能读取普通 JSONL 和 zstd segment。`after` 与 `event` 同样先定位 cursor segment，不读取更老的 segment。反向读取只用于最新未压缩 JSONL 的尾 commit、snapshot window 和压缩截断。

history query 不保留跨调用数据缓存。扫描阶段借用 raw event 字段完成 schema 校验，只有进入最终窗口的行才构造领域 event，避免为随后被环形 buffer 淘汰的 payload 分配内存。查询实现必须持续满足第 13.3 节的吞吐与内存门禁；如果无缓存实现仍能达标，就不引入 fragment 尺寸、淘汰和 active tail 失效规则。

SessionQuery 可以建立自己的可重建优化，但不能成为第二事实源，也不能把查询逻辑塞回 store append 或 runner。`history.list` 和 `file.read(zork://history/...)` 复用同一 SessionQuery API。

snapshot 不进入 history 查询结果。未知或非法 cursor 返回明确错误，不静默当作流首或流尾。

### 11.2 HTTP（`HTTP-01`、`HTTP-02`）

zork-agent 只保留一套无版本业务路由，不提供 `/v1` alias。Agent 与 Gateway 共享同一份请求、响应、错误和 SSE schema，不使用宽松 JSON fallback 掩盖 malformed response。

session 只能由显式创建产生。只有持久化 `SessionCreated` 后 session 才存在；未知 session 的读取、提交、选择变化和取消返回 404，不能隐式创建空流。

全局配置 `context` 是新 session 的默认值，`POST /sessions` 可用同名字段覆盖；创建和 `ContextConfigured` 原子持久化。`PUT /sessions/{id}/context` 修改该 session 的配置，`SessionView.context` 返回当前值。字段为 `strategy: compaction | handoff` 和 `keep_recent_tokens`，后者允许为零。

### 11.3 SSE（`SSE-01`、`SSE-02`、`SSE-03`）

event ID 是 durable SSE cursor。首次连接、`Last-Event-ID` 重连和广播落后时，SSE 从 SessionQuery 补齐 cursor 之后的可发布 event；实时路径广播完整持久 event，而不是只广播“有变化”的信号。

snapshot 虽然占用 event ID，但不发布到 history 或 SSE。cursor 中出现间隔是正常现象。

provider text delta 是 transient SSE：客户端可以选择是否订阅；delta 不写 event、没有 durable 补发保证。最终 provider 结果仍作为 durable event 保存。

每个 session 的订阅独立。其他 session append 不能唤醒订阅者并触发当前 session 全历史重读。

## 12. 代码边界

目标实现只保留以下会话域模块：

```text
event_id     event ID 编码和校验
events       当前 event schema 和 migration
state        纯 fold 与 snapshot state migration
decision     Decision 与 outstanding
projection   generation/provider transcript
context      独立摘要材料与安全原文切分（纯计算）
model        唯一 ModelGateway 词汇和端口
tools        ToolRegistry 与工具纯兼容逻辑
executor     受监督 ToolExecutor
runner       单写者执行循环
supervisor   槽位、队列、恢复调度和 runner 生命周期
recovery     snapshot migration、event fold、诊断和 SessionState 恢复
store        单写、data root 锁、切段、压缩和 detach
query        持久读取、反向 commit/snapshot、启动发现和 history/cursor
deadline     全局 scheduler
service      HTTP 使用的薄门面
```

只做逐字段复制的 provider adapter、第二套 ModelPort、静态 provider 工具表、工具执行旁路、正常 append 重放、无界 dispatcher 和全流 history 读取都不属于目标实现。

HTTP wire contract 单独放在无运行时逻辑的 `zork-agent-api` crate 中；Agent handler 和 Gateway client 都直接使用这些请求、响应、错误、状态与 SSE envelope 类型，不各自复制领域结构。

## 13. 验收与测试

### 13.1 合同引用（`TEST-DOC-01`）

本文件同时是测试合同的唯一语义入口。可观察行为由自动化测试证明；只能检查模块所有权或“不增加某抽象”的要求由结构审查证明；只有产生真实旧 schema 后才有输入的 migration，在第一次版本升级时连同旧 fixture 一起提交。

涉及关键合同的测试可在定义旁标注编号，便于定位语义；不以源码注释或实现字符串匹配代替行为验证：

```rust
// Contract: docs/zork-agent-architecture.md [PERSIST-01]
```

### 13.2 测试环境（`TESTKIT-01`、`PERF-01`、`PERF-02`）

zork-agent 的行为测试直接使用 Rust，不引入脚本语言、测试 DSL、跨语言控制进程或第二套领域类型。测试采用命令式 API：测试取得 agent 发出的 provider 请求并断言，随后提供响应，也可以在任意受控时点发送 mailbox 输入。

测试环境按副作用模块独立组合。provider、mailbox、时钟、ID、workspace 文件系统、持久存储 I/O、进程、环境和生命周期各自拥有真实实现或受控实现；不存在一个包含全部行为的巨大虚拟环境接口，也不存在按工具名绕过真实 runner 的测试分支。

虚拟测试接管几乎全部副作用，但继续执行真实的 state、Decision、projection、runner、supervisor、ToolExecutor 和对应的工具编排。受控实现必须允许测试决定 provider、工具、mailbox 和 deadline 的发生顺序，允许在稳定的持久化边界检查后继续，并支持进程中断与恢复。虚拟环境不得进行真实等待、磁盘 I/O、网络 I/O 或子进程启动；其创建、重置和单步推进必须足够快，才能用于大量细粒度行为测试。

真实组件综合测试只接管 provider 对端和 mailbox 输入。它在测试进程内装配 production `SessionService`、`ProfileStore`、`ProviderRouter` 和 HTTP router，使用真实 HTTP/SSE/WS adapter、时钟、data root、workspace、StreamStore 和 shell。进程边界由单独的真实二进制合同验证配置、监听、ready PID 和信号退出；Gateway 集成测试继续启动真实 zork-agent 二进制。接管 mailbox 只表示测试决定何时通过真实接口发送输入，不得绕过 mailbox 的持久化和 runner 路径。

真实组件测试数量保持很少。一次 production composition 生命周期使用多个相互独立的 session 和 workspace 覆盖尽量多的正向场景，并集中执行一次重启以验证恢复，避免为每个场景重复支付服务和 provider 对端的启动成本。只有进程边界或无法共享同一真实拓扑的行为才拆成另一项真实测试。

纯 provider 时序合同直接组合 production `ProviderRouter` 与受控真实 TCP 对端，不额外包一层 runner。真实 socket 与 Tokio paused time 同用时必须保持 runtime 活跃，避免 runtime 在操作系统 I/O 尚未就绪时自动快进到内部计时器；只有测试脚本显式推进的时间才算经过。

虚拟实现不能代替真实 adapter 的合同证明。真实文件系统的锁、同步和原子操作，真实 shell 的解析、进程组和信号，以及 HTTP/SSE/WS framing 分别由真实合同或综合测试覆盖。同一项逻辑不因测试模式而复制；真实和虚拟模式只在明确的副作用端口处替换实现。

性能验收分为两层。虚拟层用大量独立 session 执行完整的 mailbox -> provider request -> provider response -> durable finish 路径，测量总耗时、每秒完成数和最终 event 数；工作负载中不得出现真实 sleep、磁盘、网络或进程。真实层在一次 agent 生命周期中复用 provider 对端和服务，执行多 session、真实 HTTP、真实 workspace、真实 shell、持久化和一次重启，测量启动成本与整批场景耗时。两层都保留按需运行的 release benchmark；默认 CI 验证功能正确性，不重复执行依赖主机速度的 debug 跑分。性能变更和发布前使用下述完整门槛单独验收，不能用一次偶然跑分证明性能。

event ID、event/segment 编解码、query 游标、ToolRegistry、ToolExecutor 和 provider 协议解析等局部合同直接保留在所属 Rust 模块。Gateway、Slack、Admin 和发布流程测试只消费 zork-agent 公共接口，不属于 zork-agent 测试框架。

### 13.3 稳定性能门槛（`PERF-03`、`PERF-04`）

- release 使用 10000 个完整虚拟 session，检查持久化 event 总数并报告耗时和吞吐。
- 32 MiB 半结构化高熵 segment 使用正式 zstd level 12 流式压缩，独立进程峰值 RSS 不超过 256 MiB，并报告压缩率与吞吐。
- 启动基准使用十万个真实 session 目录，fixture 构造不计入测量；精准恢复不超过 0.1 秒，请求优先恢复不超过 1 秒，后台完整检查不超过 10 秒，独立进程峰值 RSS 不超过 512 MiB。
- 正式随机查询门槛使用单个 131072-event session，执行 1024 次 before/after 查询，每次返回 1024 events；串行和 8-worker 吞吐都不得低于 20 queries/s，独立进程峰值 RSS 不超过 256 MiB。
- 极限查询压测使用 100 sessions × 每 session 100 fragments × 每 fragment 至少 16 MiB 逻辑 event 数据，执行 10000 次确定性随机访问，随机选择 session、fragment、cursor、before/after 和 1..=1000 返回上限；报告 1/2/4/8/主机最大 workers 的吞吐、p50/p95/p99，独立进程峰值 RSS 不超过 256 MiB。fixture 构造不计入查询吞吐。
