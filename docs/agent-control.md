# TakeKit 的 AI Agent 操作面（v1 已实现）

> 状态：**v1 已落地**（2026-10-21，决策 D1–D6 全部按建议采纳）。v2/v3 见 §8。
> 本文是控制面的契约：HTTP 端点、MCP 工具、CLI 子命令三方对齐，由
> `docs/tools/check-contracts.py` 第 4 项核对，改名前先看它。

## 1. 目标与非目标

**目标**：让任意 AI Agent（包括 WorkBuddy 里的我）能用自然语言驱动 TakeKit 的全部日常操作——
「把微信和飞书的窗口加进 Source」「改一下输出目录」「开始录」「停了，告诉我文件在哪」。

**非目标**（明确不做）：

- 不做网络暴露：控制面**只监听 127.0.0.1**，永不绑定其他网卡。
- 不做跨机器 / 远程控制。
- 不取代 GUI：面板仍是人的主界面，Agent 是第二个驾驶员，不是替代品。
- 不做「Agent 静默改状态」：Agent 的每个写操作必须实时反映到面板 UI（见 §6.3）。

## 2. 现状：能力全都有，缺的只是门

面板已有 40 个 Tauri 命令（`main.rs` 的 `invoke_handler`），恰好覆盖 Agent 需要的全部动作：

| 领域 | 现有命令 |
|---|---|
| 枚举 | `list_windows` / `list_mics` / `list_devices` / `env_info` / `source_preview` / `source_titles` |
| Source 管理 | `save_session`（整表写入）/ `check_sources` / `enter_crop_mode` / `exit_crop_mode` / `restore_window` |
| 录制 | `start_recording`（`StartRequest`）/ `stop_recording`（返回 `StopResult`，含落盘目录）/ `stop_source` / `overlay_finish` / `overlay_cancel` / `preheat_engine` |
| 状态 | `status`（`StatusInfo`：recording/elapsed/逐源 alive/error）/ `config_get` / `journal_get` |
| 设置 | `set_hotkey` / `set_countdown` / `set_finalize_panel` / `set_ffmpeg_path` / `set_control`（控制面开关）/ `set_language` / `set_theme` / `set_active_tab` / `pick_folder` / `pick_file` |
| 历史 | `journal_seen` / `journal_clear` / `log_delete`（单条删日志）/ `log_push` / `take_delete` / `open_take` |
| 面板自身 | `show_main` / `hide_tray` / `ui_ping` / `ui_settings` |

CLI（`takekit`）是**一次性录制器**（`takekit list` / `takekit record …`），录完就退，
**不能指挥正在运行的面板**。

Tauri 命令只能被面板自己的 WebView 用 `invoke()` 调用——这就是那道缺的门。

## 3. 方案选型

| 候选 | 做法 | 判断 |
|---|---|---|
| A. 只扩展 CLI | 让 CLI 通过某 IPC 指挥面板 | IPC 还是得选一种，等于把问题推给下一层；且 Agent 每次要拼 shell 参数 |
| B. 文件队列 | Agent 写 JSON 到监听目录，面板轮询 | 轮询延迟、竞态、无回执、无状态查询。否决 |
| C. 深链 | `takekit://add-source?...` 唤醒面板 | 适合「一键触发」，不适合富 API；URL 长度限制；任何本地程序都能触发（无鉴权）。留作 v3 的触发器idea |
| **D. HTTP 控制服务 + CLI 客户端 + MCP 面** | 面板内置 loopback HTTP 服务（复用现有命令实现），配一个 CLI 客户端和一个 MCP stdio 面 | **推荐**。三层各司其职：服务是唯一真相、CLI 是零安装通道、MCP 是 Agent 原生通道 |

选 D 的理由：

1. **零逻辑重复**：HTTP handler 直接调用面板已经在用的同一批函数（`take::begin` / `config::Config::update` / `journal` …），不存在第二套状态机。
2. **HTTP 是通用可调试面**：浏览器、curl、任何语言都能先手动验证，再上 MCP。
3. **MCP 是 Agent 原生面**：WorkBuddy / Claude Desktop 等装上即用，工具带 schema，Agent 不需要猜参数。

## 4. 威胁模型与安全边界

诚实写清楚这个控制面的信任假设：

- **绑定 127.0.0.1 + 随机端口**：挡掉远程与跨网卡访问。
- **每启动一次的 Bearer token**（32 字节随机）：挡掉浏览器网页 / 陌生程序对本地端口的扫描式误触（它们拿不到 token）。
- **token 落在 `%APPDATA%\TakeKit\control.json`**（与 `config.json` 同目录、同 ACL，仅当前用户可读）：同用户的程序理论上能读到它——**威胁模型按「同用户即信任」设计**，和读写用户自己的文件同级；不假装能防同用户恶意程序。
- **设置页一个开关**（默认开）：关掉则服务不启动、control.json 不生成；日志留一行。
- **破坏性操作要显式确认**：`DELETE /history/{id}`、`DELETE /log`、`DELETE /log/{at}` 必须带 `"confirm": true`，Agent 读漏字段就做不成破坏。
- **录制中冻结设置写入**（与面板现有规则一致，返回 409）：录制相关的字段（sources/encoder/backend/marks/raise_sources/out_root）在 recording 期间只读，`start/stop` 永远可用。

## 5. 架构与数据流

```
                 ┌──────────────────────── 面板进程（takekit-gui.exe）────────────────────────┐
                 │                                                                          │
  Agent ──MCP──▶ │  takekit mcp（stdio JSON-RPC）  ──┐                                      │
  Agent ─ shell ▶│  takekit ctl …（CLI 客户端）     ──┼─▶ http://127.0.0.1:<port>/…          │
  人/调试 ─ curl▶│  （直接 HTTP）                   ──┘   ┌──────────────┐    ┌───────────┐ │
                 │                                        │ control.rs   │───▶│ 现有命令层 │ │
                 │  WebView UI ◀── ctl:changed 事件 ◀────│ 路由+鉴权+守卫 │    │ take/config│ │
                 │  （Agent 的写操作实时镜像到面板）        └──────────────┘    │ /journal  │ │
                 │                                                          └───────────┘ │
                 └──────────────────────────────────────────────────────────────────────────┘
```

- 服务在 `setup()` 里起一个线程跑 **tiny_http**（选它不选 axum：路由调用的就是
  面板那批阻塞函数、一次一个请求，引整个异步栈比功能本身还重），绑定 `127.0.0.1:0`
  （系统分配端口）。
- 启动即写 `%APPDATA%\TakeKit\control.json`：`{ "port": …, "token": …, "pid": …, "version": … }`；退出时删除。
- 客户端（CLI/MCP）先读这个文件：**文件不在 = 面板没开（或控制开关关着）**；文件在但 pid 已死 = 崩溃留下的过期文件，同样报「面板未运行」（v1 起就查 pid，见 `takekit_core::proc::alive`）。都不重试乱连。

### 5.1 端点表（v1）

鉴权：所有请求带 `Authorization: Bearer <token>`。错误统一 `{ "code": "...", "message": "..." }`（HTTP 状态码语义化：400/401/404/409/500）。

| 方法 & 路径 | 复用 | 请求体 | 返回 |
|---|---|---|---|
| `GET /state` | `status` + `config_get` + `journal_get` | — | 全量快照（§5.2） |
| `GET /devices?kind=window\|desktop\|camera\|mic\|system-audio\|window-audio` | `list_windows` / `list_devices` / `list_mics` | — | 可加入的候选列表 |
| `POST /sources` | `save_session` 的单源版 | 原生 `Source` JSON，或便捷 spec（`{kind, target?, monitor?, crop?, fps?, quality?, sound?}`，服务端解析 hwnd）；**带 `capture` 字段即按原生 Shape 解析**，否则按 spec。`kind: "window"` + `sound: true` = 该窗口的声音写进它自己的文件（一条源、一个文件、两条流）；`kind: "desktop"` + `sound: true` 同理，写的是**整台机器的系统声音**（Windows 没有分显示器的音频）；`kind: "window-audio"` / `"system-audio"` 仍保留，用于只录声音不要画面 | 新快照 |
| `DELETE /sources/{index}` | 同上 | — | 新快照 |
| `PUT /sources` | `save_session` | `Source[]` | 新快照 |
| `PATCH /config` | `set_*` 系列合并 | Config 子集 | 新快照 |
| `POST /record/sources` | `Recording::add_source` | 与 `POST /sources` 同两种 Shape；加进**正在录的这一条 take** | `{ id, start }`——`start` 是新文件在该 take 时间轴上的起点（秒），也是 `take.json` 里那个数 |
| `POST /record/pause` | `Recording::pause` | 无 | `{ paused: true, pause_len, elapsed }`——停钟并让每个源收尾当前文件；新段在这里切开 |
| `POST /record/resume` | `Recording::resume` | 无 | `{ paused: false, pause_len: 0, elapsed }`——下一帧开新段，接在切点上 |
| `POST /record/start` | `start_recording` | `{ quality? }`（0..=51，默认 20；其余取当前设置） | `{ started, countdown_secs }` |
| `POST /record/stop` | `stop_recording` | — | `StopResult`（含 `dir`） |
| `GET /history` | `journal_get` | — | `{ takes, log }` |
| `GET /log?level=warn\|error` | `journal.log` 单独一面 | — | `LogRecord[]`（诊断第一步） |
| `DELETE /log` | `journal_clear("log")` | `{ "confirm": true }` | 新快照 |
| `DELETE /log/{at}` | `journal.remove_log` | `{ "confirm": true }`；`at` = 条目的 `at` 戳（unix ms） | 新快照 |
| `DELETE /history/{id}` | `take_delete` | `{ "confirm": true }`；`id` = take 文件夹名（`dir` 的最后一段） | 新快照 |

**约定（Agent 契约）**：所有写操作都返回操作后的新 `/state` 快照——Agent 不需要再查一次；
下标以 `/state` 返回的 `sources` 数组顺序为准，Agent 不靠记忆猜位置。

**原始 `Source` 的两条硬规则**（便捷 spec 不受影响，id 由服务端生成）：

1. **id 是文件名**（`win` → take 目录里的 `win.mkv`）：只允许字母、数字、`-`、`_`，≤64 字符——
   带分隔符或 `..` 的 id 会逃出 take 目录，服务端直接 400。
2. **id 必须唯一**：两个源共用一个 id 会写同一个文件、按 id 的逐源停止也会失效——重复即 409。

**`PATCH /config` 先验证后写入**：未知键 400；类型不对（布尔位给了字符串、秒数给了小数）
同样 400，不会静默按默认值落盘——「坏值不改配置」是这条路由自己的合同。倒计时秒数的
取值域就是配置文件的 u8（0..=255），越界拒绝而不钳制；热键先过注册表再落盘。

### 5.2 `/state` 快照 schema

**形状 = 磁盘上的形状**（`config.json` / `journal.json` 的 serde 序列化），外加每个
source 一个 `index`——不发明第二套 schema，Agent 写回去的 JSON 就是面板自己存的 JSON。
`capture` 是 serde 的外部标记枚举（`{"Window": {…}}`），`encoder` 用 `PATCH` 也认的
小写短名、`null` = 自动，`history.takes` 是 `TakeRecord`（`dir` 全路径、`at` 是 Unix 毫秒）：

```json
{
  "app": { "version": "0.1.0" },
  "config": {
    "hotkey": "F9",
    "countdown_secs": 3,
    "finalize_panel": true,
    "out_root": "D:\\takes",
    "ffmpeg_path": null,
    "encoder": null,
    "backend": "windows",
    "marks": true,
    "raise_sources": false,
    "language": "zh",
    "theme": "auto",
    "control": true
  },
  "sources": [
    {
      "index": 0,
      "id": "win",
      "capture": { "Window": { "hwnd": 131542, "pid": 20488, "title": "微信",
                               "crop": { "top": 0, "bottom": 0, "left": 0, "right": 0 } } },
      "fps": 30,
      "quality": null,
      "audio": false
    }
  ],
  "status": { "recording": false, "elapsed": 0.0, "error": null, "live": [] },
  "history": {
    "takes": [ { "dir": "D:\\takes\\2026-10-21-22-30-00", "at": 1761057000000,
                 "seconds": 12.4, "bytes": 1048576, "failed": 0 } ],
    "log": []
  }
}
```

### 5.3 MCP 工具（v1 起 13 个，暂停/分段与录制中加源补 3 个 → 现 **16 个**）

`takekit mcp` 作为 `takekit` 二进制的一个子命令，stdio 上讲 MCP（newline-delimited JSON-RPC，
手写最小子集：`initialize` / `tools/list` / `tools/call`，不引 SDK；如后续 `rmcp` 成熟可换）。

| 工具 | 参数 | 说明 |
|---|---|---|
| `takekit_state` | — | 上面那个快照 |
| `takekit_list_devices` | `kind` | 枚举窗口/桌面/摄像头/麦克风 |
| `takekit_add_source` | `kind`, `target`（窗口标题子串 / 摄像头名或链接子串）, `monitor?`, `crop?`, `fps?`, `quality?`（0..=51）, `sound?` | 加一个源；`sound=true` = 该源的声音写进同一文件（窗口=本进程树回环，桌面=整机系统声音） |
| `takekit_remove_source` | `index` | 按 `/state` 下标删 |
| `takekit_set_sources` | `sources[]` | 整表替换 |
| `takekit_set_settings` | `{ hotkey?, countdown_secs?, out_root?, ffmpeg_path?, encoder?, backend?, marks?, raise_sources?, language?, theme?, finalize_panel? }` | 改设置；`ffmpeg_path` 传 `null` = 回到 PATH 查找 |
| `takekit_start_recording` | `quality?`（0..=51，默认 20；其余取当前设置，倒计时走 `countdown_secs` 设置） | 开始（含倒计时） |
| `takekit_record_add_source` | `kind`, `target?`, `monitor?`, `crop?`, `fps?`, `quality?`, `sound?` | **加进正在录的这一条 take**，立刻开录；返回的 `start` = 新文件在该 take 时间轴上的起点（秒），`take.json` 逐源记这个数 |
| `takekit_pause_recording` | — | 暂停：停钟 + 每个源收尾当前文件，新段从这里切开；**仅 Windows 后端** |
| `takekit_resume_recording` | — | 恢复：每个源在下一个样本上开新段，采集从未停过 → 无设备预热损失 |
| `takekit_stop_recording` | — | 停止，返回落盘目录 |
| `takekit_history` | — | 历史 + 日志（全景） |
| `takekit_log` | `level?`（warn/error） | **只有日志**——「出了什么事」的第一站，含控制面自己失败操作的留痕 |
| `takekit_clear_log` | `confirm: true` | 清空日志，destructive |
| `takekit_delete_log` | `at`, `confirm: true` | 单条删日志（按 `at` 戳），destructive |
| `takekit_delete_take` | `id`, `confirm: true` |  destructive，双重确认 |

## 6. 四个必须做对的细节

### 6.1 枚举要能「按名字找到」

Agent 听到的是「微信窗口」，`Source` 要的是 `hwnd`。所以 `list_devices(kind=window)` 必须返回
`{ hwnd, pid, title, x, y, w, h }`（矩形摊平，够 Agent 判断位置；不 expose 进程名与可见性），
`takekit_add_source` 接受**标题子串**并在服务端解析：
唯一命中 → 直接用；多个命中 → 返回候选列表让 Agent 消歧；零命中 → 报错附上当前可见窗口标题。
（与 CLI `--window` 的标题匹配语义一致，不发明新规则。）

**身份 = `hwnd` + `pid`，不是标题**（2026-10-23）：标题随时会变（VS Code 换文件、Chrome 换标签），
所以 `capture.Window.title` 只是「最后一次看到的标题」这块**显示缓存**——解析和录制从不按它回落。
句柄没了就报错让 Agent 重新加源；按旧标题去猜同名窗口比失败更糟，那会静默录到别人的窗口。
`pid` 在 `list_devices` 的返回里本来就有，加源时服务端一并写入；`pid: 0` 是旧配置的「不知道」，
退化成只校验句柄（不认新不认旧，也不用它拒绝任何东西）。

### 6.2 错误面：机器说一种话

控制面 / CLI / MCP 的错误消息用**英文稳定码 + 可读 message**（Agent 对英文无障，且码可被脚本 grep）；
面板 UI 的人话继续走 `err.*` 双语词条。两套语言面各管各的，不混。
（这是决策点 D5，默认按此实现，可改。）

### 6.3 镜像：Agent 的写操作必须马上出现在面板上

控制服务每次写操作成功后，向主窗口 emit 一个内部事件 `ctl:changed`（载荷：`{ what: "sources" | "config" | "history" | "status" }`），
`main.js` 监听后整面板重渲染（载荷只作语义标记，不参与分流——一次罕见的全量重sync 比细粒度 diff 更不容易露馅）。否则「Agent 加了源、面板看不见」，用户会以为没生效——
这是本方案里唯一新增的前端改动（约 30 行 + 一个事件常量）。

### 6.4 失败留痕：Agent 干砸的事要进日志

HTTP 响应只到 Agent 手里，用户事后翻日志时什么都没有。所以**每条失败的写请求**
（POST/PUT/PATCH/DELETE，401 除外）都往面板的日志里写一行：

```
control: POST /record/start failed (err.prepare-failed)     ← text
the capture engine could not be prepared: …                 ← detail（英文原句）
```

5xx 记 Error、4xx 记 Warn；读请求不记（轮询不是事件），401 不记（扫描不是故事）。
日志文本是**英文**：它是机器面产物的存档（D5 口径），面板只是把它显示出来；面板自己
写入的条目仍是随界面语言的人话。成功不记——成功的证据在历史和快照里。

## 7. 接入方式（三种，按需选）

### 7.1 MCP 接入（推荐；之后我就能直接动手）

在 `~/.workbuddy/mcp.json` 的 `mcpServers` 里加（路径换成实际构建产物位置）：

```json
{
  "mcpServers": {
    "takekit": {
      "command": "C:\\<TakeKit 仓库>\\target\\release\\takekit.exe",
      "args": ["mcp"]
    }
  }
}
```

写完后到连接器管理页右上角「自定义连接器」对新服务器点 **Trust** 启用，重启会话。
之后我说话就能驱动：「把微信和飞书加进 Source」会变成：

1. `takekit_list_devices(kind="window")` → 找到微信、飞书的 hwnd；
2. `takekit_add_source` ×2；
3. `takekit_state` → 回读确认（Agent 契约保证写后即见）。

### 7.2 CLI 接入（零安装，现在就能用）

不装 MCP 也行，任何能跑 shell 的 Agent 都能指挥：

```sh
takekit ctl state                      # 看状态
takekit ctl devices --kind window      # 列窗口
takekit ctl add --kind window --target 微信
takekit ctl add --kind desktop --sound on   # 桌面 + 整机系统声音
takekit ctl remove --index 1
takekit ctl config --out-root D:\takes --countdown 5
takekit ctl config --ffmpeg-path ""    # 清掉 ffmpeg 路径覆盖，回到 PATH 查找
takekit ctl start
takekit ctl stop                       # 打印落盘目录
takekit ctl history
takekit ctl delete --id 2026-10-21-22-30-00 --confirm
```

`ctl add` 的 spec flag：`--kind`（必填，window|desktop|camera|mic|system-audio|window-audio）、
`--target`（标题/设备名子串）、`--monitor`、`--crop T,B,L,R`、`--fps`、`--quality`、
`--sound on|off`（window/desktop 才认，其余 400）。
`ctl config` 的设置 flag（与 MCP `takekit_set_settings` 同键）：`--hotkey`、
`--countdown`、`--out-root`、`--ffmpeg-path`（空串 = 清掉覆盖）、`--encoder`、
`--backend`、`--marks on|off`、`--raise on|off`、`--language`、`--theme`、
`--finalize-panel on|off`。
其余：`ctl log [--level warn|error]`、`ctl clear-log --confirm`、
`ctl delete-log --at <unix-ms> --confirm`。

### 7.3 curl 接入（调试用）

`control.json` 里同时有 `port` 和 `token`，一次读出来（Git Bash / sh）：

```sh
INFO=$(python -c "import json,os;d=json.load(open(os.environ['APPDATA']+'/TakeKit/control.json'));print(d['port'],d['token'])")
curl -H "Authorization: Bearer ${INFO#* }" "http://127.0.0.1:${INFO%% *}/state"
```

PowerShell 里则是：

```powershell
$d = Get-Content "$env:APPDATA\TakeKit\control.json" | ConvertFrom-Json
curl -H "Authorization: Bearer $($d.token)" "http://127.0.0.1:$($d.port)/state"
```

## 8. 分期

| 期 | 内容 | 规模 |
|---|---|---|
| **v1 ✅** | §5 全部 13 端点 + token/control.json + `ctl:changed` 镜像 + 失败留痕 + 设置开关 + `takekit ctl` + `takekit mcp` + 守卫（409/confirm） | gui +`control.rs`、cli +`client.rs`/`ctl.rs`/`mcp.rs`、新增依赖 tiny_http+getrandom（gui）/ ureq（cli） |
| **v2** | SSE 事件流（`take:done` 推送，Agent 不必轮询）；`open_take`/资源管理器 Reveal；面板未运行时自动拉起；crop 编辑 API | |
| **v3** | `takekit://` 深链做一键触发（手机快捷指令、定时器）；录制中逐源状态推送 | |

## 9. 测试方案

- `docs/tools/check-contracts.py` 扩一节：**端点 ↔ MCP 工具 ↔ CLI 子命令三方对齐**（防再漂移，第 1 轮删过的死事件 `take:stopped` 就是这类漂移），接 CI。
- 契约脚本沿用负向注入测试（临时加假端点/假工具，确认能红）。
- E2E：起面板 → `takekit ctl state` → add 两个源 → start → stop → 校验 `StopResult.dir` 真实存在且文件非空 → delete 走 confirm 缺字段应 400。
- 安全回归：换错 token 401；服务关掉后 control.json 不存在、客户端报「面板未运行」。

## 10. 决策记录（2026-10-21，全部按建议采纳）

| # | 问题 | 建议 |
|---|---|---|
| D1 | 控制服务默认开还是默认关？ | **默认开**（loopback+token，威胁模型见 §4）；设置页可关 |
| D2 | HTTP 还是命名管道？ | **HTTP**（可 curl 调试、跨语言零成本）；管道略安全但客户端要写 Win32 |
| D3 | MCP 做成 `takekit mcp` 子命令还是独立二进制？ | **子命令**（一个 release 产物，少一个分发单元） |
| D4 | 面板没开时客户端报错还是自动拉起面板？ | v1 **报错**（可预期、无副作用）；v2 做自动拉起 |
| D5 | 机器面错误语言？ | **英文稳定码**（§6.2）；人类面保持 zh/en 双语 |
| D6 | 写操作后 UI 镜像的粒度？ | 事件 + 整面板重渲染（v1 够用，不做细粒度 diff） |

## 11. 明确不做的事

- 不做「Agent 替人点击 GUI」（UIA/截图那条路）——那是没有 API 时的权宜，本方案就是从根上免掉它。
- 不改 `Source` / `Config` 的序列化形状（Agent 面直接吃现有结构，不发明平行 schema）。
- 不在 v1 碰深链、自动拉起、SSE。
