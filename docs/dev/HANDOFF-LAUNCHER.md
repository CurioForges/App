# Atelier 桌面启动器 —— 交接文档

> 更新时间：2026-11-12
> 仓库：`G:\Atelier\Atelier`（Tauri 2 + React 19，Rust 后端）
> 上一份交接：`G:\Atelier\atelier-free\docs\HANDOFF-LAUNCHER.md`（发行包侧，仍然有效）

---

## 1. 一句话现状

启动器已经能装、起、停 Atelier Free，首启三步引导可用，主界面是**域能力面板（只读状态）**，
设置分全局/安装两层。构建与测试干净（Rust 17 项、前端 2 项）。

**验收版本：v0.1.19**（2026-11-12：四域可起、工作台可用、停止干净；pipelines zoneinfo、权重镜像取用、
Windows 分段下载、安装器编码四处缺陷已修，见 §7.1–§7.3；视频管线 S03 的 `Expected str, got VPath`
已定位为 Cython 注解 dict 类型校验并修复，见 §7.4；S04 术语层的 `target_invalid` 与失败日志缺原因
一并修掉，见 §7.5；IndexTTS 的 vendored 代码补进发行包，见 §7.6；S05 的 `tts_path` 参数类型已修，
见 §7.7；插件构建已关掉注解运行时校验并加了编译产物冒烟测试，见 §7.8；
插件里 `open('vfs://…')` 因 Cython 缓存 builtins 而失效的问题已修，见 §7.9；
多比例封面丢失与摘要文件位置已修，见 §7.10）。

---

## 2. 文件地图

```text
src-tauri/src/
  main.rs            命令注册、托盘（打开/数据/模型/重新设置/停止/退出）、关窗隐藏
  contract.rs        契约常量：ATELIER_ROOT / GCP_MODELS_DIR / HF token 两别名 / 四域名
  settings.rs        launcher.json schema 2（shared + installations）+ 旧格式迁移
  credentials.rs     DPAPI 加密的 HF token，只有末四位提示对外
  locations.rs       目录校验、磁盘空间、data/ 目录联接（mklink /J）、卸载友好删除
  setup.rs           首启引导的命令：选目录、校验、测 token、完成、重开引导、打开目录
  capabilities.rs    能力面板数据（目录×安装态×运行态×授权态）+ 全局设置读写
  variant_runtime.rs 端口分配、生成 kernel_boot.atelier.json、启动/停止/SSE 状态、跨安装互斥
  catalog.rs         6 个域的展示元数据（4 免费 + daemons/publishers 收费）+ licensed() 钩子

src/
  main.jsx           域能力页（实例卡 + 能力列表 + 管线子行）、全局设置独立页、内嵌工作台
  setup-steps.mjs     引导页集合与显示顺序（PAGE_ORDER），有测试守着
  views/Setup.jsx    三步引导（顺序从 setup-steps.mjs 取）
  design-system/     Web 域共享组件快照（勿改，见其 README）
  assets/tutorials/  8 张手绘 SVG 教程图
  style.css          全部样式（含启动页、引导、能力页）
```

---

## 3. 契约对齐（都从源码核过，别凭印象改）

| 用户选择 | 落点 | 依据 |
| :-- | :-- | :-- |
| 数据目录 | `<ATELIER_ROOT>/data` 做成**目录联接**指向所选目录 | 域代码硬编码 `<root>/data`（`domains/scheduler/impl/publish_service.py`），没有数据目录环境变量 |
| 权重目录 | 以 `GCP_MODELS_DIR` 注入 kernel | `engines/base/weights.py::models_dir()` |
| HF Token | `HUGGINGFACE_TOKEN` + `HF_TOKEN` 双别名 | `engines/config/runtime_settings.py::worker_environment` |

补充事实：

- 引擎域里**应用设置优先于继承环境**（`values['models_dir'] or inherited['GCP_MODELS_DIR']`）。启动器顺着这条走，
  不要试图压过应用内设置。
- `ollama_host` **只认应用设置**，源码明确不读环境变量（`config/llm_config.py:45`）。所以它只能靠
  `POST /api/settings` 落地，不能靠 env 注入。
- 管线是插件包：`<ATELIER_ROOT>/pipeline-plugins/<id>/{manifest.json,*.pyd}`，由
  `domains/pipelines/discovery.py` 在**域启动时**加载一次 → 装/删插件都要重启 pipelines 域才生效。

---

## 4. 已实现且验证过

- **首启三步**：权重 → Token/Ollama → 数据目录（全局在前、安装级在后），一步一页带图解；
  完成写 `launcher.json`；"重新运行初始设置"在 ⋯ 菜单和托盘都有。
- **启动/停止**：自建 `kernel_boot.atelier.json` 分配空闲端口；等 `/api/assembly` 四域 `status=active`；
  停止优先走命名事件，没有事件则 `taskkill /T /F`，等待有 30s 上限（v0.1.3 与 v0.1.5 都走这条，
  界面会提示"已强制结束：当前 Kernel 没有优雅停机接口"）。
- **单实例 + 托盘常驻**：第二次启动唤醒已有窗口；关窗只隐藏；托盘五项。
- **跨安装互斥**：启动前探测其它 `atelier-kernel.exe`，弹窗列出路径 + 风险说明，
  用户确认后才停对方（后端 `stop_foreign != Some(true)` 时也会拒绝，不会静默动手）。
- **能力面板**：6 个域（4 免费 + 2 收费）状态只读；`web` 标常开；收费域未授权显示
  "未授权 · 了解授权"胶囊。
- **全局设置独立页**：标题栏右侧小圆按钮切换；保存时若实例在跑，`POST /api/settings`
  写进引擎设置（下一次调用生效），否则提示下次启动生效。
- **schema 2 迁移**：旧扁平 `launcher.json` 自动迁移并回写，有单测覆盖。

---

## 5. 存储

`%LOCALAPPDATA%\Atelier\launcher.json`

```json
{
  "schema": 2,
  "shared": {
    "models_dir": "", "huggingface_token_hint": "", "ollama_host": "",
    "ollama_token_hint": "", "close_to_tray": true, "catalog_url": ""
  },
  "installations": {
    "free": { "data_dir": "", "setup_confirmed": true }
  }
}
```

`%LOCALAPPDATA%\Atelier\credential.bin` 是 DPAPI 加密的 HF token，只有当前 Windows 账号能解。
旧版本备份：`launcher.schema1.bak`。

---

## 6. 关键设计决定（别轻易回退）

1. **域开关已删除**。曾经的"启停开关"整套代码（`disabled_domains`、`set_domain`、
   `keep_enabled_domains`、`activity.rs` 探针）已清理。理由：域生命周期归 kernel 管，
   启动器只报状态。要恢复的话，注意 `POST /api/domain-control` 存在且 `web` 被产品级禁止 stop。
2. **售卖单位是域，不是变体**。变体只剩"当前装的是哪个包"这个身份。
3. **付费域要"可见但不可用"**：默认安装没有这两个域，但界面必须列出来并给授权入口，
   否则用户不知道它们存在。授权/付费流程未实现，`catalog::licensed()` 是唯一钩子，今天恒 false。
4. **不并发**：一台机器同一时间只跑一个 kernel，跨安装也要互斥。

---

## 7. 待办

| 优先级 | 事项 | 位置 |
| :-- | :-- | :-- |
| 高 | **启动器自更新**：版本源 `NomadAnget/Atelier` 的 release → 下载 exe → sha256 → `apply-update.ps1` 换文件（保留 `.old` 回滚） | 更新链路其余部分已完成：逐域下载/校验/暂存/原子换入/自动启动/预热/实时速率/环形进度覆盖层 |
| ✅ | **视频管线 S03 崩溃已修**：`Expected str, got VPath` | 真因不是引擎边界，而是 Cython 对 `rep_clip: dict[str, str]` 的注解类型校验；已改为 `dict[str, Any]`（`atelier-pipeline-video` `6895f7f`，随 AtelierFree v0.1.13 发布），详见 §7.4 |
| 中 | 远端可售管线目录（用户已定方案 B） | `settings.shared.catalog_url` 已留字段；需拉取 + 缓存 `catalog.json` + 离线兜底（拉不到就只显示已装） |
| 中 | 授权/付费流程 | `catalog::licensed()` |
| 低 | 离线权重包 `AtelierFree-offline-models-<ver>.zip`（把 `atelier-models` 的资产打进一个可选包，给断网机器） | 组装 workflow 加一个 flag；主包仍不带权重 |
| 低 | `install.ps1` 每次更新都在 `%TEMP%` 留下 `atelierfree-install-*/`（约 295 MB/次，本机清掉了 16 个） | 发行包脚本，或启动器装完后顺手扫一遍 |

### 7.1 v0.1.5 实测记录（2026-11-12）

- **升级路径通**：启动器「⋯ → 更新发行包」走发行版 `install.ps1`，v0.1.3 → v0.1.5 完成，
  `VERSION` 与界面版本号都变成 `0.1.5`。耗时主要花在 `Expand-Archive`（约 6 分钟 / 14k 文件）。
- **缺陷**：带 `ATELIER_ROOT` 启动时，pipelines 域反复崩溃重启（`restart_count` 持续增长，
  assembly 里 `status=faulted`），报错 `ModuleNotFoundError: No module named 'zoneinfo'`。
  干净的 0.1.5 解压目录同样复现，去掉 `ATELIER_ROOT` 后四域全 `active`，所以不是升级残留，
  而是 `atelier-pipelines-host` 的打包缺 `zoneinfo`（engines/scheduler 的 artifact 里都有
  `_zoneinfo.pyd`，pipelines 没有）。触发链：`ATELIER_ROOT` → 域从
  `<root>/runtime-site-packages` 导入 pydantic → pydantic `from zoneinfo import ZoneInfo` 失败。
- **启动器不为此改契约**：`ATELIER_ROOT` 是发行版自己的 `start.ps1` 也会设置的值，启动器不动它；
  修复放在 pipelines artifact（`packaging/build.ps1` 加 `zoneinfo`/`_zoneinfo`/`tzdata`），随 0.1.6 出。
- **插曲**：更新期间如果 launcher 恰好在解压未完成时启动了 kernel，会拿到新旧混合的域二进制，
  表现为 pipelines 崩溃 + 界面卡在「启动中…」。更新完先等 `VERSION` 落定再启动。
- **停止路径**：v0.1.5 的 kernel 仍然没有优雅停机事件，停止照旧走 `taskkill /T /F`，
  界面提示"已强制结束"。域进程全部回收，实测 2 秒内清干净。
- **热修（本机，已作废）**：当时把 `%LOCALAPPDATA%\AtelierFree\atelier-pipelines-host\` 换成重建产物先跑通；
  0.1.6 用发行包重装后这个目录已被正版 artifact 覆盖。

### 7.2 v0.1.6 发布与复验（2026-11-12）

修复链（全程走发行包自己的流水线，启动器一行没改）：

1. `atelier-pipelines` 加 `--include-package=zoneinfo --include-module=_zoneinfo --include-package=tzdata`
   → 提交 `320b19c`，推到 `fix/pipelines-zoneinfo` 并快进 `reel-solo-kernel`。
2. dispatch `artifact.yml` → 12 分 53 秒出 `AtelierPipelines-k0.5.2-s0.5.2-320b19c….zip`（30 MB，651 项，
   含 `payload/atelier-pipelines-host/_zoneinfo.pyd` 与 `tzdata/`）→ 落在 `domain-artifacts` release。
3. dispatch `release.yml`（`NomadAnget/AtelierFree`）→ 提交 `30fd1d0 release: v0.1.6`：`VERSION=0.1.6`、
   `train=0.1.6`、pipelines 指针指到 `320b19c`、子模块指针同步 → 打 tag `v0.1.6` → 自动 dispatch
   `assemble-distribution.yml`（1 分 57 秒）→ Release `AtelierFree v0.1.6`（`AtelierFree-v0.1.6.zip` 282.6 MB）。

复验：启动器「更新发行包」→ 165 秒装完 → `VERSION=0.1.6`、`DISTRIBUTION.txt` 首行 `AtelierFree 0.1.6`、
`atelier-pipelines-host\_zoneinfo.pyd` 存在 → 点启动 **2 秒内四域全 active**（0 重启）→ 自动进入内嵌工作台 →
停止 2 秒内回收干净。

> 注意：`release.yml` 会从最新 tag 自动 +1 版本号，并把各域「最新分支 HEAD」写进指针。发版前先确认
> 五个域的分支 HEAD 与预期一致，否则会把没打算发的域一起带进包。

### 7.3 权重镜像（2026-11-12）

上游把 SHAS 多语言分类头从 Google Drive 撤了（[mt-upc/SHAS#7](https://github.com/mt-upc/SHAS/issues/7)，
README 标注 unavailable），任何新机器第一次跑语段切分都会 404。做法是**建公开镜像仓**
`NomadAnget/atelier-models`，而不把权重塞进发行包：

- Release `model-artifacts` 里放 `shas-multilingual-3bbb8e8787e3.pt`（64.1 MB）+ `.sha256` + `SHA256SUMS.txt`；
  资产名带 sha256 前 12 位、永不覆盖；`models/<id>/provenance.json` 记原始 file id、许可、取用时间；
  `NOTICES.md` + `LICENSES/` 放 MIT（SHAS）与 Apache-2.0（XLS-R）全文与引用。
- `Verify model artifacts` workflow 在发布后自动核对：每个权重资产必须能匿名下载、大小与 sha256
  跟 provenance 一致、sidecar 一致、没有无出处的资产。
- 引擎侧（`atelier-engines`，0.1.8/0.1.9）：`base/weights.py` 的 `MODELS_MIRROR` + `fetch_model()`；
  `speech_segment.py` 的顺序是 本地 → 镜像（sha256）→ 上游（历史兜底）→ **降级能量切分 + WARN**，
  不再让任务因权重缺失失败。
- 红线：gated 权重（pyannote 等）永不进镜像仓，只能用户自己拿 token。

同批修掉的三个 Windows 专属坑（跑真实环境才暴露，编译检查看不见）：

| 问题 | 修法 |
| :-- | :-- |
| `fetch_http_segmented` 用 `os.pwrite`/`os.ftruncate`（POSIX 专属），Windows 上每条分片重试 20 次全败 | `truncate` 预分配 + 每段独立句柄按偏移写 |
| `base/__init__.py` 没导出 `fetch_model` → 引擎加载 `ImportError` | 补导出 |
| 更新到 0.1.8 失败：`install.ps1` 无 BOM，PS 5.1 按 ANSI 读，中文注释被拆成语法错误 | 存成 UTF-8 with BOM（判据：PS 5.1 `Parser::ParseFile` 零错误） |

0.1.7 / 0.1.8 / 0.1.9 的发布都走 `release.yml`（tag + assemble），每次约 2–3 分钟；
engines artifact 每次 12–15 分钟。发布前必须确认五个域分支 HEAD 与预期一致——中途发现
`atelier-engines` 的 `reel-solo-kernel` 分支被删过一次，`release.yml` 直接以
`No commit found for SHA: reel-solo-kernel (HTTP 422)` 报错；把提交重新推回同名分支即可恢复。

已清掉的旧待办（2026-11-12）：`catalog.rs` 的 `Probe` 枚举与 `DomainMeta.probe`、死函数
`is_paid`/`pipeline_dir`、死 CSS `.cap-toggle/.cap-switch`、死组件 `SwitchCtl`、
`variant-catalog.mjs` 与 `variant-store.mjs`（连同它的测试）、README 里过时的变体页描述。
编译零警告，前端测试换成了守着引导页顺序的 `tests/setup-steps.test.mjs`。

### 7.4 S03 `Expected str, got VPath` 的真因（2026-11-12，已修）

**现象**：视频任务跑到 S03 语言识别（进度 42.25%）失败，scheduler 记 `err=Expected str, got VPath`，S03 最后一条日志是 `S03 送识别:单独 N 个,长段单元 M 个`。

**先前的错误判断**（已作废）：以为 `ctx.vdir()` 返回的 VPath 被直接当 `input_data` 传给了
`engines.LangIdEngine`，建议在 `ctx.call_engine` 序列化边界统一转原生路径。该判断不成立——
错误发生在 `call_engine` **实参求值阶段**，C4 编码、引擎域、消息总线都还没参与。

**真因**：Cython 3 的 `annotation_typing` 会对**注解为内建标量容器的下标读取**做类型校验。

```python
rep_clip: dict[str, str] = {}       # s03_asr.py:138
rep_clip[tid] = lid                 # 实际存的是 VPath（ctx.vdir() / asd_dir 组合）
... 'input_data': rep_clip[tid]     # 读这一下就抛 TypeError
```

最小复现（Cython 3.3.1）：

| 写法 | 结果 |
| :-- | :-- |
| `d: dict[str, str]` 存入非 str，赋值 | 不报 |
| 同上，`d["x"]` **读取** | `TypeError: Expected str, got <类型>` |
| 同上，`.items()` / `.values()` | 不校验 |
| `d: dict[str, Path]` 存入非 Path，读取 | 不校验（只校验内建标量） |

**修法**：`rep_clip: dict[str, Any] = {}`（一行），提交 `atelier-pipeline-video@6895f7f`，
video artifact 重建后随 AtelierFree **v0.1.13** 发布。修完在本地用同一编译插件 + 假 StepContext
验证：越过了 166 行，`LangIdEngine` 收到的 `input_data` 是 VPath，后续由 C4 正常编码。

**为什么不选当时列的两个方案**：

- 方案 A（在 `ctx.call_engine` 里把所有 VPath 转原生路径）：修不到这个 bug，而且会破坏
  `output` / `outputs` / `output_paths` 依赖 C4 引用建对象、回传 `{owner, virtual_path, physical_path}` 的语义。
- 方案 B（S03 里 5 处 `str(fs.native(...))`）：能绕开，但只是掩盖，且丢掉 VPath 的 C4 语义。

**同类隐患**：凡是注解成 `dict[str, str]`（或其它内建标量）却存放 VPath / Path 的变量，都会在下标读取时
抛这个错；S02 的 `dict[str, Path]` 不受影响（Path 不是内建标量）。

### 7.5 S04 术语层 `target_invalid` 与缺失的失败原因（2026-11-12，已修）

**现象**：S03 修好后，同一任务在 **55.0%（`progress_message=S04 术语层`）** 再次失败，
scheduler 记 `err=target_invalid`；而管线日志只有一句「管线失败」，没有任何原因。

**怎么定位**：查 `F:\Atelier\data\atelier_kernel.db` 的 `audit` 表——最后一条是
`pipelines -> engines.term_capabilities`（completed），**之后再没有任何消息进入审计**。
`target_invalid` 是内核「零盲发前置拦截」的返回：目标 handler 不存在 / 目标域未 Active 时根本不会进管道，
自然也不会落 audit。这条「最后一条审计 + 下一条缺失」就是消息被前置拦截的特征。

**真因**：`KnowledgeStore.channel_domain()` 查 `daemons.channels`，而**免费发行版没有 daemons 域**
（装配只有 web / scheduler / pipelines / engines）。S04 用 `source_meta.channel_id`（YouTube 频道
`UCQ-…`）必定走到这里；而 runner 启动时手动任务的 `job.channel_id` 为空，所以这个坑之前一直没被触发。

**修法**：

- `knowledge_store.py`：`channel_domain` 只捕获 `status == 'target_invalid'`，降级为「无领域」（通用池），
  记一次日志并记住，后续不再发这条消息；其它错误照旧抛出（不吞真故障）。
- `context.py` / `execution.py`：`finish(state, error='')` 把失败原因拼进「管线失败」那一行，
  失败时改用 ERROR 级别。现在 UI 上直接能看到 `管线失败: target_invalid`，不用再翻 DB。

**提交**：video `15ae823`、pipelines `67343b6`（顺带把 pipelines 里指向 video 的 gitlink 从 `993368b` 更到新提交），
随 **v0.1.14** 发布。

### 7.6 IndexTTS 代码没进发行包（2026-11-12，已修）

**现象**：v0.1.14 上任务跑到 **63%（S05 TTS）** 失败：

```text
engines.IndexTTSEngine 执行失败: map_concurrent 子进程失败:
RuntimeError('Model load failed: IndexTTS 子模块未初始化，请执行:
  git submodule update --init src/engines/media/indextts')
```

**先排除的**：不是权重目录问题。`G:\Atelier\atelier-solo\checkpoints` 里 `indextts-2.5` 在，
同一次任务里 Whisper / LangId / SHAS 都从同一个目录加载成功。缺的是**代码**：
安装目录 `engine-root/engine-code/domains/engines/media/` 只有 `indextts_tts.pyc`，没有 `indextts/`。

**为什么缺**（三处排除 + CI 不拉 submodule）：

| 位置 | 原来的行为 |
| :-- | :-- |
| `.github/workflows/artifact.yml` 的 checkout | 没有 `submodules: recursive`，CI 里子模块是空目录 |
| `.github/workflows/artifact.yml` 运行时代码 staging | 显式跳过 `media/indextts` |
| `packaging/prepare_engine_runtime.py` 的跳过集合 | 含 `indextts`，pyc 与资源两层都不碰它 |
| `packaging/build.ps1`（宿主 Nuitka） | 同样跳过——这条**保留**，宿主进程不 import 它 |

**修法**：

- CI checkout 加 `submodules: recursive`（子模块是公开仓 `index-tts/index-tts`）；
- staging 不再对 `media` 做特殊排除；
- `prepare_engine_runtime.py` 在 pyc / 资源之后，把 `media/indextts` **原样**拷进
  `engine-code/domains/engines/media/indextts`（源码，不参与 pyc 编译）。
  `indextts_tts.py::_REPO_DIR` 正是按 `__file__` 旁边的 `indextts` 目录判定的，路径对上即可加载。

产物 +38 MB（`indextts/` 25.6 MB + 其余资源）。

**许可**：这份 vendored 代码是 **bilibili Model Use License**（允许再分发，要求保留版权与许可全文、
把条款传递给下游、对改动做声明）。所以 `media/indextts/LICENSE` 随树原样进包；以后若改这份代码，
发布页要写"改动未经原权利人背书"的声明。

**验证**：本地跑 `prepare_engine_runtime.py` 得到 `vendored=335`，且
`engine-code/domains/engines/media/indextts/indextts/infer_v2_5.py` 存在；发版后再从发行包核对同一路径。

### 7.7 S05 `tts_path` 参数类型（2026-11-12，已修）

**现象**：v0.1.15 上任务跑到 63%（S05），在「语速修正」之后失败：

```text
管线失败: Argument 'tts_path' has incorrect type (expected str, got WindowsPath)
```

**真因**：`s05_tts.py:168` 把 `ctx.native(res["path"])`（永远是 `pathlib.WindowsPath`）传给
`TranslationPair.set_dub(tts_path: str, …)`；Cython 对**参数注解**做硬校验，直接抛 `TypeError`。
和 §7.4 是同一类问题（Cython annotation typing），只是这次卡在函数参数而不是 `dict` 下标。

**修法**：`TranslationPair.set_dub` 与 `AudioUnit.set_dub` 的参数注解改成 `Any`，内部
`str(tts_path)` 入库（字段本来就是 `Optional[str]`）。提交 `atelier-pipeline-video@df1cf24`，随 **v0.1.16** 发布。

**经验**：编译后的 video 包对 `str` 注解参数是硬校验。以后凡是给带 `str` 注解的函数传
`ctx.native()` 的返回值（Path）或 VPath，要么调用点显式 `str(...)`，要么把注解放宽；
排查时看错误文案就能分辨：`Expected str, got …` 是容器下标，`Argument 'x' has incorrect type` 是参数。

### 7.8 从根上关掉 Cython 注解运行时校验（2026-11-12）

§7.4（dict 下标）和 §7.7（函数参数）是同一类问题的两种形态。只补单个调用点，下一个新函数还会再炸——
所以这次改在构建层：

- `packaging/build.ps1` 生成的 setup 与 `packaging/setup.py` 都加上
  `compiler_directives={'language_level': '3', 'annotation_typing': False}`。
  这些模块是纯 Python，注解本来就只是文档；关掉后 Cython 不再把 `str` / `dict[str, str]` 编译成
  运行时类型断言，这类 `TypeError` 从机制上消失。
- 回归测试：`packaging/check_plugin.py`，在 CI 构建完成后对**编译产物**跑两条冒烟：
  1. `TranslationPair.set_dub` / `AudioUnit.set_dub` 收 `Path` → 存成 `str`；
  2. `AudioUnit.add_pair(src_text=Path(...))` → 带 `str` 注解的参数收 Path 不报错
     （这条专门证明开关仍生效；谁把它加回去，构建立刻失败）。
  工作流 `.github/workflows/artifact.yml` 已把该检查放在 Build 之后，失败即不出包。
- 代价：失去注解带来的那点 Cython 优化（这些模块的热点是 numpy，可忽略）；类型正确性交回业务代码
  （`set_dub` 内部仍 `str()` 归一）。

提交 `atelier-pipeline-video@cf0e45c`，随 **v0.1.17** 发布。

### 7.9 插件里 `open('vfs://…')` 失效（v0.1.17 暴露，已修）

**现象**：整条管线跑到 S13 完成后失败：

```text
管线失败: [Errno 22] Invalid argument: 'vfs://jobs/<job_id>/summary/pipeline_summary.json'
```

重试时因为检查点全部命中，2 秒就挂在同一行。

**真因**：Cython 在**模块导入时**把 `builtins.open` 缓存成模块级变量；而 `vfs://` 拦截是
`PipelineDomain.boot()` 里才 patch `builtins.open` 的，**晚于插件导入**。于是编译后的
`runner.py` 里的 `open('vfs://…')` 永远拿到未 patch 的内置 open，Windows 把 `vfs://` 当普通文件名
→ `EINVAL`。

- 实证：写一个 5 行 Cython 模块，`import` 之后再 patch `builtins.open`，模块里的 `open()` 仍走原函数并抛同样的错。
- 为什么之前没暴露：管线从来没跑到 S13 之后写 summary 这一步。

**修法（两层）**：

1. `runner.py` 改用 `VPath.write_text`——host 层 `vfs.py` 用的是 `builtins.open` 运行时查找，
   永远命中拦截器；`check_plugin.py` 增加源码扫描：插件里禁止 `open('vfs://…')`，CI 直接失败。
2. `PipelineDomain.__init__` 在 `discover()` **之前**调用 `artifacts.enable(sdk)`，让随后导入的编译模块
   缓存到已 patch 的 `open`；`boot()` 里的原调用保留（幂等）。

**提交**：video `c0d9a23`、pipelines `8f2adf9`，随 **v0.1.18** 发布。

**教训**：Cython 插件里不要直接调 `open()` 写 vfs 路径；一律走 `ctx.vfs_ops()` / `VPath` API。

### 7.10 多比例封面丢失 + 摘要文件位置（v0.1.18 暴露，已修）

**现象**：任务结果面板只剩 16:9 封面；`pipeline_summary.json` 被当成任务产物对象登记。

**封面真因**：S11 用 `ctx.exists(p)` 判断引擎导出的封面。`p` 是 image_tool 导出的**原生路径字符串**，
`ctx.exists` 会把它当成 `(owner=pipelines, virtual_path=原生路径)` 去查内核对象 → 永远 `False`，
4:3/1:1/3:4 全被丢掉；16:9 因为是无条件赋值才留下来。
修法：直接按导出文件是否存在判断（`ctx.native(p)` + `Path(...).is_file()`），不走对象查询。

**摘要真因**：C4 迁移时把 `pipeline_summary.json` 写成了 C4 对象并塞进 `objects`，于是它出现在任务输出里；
但 web 与 scheduler 都按原生文件 `data/pipelines/jobs/<job_id>/pipeline_summary.json` 读它
（`web/server.mjs` 的 `jobDir` 回退、`scheduler/impl/publish_service.py` 的 `_jobs_dir`）。
修法：写回任务数据目录（`artifacts.job_dir(job_id)/pipeline_summary.json`），不再登记对象、
不再出现在 `objects` 或 runner 返回值里。web 的 `summaryObject` 找不到 `output.summary` 时会自动
回退读这个文件，面板与发布链都不受影响。

**提交**：video `28e645f`，随 **v0.1.19** 发布。

**注意**：这条任务的 S11 已经写进检查点，命中检查点的重跑会沿用旧结果；要看到多比例封面需要
清检查点重跑（或新建任务）。

---

## 8. 环境与操作

**构建**：`npm run tauri build -- --no-bundle` → `src-tauri/target/release/atelier.exe`
**测试**：`cargo test --release`（在 `src-tauri`）、`npm test`
**开发**：`npm run tauri dev`

**本机现状**：安装在 **`F:\Atelier`**（v0.1.17，2026-11-12 用启动器装；四域 `active`）。本地仓库：
`G:\Atelier\atelier-free` 在 `main@6a17783`（v0.1.19），`domains/pipelines` 在 `8f2adf9`、
`domains/engines` 在 `b71ece3`、`definitions/video` 在 `28e645f`（均已推送；pipelines 里的 video gitlink
仍停在 `15ae823`，不影响发布——video 走独立 artifact）。
`G:\Atelier\atelier-solo\runtime` 还有一份开发态 kernel（不是安装，但会被跨安装互斥检测到——这是用户有意保留的测试对象）。

**⚠ 别用 PowerShell 改源码文件**：`Get-Content`/`Set-Content` 在这个环境会按 GBK 读写，
把中文写成乱码（本项目已经中招三次：8 张 SVG、`main.jsx` 全文件）。改文本一律用编辑工具，
批量改 JSON 用 `python -c` 显式 `encoding='utf-8'`。

**⚠ 运行中的实例会覆盖手工改的 `launcher.json`**：设置常驻内存，任何一次保存都会整份写回。
要手改状态，先关启动器。

---

## 9. 验收清单（改完自检）

1. `cargo test --release` 17 项全过；`npm test` 2 项全过
2. 冷启动出 logo 页 → 进能力面板（不是引导，除非删了 `launcher.json`）
3. 点"启动"：分配端口 → 四域 active → 内嵌工作台可打开
4. 点"停止"：kernel 与域全部回收（v0.1.6 仍提示"已强制结束"——Kernel 还没有优雅停机事件）
5. 关窗 → 进程仍在托盘；再双击 exe → 唤醒原窗口而不是开第二个
6. ⋯ 菜单 →"重新运行初始设置"→ 三页顺序为 权重 → Token → 数据
7. 全局设置保存：实例在跑应提示"已同步…下一次引擎调用生效"
