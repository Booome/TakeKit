# 用 API 录制（分支 `feat/api-recording`）

> 2026-10-20 起，是一次**技术试探**，不是既成方案。读之前先看第 4 节：这里有
> 一条实测出来的硬约束，决定了这条路能不能接进主线。
>
> **2026-10-20 更新：已接进 core 与 GUI**（见第 7 节）。窗口源与后台录制已实测
> 成立；摄像头 / 麦克风链路仍待设备验证。
>
> **2026-10-23 更新：麦克风已实测通过；新增系统声音与窗口声音捕获**
> （WASAPI loopback，见第 8 节）。摄像头仍无 MF 可见设备。

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
├─ loopback.rs   WASAPI loopback：系统声音 / 单窗口声音（进程树）
├─ runner.rs     DeviceCapture：三者的线程骨架（停标志 / 失败槽 / 打开超时）
├─ mediatype.rs  媒体类型的构建与拆解（尺寸/帧率打包成 u64）
├─ reader.rs     Source Reader 的读循环
├─ writer.rs     Sink Writer：H.264+AAC 写 MP4
└─ frame.rs      帧与音频块的公共类型
```

CLI：`takekit media --list`、`takekit media --screen 1 --camera <名> --mic <名>
--system-audio --window-audio <hwnd> --duration 5`。

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
| **麦克风链路** | ✅ 已实测（2026-10-23，Focusrite USB 声卡）：AAC 48kHz 立体声，多源同录稳定。两个实测坑见第 8 节 |
| **窗口源** | ✅ 已实测（2026-10-20，后台窗口铁证三联：被完全盖住时桌面录制里已不可见、窗口录制仍完整）；最小化窗口 0 帧，开录前自动恢复 |
| 系统声音 / 窗口声音 | ✅ 已实测（2026-10-23，`loopback.rs`）：见第 8 节 |
| 接进 GUI | ✅ 已做（第 7 节；音频源 2026-10-23，见第 9 节） |
| 与 `Capture`/`Source` 配置模型对齐 | ✅ 已做（第 7 节：`Settings`/`Config`/`StartRequest` 带 `backend`；第 11.3 节 `Source` 增 `audio`） |
| 编码器选择 | 由 MF 自己挑（软件 H.264）。没有走 core 的 NVENC/QSV/AMF 探测 |
| 鼠标光标 | ✅ 已对齐（2026-10-21）：`CursorCaptureSettings::WithCursor`（原 `Default` 实测不录光标），与 ffmpeg 的 `draw_mouse=1` 一致。实测：抖动鼠标录 3s，抽帧见白色箭头 |
| 多显示器 | ✅ 已支持（2026-10-21）：picker 按 `env_info.monitors` 逐屏列出（`Desktop { rect: 屏矩形 }`），`monitor_index_for` 把 rect 映回 `Monitor(index)`；无 rect / 映射落空回落第一屏。**WGC 只能一屏一路**，"整个桌面"行仅 ffmpeg 后端保留 |

### 要验证摄像头/麦克风，需要什么

插上一个麦克风 / 打开一个 MF 能看见的摄像头，再跑
`takekit media --list` 确认它出现，然后
`takekit media --camera <名> --mic <名> --duration 5`。

---

## 6. 下一步（按价值排序）

1. **验证摄像头**（有 MF 能看见的设备就能跑，代码已就绪）。
2. ~~系统声音（WASAPI loopback）未实现~~ → ✅ 已做并实测（2026-10-23，见第 8 节）。
3. ~~多路漂移观测~~ → ✅ 已测（2026-10-23，见第 11 节）：**没有可测的时钟漂移**，
   不做 slew 校正。原计划的 `SourceRecord.drift_ppm` 字段也取消——上报口径修好之后
   `sources[].duration` 本身就是文件终点，两路相减即漂移量，加字段等于给一个假差做补偿。
   真正遗留的问题不是漂移，是**音频时间线被一次性事故拉长**（30 分钟里多出 4.9 秒），
   修法见第 10 节末尾。
4. ~~把"按帧率补齐"从 CLI 搬进 `takekit-media`~~ → 已由 core 的 `write_frames`
   承担（第 7 节）。
5. ~~接进 GUI~~ → 已做（第 7 节）。
6. 决定要不要保留 ffmpeg 后端做回退（WGC 在某些环境会拿不到帧）。
7. ~~GUI 音频源（麦克风 + 系统声音/窗口声音）~~ → 已做（2026-10-23，第 9 节，实测 23/23）；~~多显示器映射~~ → 已做（2026-10-21，逐屏 picker + rect→Monitor 映射）。

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

> 本节是 **2026-10-20 接入当日的快照**。下表有两条已被后续更新取代，划掉保留是为了
> 记录当时的判断，现状以 §5 / §6 的更新为准。

| 项 | 状态 |
|---|---|
| 桌面源 | ~~Windows 后端只录**第一块显示器**~~ → ✅ 已支持（2026-10-21，picker 逐屏 + rect→Monitor，见 §5） |
| 桌面 crop | 忽略（prepare 不再报错；面板默认无 crop 不受影响） |
| 光标 | ~~仍不录（`CursorCaptureSettings::Default`）~~ → ✅ 已对齐（2026-10-21，`WithCursor`，见 §5） |
| 摄像头 / 麦克风 | 代码就绪，本机无设备未验证 |
| 编码器 | MF 自选（软件 H.264），quality（0..=51，`Session::prepare` 校验）映射为分档 bitrate（`bitrate_for`） |

---

## 8. 2026-10-23：麦克风实测 + 系统声音 / 窗口声音（WASAPI loopback）

### 实测结果

| 实验 | 结果 |
|---|---|
| 麦克风（Focusrite USB） | `mic: 123 frames delivered`，AAC 48kHz 立体声 5.14s ✓ |
| 系统声音 `--system-audio` | sine 440Hz 对照：370 帧 / 3.73s，mean −24.7 dB（非静音）✓ |
| 窗口声音 `--window-audio <hwnd>` | sine 880Hz 对照：498 帧 / 5.01s，mean −5.2 dB ✓ |
| 窗口声音隔离性 | 放着 sine、录无声窗口 → −91 dB 纯静音；只收目标进程树 ✓ |
| 多源同录 `--mic --system-audio` | mic 98 帧 + loopback 402 帧，AAC 各 ~4.1s ✓ |

### Media Foundation 三个实测坑（麦克风/摄像头通用）

1. **source reader 析构会 shutdown 它的 media source**：用临时 reader 偷看
   native media type 会让后续 reader 报 `MF_E_SHUTDOWN`。解法：**一个 source 只建
   一个 reader**（`reader::create_reader`），native type 从 reader 上取。
2. **`ReadSample` 返回空 sample ≠ EOS**：类型变更 / 流 tick 都会交空 sample，
   必须循环重读，只有 EOS flag 才算完（`reader::next_sample`）。否则
   `MF_E_SINK_NO_SAMPLES_PROCESSED`——pump 零帧的假死。
3. **`--window-audio` 的崩溃（heap corruption `0xC0000374`）**：windows-rs 的
   `PROPVARIANT` 带 `Drop`（析构 `PropVariantClear` → `CoTaskMemFree`），blob 数据
   若指向栈上结构，函数返回即对栈地址 free → 进程 fail-fast 无输出。blob 必须
   `CoTaskMemAlloc` 分配。

### 进程回环（per-process loopback）要点

- 全局回环：`IMMDevice::Activate` + `AUDCLNT_STREAMFLAGS_LOOPBACK`，收全喇叭混音。
- 单窗口回环：`ActivateAudioInterfaceAsync` + `PROPVARIANT` blob 装
  `AUDIOCLIENT_ACTIVATION_PARAMS`（`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`）。
  两个必知点：
  - 设备 ID 必须用**虚拟设备** `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`
    （`"VAD\Process_Loopback"`），传默认输出设备 ID 会报
    `0x80070002 ERROR_FILE_NOT_FOUND`。
  - 窗口 handle 先 `GetWindowThreadProcessId` 转成 PID，过滤以**进程树**为单位
    （窗口所属进程 + 其子进程），这也是"一个窗口"在用户心智里的真实范围。
- 两路都开 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` 统一到 PCM16 48kHz 立体声，
  编码器不用猜格式。

---

## 9. 2026-10-23：GUI 与 MCP 接入音频源（实测 23/23）

### 接入面

- **core**：`Capture` 新增 `Mic{device}` / `SystemAudio{}` / `WindowAudio{hwnd,title}`；
  音频源两个后端都走进程内采集（ffmpeg 无 WASAPI loopback），产物强制 `.mp4`
  （MF sink writer 按扩展名选容器、不认 Matroska）。
- **控制面** `resolve_spec`：`kind=mic|system-audio|window-audio`（window-audio 用
  `target` 标题子串解析窗口，歧义拒绝策略与视频窗口一致）。
- **MCP**：`takekit_add_source` kind 扩到六种；`takekit_list_devices` 同步。
- **GUI**：picker 新增「麦克风」「声音」两组（声音组 = 系统声音 + 各窗口声音行）；
  `list_mics` 命令供麦克风列表；音频行图标/摘要（`AAC · Qn`）/编辑器隐藏帧率与裁剪。

### E2E（一次性 scratch 脚本，23/23；脚本本身不入库，结论如下）

| 环节 | 结果 |
|---|---|
| MCP initialize / tools/list | 13 tools，kind 描述含音频 ✓ |
| list_devices kind=mic | Focusrite USB Audio ✓ |
| add_source ×3 音频 + 重复添加 | 全部成功（重复自动编号 sys-audio-2）✓ |
| start→7s sine→stop（windows 后端） | 4 文件：mic / sys-audio / sys-audio-2 / win-audio ✓ |
| volumedetect | mic −8.7dB，sys-audio −6.6dB，win-audio −24.2dB（均非静音）✓ |
| GUI 行渲染（CDP 截图） | 图标/标签/AAC·Q20 摘要正确 ✓ |
| picker（CDP） | 5 组 15 项，含「麦克风」「声音」组 ✓ |

### 实测踩坑（测试工具侧）

1. **ffmpeg volumedetect 的统计行在 stderr**——`execFileSync` 只回 stdout 时解析为空，
   要用 `spawnSync` 合并两路。
2. **WebView2 CDP 端口 9222 被本机占用**（`/json/list` 返回 404 空 body，curl/node
   fetch 皆然）→ 换 9233 即好。target 匹配用 `tauri.localhost`（Tauri 的页面 URL），
   不是 `index.html`。
3. **MCP 添加的源 `quality: null`**（会话级默认）→ GUI 摘要曾显示 `Qnull`；
   已回落 `DEFAULT_QUALITY` 显示实际生效值。

## 10. 2026-10-23：虚拟摄像头三连改进 A/B/C（Mikey Cam 排查的落地）

背景：Mikey 虚拟摄像头（`softcam.dll` 注册为 DirectShow filter）在宿主未运行时仍
出现在设备列表，排查结论是"注册常驻、枚举报的是系统注册了什么"——不是 bug。但连带
发现三个真问题，逐个修掉：

### A. 报错可读化（「存码显文」）

core 产出稳定码 `err.<name>|<device>`，各扇门各自渲染——GUI 走 i18n（`codeText`/
`uiErrorText`）、CLI 走 `diagnose::explain()` 英文句、MCP/control 面走 `ui_fail`。

| 码 | 触发 | 英文句要点 |
|---|---|---|
| `err.cam-no-signal` | `Could not find output pin` | registered but sends no video — start its host app |
| `err.cam-not-mf` | windows 后端开 dshow-only 相机 | record it on the ffmpeg backend |
| `err.device-gone` | `Could not find … device` | check the device is connected |
| `err.cam-open` | `Could not set video options` / `I/O error` | 若是虚拟摄像头先启动宿主，否则可能被占用 |
| `err.ffmpeg-died` | 无输出无日志 | ffmpeg exited without writing anything |

诊断匹配窗口 `tail(log, 20)`（关键签名行可能不在末 3 行）；未命中回落原文。

### B. 列表标注设备状态（无信号徽标）

`ffmpeg -list_devices` 的内联标记零成本读活性：`(video)` → `live: Some(true)`，
`(none)` → `Some(false)`（注册但无输出 pin = 虚拟摄像头宿主未启动）。`DshowCamera`
结构带 `live: Option<bool>`。GUI picker 对 `live: false` 行加 warn 色圆点 +
「无信号 · 先启动宿主程序」徽标（`.pick.no-signal`），CLI 打 `(no signal)`。

### C. 统一列表口径（dshow ∪ MF 合并枚举）

新模块 `takekit-core/src/devices.rs`：`CameraEntry { name, live, dshow, mf, link }`，
dshow 顺序优先、MF-only 条目垫底。四扇门消费同一合并列表：

| 扇门 | 位置 |
|---|---|
| CLI `takekit list` | `[dshow]`/`[dshow+mf]` 后端标注 |
| CLI `takekit media --list` | 同上；`resolve_camera` 对 dshow-only 给 `err.cam-not-mf` 句 |
| GUI `list_devices` invoke | picker meta 显示 `DirectShow`/`Media Foundation`/`DirectShow + MF` |
| MCP/control `GET /devices?kind=camera` | `{name, live, dshow, mf, link}` |

### 实测矩阵（2026-10-23，Mikey 客户端开/关对照）

| 项 | Mikey OFF | Mikey ON |
|---|---|---|
| `takekit list` | `Mikey Cam  (no signal)  [dshow]` ✓ | `Mikey Cam  [dshow]` ✓ |
| `takekit media --list` | 同上 ✓ | 同上 ✓ |
| GUI picker（CDP 截图 + DOM 断言） | 徽标「无信号 · 先启动宿主程序」✓ | `DirectShow` 标注、无徽标 ✓ |
| MCP `/devices?kind=camera` | `{dshow:true, mf:false, live:false}` ✓（修前为 `[]`） | `live:true` ✓ |
| windows 后端开 Mikey | `err.cam-not-mf` 句（走 ffmpeg 后端）✓ | 同左（MF 不可见） |
| ffmpeg 后端开 Mikey | `err.cam-open` 句（指宿主程序）✓ | 正常录制 ✓ |

单测 165 通过（含 `devices::tests` 3 个 merge 用例、`diagnose` 8 个签名用例）。

---

## 11. 2026-10-23 晚：多路漂移实测 + 窗口声音改为窗口源的选项

### 11.1 漂移：没有可测的漂移，原来的 0.5 秒是上报口径的 bug

CLI 先补了 `--mic` / `--system-audio`（此前只有 GUI 和 `media` harness 能录音频，`record` 录不了多路）。

第一轮烟测报 `screen 60.473 / mic 59.980`，差 0.493s，表面 8200 ppm——对晶振不可能。
根因：`write_audio` 的 `seconds = bytes/(rate*ch*2)` 只累加真实到达的 chunk 字节，**不含
`AudioWriter` 补的前导静音**。麦克风打开花 0.56s，静音垫进了文件却没进上报。
ffprobe 实测：`screen.mp4 60.533` / `mic.mp4 60.544`，**只差 11ms**，文件本来就是同步的。

修法：`AudioWriter::finish` 返回时间线终点（`next`，含前导静音），`write_audio` 用它当
`seconds`。修后 20s 复测 delta 从 +0.471 → **+0.023**，`take.json` 与实际文件对齐。

四轮实测：

| 时长 | 上报差 | 文件差 |
|---|---|---|
| 20s | +0.023 | — |
| 180s | +0.016 | +16ms |
| 600s | +0.041 | — |
| 1800s | **+4.891** | +4.85s |

结论：**分钟级没有漂移**，差的量级就是 AAC 帧量化（23ms）+ 末缓冲（~40ms）；
30 分钟那次多出的 4.9 秒是**一次性事故**——10 分钟干净、30 分钟突然多，不是线性速率。

那 4.9 秒是真写进文件的：`84624 × 1024 / 48000 = 1805.312s`，而真实流逝 1800.5s——
30 分钟里采集 pump 多交了约 23 万个样本（0.27%）。

### 11.2 音频时间线会被一次性事故拉长（已修，2026-10-27）

原来的 `AudioWriter::write` **忽略 `chunk.time`**（除首个 chunk），按 `next = start + duration` 连续写：

- 设备真断档 → 洞里补不出声音，但既不补静音也不报错，后续整体前移（同步保住，丢内容）
- 设备重投/回退 → 字节重复累加，**后续整体后移，永久失去同步** ← 30 分钟那次属于这类

修法：`write` 拿 `chunk.time` 与 `next` 比，容差 40ms（约一个缓冲，`SKEW_TOLERANCE`）：

- `skew > 容差` → 当真实空洞，写静音补到 `chunk.time`
- `skew < -容差` → 当重投/回退，**跳过该 chunk**（宁可丢内容，保住同步）
- 其余 → 仍按 `next` 首尾相接，吸收抖动

🔴 **上表漏了一个前提，补上之后才敢上线**：WASAPI 的时间戳与字节数**不一定在同一绝对基准上**。
如果设备时钟整体偏 200ms，上面两条会把**每一个 chunk** 都判成空洞并补静音——文件被静音撑爆，
比原来的 bug 更糟。所以判定用的是**变化量**：`AudioWriter::skew_base` 记住第一次可测量的
偏差，之后只看 `skew - base`。偏移被学掉，事件才被修。

判定逻辑抽成了纯函数 `place(arrival, next, &mut base) -> Placement{Chain, AfterHole, Skip}`，
两条单测钉住：`a_redelivered_chunk_is_dropped_and_a_real_hole_is_filled`、
`a_standing_clock_offset_is_learned_not_repaired`（50 个连续偏移 chunk 必须全部 `Chain`）。

**未做的验证**：30 分钟那次事故是偶发，没有可复现的注入手段；单测覆盖的是判定，不是真实设备。
下次长录制的 `probed` 与 `duration` 差值仍是唯一实测证据。

### 11.3 窗口声音改为窗口源的选项

原来 picker 里每个窗口出现两次（`Capture::Window` 视频 + `Capture::WindowAudio` 独立音源），
用户得自己保证两次选的是同一个窗口——选错就是"画面对、声音错"。

现在：`Source` 增 `audio: bool`（`#[serde(default)]`，旧 take.json 照常读）；窗口源的行上有
一个**喇叭按钮**（有声 / 划掉两态，点一下即生效落盘，不进编辑器），编辑器里同一个字段反过来
叫「静音」；产物从"视频 + 另一个音频文件"变成**一个文件两条流**。

语义方向要说清，否则两处必然打架：**行内图标表示"现在是什么"，编辑器开关表示"要不要静音"**，
它们读写的是同一个 `audio`。默认 `audio: false` = 静音 = 喇叭划掉 = 编辑器开关勾上。

实现要点：

- **`MuxSink`**（`takekit-media/src/writer.rs`）：一个 sink 两条流。MF 要求 `AddStream` 早于
  `BeginWriting`，而视频尺寸要等首帧，所以顺序是：等首帧 → 加视频流 + 音频流 → `begin`。
- **`write_av`**：一个线程抽两个通道（每轮先抽干音频 `try_recv`，再视频 `recv_timeout(2ms)`）——
  sink writer 不能交给两个线程。首帧到达前的音频留在队列里（队深 64 chunk，一块 10ms，
  合计 ≈0.64s——`takekit-media/src/frame.rs` 的 `AUDIO_QUEUE`，够）。
- **`VideoWriter::flush` / `AudioWriter::end`**：拆出来，让流可以挂在别人的 sink 上，
  `finish`/`finish_at` 仍各自负责"自己的文件"。
- **`window-audio` 源保留**：CLI `--window-audio`、控制面 `kind: "window-audio"` 仍可只录声音
  不要画面；控制面新增 `kind: "window" + sound: true` 等价于面板里的这个开关。

实测（2026-10-23 16:0x）：

| 项 | 结果 |
|---|---|
| `--window "Chrome" --window-sound -d 6` | 一个 `win.mp4`：H.264 6.400s + AAC 6.357s，`mean_volume -17.8 dB / max -4.0 dB`（真内容）✓ |
| 无声窗口（VirtualBox） | 不报错、视频完好，音轨只覆盖 3.03s（该进程树只交了 3 秒包）——已知行为 |
| GUI 13/13 | `audio:true` 恢复后行上是"有声"喇叭（无文字，tooltip 说明点击效果）；点一下 → `audio=false` 落盘、图标转静音；再点回 `true`；编辑器「静音」未勾、勾上后写入 `audio=false`；picker 不再重复列窗口 ✓ |
| `cargo test --workspace` | 172 通过 |

### 11.4 系统声音的时间戳单位错了 208 倍（已修，2026-10-29）

**用户报的现象**：加一个「系统声音」源，录了几秒，停下来文件长度≈0。

现场（`草稿/2026-10-29-17-28-16/take.json`）：文件 3289 字节、`duration: 0.09`、`probed: 0.106`，
但 `frames: 776` —— 音频源的这个数字是**包数**，776 个 10ms 包 = 7.8 秒音频。**包都到了，只写进去 4 个。**

根因在 `loopback.rs` 的 `GetBuffer`：

```text
GetBuffer(ppData, pNumFramesToRead, pdwFlags, pu64DevicePosition, pu64QPCPosition)
                                               ^^^^^^^^^^^^^^^^^^   ^^^^^^^^^^^^^^^^
                                               音频帧计数             QPC，100ns
```

代码取了第 4 个（设备位置，单位是**帧**）却当 100ns 用，第 5 个（QPC）传了 `None`。
48kHz 下 1 秒 = 48000 帧，于是时间轴被压缩 **48000/10⁷ ≈ 1/208**：7.8 秒的包串在一起只有 0.037 秒。

**为什么 10-27 之前没炸**：那时 `write` 只在首个 chunk 读一次时间戳，之后按字节连续写，
文件长度是对的（错的是漂移，看不见）。11.2 的 `Skip` 规则一上线，每包只推进 ~480 单位、
字节却推进 10ms，偏差每包累积 10ms —— **4 个包就超过 40ms 容差，之后全部被判"重投"丢掉**。
是 11.2 的守卫把一个旧单位错误放大成了"文件被截断"。

修法：取第 5 个出参（QPC，与 take 时钟同为 QPC 派生、同单位）；驱动不答 QPC 时退回帧数换算
`stamp_of(qpc, device_position)`（长度对，只带端点自己的速率误差）。纯函数 + 两条单测：
`a_device_position_counts_frames_not_hundred_nanosecond_units`、`a_qpc_position_is_taken_as_it_comes`。

实测（2026-10-29）：

| 路径 | 命令 | 结果 |
|---|---|---|
| CLI，有声 | `media --system-audio -d 5` | 5.034s / 163 KB / 501 包 |
| CLI，静默 | `media --system-audio -d 3` | 3.029s / 300 包（loopback 静默也发包） |
| GUI 控制面 | `POST /record/{start,stop}`，源 `system-audio` | manifest 4.93s · probed 4.949s · 493 包 |
| `cargo test --workspace` | — | 9 / 66(+8) / 94 / 39 全绿 |

🔴 **教训**：出参的语义只在 `windows` crate 的类型上，注释里那句 "WASAPI's device time is in 100 ns
units" 是我自己写的、也从来没被验证过。**同一个 API 有两个时间出参时，先确认要的是哪一个**
（QPC = 绝对时刻，device position = 流逝的帧数），别拿名字猜。

### 11.5 补的静音切在半个采样上，整条音轨变成噪声（已修，2026-10-29）

**用户报的现象**：录一个窗口并打开「带它自己的声音」，7.68 秒的文件有音轨、长度也对，
但里面是**刺耳的噪声**。这一条和 11.4 是两个独立的 bug，只是撞在同一天。

现场（`草稿/2026-10-29-17-41-29/ego.mp4`）测音频内容，三项独立指标同时异常：

| 指标 | 实测 | 正常范围 |
|---|---|---|
| 一阶差分/平均幅度 | **1.125** | 语音/音乐 0.05~0.4（白噪声 ≈1.4） |
| L/R 相关度 | **0.029** | 真立体声 0.3~0.99 |
| 均值电平 / 满刻度采样 | **-6.2 dB / 2.84%** | 不该有 2.84% 的采样压在 0dB |
| 每 0.5 秒 RMS | **全程 -6.0 dB，零起伏** | 真声音必然有动态 |

**根因：`write_silence` 写出的字节数不是整帧。**

取 17.16 ms 的前导静音 → `171616 × 192000 / 10⁷ = 3295` 字节。48kHz 立体声 16-bit 一帧 4 字节，
3295 = 823 帧 + **3 字节**。PCM 交给 sink 时是**一条连续字节流**，多出 3 字节之后每个采样都由
"自己的后半个 + 邻居的前半个"拼成 —— 字节序反了、声道也反了，于是变成响亮的宽带噪声。

**为什么时好时坏**：断帧只在字节数不能被 4 整除时发生，概率 3/4。同一段代码：

| 条件 | 结果 |
|---|---|
| CLI `media --system-audio` × 6（都补了未对齐静音） | 6/6 噪声：-5.2 dB · `diff/abs` 1.629~1.634 |
| 同一命令 × 6（修复后） | 6/6 干净：`diff/abs` 0.058，`align4=0` |
| 早先两次"看着正常"的运行 | 静音字节数恰好是 4 的倍数 |
| GUI 桌面源 + 系统声音（MuxSink） | 修复后干净：0.058 / 相关 1.000 |

**定位过程值得记**：先用 `TK_DUMP_AUDIO` 把**编码前**的字节转储出来 —— 转储是干净的 440Hz
正弦（`diff/abs` 0.058、相关 1.000、时长精确 4.000s），而同一进程写出的 mp4 是噪声。
一步就把范围从"采集/格式"切到"写入"。

修法：
- `silence_bytes(span, bps, bpf)` 把静音字节数**就近取整到整帧**，纯函数，单测
  `inserted_silence_never_cuts_a_frame_in_half` 钉住（含 3295 → 3296 这个真实数字）；
- `write_silence` 返回**实际写到哪**（`from + duration_of(bytes)`），调用方（前导静音、中途空洞）
  把下一个 chunk 贴着它放 —— 时间和字节数从此不会各说各话；
- `AudioWriter` 新增 `bytes_per_frame` 字段（原来只有 `bytes_per_second`，推不出帧长）。

🔴 **教训**：凡是往 PCM 流里"垫东西"，**必须按帧对齐**。写之前我把它当"补几个零"，实际上是在
改整条流的相位。另一个：用户报"声音有问题"时，**只量时长不够** —— 这次的时长完全正确，
坏的是内容；`diff/abs` + L/R 相关度 + 满刻度占比三个数一起看，才能把"噪声"和"静音"分开
（数字静音经 AAC 编码后也有本底噪声，电平判据会误报）。

> 未覆盖：这条修的是"补静音"的路径。**因设备真实卡顿而补的中途空洞**（`Placement::AfterHole`）
> 走的是同一个 `write_silence`，但这一次没有造出真实卡顿去验证。

## 12. 2026-10-23：窗口源的身份 = hwnd + pid，标题只是显示缓存

**用户报的问题**：用标题标识窗口，VS Code 换文件、Chrome 换标签都会改标题，程序还认得对吗？

**认得对，但有三处地方说错了话**（全部实测修正）：

| 位置 | 旧行为 | 现在 |
|---|---|---|
| 采集身份 | 一直是 `hwnd`（`ScreenTarget::Window`、`captureKey`、预检都只看句柄） | 不变，且**加 pid 校验**：句柄被系统回收给别的进程时，`resolve_window` 直接报错而不是录到陌生窗口 |
| `resolve_window` | 句柄失效后**按旧标题兜底匹配**（`find_window` + `pick_only`） | 兜底**删掉**。它与「句柄是身份」自相矛盾，而且是最糟的一种：真按旧标题猜中一个同名窗口，就静默录了别人的画面；标题改过则报「没有可见窗口匹配」——窗口明明开着 |
| 行的名字 | 添加时的标题快照，切文件后永远印旧名 | 每 1.2s 用 `live_title(hwnd)`（`IsWindow` + `GetWindowTextW`，**绝不走 `window_by_hwnd`**，那个内部会 `restore()` 把用户最小化的窗口弹出来）刷新，只更新内存、不写盘 |

**旧配置兼容**：`pid` 用 `#[serde(default)]`，缺省 `0` = 不知道 → 退化为只校验句柄（和以前一样，不会因为读不到 pid 就拒绝一份好好的配置）。单测：`a_window_capture_without_a_pid_still_loads`。

**为什么不在最小化时允许盲裁**：最小化时 `window_capture_size` 给的是 `rcNormalPosition`（恢复后的尺寸），裁剪的四条边缘仍然算得出来，录制前 `place_for_capture` 也会先把窗口恢复——技术上"能裁"，但**没有画面可对照**。2026-10-23 起：无画面 → 缩略图 disabled、点不开工作区，页脚给「恢复窗口」按钮，恢复后自动重抓静帧。

**实测**（`check-ui-matrix`，当时 111/111）：脚本自建一个 WinForms 窗并最小化 → `minimized-no-crop` / `restore-offered` / `crop-closed` 全绿；点「恢复窗口」→ `unminimize` → 重抓 → `restore-window/turns-croppable` 绿。行标题用假标题 `STALE …` 播种验证（`row-title-follows-window`）。

**顺手修掉的一处**：`WindowAudio` 的预检原先调 `window_by_hwnd`，会把用户最小化的窗口恢复回来才检查——声音源根本不需要窗口可见。改用 `window_is_alive`（`IsWindow` + pid），检查不再动用户的屏幕。

## 13. 2026-10-23：桌面源也能带声音（= 整台机器的声音）

**用户问**：整个桌面录制是不是也该支持声音，只不过支持的是全局声音？——**是**，而且只能
是全局的：Windows 的 WASAPI loopback 按**渲染端点**分，不按显示器分。所谓"录第二块屏的
声音"在系统里不存在这种东西（除非那台显示器自己有 HDMI 音频端点并成为默认设备）。所以
选项挂在桌面源上，含义一律是"这台电脑现在放出来的声音"。

**实现**：`start_window_av` 泛化成 `start_av`，按 capture 决定两个 target，其余一字不改
（同一个 `write_av`、同一个 `MuxSink`）：window → `ScreenTarget::Window(hwnd)` +
`LoopbackTarget::ProcessTree(pid)`；desktop → `ScreenTarget::Monitor(index)` +
`LoopbackTarget::All`。`Capture::Desktop` 本身**没有加字段**——声音本来就是 `Source.audio`。

**实测**（`takekit record --windows --desktop-sound -d 6`，ffplay 放 440Hz 正弦）：

| 检查 | 结果 |
|---|---|
| 流 | H.264 3440×1440 + AAC 48kHz stereo，两条流一个文件 |
| 时长 | video 6.3666s / audio 6.3787s（差 12ms，与窗口声音同量级） |
| 音量 | `mean_volume -6.5 dB / max 0.0 dB`（真内容，非静音） |
| take.json | `"audio": true`，`duration` 仍是文件终点 |

**顺手修掉一个静默失败**：ffmpeg 后端下 `audio: true` 的源会被 `start_ffmpeg` 当普通视频源
处理 —— 喇叭是亮的、文件是哑的，与"麦克风坏了"无法区分。现在 `Session::prepare` 直接按名字
拒绝（"… needs the Windows backend (WGC + MF); ffmpeg has no WASAPI loopback"），单测
`sound_inside_the_picture_needs_the_windows_backend` 钉住两侧（ffmpeg 拒、windows 收）。

**三端**：CLI 新增 `--desktop-sound`（与 `--window` 互斥，说了原因）；MCP 的 `sound` 白名单
本来就在，改的是描述与 `kind=desktop` 的生效范围；控制面 `POST /sources` 的 `sound` 判断从
`Capture::Window` 放开到 `Window | Desktop`。**`check-three-surface` 抓到的正是这一类**：
字段在 schema 里有、在转发链上被丢掉，只有端到端跑一遍才看得见（现在是 14/14）。
