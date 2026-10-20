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

Each source gets **its own encoder process**. That is not a shortcut, it is the
point: a single ffmpeg fed several inputs picks the input to read next by
timestamp, which starves the desktop grabber down to about half the requested
frame rate. Separate processes keep every source at its own rate.

## What it does today

- Sources: a **window** (by window handle — `--window "#12345"` — or by a unique
  title), the **whole desktop**, and **DirectShow capture devices** (webcams,
  phones acting as webcams).
- One timestamped directory per take, with a `take.json` manifest recording the
  encoder, the sources and each file's duration.
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
- Marks (click ripples, drag trails) come from [MouseShow](https://github.com/Booome/MouseShow),
  which is linked as a library rather than launched as a separate process.

## License

MIT. See [LICENSE](LICENSE).

---

## 中文

**一次开机，多路同时录，出来的文件天然对齐。**

录屏通常是拼出来的：一个工具录屏幕、另一个录摄像头，再到剪辑软件里对齐。TakeKit 把一次录制当成电影里的「一条 take」——所有源同时开始、同时结束，文件本来就是对好的。

**每个源一个独立 ffmpeg 进程**，这不是偷懒而是必需：单个 ffmpeg 喂多路输入时会按时间戳挑下一个该读谁，抓屏那一路会被饿到只剩一半帧率。拆开进程才能让每路都跑满自己的帧率。

现在能做的：

- 源：**窗口**（按窗口句柄 `--window "#12345"`，或按唯一标题）、**整个桌面**、**DirectShow 采集设备**（摄像头、当摄像头用的手机）
- 桌面面板（Tauri，`crates/takekit-gui`）：列可录的源、记住上次的设置、保存录制历史（点一条直接进目录）
- 每次录制一个时间戳目录，带 `take.json`（编码器、各源、各自时长）
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

## 当前状态与待办

面板已可日常使用：源列表、设置记忆、F9 开/停、托盘、录制历史、日志页签、
倒计时 3/2/1/GO + 提示音 + Esc 取消、快捷键/语言/主题设置。录制后端默认走
**Windows API**（WGC + Media Foundation），窗口被遮挡也能录；ffmpeg 后端保留为回退。

**待办**（大致按优先级）：

1. **B4 收尾流程**：停止时逐源 finalize 清单 + `take:finalize|source|done` 事件。
   实施前先读本地工作区 `.superpowers/sdd/overlay-tray-plan/plan.md` 的 Task B4，
   并对照 `docs/api-recording.md`——API 后端下收尾语义已经变了。注意：**不要再补
   "已保存"横幅**，成功反馈由历史行承担。
2. **音频通道补全**：系统声音（WASAPI loopback）尚未实现；麦克风与摄像头代码就绪
   但本机无设备未验证（`docs/api-recording.md` §5）。有设备后跑
   `takekit media --list` → `takekit media --camera <名> --mic <名>`。
3. **多路漂移观测**：每路估算漂移率（ppm）写进 `take.json`，30 分钟长录拿数据，
   视结果决定是否做 slew 校正（设计讨论见 2026-10-20 工作日志）。
4. **状态条 failed 态**：`StatusInfo.error` 与 `last_error` 已就位，只差 UI。
5. **B6**：`finalize_panel == false` 的停止路径。
6. 待拍板：单实例保护；提示层创建失败降级为"无提示层 + 记探针"（现在会拖垮整个
   app）；历史/日志「清空」是否连磁盘文件一起删；CLI `--marks` 与 UI「鼠标效果」
   文案对齐；ffmpeg 路径可配置。
7. 未实测：150% 缩放、多显示器、深色主题下历史页浮窗/徽章观感；历史行双击会开
   两个资源管理器窗口（未节流）。
