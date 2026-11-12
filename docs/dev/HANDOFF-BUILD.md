# 交接：启动器 / 安装器 / 更新链路

> 压缩上下文用。上一份详细文档见 `docs/dev/HANDOFF-LAUNCHER.md`（契约与域能力面板时代的部分仍有效），
> 本文只写**现在正在做的事**和下一步的手感。

## 1. 一句话现状

启动器已能**自建安装**（不再调 `install.ps1`）：读 `release/artifacts.json` → 逐域下载/校验/暂存/组装 →
原子换入 → 启动实例 → `engines.prewarm` 预热环境 → 直接进工作台；全过程有环形进度覆盖层
（百分比/实时速率/单行日志，预热段带小恐龙）。启动器自身也支持自更新
（`NomadAnget/Atelier` 的 release，已发 v0.1.0）。构建与测试全绿：`cargo test` 19 项、`npm test` 2 项。

**2026-11-12 修掉的"装了也不通"**（两个独立故障叠在一起，症状都是「全部未安装」）：

1. 组装缺了发行包的关键步骤：内核 zip 的 `kernel-runtime/` 没摊平、`kernel_boot.json` 从来没写过、
   `engine-root` 被当成"用户目录"在解压时删掉了、SDK wheel 被当压缩包解开成 `atelier_sdk/`。
   于是根目录里没有 `atelier-kernel.exe`、没有 `kernel_boot.json` → `installed=false`、
   点开实例报「无法读取变体配置：系统找不到指定的文件 (os error 2)」。
2. 引导第 3 页把**解析后的 `<root>/data`** 回填进"安装与数据目录"这一栏，用户直接点下一步就会把安装根
   变成 `<root>/data`，data 再套一层 → `…\AtelierFree\data\data`。默认值必须是根目录本身。

这两条现在都有测试兜着（见 §3 末）。

**2026-11-12 修掉的"实时速度消失"**：预热阶段速率和日志是两条独立事件发的，日志那条不带 rate，
每来一条日志就把界面上的 `x.x MB/s` 抹掉。现在合成一条事件（§3.2），并且 `⋯ → 准备运行环境`
可以随时重开这个覆盖层（构建幂等，已建好就秒退）。

## 2. 文件地图（只看这几个）

```text
src-tauri/src/
  installer.rs    ★ 安装/更新/预热全在这里：run_with(app, start) / install_only / prewarm_only
                    download(→gh 拉包+轮询文件大小算速率) / verify(certutil sha256) /
                    extract(tar→Expand-Archive 兜底) / swap_tree(逐项 rename + journal 回滚)
                    prewarm_domains(探测 <domain>.prewarm 并调用) / poll_prewarm(轮询 /api/logs)
  migrate.rs      ★ 刚写、已挂 mod，未接命令：measure / move_root(同盘 rename、跨盘 staging+校验)
  self_update.rs  ★ 启动器自更新：check / download / write_apply_script / spawn_apply
  settings.rs     ★ schema 3：shared.install_dir / models_dir / close_to_tray / catalog_url
  variant_runtime.rs  install_dir() = settings.install_dir → ATELIER_HOME → %LOCALAPPDATA%\AtelierFree
  setup.rs        引导三页的落点；第 3 页(数据目录)同时是安装根，data 落在 <root>/data
src/
  main.jsx        域能力页 + 覆盖层 UpdateOverlay + 首装状态机 afterSetup()
  DinoGame.jsx    等待预热时的小恐龙
  views/Setup.jsx 三步引导(安装与数据目录 / 模型权重 / 令牌)
docs/screenshots/ 4 张 README 用的截图 + social-preview.png
```

## 3. 关键契约（别猜）

- web 域处理器调用（`domains/web/server.mjs:217`）：
  `POST /api/handler-call {target:"engines.prewarm", message_type:"Durable", params:{}, data:{}}`
  → 拿 `message_id` → `POST /api/handler-result {message_id, timeout:<秒>}` → 回执里 `data.started` 是**要等几个 ready**
- 预热日志在 `GET /api/logs?limit=N` 的 `logs[].message`，前缀 `[PREWARM]`；轮询要**先取当前最大 id 作基线**，
  否则历史 ready 会被误算
- 安装根 = 用户在引导第 3 页选的目录；`<root>/data` 是它的子目录，不需要目录联接
- `install.ps1` 仍在仓库/release，只服务"没有启动器的人"，启动器不再调用它

### 3.1 目录契约：启动器必须组装出的树

**组件来源（2026-11-12 改造）**：域 artifact 不再从私有域仓库取，Free 的装配流程把
5 个 zip + `.sha256` + 改写后的 `artifacts.json` + `kernel_boot.free.json` **镜像进 Free 自己的
release**；启动器全程匿名 HTTPS，**不依赖 `gh`**：

```text
清单   https://github.com/NomadAnget/AtelierFree/releases/latest/download/artifacts.json
组件   https://github.com/{repo}/releases/download/{release}/{asset}      # repo/release 取自清单
旁挂   {组件地址}.sha256
内核   https://github.com/NomadAnget/Atelier-Kernel/releases/download/v{kernel}/{asset}
```

清单里的 `repo`/`release` 已改写成 Free 自己，`source_sha` 保留真实域仓库提交；顶层
`release` 字段是镜像 tag（启动器拿它当 VERSION）。发布脚本写 JSON 时**不能带 BOM**。
预览校验：`cargo test --release -- --ignored live_manifest`（真实联网，21 秒）。

来源是 `AtelierFree/.github/workflows/assemble-distribution.yml`（发行包怎么组出来的），别自己发明：

| artifact | zip 里的形状 | 装到根目录后 |
| :--- | :--- | :--- |
| `AtelierWeb-*.zip` | `payload/` | `atelier-web.exe`、`dist/` |
| `AtelierScheduler-*.zip` | `payload/` | `atelier-scheduler/` |
| `AtelierPipelines-*.zip` | `payload/` | `atelier-pipelines-host/` |
| `AtelierEngines-*.zip` | `payload/` | `atelier-engines-host/`、`engine-root/`（含 `engine-root/sdk/` 的 wheel）、`runtime-site-packages/` |
| `AtelierVideoPlugin-*.zip` | `payload/` | `pipeline-plugins/video_translate/` |
| `Atelier-Kernel-runtime-*.zip` | `kernel-runtime/`（**没有 payload/**） | **只取 `atelier-kernel.exe` 放到根**，docs/README/template 不装 |
| `atelier_sdk-*.whl` | 单文件 | `sdk/<whl>`，绝不展开 |
| 仓库 `kernel_boot.free.json`（同一 tag） | — | 写成根下 `kernel_boot.json` |

组装顺序：全部下载 → 全部解压进 `.staging/<train>/tree` → 写 `kernel_boot.json` → 一次 `swap_tree` 换入。
`swap_tree` 只跳 `data/`、`models/`；`engine-root/` 逐子项合并并跳过 `.venvs`（已建环境）、`data`。
`VERSION` 和 `.staging` 清理放在**换入后、启动前**——预热可能几十分钟，中途被杀也必须留下完整目录。
`retire_legacy()` 清掉旧版启动器留下的 `kernel-runtime/`、`atelier_sdk/`、`atelier_sdk-*.dist-info/`。

判定"已安装"= `atelier-kernel.exe` + `kernel_boot.json` 都在（`start.ps1` 是给无启动器用户的，
启动器不装它；`variant_runtime::variant_info` 与 `capabilities::gather` 两处都按这个判据）。

**回归测试**（`installer.rs` 的 `tests`，跑真实的下载缓存，缓存不在就自动跳过）：
`assembling_cached_artifacts_matches_the_distribution_layout` 断言根上有内核 exe、没有 `kernel-runtime/`、
没有内核自带 docs、`engine-root/` 在、`pipeline-plugins/video_translate` 在、`sdk/` 里恰好一个 wheel；
`swap_keeps_user_directories_and_built_environments` 断言 `data/`、已建 `.venvs` 不被覆盖且能回滚。

### 3.2 覆盖层的进度来源（别再造第二条事件）

- 安装阶段：`download()` 按文件大小算速率与百分比，`extract()` 发不确定态。
- 预热阶段：`poll_prewarm()` **每 2 秒只发一条**事件，里面同时带 `rate`（uv 缓存 8 秒窗口的增量）、
  `detail`（读环境账本的就绪数）、`log`（最新一条 `[PREWARM]`）。以前速率和日志分两条发，
  日志那条不带 rate，界面上的实时速度就被下一条事件抹掉——看起来像"速度被删了"。
- 就绪数不数日志，读 `engine-root/.venvs/engine-environments.json`（engines 域建好一个环境写一个），
  分母是 `engine-root/domains/engines/environments/*/windows-x86_64` 的个数。
- **uv 缓存体积在独立线程里采样**：`.uv-cache` 几万个文件，机械盘上递归统计一次要几十秒。以前在
  轮询线程里同步量，第一帧就被卡住，界面停在「预热已受理」上好几分钟不动（实测 trace：进 poll 后
  21 秒没有任何事件）。现在轮询线程只读采样线程的最近结果，每 2 秒必发一帧。
- 排障开关：`ATELIER_TRACE=1` 写 `%LOCALAPPDATA%\Atelier\trace.log`；`ATELIER_TRACE_PREWARM=1`
  在启动 3 秒后自动跑一遍预热（不用点菜单就能复现覆盖层问题）。

### 3.4 已知问题：中断的构建会让环境永远卡在 n/8

engines 域用 `.engine-builds/<env>-<platform>-<hash>-<ts>/.atelier-building` 标记"正在建"。
构建被强杀（任务管理器、`taskkill /F`、崩溃）后标记会留下，而**同一个 kernel 进程里该环境不再重试**
（回执直接 `started=0`），于是界面永远显示 7/8。

启动器这边能做的（目前手动）：删掉那个半成品目录 + 重启 kernel。要做成一键，需要 engines 域改判断
（标记里写 pid/时间戳，pid 不在了就重建）。在那之前，强杀启动器会留下这个坑，正常退出不会
（`RunEvent::ExitRequested` 会走 `stop_sync`）。
- 结束条件：`ready >= total`，或者连续 120 秒没有新的 `[PREWARM]` 行（某环境构建失败时不无限挂着）。
- 回执 `started=0` 有两种意思：全都就绪，或者构建已经在跑（预热幂等）。安装流程按 0 收工，
  `⋯ → 准备运行环境` 走 `wait_for_unknown=true` 继续轮询；但日志会先用环境账本说清是哪种。
- 速率的基线必须**先量一次**再用差值，否则第一个窗口把整个 uv 缓存当成刚下载的（实测屏幕出现过 `1414 MB/s`）。
- 小恐龙在卡片**外面**（`.dino-dock` 固定在窗口底部，`pointer-events:none`，只 canvas 收点击），
  不画自己的底色/圆角；窗口 `minHeight` = 900，所以它永远落在可见区内。
- 下载会**退避重试 4 次**（2s/4s/8s，`fetch_once` + `DOWNLOAD_ATTEMPTS`）：GitHub 资源 CDN 偶发
  `EOF`/connection reset，一次失败就中断整个安装太脆。旁挂 sha256 同样重试 3 次，并且区分
  「发布方没发」和「网络没拿到」。错误只留 `reason_of()` 抽出的最后一句原因，
  绝不把整条带签名的 URL 甩到界面上。

### 3.3 退出语义（`main.rs`）

| 动作 | 实例 |
| :-- | :-- |
| 点窗口 X（`close_to_tray=true`，默认） | **不停**：窗口隐藏到托盘，后台构建/工作台继续 |
| 托盘 → 退出 Atelier | **停**：`app.exit(0)` → `RunEvent::ExitRequested` → `stop_sync()` 一次 |
| 窗口关闭且 `close_to_tray=false` / 系统关机 | **停**：同一条 `ExitRequested` 路径 |
| 任务管理器结束进程 / 崩溃 / `taskkill /F` | 不停（没有任何清理机会）；下次启动会接上那个实例，卡片显示「运行中」 |

`stop_sync()` 先发命名事件 `Local\AtelierKernelShutdown-<pid>` 让内核自己退（等 30 秒），
老内核没有该事件就 `taskkill /T /F` 整棵树。幂等标志防止重复停。

**`settings::load()` 会剥 UTF-8 BOM**：PowerShell 的 `Set-Content`/`Out-File` 默认加 BOM，
以前会让整个 JSON 解析失败 → 所有设置静默回默认、直接掉进首屏向导（实测踩过）。
现在解析不了会把坏文件备份成 `launcher.corrupt.json` 再回默认，并有回归测试。

## 4. 下一步（按顺序，一批做完再测）

1. **`migrate_install(from, to)`**：停实例 → 空间预检 → `migrate::move_root` → 写 `settings.install_dir`
   → 用新路径启动并等四域 active → 失败反向搬回 + 还原设置；`⋯` 菜单加入口，复用覆盖层显示 done/total + 速率
2. 报错文案：目录缺失时显示「未检测到已安装的版本」而不是 `os error 2`
3. 已发布：`NomadAnget/Atelier` v0.1.0（`atelier.exe` + `SHA256SUMS.txt`），自更新可端到端跑；本轮修复未发版
4. 启动器装出的树与 `install.ps1` 装出的整包相比，少了 `start.ps1 / install.ps1 / README.md / assets/ /
   DISTRIBUTION.txt`。域目录本身一致；要不要补这几份文件由"启动器用户是否需要脱离启动器启动"决定

## 5. 环境与操作

```powershell
# 构建/测试
cd G:\Atelier\Atelier; npm run build; npm run tauri build -- --no-bundle
cd src-tauri; cargo test --release        # 需要先关掉运行中的 atelier.exe，否则 exe 被占用
cd .; npm test
```

**⚠ 改文件只用 `edit` / `write` 工具**。用 PowerShell 字符串拼接改源码已经连续失败三次：
`\"` 在 PowerShell 里不是转义、中文按 GBK 往返会乱码、`pwsh` 命令在本机不存在（只有 `powershell.exe` 5.1，
读无 BOM 的 UTF-8 文件会按 ANSI 解析）。真要用脚本，必须写成文件 + UTF-8 **with BOM** 再 `powershell -File`。

**⚠ 构建前必须退出启动器**（托盘 → 退出），否则 `target\release\atelier.exe` 被占用，报的是 link 失败。

## 6. 本机状态（2026-11-12 4:30）

- 安装根：`F:\tmpdata`，v0.1.12，**已装好并在运行**（四域 active，工作台嵌在启动器窗口里）
- 根目录内容：`atelier-kernel.exe`、`atelier-web.exe`、`kernel_boot.json`、`kernel_boot.atelier.json`（每次启动重写端口）、
  `VERSION`、`dist/`、`atelier-{scheduler,pipelines-host,engines-host}/`、`engine-root/`、`runtime-site-packages/`、
  `sdk/`、`pipeline-plugins/`、`data/`、`models/`
- 环境：**还没建**（测试时预热跑到一半被我停掉，半成品目录已删）。首次真正用到某个引擎时按需构建，
  `engine-root/.venvs/.uv-cache` 里已有约 790 MB 缓存可复用
- 下载缓存：`%LOCALAPPDATA%\Atelier\download-cache\0.1.12`（命中即跳过下载，重装只需解压）
- `%LOCALAPPDATA%\AtelierFree` 是旧错误布局，已整个删掉；`launcher.json` 里 `install_dir=F:\tmpdata`、
  `models_dir` 空（默认 `F:\tmpdata\models`）、`setup_confirmed=true`
- 换安装位置：`⋯ → 重新打开引导`，第 3 页选新根目录（会重新下载/组装；缓存命中，几分钟）
- 复现"首装"：删 `launcher.json` 或把 `setup_confirmed` 置 false
