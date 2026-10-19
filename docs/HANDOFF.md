# TakeKit 交接文档

> 写于 2026-10-19（B3 完成后更新）。假设读者**没有**之前的对话记录。
> 全部改动**还没有提交**（仓库里所有东西都在工作区）。先读第 2 节。

---

## 0. 一句话现状

Windows 桌面录屏工具，Tauri 面板已可日常使用：列可录的源、记住上次设置、**F9** 开录/停录、托盘图标、**录制历史（点一行直接进目录）**、**日志页签**、**右上角浮窗提示**、**倒计时开关**。
"录制过程"层已补齐：**倒计时 3/2/1/GO + 每数字提示音 + Esc 取消 + 倒计时中再按 F9 取消 + 0 秒直启路径 + 热键停止后主窗口回前台**。
下一步是 B4（收尾流程）。

---

## 1. 仓库与环境

| 项 | 值 |
|---|---|
| 仓库路径 | `<workspace>/20-方向/短视频/tools/TakeKit`（**含中文**，见 §8 第 10 条） |
| 平台 | 仅 Windows（大量 Win32 调用），rustc edition 2021 工作区 |
| 三个 crate | `takekit-core`（采集/编码/会话）、`takekit-cli`（bin 名 **`takekit`**）、`takekit-gui`（Tauri 2 面板） |
| 依赖 | ffmpeg 需在 PATH；GUI 用 Tauri 2 + WebView2；`mouseshow` 是 git submodule（以库形式链接，提供鼠标点击波纹） |
| 屏幕（本机） | 3440×1440，100% 缩放，单显示器，任务栏 y=1392–1440 |
| 数据目录 | `%APPDATA%\TakeKit\config.json`、`%APPDATA%\TakeKit\journal.json` |
| 默认输出 | `%TEMP%\<YYYY-MM-DD-HH-MM-SS>\`（每次录制一个目录，含 `screen.mkv` / `win.mkv` / `take.json` / `win.log`） |

面板窗口固定 **900×640**（客户区），不可缩放、不可最大化。

---

## 2. 第一件事：确认状态，不要直接 commit

```powershell
git status --short
```

当前快照（2026-10-19，B3 完成后）：

```
 M .gitignore / Cargo.lock / Cargo.toml / README.md
 M crates/takekit-cli/src/main.rs
 M crates/takekit-core/{Cargo.toml, src/{encoder,ffmpeg,lib,source,take}.rs}
 M crates/takekit-gui/{src/*.rs, ui/*, tauri.conf.json}
 m mouseshow                      ← 子模块改动，**不是本轮工作**，别顺手提交
?? DESIGN.md
?? crates/takekit-core/src/session.rs
?? crates/takekit-gui/            ← 整个 GUI crate 都是新加的
?? docs/                          ← 提交前先按下面的红线检查脱敏
```

**提交前红线**：`docs/` 里有本机路径样例，`.gitignore` 已挡 `docs/tools/*.ps1`；
提交前跑一遍 `git status --short` 并确认没有 `C:\Users\` / 用户名 / 密钥值。

**用户的硬规则**：

- **不要自动 commit / push**。只有用户明确说"提交"才提交。
- 沟通用中文；**代码、注释、标识符用英文**；`docs/` 下的文档用中文。
- 新行为走 **TDD**（先写失败测试再实现）；**clippy `-D warnings` 必须干净**。
- 每次实质工作后，往 `.workbuddy/memory/YYYY-MM-DD.md` **追加**一条日志（append-only，不回改）。

---

## 3. 构建 / 测试 / 静态检查

```powershell
# 关掉正在跑的实例再构建（会锁 exe）
Get-Process takekit-gui -ErrorAction SilentlyContinue | Stop-Process -Force

cargo build --release -p takekit -p takekit-gui     # 约 2 分钟
cargo test --workspace                              # 见下方基线
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check                                   # 允许有既有 diff，见下
```

**当前基线（2026-10-19，B3 完成后）**

- `takekit-core`: 33 passed, 4 ignored（4 个 `#[ignore]` 是实机测试，`cargo test -p takekit-core -- --ignored` 才会跑，会开真窗口）
- `takekit-cli`: 7 passed
- `takekit-gui`: 60 passed
- clippy `-D warnings`：干净
- `cargo fmt --check`：**全仓干净（0 处 diff）**。上一版这里记着 11 处"既有 diff"白名单，
  B3 收尾时统一跑了一次 `cargo fmt --all` 把它清零了：以后改完直接 `cargo fmt --all`，
  不需要再按行号维护白名单。

> edition 是 **2024**（根 `Cargo.toml`），不是 2021；单独格式化某个文件用
> `rustfmt --edition 2024 <file>`。

**隔离运行（不碰用户真实配置）**

```powershell
$root = "$env:TEMP\tk_run"
$env:APPDATA = $root                       # config.json / journal.json 都落这里
$env:TAKEKIT_PROBE_LOG = "$root\probe.log"
$env:WEBVIEW2_USER_DATA_FOLDER = "$root\wv2"   # 缓解①：沙箱阻断默认 UDF 写入
$env:TAKEKIT_WEBVIEW_NO_SANDBOX = "1"          # 缓解②：注入 DLL 打崩渲染进程
Start-Process -FilePath "target\release\takekit-gui.exe" -WorkingDirectory (Get-Location)
```

两条铁律（2026-10-19 实测，详见 `docs/webview2-agent-sandbox.md`）：

1. **启动与交互必须在同一个工具调用内完成**——命令结束后沙箱会延迟回收它启动的进程树，跨调用驱动会反复撞上"进程已死"。
2. **每次启动前清场**（`Get-Process takekit-gui | Stop-Process -Force`）——残留实例占着 F9，新实例 `hotkey:unavailable`，之后所有 F9 都打到旧实例上，探针毫无痕迹。启动后先确认探针里有 `hotkey:ready F9` 再按 F9。

抓屏：倒计时期间用 .NET 中心裁剪（全屏编码 ~4s 会错过整个倒计时；ffmpeg gdigrab 在遮罩升起时会挂住）。

---

## 4. 架构地图

```
crates/
├─ takekit-core/          ← 所有"决定"都在这里，GUI 只是壳
│   ├─ session.rs         Session::prepare → Prepared → start → Recording
│   ├─ take.rs            Take::create / 目录与 take.json
│   ├─ source.rs          Capture{Desktop,Window,Camera}、list_windows、is_recordable、
│   │                     bring_to_front、virtual_screen_rect、偶数尺寸 crop
│   ├─ encoder.rs         NVENC → QSV → AMF → x264，每个都用一帧试编验证
│   └─ ffmpeg.rs          locate / spawn / 探针 / list_devices
├─ takekit-cli/           ← 薄壳，bin 名 `takekit`
└─ takekit-gui/
    ├─ main.rs            tauri 命令 + setup（窗口/托盘/提示层/热键）
    ├─ take.rs            take 生命周期：begin / status / stop / stop_source / 热键
    ├─ config.rs          %APPDATA%\TakeKit\config.json（设置）
    ├─ journal.rs         %APPDATA%\TakeKit\journal.json（历史 + 日志）★ 本轮新增
    ├─ place.rs           窗口位置记忆（-32000 哨兵在这里处理）
    ├─ reveal.rs          打开 take 目录（is_dir 先判）★ 本轮新增
    ├─ overlay.rs         全屏提示层窗口（B2 骨架，B3 才真正用）
    ├─ tray.rs            托盘图标（灰点/红点）
    ├─ tray_panel.rs      托盘迷你面板
    ├─ shortcut.rs        热键字符串规范化
    ├─ proto.rs           IPC 类型（snake_case，Tauri 不重命名）
    └─ ui/
        ├─ index.html     面板：三页签（源/历史/日志）+ 设置浮层（语言/主题）
        ├─ shared.js      `invoke` / `PROBE` / `$` / `clock` / `applyTheme`（共用，先加载）
        ├─ i18n.js        中英文字典 + `t()` + `applyI18n()` + `uiErrorText()`（面板与托盘共用）
        ├─ beep.js        Web Audio 合成音（面板与提示层共用）
        ├─ main.js        全部面板逻辑
        ├─ style.css      `--tk-*` tokens，浅色/深色双主题
        ├─ overlay.{html,css,js}
        └─ tray.{html,css,js}
```

**一次录制的数据流**

```
按钮/F9 → main.js toggleRecording()
  → pruneSources()（不可用的源先剔掉）
  → invoke("start_recording") → take::begin()
       bring_to_front(每个窗口源) → Session::prepare（最多 2 秒探针）
       ├─ countdown_secs == 0：藏主窗口 → 50ms → spawn（返回 started=true）
       └─ countdown_secs > 0：存 pending → 藏主窗口 → overlay:countdown
            遮罩自己跑 3/2/1/GO（rAF + performance.now，每个数字 beep）
            GO 结束（+1.0s）→ invoke("overlay_finish")
              → 收遮罩 → 50ms 落帧 → take::spawn()（主窗口录制期间保持隐藏）
              → prepared.start() → AppState.recording → emit take:started
              → 失败：遮罩已收、主窗口回来、last_error、emit take:failed
            Esc（Rust 每 30ms 轮询 GetAsyncKeyState）或倒计时中再按 F9 → take::cancel()
       → 录制中：主窗口隐藏，托盘红点 + 托盘面板提供计时与停止
  → 250ms 轮询 invoke("status")：计时、逐源字节、dropUnusable()
  → 停止（按钮 / F9 / 托盘面板）→ take::stop(announce)
       recording.stop() → StopResult{dir,duration,sources[],failed}
       → record(&result) 写 journal.json  ★ 历史在这里落库，不靠前端
       → show_main_window()（热键停止时被录窗口在前台，面板必须回来）
       → 只有托盘调用时 emit_to("main","take:stopped")；面板自己调用则用返回值
  → main.js showSaved()：failed>0 才弹浮窗；随后 refreshJournal() 刷新页签与绿点
```

---

## 5. 不可随手改的契约

这些是踩过坑之后定下来的，改动前先看 `DESIGN.md` 对应小节。

1. **窗口的身份是 hwnd，不是标题**。`Capture::Window{hwnd,title,crop}`，`title` 只用于显示和兜底。重名窗口（"Trae CN" 有两个）曾导致"按 F9 只闪一下"。
2. **位置记忆**：物理像素、建窗时定位（不是先摆默认位再移）、拖动 400ms 限流。
   `-32000` 是 Windows 的最小化哨兵：**两轴都是 -32000 才判否**（只有一轴可能是"显示器在主屏左边"），写入侧 `save()` 拦、读取侧 `restore()` 也过滤（让已污染的配置自愈）。
3. **奇数尺寸**：`source.rs::filter_chain()` 在 `-vf` 链**末尾**加 `crop=trunc(iw/2)*2:trunc(ih/2)*2`。用 `crop` 不用 `scale`（不重采样，偶数尺寸下是空操作），放链尾才能覆盖任何来源。
4. **源可用性只看 hwnd**：`is_recordable` = 存在 + 可见 + 非 cloak；**最小化不算不可用**（`bring_to_front` 会恢复它）。picker 另用 `is_switchable`（任务栏规则：非 tool / 非 cloak / 无 owner 或 `WS_EX_APPWINDOW`）。
5. **热键只是"告诉主窗口去按按钮"**：`emit_to("main","hotkey:toggle")`，主窗口 JS 调自己的 `toggleRecording()`。不要在热键回调里直接 `begin`（首次按下时还没有参数）。
6. **历史由 Rust 写**（`take::stop` 里），不是前端写——从托盘面板停的那次也要记；**收尾自己失败时也记一条**（`failed:1`，`duration:None`），否则那个 take 目录没有任何入口。
7. **分层**：提示层 `overlay` 是独立窗口（整屏、`WS_EX_TOOLWINDOW`）；面板里的浮窗是 `z-index: 50`，压过 picker（`10`）与所有内容，**并且不参与布局**——它盖住内容是有意的（用户明确要求）。
8. **成功的"已保存"不弹任何东西**（历史承担反馈），**只有警告/错误**弹浮窗；日志只记警告/错误。

---

## 6. 数据文件

### `config.json`（设置，`config.rs`）

`hotkey`（默认 `F9`）、`countdown_secs`（默认 3，B3 才用）、`finalize_panel`（B4 才用）、
`sources[]`（id / capture / fps / quality）、`out_root`、`encoder`、`marks`、`window_pos{x,y}`。

- 缺字段用默认值；坏 JSON / BOM 回退默认（**不清空文件**）。
- 所有写入走加锁的"读-改-写"，内容没变**不落盘**。
- `save_session` 只接受会话字段（sources/out_root/encoder/marks），避免前端把自己不拥有的 hotkey 回写覆盖。

### `journal.json`（历史 + 日志，`journal.rs`）

```json
{
  "takes": [ { "dir": "...", "at": 1792401823424, "seconds": 4.166, "bytes": 273292, "failed": 0 } ],
  "log":   [ { "at": 1792401294463, "level": "error", "text": "一句结论", "detail": "技术细节", "dir": "..." } ],
  "seen_takes": 0,
  "seen_log": 0
}
```

- `takes` 最多 **20** 条、`log` 最多 **50** 条，新记录插队首（不排序，靠插入顺序）。
- **原子写**：写 `<name>.tmp` 再 rename 覆盖（staging 由 `takekit_core::fsx::write_atomic` 统一；坏文件先备份 `*.corrupt`）。
- 坏文件读成空 journal，不报错。
- **未读语义**：`seen_*` 记的是**该页签最新一条的 `at`**，不是墙钟。重复访问零写入；同一毫秒落地的记录不会被误判成已读。
- `unread_log_level` 取未读里最严重的级别 → 日志页签点着色（warn 橙 / error 红）。

---

## 7. 验证手法（最省时间的一节）

### 7.1 探针日志 `TAKEKIT_PROBE_LOG`

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

B3 倒计时（按时间顺序）：
overlay:wake（Rust→遮罩，唤醒音频，无回写探针）
prepare: [("locate",ms),("encoder",ms),("engine",ms)]   ← 点击路径各段耗时
engine:Dda|Gdi probed=true|false                        ← false = 本支用 GDI，探测在后台
spawn: [("id",ms),…]                                    ← 逐源 spawn 耗时
settings-applied lang=zh|en theme=auto|light|dark hotkey=<显示值>   ← 页面实际应用的值
settings-changed lang=… theme=…                                      ← 每次切换（命令与事件都走）
countdown:begin secs=N | countdown:off
overlay:countdown:N / overlay:digit:N t=秒 audio=running|suspended / overlay:go
overlay:finish-invoked → spawn:ok | spawn:failed <msg>   （遮罩已收，间隔 ≈50ms）
overlay:stop / overlay:dismiss
overlay:cancel-invoked → cancelled | cancel:nothing
begin:failed <msg>（prepare 失败）/ stop:failed <msg>（收尾失败）/ overlay:show-failed <e>
ui:tick-first（每次 take 首次成功轮询）/ ui:show-main / main:close-hidden
js-error:<msg>
```

**事件 vs 探针**：`take:started`、`take:stopped`、`take:cancelled`、`take:failed`、
`take:beep`、`overlay:countdown`、`overlay:wake`、`hotkey:toggle`、`ui:settings` 是 **Tauri 事件**
（`emit_to`，前端用 `listen` 接），不会出现在探针日志里；它们的前端处理才写 `PROBE`。

**`js-error:` 是最有用的**：`index.html` 里挂了 `window.onerror`，JS 一抛就写进探针——不用猜"为什么界面不动"。

### 7.2 让面板"按 F9"（不要用 keybd_event）

`tauri-plugin-global-shortcut` 建隐藏窗口（class `global_hotkey_app`）+ `RegisterHotKey`，
F9 的 `id = 168`。所以：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File docs\tools\ui-probe.ps1 -Action hotkey
```

`docs/tools/ui-probe.ps1` 还支持 `-Action shot|click|focus`（`click` 用 client 坐标，`shot` 会先把窗口拉到前台）。
**`keybd_event` 注入的按键在本机无法触发 `RegisterHotKey`**（已用自己注册的 F9 独立对照过），别浪费时间。

### 7.3 点面板里的按钮

- 合成鼠标点击会被**前台窗口**抢走：先 `-Action focus`，或干脆用 `-Action hotkey` 走热键路径。
- ⚠️ **UIA 当前不可用（2026-10-19 回归）**：`FromHandle + Descendants` 只拿到 16 个空 Pane，WebView2 内容树不发布（聚焦 + 真实点击后依旧）。以前会话能拿到 40 个元素并 `InvokePattern` 点按钮——若你要走这条路，先重新验证，不行就退回坐标点击（估坐标会偏 50px，宁可先用 `-Action shot` 拿窗口矩形再算）。
- 别对着截图目测坐标：估偏 50px 会点到面板外，现象是"点了没反应"。

### 7.4 判据用像素/JSON，不要"看截图说没问题"

- 录到的画面：抽帧比对**具体像素值**（自建纯色窗口）。
- 历史是否落库：直接读 `journal.json`，比看界面可靠。

---

## 8. 陷阱清单（都是真踩过的）

1. **`additional_browser_args` 是整体替换，不是追加**。它会顶掉 wry 默认的
   `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` → WebView2 创建失败
   `0x8007139F` → 整个 app 起不来。B3 需要的 autoplay，wry 默认就在传，**什么都别加**。
2. **`ui/` 是编译期嵌入的**：改了 `ui/*.js|css|html` 必须 `cargo build --release` 才生效。
   改了行为却验证旧二进制 = 假阴性（踩过）。
3. **`config.json` 带 UTF-8 BOM 会静默回退默认值**。PowerShell `Set-Content -Encoding UTF8` 会写 BOM；
   用 `[IO.File]::WriteAllText($p, $s, (New-Object System.Text.UTF8Encoding $false))`。
4. **`IsWindowVisible` 对最小化窗口返回 `true`**。判"在不在用户眼前"要用 `IsIconic` 或坐标 `-32000`。
5. **Windows 11 静默忽略后台进程的 `SetWindowPos(HWND_TOP)`**（返回 TRUE、Z 序不动）。
   要抬窗口必须 `HWND_TOPMOST` → `HWND_NOTOPMOST` 两步，带 `SWP_NOACTIVATE`。
   恢复最小化窗口**必然**激活它（系统语义，不是 bug）。
6. **`keybd_event` 触发不了 `RegisterHotKey`**（本机）。用 `PostMessage(WM_HOTKEY)`。
7. **P/Invoke 的 `StringBuilder` 必须 `CharSet = CharSet.Unicode`**，否则 `GetClassNameW` 只读到首字符（"Tauri Window" → "T"）。
8. **`Add-Type` 里的方法不能叫 `Main`**（撞 C# 入口点，warning-as-error 直接失败）。
9. **`explorer.exe` 的退出码没有意义**：spawn 出去就不管，别 `wait()` 判成功。
10. **仓库路径含中文**：`powershell -File <中文路径>` 会按 ANSI 读坏路径。脚本放在仓库内、从仓库根目录调用；
    或者用 `-Command` 传。
11. **每次 `shell` 调用都是新的 PowerShell 会话**：`Add-Type` 不跨调用，脚本要么一个文件跑完，要么每次重新 `Add-Type`。
12. **`Start-Process cargo ... -Wait` 读不到 EOF** 时用 `-RedirectStandardOutput <file>`，再读文件。
13. **`$host` / `$pid` 是 PowerShell 自动变量**，不要拿来当自己的变量名。
14. **沙箱命令结束后会回收它启动的进程树**：GUI 交互必须在单个工具调用内完成；跨调用驱动前先 `Get-Process takekit-gui` 探活。
15. **每次起实例前清场**：残留实例占着 F9（新实例 `hotkey:unavailable`），之后所有 F9 都打到旧实例上，探针毫无痕迹——表现为"按了没反应"。
16. **PowerShell 5.1 按 ANSI 读无 BOM 的 .ps1**：脚本里不要出现中文（`powershell -File` 中文路径同坑，见第 10 条）。
17. **`Start-Process -RedirectStandardOutput/Error` 在本沙箱必失败**：注入的 `HTTPS_PROXY`+`https_proxy` 双大小写键撞 PowerShell 已知缺陷。要捕获输出就让命令写文件再读。
18. **gdigrab 在遮罩升起时会挂住**；倒计时期间的截图用 .NET 中心裁剪（全屏编码 ~4s 会错过整个 4s 倒计时）。

---

## 9. 已完成

| 编号 | 内容 | 备注 |
|---|---|---|
| A1–A4 | 配置持久化、热键规范化、take 生命周期抽出、托盘图标 | 修掉"双注册导致热键完全不可用" |
| A5 | 托盘迷你面板 | 定位用 UIA `BoundingRectangle`，别目测 |
| A6 | 窗口身份改 **hwnd** | 计划外：重名窗口导致"F9 只闪一下" |
| A7 | 本地记忆（会话 + 窗口位置） | 建窗时定位，消灭闪跳 |
| A8 | 固定 900×640 + 源列表滚动 | `resizable(false)` 在 Win 上不去掉最大化按钮，需 `maximizable(false)` |
| A9 | picker 标「已添加」并置灰 | |
| A10 | picker 只列"任务栏会给按钮"的窗口 | 判据取自 UIA 读到的任务栏真实按钮 |
| B1 | 窗口源置前 | topmost 两步法；抬窗口必须早于 `prepare` |
| B2 | 全屏提示层骨架 | 整屏覆盖、隐藏后不参与命中测试 |
| 修 | GDI 回退从来没工作（`hwdownload` 对 gdigrab 不成立 → 0 字节） | `download_filter` 按引擎给 |
| 修 | 位置被最小化哨兵污染（`-32000`）→ 每次启动挤左上角 + 双实例重叠 | 写入/读取两侧都拦 |
| 修 | 奇数尺寸窗口录出 0 字节 | `crop` 放 `-vf` 链尾 |
| **C1** | **录制历史 · 日志页签 · 浮窗提示** | 见下 |
| **B3** | **倒计时 · 音频 · spawn 编排 · Esc 取消 · 倒计时开关 · 0 秒路径 · 热键停止回前台** | 见下 |

**C1 要点**（用户插入的需求，2026-10-19）

- 三页签 `源 · 历史 · 日志`；历史整行可点 → `open_take` 开资源管理器；按今天/昨天/更早分组，留 20 条。
- 「新」徽章 + 绿框标最新一条；页签绿点表示未读；**不自动切页签**。
- 浮窗固定右上、`z-index 50`、5 秒自动收起或 `×`、最多 3 行、同时只显示一条。
- 日志只收警告/错误，可复制、可「打开目录」。
- 单测：`journal.rs` 14 个 + `reveal.rs` 4 个 + `take.rs::record_to` 1 个。

**B3 要点**（2026-10-19 完成并实机验证）

- `begin` 拆成 prepare（存 `AppState.pending`）+ `spawn`（`overlay_finish` 命令触发）；`cancel` 丢弃 pending。三态由 `pending` / `last_request` / `last_error` 表达，`StatusInfo.error` 供 B5 的 failed 态。
- 事件驱动：`overlay:countdown{secs}` → 遮罩 `performance.now()`+rAF 单时钟（**t0 取第一帧**，否则首帧延迟会吃掉第一个数字的时间）→ GO 时 `beep(880,1000)`，`+1.0s`（GO 结束）invoke `overlay_finish`（收遮罩 → 50ms → spawn）。
- **Esc 取消不走 JS keydown**：遮罩 `focused(false)` 永远收不到按键（按键属于前台窗口）。改由 Rust 侧 30ms 轮询 `GetAsyncKeyState`（`source::escape_pressed`，begin 起线程、spawn/cancel 停）。`keybd_event` 注入的 Esc **能**被该轮询看到（与 RegisterHotKey 不同）。
- **音频时钟**：`AudioContext` 在页面加载时创建、`begin()` 探针前发 `overlay:wake` 唤醒——否则第一个 beep 要现场建上下文（异步），**听觉上第一个间隔会短**（用户正是按声音计时的）。探针带 `audio=running` 可验证。
- **取消必须显式停表**：Rust 取消时发 `overlay:stop`——只隐藏窗口不够，Chromium 对刚隐藏的窗口仍供帧一两秒，2/1/GO 的提示音会照常播完。
- **倒计时开关**：`set_countdown(secs)` 命令 + 参数区"鼠标效果"右边的 switch（on=3/off=0，会话内记住上一个非零值）。`secs==0` 走 DESIGN §4.6 直启路径（无遮罩、50ms settle、直接 spawn、主窗口发声）。
- **热键停止后主窗口回前台**（`take::stop` 末尾 `show_main_window`，用户 2026-10-19 要求）。
- **WebView2 参数只能走环境变量**：`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 进程级统一供给；per-window `additional_browser_args` 会造成"同 UDF 多环境参数不一致"→ 0x8007139F → 主窗口建失败（B2 那个错误的真正根因）。全文见 `docs/webview2-agent-sandbox.md`。
- 单测：`status_of`（error 映射）×2 + `AppState` 默认态 ×1 + proto 序列化 ×2 = gui 57 passed。
- 实机验证（`docs/tools/tk-verify-b3.ps1`，隔离 APPDATA）：`digit:3/2/1 t=0.000/1.000/2.001 audio=running`；done→spawn 11–17ms；take 4.666s / 650KB / failed:0；Esc 与热键取消均无残留 take、无余音；进程 240s 稳定。

---

## 10. 未完成

### B4 收尾流程（下一个任务）

计划见 `docs/overlay-tray-plan.md` **Task B4**（含接口与 6 个 Step）。要点：

- `stop_next()` 逐源推进 + `take:finalize|source|done` 事件 + 提示层清单。
- ⚠️ **不要再补一条"已保存"横幅**——C1 已经把成功反馈交给历史行了。
- B3 已顺手做掉：**热键停止后主窗口回前台**（`take::stop` 末尾 `show_main_window`），以及 **0 秒倒计时路径**（`begin` 里 `secs == 0` 分支：不显示遮罩、50ms  settle、直接 spawn、`take:beep` 事件让主窗口发声）。B6 因此只剩"收尾面板关闭"后半段。

### 之后

- **B5 已完成**（2026-10-19）：热键 chip（`set_hotkey`，录入态 + 失败回原值，DESIGN §4.12）、
  语言切换（`set_language` + `i18n.js`，两页签与托盘同步）、主题切换（`set_theme` + `data-theme`）。
  剩下的"状态条 failed 态"仍未做——`StatusInfo.error` 与 `last_error` 已经就位，只差 UI。
- **B6**：`finalize_panel == false` 的停止路径。
- **B7**：文档与台账回填。

### 用户已排队的功能（2026-10-19 用户定）

1. ~~运行中修改录制快捷键~~ → **已完成**（DESIGN §4.12，含录入态与失败回退）。
2. ~~语言切换、主题切换~~ → **已完成**（效果图 `docs/ui/settings-mockup.html`，实现见 §4.12）。
3. **循环综合 review**：对本工程全部代码（含 submodule）做循环 review（性能/复用性/架构/质量），每轮全新独立检测直到没有新问题；该流程要沉淀成一个 skill，范围可描述（整个工程 / 近几个 commit / 未提交文件 / 指定文件）。

### backlog（`.superpowers/sdd/overlay-tray-plan/backlog.md`）里等用户拍板的

1. ~~Trae 64×100 小组件~~ → 用户已决定**保持现状**。
2. 未实测项回填（150% 缩放、托盘 Z 序、深色对比度）。
3. 面板小瑕疵：托盘逐源清单显示 `Source.id` 而非显示名；`Esc` 后 `tray:hidden` 重复 2–3 次（纯探针噪音）。
4. ~~置前失败要不要拦~~ → 已改为"从源头剔掉不可用的窗口"。
5. ~~奇数尺寸~~ → 已修。
6. B 阶段主线（见上）。
7. CLI `--marks` 是否与 UI「鼠标效果」文案对齐（待定）。
8. ffmpeg 路径可配置（待定，排在提示层之后）。
9. **WGC 采集后端**（录"被遮挡但没最小化"的窗口）：需要 Rust 侧 Windows Graphics Capture → 帧喂 ffmpeg stdin，ffmpeg 自身做不到。工作量中等，**待用户决定是否排期**。
10. **单实例保护**（重复启动两个进程 / 僵尸图标）：**待用户拍板**。
11. 教训：`additional_browser_args` 陷阱。
12. **提示层创建失败会拖垮整个 app**（`overlay::create(...)?`）→ 应降级为"无提示层 + 记探针"，**待议**。
13. 历史/日志的「清空」只清列表，不动磁盘上的 take（是否要连文件一起删是另一个决定）。
14. **C1 新 UI 只在浅色 + 100% + 单显示器下验证过**。
15. 历史行双击会开两个资源管理器窗口（未节流）。

---

## 11. 已知未测 / 已知缺陷

- **主窗口在录制期间保持隐藏**（B3 已做：`begin` 藏面板，`overlay_finish` 只收遮罩，
  停止时 `take::stop` 再请回来）。面板隐藏期间用托盘面板或 F9 停止。
- 深色主题下 C1 的浮窗/徽章观感未验证（只在浅色跑过）。
- 150% 缩放、多显示器未验证（本机 100% / 单屏）。
- `Trae CN` 那个 64×100 的置顶小窗仍会出现在 picker 里（用户已决定接受）。
- 4 个 `#[ignore]` 实机测试会开真窗口，别在用户正用机器时随便跑。

---

## 12. 文档与台账在哪（唯一出处，别建副本）

| 内容 | 位置 |
|---|---|
| 设计契约（视觉/组件/记忆/历史日志） | `DESIGN.md`（**§4.10 记忆**、**§4.11 历史·日志·浮窗** 是最新的） |
| B 阶段实施计划（B1–B7，含接口与验证步骤） | `docs/overlay-tray-plan.md` |
| 进度与决策记录 | `.superpowers/sdd/overlay-tray-plan/progress.md` |
| 待办队列 + 规则 | `.superpowers/sdd/overlay-tray-plan/backlog.md` |
| 每日工作日志（append-only） | `<workspace>/.workbuddy/memory/2026-10-19.md` |
| 磁盘性能分析（历史） | `docs/perf-review.md` |
| 音频同步待办（历史） | `docs/sync-audio-backlog.md` |
| **沙箱与 WebView2（必读，含启动配方）** | `docs/webview2-agent-sandbox.md` |
| **API 录制试探（分支 `feat/api-recording`）** | `docs/api-recording.md`（WGC + Media Foundation，含"静止画面没有帧"这条硬约束） |
| C1 的设计稿 | `docs/ui/history-mockup.html`（三方案对比）、`docs/ui/history-c.html`（定稿 100%） |
| 面板自动化脚本 | `docs/tools/ui-probe.ps1`（**本地 scratch，默认不入库**） |
| B3 端到端验证脚本 | `docs/tools/tk-verify-b3.ps1`（**本地 scratch，默认不入库**；自包含：清场+启动+F9/Esc/开关全流程） |

**维护规则**（用户定的，来自仓库根的 `README.md` 与 `AGENTS.md`）：
每次实质工作后往当天的 memory 文件**追加**一条日志，不回改历史；不自动 commit & push。

---

## 13. 建议的第一天上手顺序

1. 读 `DESIGN.md` §4.10、§4.11（十分钟，能省掉一半返工）。
2. 读 `.superpowers/sdd/overlay-tray-plan/progress.md` **最后两条**（B2、C1），那里有关键决策与踩坑。
3. 跑一遍：`cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`，确认基线（core 27 + cli 6 + gui 57）。
4. 用**隔离 APPDATA**（§3）起一次 release，点三个页签，确认浮窗、历史、日志都在。
5. 接 **B4 收尾流程**（`stop_next()` 逐源推进 + `take:finalize|source|done` 事件）；先读 `progress.md` 最后一条（B3）里"实测推翻计划"的三段。
6. 每完成一个 Step 就更新 `progress.md` + 当天 memory，**不要攒到最后**。
