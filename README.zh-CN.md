# TakeKit

[English](README.md) | **简体中文**

一次开机、多路同时录：一个窗口或整个桌面，加上摄像头、麦克风和电脑正在播放的声音 —— 录完的文件天然对齐。

> 状态：**早期（0.1.0）**，仅 Windows，暂无安装包（从源码构建）。作者每天在用，但 CLI 参数仍可能变。

## 为什么

录屏通常是拼出来的：一个工具录屏幕、另一个录摄像头，再到剪辑软件里对齐。TakeKit 把一次录制当成电影里的「一条 take」—— 所有源同时开始、同时结束，文件本来就是对好的。

**每个源一个独立编码器**，这不是偷懒而是必需。ffmpeg 后端下是每个源一个 ffmpeg 子进程，Windows API 后端则是每个源一个进程内 writer 线程。单个 ffmpeg 喂多路输入时会按时间戳挑下一个该读谁，抓屏那一路会被饿到只剩一半帧率；编码器分开，每路才能跑满自己的帧率。

## 功能

- **源**：窗口（按句柄或标题）、整个桌面、DirectShow 采集设备、麦克风、系统声音（WASAPI 回环）、以及单个窗口自己的声音。
- **一次录制一个时钟。**所有源共用一个时间原点，跨源的时长差在一两帧以内 —— 后期不需要对齐这一步。
- **录到一半还能加源。**窗口、桌面、摄像头、声音都可以中途加入，立刻开录：新文件从它自己的第一帧起算，那个时刻写进清单，下游不用猜它在时间轴的哪儿。
- **暂停会把这段时间真的切掉。**停钟 → 各源收尾当前文件 → 恢复时在上一段结束的那一帧开新文件，边界正好接上。产出直接是一堆首尾相接的段（`chrome-part1.mp4`、`chrome-part2.mp4`…），不用事后找时间切。**被切开的源每一段都带编号；没被切开的源不带**（`chrome.mp4`）——文件名里出现编号就意味着它是一组中的一份。仅 Windows 后端。
- **两套采集后端。**
  - *Windows API*（WGC + Media Foundation），全进程内：窗口被别的窗口盖住也照样录。面板默认走这条。
  - *ffmpeg*：每个源一个子进程。CLI 默认走这条，作为兼容回退。
- **有硬件就真用**：NVENC → QSV → AMF → libx264，每个候选都先用一帧试编验证 —— 驱动说有、实际用不了的机器也能拿到能用的结果。
- **桌面面板**（Tauri 2 + WebView2）：源列表带静帧预览、逐源裁剪、带提示音的倒计时、托盘图标、录制历史、日志页签、全局热键、中英文 × 明暗主题。
- **鼠标效果**：点击波纹与拖动轨迹，来自 [MouseShow](https://github.com/Booome/MouseShow)，以库的形式链接，不另起进程。
- **Ctrl-C 是「停止并收尾」，不是杀进程。**MP4 正常收尾、清单照写，中断的 take 仍然是能用的 take。
- **给 agent 留了口**（不是所有人都想点鼠标）：面板内置回环 HTTP 控制服务，配 `takekit ctl` 命令行客户端，以及基于 stdio 的 MCP server。

## 环境要求

| | |
|---|---|
| 系统 | Windows 10（1809 或更新）或 Windows 11，x64。开发与测试均在 Windows 11 |
| 构建 | Rust 1.85+（edition 2024）、Visual Studio C++ 生成工具 |
| 面板 | WebView2 运行时（Windows 11 与绝大多数 Windows 10 自带） |
| ffmpeg | 需在 `PATH` 中 —— **仅** ffmpeg 后端需要；Windows API 后端不用它 |

## 构建

```bash
cargo build --release -p takekit -p takekit-gui
# target/release/takekit.exe      —— CLI
# target/release/takekit-gui.exe  —— 面板
```

测试：`cargo test --workspace`。面板的界面资源是**编译期嵌入**的，所以改了 `crates/takekit-gui/ui/` 下的任何文件都要重新构建才生效。

## 用法 —— 面板

运行 `takekit-gui.exe`：

1. 加源（窗口 / 桌面 / 摄像头 / 麦克风）；点一张源的缩略图就能裁剪它。
2. 按 **F9**（默认热键）或录制按钮。3 秒倒计时，**Esc** 取消。
3. 再按 **F9** 停止。录制默认落在 `%TEMP%\<时间戳>\`，输出目录可在设置里改。

一次录制 = 每个源一个文件 + 一个 `take.json` 清单。历史在面板里，点一行直接打开目录。

设置项：热键、倒计时长度、语言、主题、ffmpeg 路径、以及 agent 控制服务是否开启。

## 用法 —— 命令行

```bash
# 能录什么？
takekit list
takekit list "VirtualBox"        # 按标题过滤窗口

# 一个窗口 + 摄像头 + 麦克风，录 60 秒
takekit record --window "VirtualBox" --cam "My Camera" --mic "Microphone (USB)" --duration 60

# 整个桌面连声音，从顶部裁掉 33px，回车停止
takekit record --desktop --desktop-sound --crop 33,0,0,0
```

`--window` 接受窗口句柄（`--window "#12345"`）或标题片段。产物默认放在 `takes/<时间戳>/`（`-o` 可改根目录）。

### `takekit record`

| 参数 | 含义 |
|---|---|
| `--window <标题或#句柄>` | 录一个窗口 |
| `--desktop` | 录整个桌面，而不是窗口 |
| `--windows` | 用 Windows API 采集而非 ffmpeg 子进程（被遮挡的窗口也能录；单显示器） |
| `--crop T,B,L,R` | 裁剪内缩像素，逐源生效 |
| `--cam <设备>` | 同时录一个视频设备（`takekit list` 查看） |
| `--mic <设备>` | 同时录一个麦克风（`takekit media --list` 查看名字） |
| `--system-audio` | 同时录扬声器正在播放的声音，单独成文件 |
| `--window-sound` | 窗口自己的声音，写进它的画面同一个文件（需 `--window`） |
| `--desktop-sound` | 桌面的声音写进桌面那个文件（整台机器 —— Windows 没有分显示器的音频；**不能与 `--window` 同用**） |
| `--encoder auto\|x264\|nvenc\|qsv\|amf` | `auto` 逐个试编，挑实际能用的最好的一个 |
| `--quality 0..=51` | 对 x264 近似 CRF，对硬件编码是 CQ/QP；0 = 无损（默认 20，越界在创建 take 目录前就拒） |
| `--fps <N>` | 采集帧率，默认 30 |
| `-d, --duration <秒>` | 录 N 秒后停，否则按回车停 |
| `-o, --out <目录>` | 输出根目录，默认 `takes` |
| `--marks true\|false` | 把点击波纹和拖动轨迹画进画面，默认 `true` |
| `--ffmpeg <路径>` | 指定 ffmpeg，而不是从 `PATH` 找 |

### `takekit media`

同样的思路，但只走 Windows API，完全不起 ffmpeg 子进程：

```bash
takekit media --list                       # API 能看到什么
takekit media --window 0x1234 --window-audio 0x1234 --duration 30
takekit media --screen 1 --system-audio --fps 60 --bitrate 20000
```

`--cfr` 让画面按 `--fps` 采样，而不是「变了才写」。两条路写出来的都是定帧率文件（编码器会补重复帧补齐空档）；这个开关只决定空档里放的是「那一刻的画面」还是「最后收到的那帧」。

### `takekit ctl` 与 `takekit mcp`

`ctl` 指挥**正在运行的面板**（加源、改设置、开始、停止、读历史、删一次录制），输出 JSON，方便脚本用：

```bash
takekit ctl state
takekit ctl add --kind window --target "#12345"
takekit ctl start
takekit ctl stop
```

`mcp` 用 stdio 讲 MCP，agent 可以用带 schema 的工具做同样这十六件事（`takekit_state`、`takekit_list_devices`、`takekit_add_source`、`takekit_start_recording` …）。把 MCP 客户端指向 `takekit mcp` 即可，面板需在运行。控制服务只监听 `127.0.0.1` 的随机端口，并要求一个每次启动重生成的 Bearer token —— 面板把它写在 `%APPDATA%\TakeKit\control.json`。

## 一次录制长什么样

```
takes/2026-10-18-11-31-33/
├─ win.mkv
├─ cam.mkv
└─ take.json
```

```json
{
  "encoder": "h264_nvenc",
  "hardware": true,
  "started_at": "2026-10-18-11-31-33",
  "duration": 60.0,
  "start_clock": "frames",
  "sources": [
    { "id": "win", "file": "win.mkv", "start": 0.0, "duration": 60.0,
      "source": { "id": "win", "capture": { "Window": { "hwnd": 133890, "pid": 4321,
                                           "title": "VirtualBox",
                                           "crop": { "top": 0, "bottom": 0, "left": 0, "right": 0 } } },
                  "fps": 30, "quality": null, "audio": false } },
    { "id": "cam", "file": "cam.mkv", "start": 0.0, "duration": 60.0,
      "source": { "id": "cam", "capture": { "Camera": { "device": "My Camera",
                                           "crop": { "top": 0, "bottom": 0, "left": 0, "right": 0 } } },
                  "fps": 30, "quality": null, "audio": false } }
  ]
}
```

`start_clock` 说明这些偏移是拿什么当原点的（`frames` = 文件自己第一帧，精确；`spawn` = 编码器启动时刻，
ffmpeg 后端能看到的最好水平）；`paused` 只在暂停过的 take 里出现，源上的 `frames` 只有 Windows 后端才有。

清单记下用了哪个编码器、每个文件各自的时长 —— 对不上是看得见的，不用等进了剪辑软件才发现。
容器跟后端走：Windows API 后端是 `.mp4`，ffmpeg 后端是 `.mkv`；纯音频源一律是 `.mp4`，即使所在的 take 是 `.mkv`。

## 实现概要

```
crates/
├─ takekit-core/    所有"决定"：会话生命周期、源、编码器选择、take 与清单、ffmpeg
├─ takekit-media/   Windows API 采集：WGC 抓屏/抓窗口、Media Foundation 摄像头与麦克风、
│                   WASAPI 回环、MF SinkWriter（H.264 + AAC 写 MP4）
├─ takekit-cli/     薄壳，二进制名 `takekit`
└─ takekit-gui/     Tauri 2 面板：命令、控制服务、历史、托盘、提示层、界面
```

- 决定都在 core 里，前端只负责问。GUI 与 CLI 共用同一份实现，不存在第二套状态机。
- Windows API 后端整条链路在一个进程、一个时钟上跑 —— 这是它能省掉后期对齐的原因。
- 窗口的身份是**句柄 + 属主进程**，不是标题：标题会变（浏览器换标签、编辑器换文件），句柄号会被回收。

## 文档

| 内容 | 位置 |
|---|---|
| 设计契约：视觉、组件、持久化、键盘 | [`DESIGN.md`](DESIGN.md) |
| 开发笔记：架构地图、实现契约、探针与验证、陷阱清单 | [`docs/development.md`](docs/development.md) |
| Windows API 采集后端详解 | [`docs/api-recording.md`](docs/api-recording.md) |
| Agent 控制面：HTTP 路由 ↔ MCP 工具 ↔ CLI | [`docs/agent-control.md`](docs/agent-control.md) |

## 已知限制

- **仅 Windows。**Win32 / WGC / WASAPI 占的比重太大，移植不是顺手的事。
- **桌面声音是整台机器的声音。**WASAPI 回环按播放端点划分，不按显示器；「只录这块屏的声音」操作系统不提供。
- **`--windows` 的桌面路径只录一个显示器** —— 第一个，或裁剪区域落在的那个。
- **没有安装包、没有代码签名**，Windows SmartScreen 会对二进制报一次。
- **Media Foundation 的摄像头是最没验过的一条路**：需要设备在 MF 里可见，而虚拟摄像头往往只有 DirectShow 注册。

## 许可证

MIT，见 [LICENSE](LICENSE)。
