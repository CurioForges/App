# 启动器自更新 + 更新进度展示（设计草案）

> 状态：**已记录，未实现**。2026-11-12 用户提出并修正方向。
> 方向修正：**启动器不再调用 `install.ps1`**。启动器自己从仓库读**域组合配置**，自己下载各域
> artifact、自己组装安装目录、自己构建域运行环境。
> `install.ps1` 仍保留在仓库与 release 资产里，只服务「没装启动器的人」——两条路互不依赖，
> 但必须咬合到同一份 `release/artifacts.json`（见 §4.5）。

---

## 1. 为什么可以不要 install.ps1

`release/artifacts.json` 本来就是「域组合配置」：

```jsonc
{ "schema": 1, "train": "0.1.9",
  "kernel_version": "0.5.2", "sdk_version": "0.5.2",
  "artifacts": {                       // 每个域一个 artifact 仓库 + 资产名 + 源码 sha
    "web":       { "repo":"NomadAnget/atelier-web",  "release":"domain-artifacts", "asset":"AtelierWeb-k0.5.2-s0.5.2-<sha>.zip", "source_sha":"…" },
    "scheduler": { … }, "pipelines": { … }, "engines": { … },
    "video":     { … } } }
```

启动器要做的四件事，`install.ps1` 只做了第 2、3 件的一半：

| 步骤 | 现在谁做 | 以后谁做 |
| :-- | :-- | :-- |
| 读组合配置（装哪些域、什么版本） | 由 Free 仓库构建时写死进 zip | **启动器**拉 `release/artifacts.json` |
| 下载 kernel + 各域 artifact | `install.ps1` 下 282 MB 整包 | **启动器**逐域下载（自己算进度/速率/校验） |
| 组装安装目录 | `Expand-Archive` 整包解压 | **启动器**按域解压、原子换入 |
| 域运行环境（uv venv） | 首用时由 engines 域自己建 | **启动器**按 manifest 里的 env 清单 `uv sync` |

顺带解决的既有毛病：整包 282 MB 每次全下、`%TEMP%` 漏 295 MB、BOM/`taskkill` 那类脚本坑、
失败后安装目录处于半新半旧状态（现在天然变成"暂存 + 原子换入"）。

---

## 2. 进度模型（契约）

```jsonc
{ "job": "install" | "launch" | "self-update" | "env",
  "phase": "resolve|plan|download|verify|stop|stage|swap|env|start|check|done|fail",
  "label": "正在下载 创作管线",
  "percent": 37.5,                      // 0–100；null = 不确定(环形转圈,不显示数字)
  "done": 12345678, "total": 296000000, // 字节量(下载/解压/venv) → 速率与 ETA
  "rate": 12500000,                     // 字节/秒,Rust 侧算
  "detail": "3/8 个组件",
  "log": "…" }
```

- Tauri 事件 `atelier://progress`（≤4 Hz 节流）+ `atelier://progress-log`（滚动日志，留 200 行）。
- **总进度** = 各组件按**字节权重**加权（kernel 2.7 MB、web 94 MB、pipelines 30 MB、engines 189 MB、video ?）；
  `percent: null` 只出现在 `resolve/plan/stop` 这种无字节的阶段。
- 诚实原则不变：拿不到真量就 null。

---

## 3. 阶段与来源

| 阶段 | 具体动作 | 进度来源 |
| :-- | :-- | :-- |
| `resolve` | `GET /repos/NomadAnget/AtelierFree/releases/latest` → tag；`raw.githubusercontent.com/.../<tag>/release/artifacts.json` | 不确定 |
| `plan` | 只装「免费四域 + video 插件」；付费域等授权后再加（`catalog::licensed()`）；算总字节 | 不确定 |
| `download` | 每域：`<repo>/releases/download/domain-artifacts/<asset>`；kernel：`Atelier-Kernel` release；SDK wheel 同 release | **精确**（流式字节累计） |
| `verify` | 每个资产的 `.sha256` 旁挂比对（挂不上就拒绝，不降级） | 精确（快） |
| `stop` | 启动器自己的 `stop_sync()`（已有） | 不确定 |
| `stage` | 解到 `<install>/.staging/<域>/`，绝不动现有安装 | 按条目计数 |
| `swap` | 逐域原子换入：旧目录改名 `.old-<ver>` → 新目录 rename → 失败则回滚 | 按组件计数 |
| `env` | 读 `engine-root/domains/engines/environments/<env>/windows-x86_64/{.python-version,pyproject.toml,uv.lock}`，用 `engine-root/uv.exe` 逐个 `uv sync --frozen` | 按 env 计数 + uv 输出行 |
| `start` / `check` | 启 kernel（`kernel_boot.atelier.json`，已有）→ 轮询 `/api/assembly` 四域 `active` | 状态机 |

`data/`、`engine-root/`、`models/` 永远不进 swap 范围（与 `contract::PRESERVED` 一致）。

---

## 4. 落地顺序

1. **Rust：`installer.rs`** — 组合配置解析 + 逐域下载（reqwest 流式 + 断点续）+ sha256 + 暂存 + 原子换入。
   先只做「免费四域 + video + kernel + sdk」，env 阶段留空。
2. **进度事件**：上面这套天然有字节数，接 §2 的模型发事件；UI 覆盖层同时做。
3. **`env` 阶段**：调 `uv sync`，把 uv 的 stdout 行转成日志事件；失败只警告，不阻断启动
   （引擎首用时还会再试一次）。
4. **启动器自更新**（独立一条流）：版本源 `NomadAnget/Atelier` release → 下 exe → sha256 →
   `apply-update.ps1` 等旧进程退出后 `Move .old` + 换名 + 启动；失败还原 `.old`。
5. **`install.ps1` 保留不动**：它是「没有启动器」时的独立安装器（整包 zip + 校验 + 停旧进程），
   仓库与 release 资产都留着。启动器**不再调用它**，两条路互不影响。要求：
   - 两者产出同一套目录结构，都从 `release/artifacts.json` 取版本（`install.ps1` 用整包，
     启动器用逐域 artifact）——版本一旦不一致，用户会得到"启动器装 0.1.9、脚本装 0.1.9 但域是旧的"的错觉；
   - 发布后抽一次「脚本装的树 vs 启动器装的树」文件清单比对，允许差异只在 `VERSION`/时间戳；
   - `.ps1` 一律 **UTF-8 with BOM**（PS 5.1 会按 ANSI 读无 BOM 文件，中文注释会变成语法错误，
     0.1.8 更新就是这么挂的）。

---

## 5. UI

- 点「更新」→ 实例卡进入 `updating`：卡片整体变暗 + 覆盖层；中间 SVG 环形进度（直径 ~120px、粗 8px，
  底 `--line`、进度 `--accent`）；环心显示百分比（不确定时转圈）；环下：阶段文案 → 明细行 →
  滚动日志（最多 5 行，`aria-live="polite"`）→ 底部「累计 01:23 · 约剩 02:10」。
- 覆盖层拦截卡片点击；`download/verify` 阶段可取消，`swap` 之后不给取消（避免半文件）。
- 失败：环变红 + 人话 + 「重试 / 查看日志」，日志保留。
- 成功：100% → 短暂 ✓ → 淡出 → 卡片显示新版本号，四域状态重拉。

---

## 6. 验收

1. 点「更新」1 秒内出覆盖层，百分比单调，最终与 `VERSION` 一致。
2. 下载阶段显示每个组件的速率与 `已下/总量`，总进度按字节加权。
3. 断网/校验失败 → 现有安装**完全不受影响**（暂存目录被丢弃）。
4. 无网但各域 artifact 已缓存（`%LOCALAPPDATA%\Atelier\download-cache`）→ 直接走 `stage`。
5. 新机器首次启动：env 逐个出现（speech / hf4-media / hf5-media …），失败不阻断。
6. 启动器自更新：换文件后版本号变化；故意写坏新包 → 回滚到旧版并能启动。
7. 双路一致性：同一版本下，`install.ps1` 装出来的目录树与启动器装出来的目录树文件清单一致
   （`VERSION`、时间戳除外），`atelier-kernel.exe --headless` 两边都能起四域。

---

## 7. env 阶段:启动器预热(已定方案)

更新完成后**不停在体检**,而是自动启动 kernel 并触发一次域环境预热,进度从 kernel 日志/SSE 读回来。

契约(待 engines 域提供):

```text
handler: engines.prewarm
payload: { "envs": ["speech","hf4-media","hf5-media"] }   // 可空 = 全部
返回:    { "started": n }   之后进度走日志流,不阻塞
日志行:  [ENGINES/env] building speech (uv sync) … done
```

启动器侧:

1. `swap` 完成后调 `variant_runtime::open_workbench(None, None)` 等四域 active;
2. 若有 `engines.prewarm` 则调用(能力探测:assembly 的 engines.routes 里有这个 key 才调);
3. 订阅 `GET /api/stream?event=log`,把 `layer=ENGINES` 且含 `env` 的行转成进度事件
   (`phase: "env"`, `percent: null`, `detail: 环境名`);
4. 预热完成或超时(**10 分钟**)就发 `done`;**预热失败不影响更新结果**,只在日志里留一行。

为什么由域来建而不是启动器自己 `uv sync`:环境目录名是
`.venvs/.engine-builds/<env>-<platform>-<hash>`,哈希由域按 pyproject/uv.lock 内容算。
启动器复刻这套命名等于把同一规则写两遍,这正是本轮几个 bug 的共同成因。

验收:新机器首次更新后,日志里能看到每个 env 的 building → done;再次更新时这些行不再出现。
### 7.1 统一预热处理器标准(代替 engines 专用入口)

内核的 `/api/assembly` 已经把每个域的 `routes` 列出来了,所以**不需要为 engines 写特例**:
只要各域遵守同一个命名/返回约定,启动器用一段通用逻辑就能预热所有域。

| 项 | 约定 |
| :-- | :-- |
| 处理器名 | `<domain>.prewarm`,域 id 取自 assembly 的 `domain_id` |
| 入参 | 可空;需要时 `{ "targets": ["speech","hf4-media"] }`,缺省 = 该域全部目标 |
| 返回 | `{ "started": n }`,**立即返回不阻塞**;`n=0` 表示已就绪无需动作 |
| 进度 | 走日志流,行首固定 `[PREWARM] <target> <state>`,state ∈ `building\|ready\|failed` |
| 幂等 | 重复调用不重复构建,直接 `started: 0` |
| 失败 | 处理器自身不抛错;构建失败以 `[PREWARM] x failed: <原因>` 报告 |

启动器的通用逻辑(不再有任何域/环境名硬编码):

```text
1. 启动 kernel,等四域 active
2. GET /api/assembly
3. 对每个 domain:若 routes 里有 "<domain_id>.prewarm" → 调一次,累加 started
4. GET /api/stream?event=log,把 "[PREWARM] …" 行转成进度(phase=env, percent=null,
   detail="<domain>/<target> 构建中"),按行序展示
5. 全部 ready / 超时 10 分钟 / 无 started → done;失败只记日志,不影响更新结果
```

这样以后新增域(比如付费的 daemons/publishers)只要实现 `<id>.prewarm`,启动器一行不用改。
---

## 8. 环境构建的进度表达（定稿）

**不预估总量。** 环境清单随管线/引擎变化，且用户只有第一次安装需要构建，历史体积表永远拿不到第一次的数据，估了也没用。

改为**只报实时速率**，用户看到数字在动就知道没卡死：

- 速率 = `engine-root/.venvs/.uv-cache` 的目录增长量 ÷ 时间差，每秒刷新；
- 环保持旋转（`percent: null`，不编百分比）；
- 日志行始终是最后一条真实的 `[PREWARM] …` 行；**不做心跳、不做交替轮转**——数字本身就在动；
- 缓存命中时速率≈0、几秒 ready，属正常（说明 uv 缓存生效）；
- 收工条件只看 `ready` 数是否达到回执里的 `started`，另设 6 小时总上限兜底。

所需环境仍由域声明决定（`prewarm` 的 targets），启动器不认识任何环境名。