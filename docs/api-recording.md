# 用 API 录制（分支 `feat/api-recording`）

> 2026-10-20 起，是一次**技术试探**，不是既成方案。读之前先看第 4 节：这里有
> 一条实测出来的硬约束，决定了这条路能不能接进主线。
>
> **2026-10-20 更新：已接进 core 与 GUI**（见第 7 节）。窗口源与后台录制已实测
> 成立；摄像头 / 麦克风链路仍待设备验证。

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
| 鼠标光标 | ✅ 已对齐（2026-10-21）：`CursorCaptureSettings::WithCursor`（原 `Default` 实测不录光标），与 ffmpeg 的 `draw_mouse=1` 一致。实测：抖动鼠标录 3s，抽帧见白色箭头 |
| 多显示器 | ✅ 已支持（2026-10-21）：picker 按 `env_info.monitors` 逐屏列出（`Desktop { rect: 屏矩形 }`），`monitor_index_for` 把 rect 映回 `Monitor(index)`；无 rect / 映射落空回落第一屏。**WGC 只能一屏一路**，"整个桌面"行仅 ffmpeg 后端保留 |

### 要验证摄像头/麦克风，需要什么

插上一个麦克风 / 打开一个 MF 能看见的摄像头，再跑
`takekit media --list` 确认它出现，然后
`takekit media --camera <名> --mic <名> --duration 5`。

---

## 6. 下一步（按价值排序）

1. **验证摄像头与麦克风**（有设备就能跑，代码已就绪）。
2. **系统声音（WASAPI loopback）未实现**——音频通道需求的另一半，现在的 `mic.rs`
   只走 Media Foundation 采集端点。
3. **多路漂移观测**：每路估算漂移率（ppm）写进 `take.json`，30 分钟长录拿数据，
   视结果决定是否做 slew 校正（音频为主时钟是候选方案）。
4. ~~把"按帧率补齐"从 CLI 搬进 `takekit-media`~~ → 已由 core 的 `write_frames`
   承担（第 7 节）。
5. ~~接进 GUI~~ → 已做（第 7 节）。
6. 决定要不要保留 ffmpeg 后端做回退（WGC 在某些环境会拿不到帧）。
7. GUI 音频源（麦克风）；~~多显示器映射~~ → 已做（2026-10-21，逐屏 picker + rect→Monitor 映射）。

---

## 7. 2026-10-20：接入 core 与 GUI

### core：`Backend::Ffmpeg | Windows`

- `Settings` / `Config` / `StartRequest` 都带 `backend`，**默认 `Windows`**。
- `Session::prepare` 在 Windows 路径下**不找 ffmpeg、不挑编码器、不探测 DDA**，
  只校验每个源还能被寻址（窗口活着 / 摄像头 MF 看得见），几百微秒。
- `Prepared::start` 分流到 `start_windows`：每源一个 writer 线程（持有 MF 对象，
  它们不是 `Send`）+ 一个采集句柄；VFR 直通——帧到了就写，时间戳用采集自己的。
  `take.json` 的 encoder 写 `mf-h264`，文件扩展名 `.mp4`（`Take::create` 接 ext）。
- spawn 失败与 ffmpeg 路径同样处理：drop 已启动的源（`Drop` 停采集让 writer
  收尾），删掉 take 目录。
- `snapshot` / `stop_source` / `stop` / `Drop` 对两套运行时一视同仁；Windows 源
  的 `alive` = writer 线程是否还在。
- **窗口源默认不 raise**（2026-10-20 改为用户开关 `raise_sources`，默认关）：
  WGC 录的是窗口本身，被遮挡也能录，raise 毁掉卖点；ffmpeg 后端录的是窗口所占的
  屏幕矩形，需要置前才不会录到遮挡物——要开才置前（`take.rs`）。
- **最小化窗口开录前自动恢复**（2026-10-20 修，录制前提、不是开关）：DWM 根本不
  合成最小化窗口，实测 WGC **0 帧**（同一窗口可见 45 帧 / 恢复后 46 帧）——不恢复
  等于录空气。恢复不抢焦点（`SW_SHOWNOACTIVATE`）；`raise_sources` 关着时 WGC
  后端顺手压到最底（不遮用户手头的活），ffmpeg 后端保持原位（压底会录到遮挡物）。
  录完不会自动最小化回去。
- 摄像头按名字映到 MF 符号链接（`camera_link`）；MF 看不见的名字会报错并提示
  ffmpeg 后端看得更多。

### GUI

- 参数区新增「采集方式」下拉（Windows API / ffmpeg），随 session 持久化；
  picker 与源行在 Windows 后端下标注「被遮挡时仍可录」。
- Windows 后端跳过 DDA 预热（`preheatFor` 直接返回）。
- i18n 双语齐全；`applySaved` 对旧 config 的缺字段读作 Windows。

### 实测（当天）

| 实验 | 结果 |
|---|---|
| 后台窗口（铁证三联） | 录 PowerShell 窗口，途中用 WorkBuddy TOPMOST 完全盖住：**同一时刻**桌面录制里 PowerShell 已不可见，而窗口录制仍是完整的 PowerShell 画面 → 后台捕获成立 |
| 动态网页（VBox 窗口，用户开的淘宝页） | 12s → 374 帧 / 12.47s / 6.1 MB，画面正确（抽帧目检） |
| 静止记事本 | 帧间隔均匀 33ms（WinUI 合成 + caret 让它一直"脏"），但 8.5s 仅 **52KB** —— 不变的帧被编码器压成近零字节 P 帧 |
| core 链路（`record --windows`） | `take.json` encoder=mf-h264、win.mp4、6.5s 时长正确 |

### 🔴 VFR 的结论（2026-10-20 实测推翻又重建）

**这条路走不出真 VFR**。写手把「上一帧到下一帧的真实间隔」写进样本时长，但
MF 的 H.264 编码器按声明帧率运行，**把每个样本的时长展开成整数个重复帧**（实测：
3 个样本 @0/0.5/1.2s、fps=10 → 输出 25 帧均匀 0.1s），样本时间戳也进不了容器
（sink 把轨道重定基到首样本）。所以文件**恒定 CFR**。

但**落位（placement）是保住的**：写给 [1.0s, 1.5s) 的内容出来就是第 10–14 帧。
多路对齐靠的正是落位——见下节。真 VFR 要走 ffmpeg 后端（`-fps_mode vfr`）或自管
封装器，已记 backlog；省数据主要由编码器承担（静止画面重复帧是近零字节 P 帧）。

### 统一时间原点与前导填充（多路同步的基础）

三条链路的帧时间戳来自硬件根时钟（WGC = QPC 系统时钟，MF = 设备时钟），但过去
各源**各自以首帧归零**，跨源零点不一致。现在 `takekit_media::clock::TakeClock`
在 take 开始时立一个原点：

- WGC：系统时钟域，直接减原点（精确，无估计）。
- MF 摄像头/麦克风：设备时钟域，首样本到达时经墙钟桥接一次，之后保留设备自身间隔。

MP4 sink 会把轨道重定基到首样本，所以共享原点以**前导填充**进文件：首帧之前的
空档补黑帧（视频）/ 静音（音频），编码器把它展开成正确帧数。结果：**每路文件的
第 k 帧 = take 时间轴的 k/fps 时刻**，首尾都对齐，多路文件可逐帧对位剪辑。
回归测试 `frames_land_where_the_take_timeline_puts_them` 解码后逐帧验内容落位。

### 多路完好性：实测结论与记账（2026-10-20）

**实测**（双路 20s @30fps：屏幕 + 动态 VBox 窗口；另加 8 进程 CPU 压力对照）：

| 判据 | 基线 | CPU 压力下 |
|---|---|---|
| 解码错误（ffmpeg 全解） | 0 | 0 |
| 帧数 = 时长 × fps（时间轴无洞） | 604/602 = 满 | 604/603 = 满 |
| PTS 间隔不规则数 | 0 | 0 |

**丢帧的形态与处理**：这条路丢不了"时间轴"——编码器把每个样本的时长展开成整数帧，
画面停顿由保持帧填满，文件永远无洞。丢的只能是**画面更新**（采集没交付那一拍），
表现为"这一槽是保持帧"。处理分两层：

1. **不发生的部分**：采集 → 通道 → 写手不丢槽（unbounded channel，写手不阻塞采集）。
2. **可见的部分**：每路**交付帧数**已记账——CLI 打印 `screen: 193 frames delivered`，
   `take.json` 的 `sources[].frames` 字段同样有（GUI 停止后可查）。交付数远小于
   `时长×fps` 说明画面大段时间静止（正常）或采集停滞（异常，可审计）。

**跨路对齐的边界**（回归测试守护）：

- `between_slot_capture_times_do_not_drift`：捕获时刻不在槽位上（0.37s/1.02s/1.53s/1.97s
  @30fps）时，内容落位仍在真值 **±1 槽**内——展开舍入**不累计**，长 take 不会越漂越远。
- **共享结束点**：停止时刻一次性闩住（`Recording::stop` / CLI 同款），所有路的文件在
  同一 take 时刻收尾。此前各路按各自收尾耗时结束，会差 1–2 帧。
- 尾样本舍入仍可能让总帧数差 **±1–2 帧**（尾部余量，不影响从 0 点对位剪辑）；实测
  20s 双路差 1 帧/33ms，落在上述边界内。

### 仍知的限制

| 项 | 状态 |
|---|---|
| 桌面源 | Windows 后端只录**第一块显示器**（WGC 一次一个输出；虚拟屏不存在） |
| 桌面 crop | 忽略（prepare 不再报错；面板默认无 crop 不受影响） |
| 光标 | 仍不录（`CursorCaptureSettings::Default`），要对齐 ffmpeg 的 `draw_mouse=1` 得改 `WithCursor` |
| 摄像头 / 麦克风 | 代码就绪，本机无设备未验证 |
| 编码器 | MF 自选（软件 H.264），quality 映射为分档 bitrate（`bitrate_for`） |
