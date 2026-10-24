# TakeKit 开发笔记（常设）

> 后续开发的常设参考。交接文档（HANDOFF）已解散，其长期有效的内容都收在这里；
> 设计契约在 `DESIGN.md`，沙箱规程在 `docs/webview2-agent-sandbox.md`，
> API 采集后端在 `docs/api-recording.md`。
> **进度与待办收在本文件 §7**（滚动维护，读者是维护者，不对外）。
> **除 §7 外只收"以后还会用到"的东西**；一次性进度不要往这里写。
> 面向使用者的介绍在双语 `README.md`（英文）/ `README.zh-CN.md`（中文），那里不放开发细节。

---

## 1. 仓库与环境

| 项 | 值 |
|---|---|
| 仓库路径 | 含中文（`…/20-方向/短视频/tools/TakeKit`），见 §7 第 10 条 |
| 平台 | 仅 Windows（大量 Win32 调用），rustc edition **2024** 工作区 |
| crate | `takekit-core`（采集/编码/会话）、`takekit-cli`（bin 名 **`takekit`**）、`takekit-gui`（Tauri 2 面板）、`takekit-media`（WGC + Media Foundation 采集） |
| 依赖 | ffmpeg 需在 PATH（ffmpeg 后端用；Windows API 后端不需要）；GUI 用 Tauri 2 + WebView2；`mouseshow` 是 git submodule（以库形式链接，提供鼠标点击波纹） |
| 数据目录 | `%APPDATA%\TakeKit\config.json`、`%APPDATA%\TakeKit\journal.json` |
| 默认输出 | `%TEMP%\<YYYY-MM-DD-HH-MM-SS>\`（每次录制一个目录，含各源文件与 `take.json`） |
| 面板窗口 | 固定 **900×640** 客户区，不可缩放、不可最大化 |

---

## 2. 构建 / 测试

```powershell
# 关掉正在跑的实例再构建（会锁 exe）
Get-Process takekit-gui -ErrorAction SilentlyContinue | Stop-Process -Force

cargo build --release -p takekit -p takekit-gui     # 约 2 分钟
cargo test --workspace                              # 基线见下
cargo clippy --workspace --all-targets -- -D warnings   # 必须零 warning
cargo fmt --all                                     # 改完直接跑，全仓 0 diff
```

- 单独格式化某个文件：`rustfmt --edition 2024 <file>`。
- `takekit-core` 有 7 个 `#[ignore]` 实机测试（会开真窗口），`cargo test -p takekit-core -- --ignored` 才跑——**别在用户正用机器时跑**。
- 基线（2026-10-24 实测）：core 59（+7 ignored）/ cli 9 / gui 84 / media 28。以 `cargo test` 实际输出为准。
- `docs/tools/check-contracts.py` 四方核对（命令 / 事件 / i18n / **控制面端点↔MCP 工具↔CLI 子命令**），CI 有门禁；改动任何一层的名字前先看它。
- **功能级三端一致性**：`node scripts/check-three-surface.mjs`——真跑 MCP（schema → stdio 调用 → 面板 → 配置）和 CLI 参数。名字对了不等于字段通了：`sound` 曾在 schema 里声明、在 HTTP 转发前被白名单丢掉，`check-contracts.py` 看不见这种漂移。
- **界面语言×主题**：`node scripts/check-ui-matrix.mjs`——zh/en × light/dark × 9 屏（源列表、卡片、最小化窗口的卡片、裁剪源卡片、裁剪工作区、裁剪被丢弃的卡片与工作区、添加源、设置），量横向滚动、文本溢出、WCAG AA 对比度，截图落 `target/checks/ui-matrix/`。它就是抓出「`.field button.browse` 选择器从未匹配、深色下 `…` 对比度 1.02」的那一个。
  - 它还钉住**尺寸列的口径**：裁剪放不下时，列表 / 卡片 / 工作区三处都必须报「整幅」。这条来自一次真事故——后端 `CropBox::rect_in` 会丢掉放不下的裁剪、整帧原样录，而列表当时照算式印 `1720×1392 → -2280×1392`，报的是一个永远不会存在的文件。**算术正确 ≠ 说得对**，凡是由算式得出的展示值，都要问一句「后端实际会给什么」。
  - 以及**行内空隙**（`row-summary-tight`）：帧率/画质格必须按文字自适应，且离尺寸列只隔 12px。定宽列装不满的那截空白不会溢出、不会截断、不会有滚动条，只有截图看得出来——用户先看到，测试才补上（2026-10-23）。
  - 以及**右侧三列是一条信息线**（`row-speaker-last` + `row-trailing-columns-line-up`）：喇叭必须在帧率之后、在最右；行从右往左排，所以任何宽度可变的格子落在数字左边都会让整列按行横移——不能出声的行因此保留一个同宽的**空槽**（`row-sound-slot`）而非少一格。这两条是同一个教训的两面：**右边的列只有在宽度不随行变化时才成列**。
  - 还会**自己造一个最小化的窗口**（WinForms 表单 + 最小化，脚本里 `probe` 那段）来测「没画面就不给裁」与「恢复窗口后能裁」；行标题则靠塞一个假标题（`STALE …`）来验证「显示的是窗口现在的名字」。这两条都是 2026-10-23 用户报的问题，靠读代码看不出来。
  - 以及**喇叭的作用域**（`row-speaker-scope`）：window 与 desktop 行必须有喇叭，mic 与 camera 行必须没有。判断种类用 `row.dataset.kind`（产品代码为测试加的语义钩子），不匹配图标 SVG——图标一改，断言就悄悄失效，那是最糟的一种绿。
- **隔离运行**（不碰用户真实配置）与 WebView2 双缓解：见 `docs/webview2-agent-sandbox.md` §4（唯一出处，配方与两条铁律都在那里）。

### 2.1 界面矩阵钉住的规则（2026-10-23，从 README 搬来）

`check-ui-matrix.mjs` 与 `check-three-surface.mjs` 的断言数**跟着「屏幕 × 语言 × 主题」增长**，
别把某一次的数字抄进文档当常量（2026-10-25 有 123 这个数，同一批断言在加屏幕后就变了）。
判据是脚本自己那行汇总全绿（`N/N`）+ `low=0 spill=0 hScroll=0`；`check-three-surface.mjs`
2026-10-25 实测 `14/14`。输出里 `trunc` 是省略号截断（设计意图，不算失败），`low` 是实测对比度
（背景沿 DOM 向上找到第一个不透明容器再算），`spill` 是没有滚动条却超出容器的文本。

| 断言 | 它保证什么 |
|---|---|
| `row-summary-tight` | 帧率/画质格按文字自适应（`slack ≤ 1px`），且离尺寸列只隔行本身的 12px（`gap ≤ 16px`）。它曾是定宽 88px 装 58px 的字，30px 死白不溢出、不截断、不出滚动条，只有截图看得出来 |
| `row-speaker-last` + `row-trailing-columns-line-up` | 右侧三列是一条信息线：行从右往左排，任何宽度可变的格子落在数字左边都会让整列按行横移；喇叭因此在**帧率之后、× 之前**，且不能出声的行留**同宽空槽**而不是少一格。实测 `fps→785 / ×→835` 七行全等 |
| `row-title-follows-window` | 行上印的是窗口**现在**的标题。脚本给一个源塞假标题 `STALE …`，跑完必须查无此名且真标题在场（只查前半条，一行涂白也能过） |
| `minimized-no-crop` / `minimized-restore-offered` / `minimized-crop-closed` | 窗口最小化时缩略图必须 disabled、点它不开工作区、「恢复窗口」在页脚 |
| `restore-window/turns-croppable` | 按下「恢复窗口」后同一张卡片变成可裁：`unminimize` → 重抓静帧 → 有图才放行 |
| `row-speaker-scope` | 喇叭只长在能出声的画面上：window 与 desktop 每行都有，mic 与 camera 一个都没有（那里是空槽）。`row.dataset.kind` 就是为这条加的语义钩子，不匹配图标 markup |
| `desktop-sound-says-whole-machine` | 桌面行的提示必须说清「整台电脑的声音」；不写这句，用户会以为是这块屏的声音 |
| `picker/window-list-carries-pid` | 选择器递上来的窗口带着 `pid`——pid 校验只在播种数据里存在等于没有 |
| `row-crop-*` / `card-dropped-*` / `workspace-*` | 尺寸列的**三处口径**（列表行 / 卡片角标 + 输出行 / 裁剪工作区）：裁剪放不下时三处都报整幅，与 `CropBox::rect_in`（留不下 2px 就整帧原样录）一致。只改一处，另两处就会印出 `0 × 1392` 这种写不出来的尺寸 |

两脚本都以 `target/checks/` 为隔离 APPDATA，**跑之前先关掉正在运行的面板**（单实例会让第二个进程
直接退出，脚本拿不到控制端口）。新增界面加进 `SCREENS`，新增语言/主题扩 `LANGS` / `THEMES`。

**开发约定**：沟通中文；代码、注释、标识符一律英文；`docs/` 下文档用中文。新行为走 TDD（先写失败测试）。不自动 commit / push——只有用户明确发话才提交。

---

## 3. 架构地图与数据流

```
crates/
├─ takekit-core/          ← 所有"决定"都在这里，GUI 只是壳
│   ├─ session.rs         Session::prepare → Prepared → start → Recording
│   │                     （Backend::Ffmpeg 与 Backend::Windows 两条运行时）
│   ├─ take.rs            Take::create / 目录与 take.json（ext: mkv 或 mp4）
│   ├─ source.rs          Capture{Desktop,Window,Camera,Mic,SystemAudio,WindowAudio}、
│   │                     Backend、list_windows、is_recordable、bring_to_front、
│   │                     偶数尺寸 crop
│   ├─ encoder.rs         NVENC → QSV → AMF → x264，每个都用一帧试编验证
│   └─ ffmpeg.rs          locate / spawn / 探针 / list_devices
├─ takekit-media/         ← Windows API 采集（详见 docs/api-recording.md）
│   ├─ clock.rs           TakeClock：全 take 统一时间原点（WGC=QPC 直减；MF=首样本桥接）
│   ├─ screen.rs          WGC 抓屏/抓窗口（被遮挡窗口可录）
│   ├─ camera.rs / mic.rs Media Foundation 设备读取（NV12 / PCM）
│   ├─ loopback.rs        WASAPI 回环：系统声音 / 单窗口进程树声音
│   ├─ runner.rs          DeviceCapture：三种设备捕获共用的线程骨架
│   │                     （开设备→报格式→读循环→stop/失败槽/Drop）
│   └─ writer.rs          MF SinkWriter：H.264+AAC 写 MP4（含前导黑帧/静音对齐）
├─ takekit-cli/           ← 薄壳，bin 名 `takekit`
└─ takekit-gui/
    ├─ main.rs            tauri 命令 + setup（窗口/托盘/提示层/热键）
    ├─ take.rs            take 生命周期：begin / status / stop / stop_source / 热键
    ├─ config.rs          %APPDATA%\TakeKit\config.json（设置，backend 默认 Windows）
    ├─ control.rs         本地 HTTP 控制面（docs/agent-control.md：路由 ↔ MCP ↔ ctl）
    ├─ journal.rs         %APPDATA%\TakeKit\journal.json（历史 + 日志）
    ├─ place.rs           窗口位置记忆（-32000 哨兵在这里处理）
    ├─ preview.rs         编辑器缩略图与裁剪工作区的取图回传
    ├─ reveal.rs          打开 take 目录
    ├─ overlay.rs         全屏提示层窗口
    ├─ tray.rs / tray_panel.rs   托盘图标与迷你面板
    ├─ shortcut.rs        热键字符串规范化
    ├─ proto.rs           IPC 类型（snake_case，Tauri 不重命名）
    └─ ui/                index.html（三页签+设置浮层）· shared/i18n/beep/main/style
                          overlay.{html,css,js} · tray.{html,css,js}
```

**一次录制的数据流（Windows API 后端）**——面板走这条（`config.rs` 默认
`Backend::Windows`）；**CLI 相反，默认 ffmpeg**，要显式加 `--windows` 才切过来。

```
按钮/F9 → main.js toggleRecording()
  → pruneSources()（不可用的源先剔掉）
  → invoke("start_recording") → take::begin()
       （Windows 后端不 raise 窗口；prepare 只校验源可寻址，微秒级）
       ├─ countdown_secs == 0：藏主窗口 → 50ms → spawn（返回 started=true）
       └─ countdown_secs > 0：存 pending → 藏主窗口 → overlay:countdown
            遮罩自己跑 3/2/1/GO（rAF + performance.now，每个数字 beep）
            GO 结束（+1.0s）→ invoke("overlay_finish")
              → 收遮罩 → 50ms 落帧 → take::spawn()
              → 失败：遮罩已收、主窗口回来、last_error、emit take:failed
            Esc（Rust 每 30ms 轮询 GetAsyncKeyState）或倒计时中再按 F9 → take::cancel()
       → 录制中：主窗口隐藏，托盘红点 + 托盘面板提供计时与停止
  → 250ms 轮询 invoke("status")：计时、逐源字节、dropUnusable()
  → 停止（按钮 / F9 / 托盘面板）→ take::stop(announce)
       recording.stop()（采集停 → writer 收尾 → StopResult）
       → record(&result) 写 journal.json（历史在 Rust 落库，不靠前端）
       → show_main_window()（热键停止时被录窗口在前台，面板必须回来）
  → main.js showSaved()：failed>0 才弹浮窗；随后 refreshJournal()
```

ffmpeg 后端的数据流差异：`begin` 里每个窗口源 `bring_to_front` → `Session::prepare`
带最长 2 秒的编码器/引擎探测 → 每源 spawn 一个 ffmpeg 子进程（详见 `source.rs` 的
`input_args`）。

---

## 4. 实现契约（改前必读，踩过坑才定下的）

1. **窗口的身份是 hwnd + pid，不是标题**。`Capture::Window{hwnd,pid,title,crop}`，`title` 只用于显示和兜底，`pid` 记添加时的属主（`0`=未知，老配置都长这样，只做 hwnd 检查）。Windows 会回收句柄号，光有 hwnd 会在窗口死后指向别人的窗口——重名窗口曾导致"按 F9 只闪一下"。
2. **位置记忆**：物理像素、建窗时定位（不是先摆默认位再移）、拖动 400ms 限流；`-32000` 最小化哨兵**两轴都是才判否**，写入侧拦、读取侧也过滤（详见 DESIGN §4.10）。
3. **奇数尺寸**：ffmpeg 路径在 `-vf` 链**末尾**加 `crop=trunc(iw/2)*2:trunc(ih/2)*2`（用 crop 不用 scale，不重采样）；API 路径用 `writer::even_frame`（NV12 分平面裁）。
4. **源可用性只看 hwnd**：`is_recordable` = 存在 + 可见 + 非 cloak；**最小化不算不可用**（`bring_to_front` 会恢复它）。picker 另用 `is_switchable`（任务栏规则）。
5. **热键只是"告诉主窗口去按按钮"**：`emit_to("main","hotkey:toggle")`，主窗口 JS 调自己的 `toggleRecording()`。不要在热键回调里直接 `begin`（首次按下时还没有参数）。
6. **历史由 Rust 写**（`take::stop` 里），不是前端写——从托盘面板停的那次也要记；**收尾自己失败时也记一条**（`failed:1`，`duration:None`）。
7. **分层**：提示层 `overlay` 是独立窗口（整屏、`WS_EX_TOOLWINDOW`）；面板里的浮窗 `z-index: 50` 压过 picker（`10`）与所有内容，**不参与布局**（盖住内容是有意的）。
8. **成功的"已保存"不弹任何东西**（历史承担反馈），**只有警告/错误**弹浮窗；日志只记警告/错误。

数据文件（`config.json` / `journal.json`）的字段、默认值、原子写与未读语义：**唯一出处是 DESIGN §4.10、§4.11**，这里不重复。

---

## 5. 验证手法

### 5.1 探针日志 `TAKEKIT_PROBE_LOG`

代码里到处是 `probe("...")`，写文件、不依赖界面。关键 stage：

```
rust:setup / rust:exit / config:... / hotkey:ready|unavailable|registered / tray:ready|idle|recording
tray:panel-ready / tray:placed|place-failed / tray:hidden|reopen-guarded|no-panel|missing
overlay:ready / overlay:shown|hidden|missing|demo / place:restore x,y / place:actual x,y
page_load:Started|Finished url=...
ui:html-head / ui:dom-ready / ui:js-main-eval
ui:ipc-sources:Nw/Md / ui:ipc-env-ok:{...} / ui:ipc-config-applied:Ns
ui:sources-pruned:N / ui:sources-dropped:N / ui:init-done
hotkey:pressed / ui:hotkey:toggle / tray:recording|idle
front:<title>=0|1 rank Some(a)->Some(b) / source:stopped <id>

倒计时（按时间顺序）：
overlay:wake（Rust→遮罩，唤醒音频，无回写探针）
prepare: [("locate",ms),("encoder",ms),("engine",ms)]   ← ffmpeg 路径各段耗时；
         Windows 路径只有 [("validate",ms)]（源可寻址性校验，微秒级）
engine:Dda|Gdi probed=true|false                        ← false = 本支用 GDI，探测在后台
spawn: [("id",ms),…]                                    ← 逐源 spawn 耗时
settings-applied lang=zh|en theme=auto|light|dark hotkey=<显示值>   ← 页面实际应用的值
settings-changed lang=… theme=…                                      ← 每次切换
countdown:begin secs=N | countdown:off
overlay:countdown:N / overlay:digit:N t=秒 audio=running|suspended / overlay:go
overlay:finish-invoked → spawn:ok | spawn:failed <msg>   （遮罩已收，间隔 ≈50ms）
overlay:stop / overlay:dismiss
overlay:cancel-invoked → cancelled | cancel:nothing
begin:failed <msg>（prepare 失败）/ stop:failed <msg>（收尾失败）/ overlay:show-failed <e>
ui:tick-first（每次 take 首次成功轮询）/ ui:show-main / main:close-hidden
js-error:<msg>
```

**事件 vs 探针**：`take:started`、`take:finalize`、`take:source`、`take:done`、
`take:cancelled`、`take:failed`、`take:beep`、`overlay:countdown`、`overlay:wake`、
`overlay:stop`、`hotkey:toggle`、`ui:settings`、`ctl:changed`（控制面写操作后的全量重同步，
见 `docs/agent-control.md`）是 **Tauri 事件**（`emit_to`，前端 `listen` 接），
不出现在探针里；它们的前端处理才写 `PROBE`。（`take:stopped` 已删除：完成态
由 `take:done` 单点承担，见 take.rs stop()。）

**`js-error:` 最有用**：`index.html` 挂了 `window.onerror`，JS 一抛就进探针——不用猜"为什么界面不动"。

### 5.2 驱动界面

- **让面板"按 F9"**：`tauri-plugin-global-shortcut` 建隐藏窗口（class `global_hotkey_app`）+ `RegisterHotKey`（F9 的 `id = 168`）。用本地 `docs/tools/ui-probe.ps1 -Action hotkey` 投 `WM_HOTKEY`。**`keybd_event` 注入的按键触发不了 `RegisterHotKey`**（实测对照过），别浪费时间。
- `ui-probe.ps1` 还支持 `-Action shot|click|focus`。合成点击会被前台窗口抢走：先 `-Action focus`；UIA 内容树在当前环境取不到（WebView2 不发布），坐标点击前先 `-Action shot` 拿窗口矩形再算——估偏 50px 会点到面板外，现象是"点了没反应"。
- 🔴 **真机首选 CDP，不点鼠标**（2026-10-20 实证）：`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>` 启动 release（`webview_browser_args` 会把外部值并进去），然后
  `curl --noproxy '*' http://127.0.0.1:<port>/json` 拿 target，用 Node 22 自带的 `WebSocket` 连
  `webSocketDebuggerUrl` 跑 `Runtime.evaluate` / `Page.captureScreenshot`。**不经过鼠标 → 不会被别的窗口遮挡**，截图是 webview 原像素（比 PrintWindow 干净）。
  - 面板页的 url 是 `http://tauri.localhost/`，**不含 `index.html`**；`tray`/`overlay` 两个窗口要排除。
  - 页面里的全局 `const`（如 `state`）和函数声明（如 `openEditor`）在 `Runtime.evaluate` 里可见。
  - ⚠️ 端口被上一轮残留的 `msedgewebview2.exe` 占了就静默失效：先 `taskkill /F /IM msedgewebview2.exe`，再用新端口。
- 无头 DOM 测试（纯前端、不启动 app）：`docs/tools/make-editor-harness.py` 用真实 index.html + 桩 IPC 生成页面，Chrome `--dump-dom` 断言 / `--screenshot` 出图（`#still-editor`、`#still-crop`、`#still-crop-drag`，加 `-en` 后缀出英文）。
- 判据用**像素/JSON**，不要目测截图：录到的画面抽帧比对具体像素值；历史是否落库直接读 `journal.json`。
- **多路完好性核对**（本地 scratch `tk-analyze.py`）：对每个文件跑三件事——ffmpeg 全解数
  解码错误、`帧数 == 时长×fps`、PTS 间隔直方图；再比跨路帧数/时长差。正常值：错误 0、
  洞 0、跨路差 ≤2 帧（尾样本舍入，见 `api-recording.md`）。

### 5.3 本地 scratch 脚本（默认不入库）

`docs/tools/` 整目录在 `.gitignore` 里（Win32/UIA 驱动脚本，随环境漂移）：
`ui-probe.ps1`（探针/截图/点击/F9）、`tk-verify-b3.ps1`（B3 端到端：清场+启动+F9/Esc/开关）、
`tk-veil-*.ps1`（遮罩动画/透明度验证）、`tk-shot-debug.ps1`、`tk-verify-settings.ps1`、
`make-editor-harness.py`（无头 DOM + 截图：源行 → 编辑源 → 裁剪工作区 → 保存）。

---

## 6. 陷阱清单（都是真踩过的）

1. **`additional_browser_args` 是整体替换，不是追加**。它会顶掉 wry 默认的 `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` → WebView2 创建失败 `0x8007139F` → 整个 app 起不来。WebView2 参数**只能走环境变量** `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`（详见 `webview2-agent-sandbox.md`）。
2. **`ui/` 是编译期嵌入的**：改了 `ui/*.js|css|html` 必须重新 build 才生效。改了行为却验证旧二进制 = 假阴性（踩过）。
3. **`config.json` 带 UTF-8 BOM 会静默回退默认值**。PowerShell `Set-Content -Encoding UTF8` 会写 BOM；用 `[IO.File]::WriteAllText($p, $s, (New-Object System.Text.UTF8Encoding $false))`。
4. **`IsWindowVisible` 对最小化窗口返回 `true`**。判"在不在用户眼前"要用 `IsIconic` 或坐标 `-32000`。
5. **Windows 11 静默忽略后台进程的 `SetWindowPos(HWND_TOP)`**（返回 TRUE、Z 序不动）。抬窗口必须 `HWND_TOPMOST` → `HWND_NOTOPMOST` 两步，带 `SWP_NOACTIVATE`。恢复最小化窗口**必然**激活它（系统语义）。
6. **`keybd_event` 触发不了 `RegisterHotKey`**（本机）。用 `PostMessage(WM_HOTKEY)`。但注入的 Esc **能**被 `GetAsyncKeyState` 轮询看到。
7. **P/Invoke 的 `StringBuilder` 必须 `CharSet = CharSet.Unicode`**，否则 `GetClassNameW` 只读到首字符。
8. **`Add-Type` 里的方法不能叫 `Main`**（撞 C# 入口点，warning-as-error 直接失败）。
9. **`explorer.exe` 的退出码没有意义**：spawn 出去就不管，别 `wait()` 判成功。
10. **仓库路径含中文**：`powershell -File <中文路径>` 会按 ANSI 读坏路径。脚本放仓库内、从仓库根调用；或用 `-Command` 传。
11. **每次 shell 调用都是新的 PowerShell 会话**：`Add-Type` 不跨调用，脚本要么一个文件跑完，要么每次重新 `Add-Type`。
12. **`Start-Process cargo ... -Wait` 读不到 EOF** 时用 `-RedirectStandardOutput <file>` 再读文件；但 **`Start-Process -RedirectStandardOutput/Error` 在本沙箱必失败**（注入的双大小写 `HTTPS_PROXY` 键撞 PowerShell 已知缺陷）——要捕获输出就让命令写文件再读。
13. **`$host` / `$pid` 是 PowerShell 自动变量**，不要拿来当自己的变量名。
14. **沙箱命令结束后会回收它启动的进程树**：GUI 交互必须在单个工具调用内完成；跨调用驱动前先 `Get-Process takekit-gui` 探活。每次起实例前清场——残留实例占着 F9（新实例 `hotkey:unavailable`），之后所有 F9 都打到旧实例上。
15. **PowerShell 5.1 按 ANSI 读无 BOM 的 .ps1**：脚本里不要出现中文。
16. **gdigrab 在遮罩升起时会挂住**；倒计时期间的截图用 .NET 中心裁剪（全屏编码 ~4s 会错过整个 4s 倒计时）。
17. **WGC 只在画面变化时交付帧**，且 **MF SinkWriter 走不出真 VFR**（编码器按声明帧率展开重复帧）——详见 `api-recording.md` §4、§7，别再对这条路径抱 VFR 预期。
18. **serde 枚举上线格式必须显式定死**（2026-10-20 踩坑）：`Backend` 没加 `rename_all = "lowercase"` 时，写出 `"Windows"` 但前端发 `"windows"`，**一个字段解析失败会毒化整个 config**（全部字段静默回退默认，还留下 `config.json.corrupt`）。IPC 枚举一律 lowercase + 往返测试。
19. **别内联复制 IPC 请求对象**：`start_recording` 的内联 req 曾漏掉 `backend`，serde 默认值让 GUI 悄悄走 ffmpeg 后端、面板却显示"Windows API"。请求体统一用 `sessionParams()` 拼。
20. **GUI 全链路测试入口**（本地 scratch `tk-gui-drive.py`）：种子 config.json（源/后端/输出目录）→ 起隔离 APPDATA 实例 → 等 `ui:ipc-config-applied:Ns`（Ns=源数，**必须等它**，之前按 F9 会丢 toggle 或 0 源）→ `PostMessage WM_HOTKEY`(id=168) 开停 → 读 journal/take.json + 解码核对。最小化窗口**开录前会自动恢复**（2026-10-20 起；WGC 对最小化窗口 0 帧，不恢复=录空气），回归测试应**故意最小化**目标窗口验证自动恢复链路——早先「prepare 拒绝 0×0 客户区」的旧行为已废。

21. **同一个按键被两层监听各处理一次**（2026-10-20 实证，编辑源 Esc）：裁剪工作区的
    `keydown` 在 stage 上处理 Esc → `closeWorkspace()` **会先把"工作区开着"的标志清掉**，
    事件继续冒泡到 window 上那个"关卡片"的监听，它读到标志已经没了 → 一次 Esc 连退两层，
    `editor` 变 `null`，紧接着的「保存」静默变成 `if (!editor) return`（界面像"保存没反应"）。
    规则：**谁消费了这个按键就 `stopPropagation()`**；全局兜底监听不要读"会被自己刚做的事改掉"的状态。
22. **同一个功能的两块 DOM 要各自赋值**：裁剪页有自己的 `<img id="crop-img">`，卡片里是
    `#editor-img`；只给缩略图设了 `src`，工作区就是一片黑——没有任何报错，用户看到的是"图没出来"。
    新增一层视图时先问一句"它有没有自己的 img / 输入框 / 按钮"。

23. **窗口源的"画面"是整个窗口，而且只有 DWM 说得准**（2026-10-21 实证，VM 窗同一时刻
    三个矩形三个答案）：`GetClientRect` 1920×1134（没标题栏）、`GetWindowRect` 1936×1173
    （多了 Win11 隐形缩放边框，左右下各 7px）、**`DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` 1922×1166 = WGC 落盘的帧**。
    还有：`PrintWindow` **默认画整个窗口**（`PW_CLIENTONLY` 才只画客户区），拿客户区尺寸去接
    整窗渲染 = 右边和底边被切掉。预览/尺寸/ffmpeg 抓取一律走 `source::window_rect`
    （优先 DWM），裁剪数字因此和落盘画面 1:1。
24. **最小化窗口连 DWM 也答不对**：`DWMWA_EXTENDED_FRAME_BOUNDS` 和 `GetWindowRect`
    都返回**任务栏图标矩形**（实测 148×22 / 146×28，看着像个尺寸），`GetClientRect` 0×0。
    用 `WINDOWPLACEMENT.rcNormalPosition`（恢复后的整窗外框）再减隐形边框
    （`visible_frame`：`SM_CXSIZEFRAME + SM_CXPADDEDBORDER`，左右下各一份、顶上没有）。
    全程不移动用户的窗口——`window_by_hwnd` 会 restore，打开个编辑器不该动别人的桌面。
25. **行内嵌按钮时行本身不能是 `<button>`**（2026-10-21，历史行删除）：按钮套按钮不是合法
    HTML，行改 `div[role=button]` + `tabIndex=0` 后要注意两件事——① 行级 keydown 要判
    `e.target === row`，否则垃圾桶上按 Enter 会「删除 + 打开」一起触发（keydown 冒泡）；
    ② 点击拦截不能只拦 `click`，行的打开监听挂在祖先上，子按钮必须 `stopPropagation`。
    另：**凡删磁盘内容的命令都要有"这是不是我们的东西"闸门**（`take_delete` 只删含
    `take.json` 的目录），journal 里的路径是可被手改的输入，不是可信值。

---

## 7. 进度与待办（滚动，读者是维护者）

> 这一节从根 `README.md` 搬来（2026-10-24，README 改成对外双语版）。
> 对外只承诺 `README.md`「Limitations」那几条；这里的日期、决策编号、待办优先级都是内部的。

**面板已可日常使用**：源列表、设置记忆、F9 开/停、托盘、录制历史、日志页签、倒计时 3/2/1/GO +
提示音 + Esc 取消、快捷键/语言/主题设置。录制后端默认走 **Windows API**（WGC + Media Foundation），
窗口被遮挡也能录；ffmpeg 后端保留为回退。

**待办**（大致按优先级）：

1. **音频通道已补全**（2026-10-23）：麦克风 / 系统声音（WASAPI loopback）/ 单窗口声音均已实现并
   实测通过（`docs/api-recording.md` §8），GUI 选择器同步跟进；摄像头仍缺 MF 可见设备未验证。
   **窗口声音已改为窗口源的一个开关**（2026-10-23 晚）：一条源、一个文件、两条流
   （`docs/api-recording.md` §11.3），picker 里每个窗口只出现一次。
2. ~~多路漂移观测~~ ✅（2026-10-23 晚，`docs/api-recording.md` §11.1）：**没有可测的时钟漂移**
   （20s/180s/600s 差 ≤0.04s，量级就是 AAC 帧量化），不做 slew 校正，`drift_ppm` 字段也不加。
   顺带修了一个上报口径 bug（前导静音没算进 `seconds`，导致 60 秒录制凭空少 0.5s）。
   **遗留**：30 分钟那次多出 4.9 秒的音频是设备一次性重投造成的，会让之后的音频永久后移 ——
   修法（用 `chunk.time` 检测空洞/重投）见 §11.2，未做。
3. ~~待拍板 5 项~~ ✅（2026-10-23 全部拍板并落地：单实例二次启动聚焦已有窗口；提示层创建失败降级为
   "无提示层 + 记探针"；历史「清空」连磁盘文件一起删（删不动的行保留）；CLI `--marks` 与
   UI「鼠标效果」文案对齐；ffmpeg 路径可配置（设置行 + CLI `--ffmpeg` + 控制面 `ffmpeg_path`）；
   另修历史行双击开两个资源管理器窗口的节流）。
4. 未实测：150% 缩放、多显示器双屏真机、深色主题下历史页浮窗/徽章观感。
5. ~~B4 收尾流程~~ ✅（2026-10-21：`take:finalize|source|done` 事件流 + 提示层逐源清单 +
   状态条 `正在写入… n/m`，「已保存」横幅按口径不做；`finalize_panel` 开关已进设置）。
6. ~~状态条 failed 态~~ ✅（2026-10-21：菱形 + 原因单行 + 查看日志）。
7. ~~光标/多屏对齐~~ ✅（2026-10-21：`WithCursor` 实测光标进画；picker 逐屏 + rect→Monitor）。

**性能 review**（2026-10-23）：录制热路径已过一遍。帧通道原为无界 `mpsc::channel`，与"队列满则丢帧"
的注释矛盾 —— 写入端落后时帧会在内存无限堆积（4K BGRA 帧约 33 MB）。已改为有界 `sync_channel` +
`offer()`（满则丢帧、writer 断开才停采集），视频队深 3、音频队深 64
（`takekit-media/src/frame.rs`）；KeepLatest 采样顺修积压时取最新帧而非队头帧（`newest_of_burst`）。
其余热点（250ms tick 链、`spare` 复用、`copy_frame`）已是优化后形态，无改动。
