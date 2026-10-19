# 用 API 录制（分支 `feat/api-recording`）

> 2026-10-20 起，是一次**技术试探**，不是既成方案。读之前先看第 4 节：这里有
> 一条实测出来的硬约束，决定了这条路能不能接进主线。

---

## 0. 一句话

新增 `takekit-media` crate：屏幕走 Windows Graphics Capture，摄像头与麦克风走 Media
Foundation，**编码与封装也走 Media Foundation**（不再 spawn ffmpeg）。CLI 用
`takekit media` 驱动。

---

## 1. 为什么值得试

现有后端是"每个源 spawn 一个 ffmpeg 子进程"。它带来三个绕不开的问题：

| 问题 | 现状 | API 路线 |
|---|---|---|
| 每个源一个时钟 | 各子进程自己计时，多源只在同一秒开始，之后各走各的 | 同一进程、同一时钟 |
| 录不了被遮挡的窗口 | gdigrab / ddagrab 抓的是桌面，谁盖住窗口谁就进画面 | WGC 直接对窗口取帧，**被遮挡也能录**（backlog 第 9 条） |
| 设备靠 ffmpeg 报名字 | dshow 设备名带特殊字符时的转义是老坑 | 符号链接 / 端点 ID 是稳定标识 |

---

## 2. 代码在哪

```
crates/takekit-media/
├─ devices.rs     枚举：显示器（WGC）、摄像头与麦克风（Media Foundation）
├─ screen.rs     WGC 抓屏 / 抓窗口，逐帧带时间戳
├─ camera.rs     Media Foundation 读摄像头，要 NV12
├─ mic.rs        Media Foundation 读麦克风，要 PCM 16 位
├─ mediatype.rs  媒体类型的构建与拆解（尺寸/帧率打包成 u64）
├─ reader.rs     Source Reader 的读循环
├─ writer.rs     Sink Writer：H.264+AAC 写 MP4
└─ frame.rs      帧与音频块的公共类型
```

CLI：`takekit media --list`、`takekit media --screen 1 --camera <名> --mic <名> --duration 5`。

### 三条链路各自的取数方式

- **屏幕**：`windows-capture` 2.0.1 包住 WGC。帧是 BGRA，自带 `SystemRelativeTime`
  时间戳。停止必须走 `CaptureControl::stop()`（向捕获线程投 `WM_QUIT`），**不能靠帧
  回调里的标志位**——画面不动时回调根本不跑。
- **摄像头**：Media Foundation 按符号链接激活设备 → Source Reader → 要 NV12（相机原生
  格式，编码器也能直接吃）。
- **麦克风**：Media Foundation 按 WASAPI 端点 ID 激活 → Source Reader → 要 PCM 16 位，
  采样率与声道数**用设备自己报的**，不强制重采样。

### 编码与封装

`IMFSinkWriter`：给它"目标是 H.264 + MP4"、喂未压缩帧，它自己插入编码器 MFT。
→ **整条链路没有 ffmpeg**。

两个必须知道的细节：

1. **RGB32 媒体类型默认是自下而上的**，抓到的帧是自上而下的，所以 BGRA 帧进写手前要
   翻转行序（`writer::copy_frame` 的 `flip`）。NV12 不用翻。
2. **H.264 要求两条边都是偶数**，所以奇数尺寸在进写手前裁掉最远一行/一列
   （`writer::even_frame`，NV12 要分平面裁）。

---

## 3. 实测结果

```
takekit media --screen 1 --duration 6 --fps 30
  → screen.mp4  3440x1440  h264  178 帧  5.933s  8.3 MB
```

画面是真的桌面内容（`signalstats` YMIN=8 / YMAX=247，不是黑屏）。基线：
`core 33 / cli 7 / gui 60 / media 10`，clippy `-D warnings` 与 `cargo fmt --check` 均干净。

---

## 4. 🔴 实测出来的硬约束：静止画面 = 没有帧

第一次跑 4 秒只录到 **1 帧、0.033 秒**。原因不是 bug：

> **Windows Graphics Capture 只在画面变化时交付帧。** 桌面静止、或会话锁着，它就一帧
> 都不给。加了 `MinimumUpdateIntervalSettings` 也只是"限制多频繁可以更新"，不会让
> Windows 定期产帧。

所以"按到达帧写文件"的写法对录屏根本不成立——文件时长会远小于真实录制时长。

**现状的解法**：写手线程按目标帧率**补齐输出**。每过一个 1/fps 的时隙，就把最新一帧
（没新帧就重复上一帧）按递递增的时间戳写进去。于是时长对得上，且每个源的时间都来自
同一进程里同一起点的 `Instant` —— 这也是多源同步的真正来源（不是各源的采集时间戳）。

代价：输出是 CFR（恒定帧率），画面静止时是一串重复帧，和 OBS 的做法一致。

---

## 5. 还没做 / 没验证

| 项 | 状态 |
|---|---|
| **摄像头链路** | 代码写完，**本机没验证**：Media Foundation 枚举到 0 个摄像头（DirectShow 能看到的都是虚拟摄像头，MF 看不见） |
| **麦克风链路** | 同上。WASAPI 复核：采集端点共 5 个，但 **`DEVICE_STATE_ACTIVE` 的是 0 个** —— 当前没有可用的麦克风 |
| **窗口源** | `ScreenTarget::Window` 已接上，未实测 |
| 接进 GUI | 未做。目前只有 CLI |
| 与 `Capture`/`Source` 配置模型对齐 | 未做：媒体层用自己的 `ScreenTarget` / 符号链接 / 端点 ID，还没映回 core 的 `Capture` |
| 编码器选择 | 由 MF 自己挑（软件 H.264）。没有走 core 的 NVENC/QSV/AMF 探测 |
| 鼠标光标 | `CursorCaptureSettings::Default`，即**不录光标**（ffmpeg 后端是 `draw_mouse=1`）——要对齐得改 `WithCursor` |

### 要验证摄像头/麦克风，需要什么

插上一个麦克风 / 打开一个 MF 能看见的摄像头，再跑
`takekit media --list` 确认它出现，然后
`takekit media --camera <名> --mic <名> --duration 5`。

---

## 6. 下一步（按价值排序）

1. **验证摄像头与麦克风**（有设备就能跑，代码已就绪）。
2. 把"按帧率补齐"从 CLI 搬进 `takekit-media`，做成一个可复用的 `Recorder`，GUI 也要它。
3. 接进 GUI：把 `Capture::Desktop / Window / Camera` 映射到媒体层的目标，并把设备枚举
   结果给面板的源列表。
4. 决定要不要保留 ffmpeg 后端做回退（WGC 在某些环境会拿不到帧）。
