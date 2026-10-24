# TakeKit

**Record several sources at once into one take.**

[中文说明见下方](#中文)

> ⚠️ Early. The CLI records windows, the desktop and capture devices into a single
> timestamped directory. A desktop panel (Tauri) lives in `crates/takekit-gui`:
> it lists what can be recorded, remembers the last setup, and keeps a history of
> finished takes.

## Why

Screen recordings are usually assembled by hand: grab the screen with one tool,
the camera with another, then line them up in an editor. TakeKit treats a
recording the way a film crew treats a take — every source starts and stops
together, so the files already line up.

Each source gets **its own encoder**: one ffmpeg child per source on the
ffmpeg backend, and one in-process writer thread per source on the default
Windows-API backend (no child processes at all). That is not a shortcut, it is
the point: a single ffmpeg fed several inputs picks the input to read next by
timestamp, which starves the desktop grabber down to about half the requested
frame rate. Separate encoders keep every source at its own rate.

## What it does today

- Sources: a **window** (by window handle — `--window "#12345"` — or by a unique
  title), the **whole desktop**, and **DirectShow capture devices** (webcams,
  phones acting as webcams).
- One timestamped directory per take, with a `take.json` manifest recording the
  encoder, and for every source **where its file starts on the take's timeline
  and how long it is** — so a source added halfway through is placed by the
  manifest, not by guesswork.
- **A take can grow while it records**: add a window, screen, camera or sound
  source mid-take and it joins immediately. Its own file begins at its first
  frame, and that instant goes into the manifest.
- **Hardware encoding when it works**: NVENC, then QSV, then AMF, falling back to
  libx264. Each candidate is probed with a one-frame encode, so a machine that
  reports an encoder but cannot actually use it still gets a working run.
- Reports the duration delta between sources at the end of every take.

## Usage

```bash
# what can I record?
takekit list
takekit list "VirtualBox"      # filter windows by title

# one window, plus a camera, 60 seconds
takekit record --window "VirtualBox" --cam "My Camera" --duration 60

# the whole desktop, cropped 33px from the top, until you press Enter
takekit record --desktop --crop 33,0,0,0
```

`takes/<timestamp>/` then holds `win.mkv`, `cam.mkv` and `take.json`.

Options worth knowing:

| Flag | Meaning |
|---|---|
| `--encoder auto\|x264\|nvenc\|qsv\|amf` | default `auto`, picks the best that actually works |
| `--quality 20` | roughly CRF for x264, CQ for hardware encoders |
| `--fps 30` | capture rate |
| `--crop T,B,L,R` | crop inset in pixels, applied per source |

## Design notes

- Windows only for now.
- ffmpeg must be on `PATH`.
- Mouse effects (`marks` — click ripples and drag trails) come from
  [MouseShow](https://github.com/Booome/MouseShow),
  which is linked as a library rather than launched as a separate process.

## License

MIT. See [LICENSE](LICENSE).

---

## 中文

**一次开机，多路同时录，出来的文件天然对齐。**

录屏通常是拼出来的：一个工具录屏幕、另一个录摄像头，再到剪辑软件里对齐。TakeKit 把一次录制当成电影里的「一条 take」——所有源同时开始、同时结束，文件本来就是对好的。

**每个源一个独立编码器**，这不是偷懒而是必需：ffmpeg 后端下是每个源一个 ffmpeg 子进程，默认的 Windows API 后端则是每个源一个进程内 writer 线程（不 spawn 任何子进程）。单个 ffmpeg 喂多路输入时会按时间戳挑下一个该读谁，抓屏那一路会被饿到只剩一半帧率。编码器分开才能让每路都跑满自己的帧率。

现在能做的：

- 源：**窗口**（按窗口句柄 `--window "#12345"`，或按唯一标题）、**整个桌面**、**DirectShow 采集设备**（摄像头、当摄像头用的手机）
- 桌面面板（Tauri，`crates/takekit-gui`）：列可录的源、记住上次的设置、保存录制历史（点一条直接进目录）
- 每次录制一个时间戳目录，带 `take.json`：编码器，以及**每个源的文件在时间轴上的起点和长度**
  ——所以录到一半加进来的源也是清单说了算，不靠猜
- **录到一半还能加源**：窗口、桌面、摄像头、声音都可以中途加入，立刻开录；新文件从它自己的
  第一帧起算，那个时刻写进清单
- **有硬件就用**：NVENC → QSV → AMF → libx264，每个都用一帧试编来验证，避免「驱动说有、实际用不了」
- 录完报告各源之间的时长差

```bash
takekit list
takekit record --window "VirtualBox" --cam "My Camera" --duration 60
takekit record --desktop --crop 33,0,0,0
```

仅支持 Windows；ffmpeg 需在 PATH 里。点击波纹/拖动轨迹由 [MouseShow](https://github.com/Booome/MouseShow) 提供，以库的形式链接。

## 文档

| 内容 | 位置 |
|---|---|
| 设计契约（视觉 / 组件 / 记忆 / 历史日志 / 键盘） | `DESIGN.md` |
| 开发笔记（构建测试、架构地图、实现契约、探针与验证、陷阱清单） | `docs/development.md` |
| WebView2 与 agent 沙箱（启动配方，唯一出处） | `docs/webview2-agent-sandbox.md` |
| API 采集后端（WGC + Media Foundation，已默认） | `docs/api-recording.md` |
| 三端一致性与语言×主题检查脚本 | `scripts/` |

## 改完一个功能必须过什么

一个功能只有 **CLI / GUI / MCP 三端都有入口、都真跑过**才算完成；界面还要在
**每种语言 × 每种主题**下不溢出、不糊。

```bash
cargo build --workspace          # 界面资源编译期内嵌，改 JS/HTML/CSS 必须重建
cargo test --workspace
node scripts/check-three-surface.mjs   # CLI 参数 + MCP schema/真实调用 → 面板 → 配置
node scripts/check-ui-matrix.mjs       # zh/en × light/dark × 7 屏，截屏落 target/checks/
```

| 检查 | 抓什么 | 判据 |
|---|---|---|
| `check-three-surface` | MCP schema 声明了字段却在转发前丢掉、CLI 缺参数、三端字段名各说各话 | 14/14 |
| `check-ui-matrix` | 页面横向滚动、文字溢出压到邻居、对比度低于 WCAG AA、**尺寸列说假话**、**行里凭空多出的空隙**、**标题不跟随窗口**、**没画面也能裁**、**喇叭出现在不能出声的行上**、**喇叭插在两个数字中间**、**加一条麦克风行就让整列数字横移** | 123/123，且 `low=0 spill=0 hScroll=0` |

`check-ui-matrix` 输出里 `trunc` 是省略号截断（设计意图，不算失败），`low` 是实测对比度
（背景沿 DOM 向上找到第一个不透明容器再算），`spill` 是没有滚动条却超出容器的文本。

**尺寸列的三处口径**（列表行 / 卡片角标 + 输出行 / 裁剪工作区）由 `row-crop-*`、
`card-dropped-*`、`workspace-*` 这组断言钉住：裁剪放不下时**三处都报整幅**，与
`CropBox::rect_in`（留不下 2 px 就整帧原样录）一致。只改一处，另两处就会开始印
`0 × 1392` 这种写不出来的尺寸。

**行内空隙**由 `row-summary-tight` 钉住：帧率/画质格**按文字自适应**，不预留列宽
（`slack ≤ 1px`），且它离尺寸列**只隔行本身的 12px**（`gap ≤ 16px`）。这一格曾是定宽
88px 装 58px 的字，30px 的死白就成了「1720×1392 ⋯空格⋯ 30fps」——不溢出、不截断、不出
滚动条，只有截图能看出来。

**右侧三列是一条信息线**（`row-speaker-last` + `row-trailing-columns-line-up`）：行从右
往左排，所以任何宽度可变的格子只要落在数字左边，就会让整列数字按行横移。喇叭因此挪到
**行尾、× 之前**（`row-speaker-last` 要求帧率格先结束、喇叭才开始），而**不能出声的行
留一个同宽的空槽**（`row-sound-slot`）——不是少一格。实测 `fps→785 / ×→835` 七行全等，
这是加上一条麦克风行之后仍然成立的，也是这组断言存在的理由（2026-10-23）。

**源行的几条规则**（2026-10-23）也在这里被钉住：

| 断言 | 它保证什么 |
|---|---|
| `row-title-follows-window` | 行上印的是窗口**现在**的标题，不是添加时的快照。脚本给一个源塞进假标题 `STALE …`，跑完必须查无此名，且真标题在场（只查前半条的话，一行涂白也能过） |
| `minimized-no-crop` / `minimized-restore-offered` / `minimized-crop-closed` | 窗口最小化时（脚本自建一个 WinForms 窗并最小化）缩略图必须 disabled、点它不开工作区、「恢复窗口」在页脚 |
| `restore-window/turns-croppable` | 按下「恢复窗口」后同一张卡片变成可裁：Rust `unminimize` → 重抓静帧 → 有图才放行，一次跑通 |
| `row-speaker-scope` | 喇叭只长在能出声的画面上：window 与 desktop 每行都有（开/关两态都在行上），mic 与 camera 一个都没有（那里是空槽，不是喇叭）。`row.dataset.kind` 就是为这条断言加的语义钩子，不靠匹配图标 markup |
| `row-speaker-last` | 喇叭是这条信息线的最后一格：帧率格的右缘必须早于喇叭的左缘。它曾经在名称和尺寸中间，把「名字 → 数字」的扫读线切断 |
| `row-trailing-columns-line-up` | 帧率格与 × 的横坐标在所有行上一致（≤1px）。混入一条麦克风行时最容易被破坏——而那种列表正是用户会有的 |
| `desktop-sound-says-whole-machine` | 桌面行的喇叭提示必须说清「整台电脑的声音」——Windows 没有分显示器的音频，不写这句，用户会以为是这块屏的声音 |
| `picker/window-list-carries-pid` | 选择器递上来的窗口带着 `pid`——pid 校验要是只在播种数据里存在，等于没有 |

两脚本都以 `target/checks/` 为隔离的 APPDATA，**跑之前先关掉正在运行的面板**——单实例
会让第二个进程直接退出，脚本拿不到控制端口。新增界面加进 `SCREENS`，新增语言/主题扩
`LANGS` / `THEMES`。

## 当前状态与待办

面板已可日常使用：源列表、设置记忆、F9 开/停、托盘、录制历史、日志页签、
倒计时 3/2/1/GO + 提示音 + Esc 取消、快捷键/语言/主题设置。录制后端默认走
**Windows API**（WGC + Media Foundation），窗口被遮挡也能录；ffmpeg 后端保留为回退。

**待办**（大致按优先级）：

1. **音频通道已补全**（2026-10-23）：麦克风 / 系统声音（WASAPI loopback）/ 单窗口
   声音均已实现并实测通过（`docs/api-recording.md` §8），GUI 选择器同步跟进；摄像头
   仍缺 MF 可见设备未验证。**窗口声音已改为窗口源的一个开关**（2026-10-23 晚）：一条源、
   一个文件、两条流（`docs/api-recording.md` §11.3），picker 里每个窗口只出现一次。
2. ~~多路漂移观测~~ ✅（2026-10-23 晚，`docs/api-recording.md` §11.1）：**没有可测的时钟漂移**
   （20s/180s/600s 差 ≤0.04s，量级就是 AAC 帧量化），不做 slew 校正，`drift_ppm` 字段也不加。
   顺带修了一个上报口径 bug（前导静音没算进 `seconds`，导致 60 秒录制凭空少 0.5s）。
   **遗留**：30 分钟那次多出 4.9 秒的音频是设备一次性重投造成的，会让之后的音频永久后移——
   修法（用 `chunk.time` 检测空洞/重投）见 §11.2，未做。
3. ~~待拍板 5 项~~ ✅（2026-10-23 全部拍板并落地：单实例二次启动聚焦已有窗口；
   提示层创建失败降级为"无提示层 + 记探针"；历史「清空」连磁盘文件一起删（删不动的行
   保留）；CLI `--marks` 与 UI「鼠标效果」文案对齐；ffmpeg 路径可配置（设置行 + CLI
   `--ffmpeg` + 控制面 `ffmpeg_path`）；另修历史行双击开两个资源管理器窗口的节流）。
4. 未实测：150% 缩放、多显示器双屏真机、深色主题下历史页浮窗/徽章观感。
5. ~~B4 收尾流程~~ ✅（2026-10-21：`take:finalize|source|done` 事件流 + 提示层逐源
   清单 + 状态条 `正在写入… n/m`，「已保存」横幅按口径不做；`finalize_panel` 开关已进设置）。
6. ~~状态条 failed 态~~ ✅（2026-10-21：菱形 + 原因单行 + 查看日志）。
7. ~~光标/多屏对齐~~ ✅（2026-10-21：`WithCursor` 实测光标进画；picker 逐屏 + rect→Monitor）。

**性能 review**（2026-10-23）：录制热路径已过一遍。帧通道原为无界 `mpsc::channel`，
与"队列满则丢帧"的注释矛盾——写入端落后时帧会在内存无限堆积（4K BGRA 帧约 33 MB）。
已改为有界 `sync_channel` + `offer()`（满则丢帧、writer 断开才停采集），视频队深 3、
音频队深 64（`takekit-media/src/frame.rs`）；KeepLatest 采样顺修积压时取最新帧而非
队头帧（`newest_of_burst`）。其余热点（250ms tick 链、`spare` 复用、`copy_frame`）已
是优化后形态，无改动。
