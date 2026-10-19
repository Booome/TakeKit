# TakeKit GUI 性能优化方向（待实施）

状态：v1 · 2026-10-18 · 范围 `crates/takekit-gui` + `crates/takekit-core` 会话路径
性质：**记录用**。本轮只做 review，不动手改；实施时按此清单执行，完成后回填结果。

## 一、根因结论

卡顿主因不是 WebView 渲染，而是：**7 个 Tauri command 全部是同步函数，按 Tauri
官方文档全部跑在主线程**（"Commands without the *async* keyword are executed on
the main thread"），其中多个每次调用都要启动 ffmpeg 子进程，最贵的一次实测 2.2 秒。
主线程被占用期间，渲染、输入、窗口拖动、IPC 整体冻结。

## 二、实测成本（本机，2026-10-18，3 次取样）

| 调用路径 | 实测耗时 |
|---|---|
| `ffmpeg::dda_available`（`session.rs` → `Session::prepare`） | **2231 / 2231 / 2278 ms** |
| `encoder::works` NVENC 探测（`encoder::pick`） | 41 / 49 / 249 ms |
| `encoder::works` QSV | 137 / 138 / 149 ms |
| `ffmpeg::list_capture_devices`（dshow 枚举） | 90 / 92 / 123 ms |
| `ffmpeg::duration`（ffprobe 单文件） | 58 / 78 / 244 ms |
| `ffmpeg::locate`（扫 PATH） | ~0.8 ms，可忽略 |

## 三、操作 → 卡顿映射（基于代码路径推断，待 probe 打点确认）

| 用户操作 | 阻塞点 | 预估冻结 |
|---|---|---|
| 点"开始录制" | `start_recording` → `Session::prepare` → `dda_available` + `encoder::pick` + spawn N 个 ffmpeg | 2.5–3 s |
| 点"停止" | `stop_recording` → 每源 `ffmpeg::stop` 等待 + `ffprobe duration` 串行 | 0.3–1 s |
| 打开"添加源" | `list_devices`（~100 ms）+ JS 串行 await 两次 IPC | 0.1–0.3 s |
| 应用启动 | `env_info`（`encoder::pick`）+ `list_devices` 串行 | ~0.15–0.35 s |
| 搜索窗口（窗口多时） | `renderPicker` 每个 input 全量重建 DOM | 随窗口数增长 |

## 四、优化方向（按收益排序）

### P0 — 慢命令移出主线程（消除整窗冻结）

1. `crates/takekit-gui/src/main.rs` 的 `env_info`、`list_devices`、
   `start_recording`、`stop_recording` 改 `async`（或 `#[tauri::command(async)]`），
   真正的进程等待再包 `async_runtime::spawn_blocking`。
   - 限制：async command 不能直接带 `tauri::State<'_, …>` 借用参数，需按 Tauri
     文档用 `Result` 返回包装 workaround；`env_info`/`list_devices` 返回类型要调整。

### P1 — 消少重复探测与串行等待（降总耗时，不只是"不冻"）

1. **`dda_available` 2.2 s 是最大单点**（`session.rs:340`、`ffmpeg.rs:79`）：
   app 空闲时预热探测并缓存引擎选择，点"开始录制"时复用。
   ⚠️ 不动"预探测再回退 gdigrab"的既有设计（DESIGN.md §7 红线），只提前 + 复用。
2. **`encoder::pick` 调了两次**（`env_info` + `Session::prepare`，GUI 传
   `encoder: None` 必走 pick）：硬件在进程生命周期内不变，缓存一次。
3. **`stop_recording` 的 ffprobe 逐源串行**（`session.rs:226`）：各源独立文件，
   可并行 spawn。
4. **设备列表缓存**：`list_devices` 每次 ~100 ms，启动时取一次，picker 打开时
   后台刷新。

### P2 — 前端交互

1. `ui/main.js` `refreshSources` 两次 IPC 串行 await → `Promise.all`；
   `openPicker` 先渲染窗口列表（`list_windows` 很快）再补设备。
2. `renderPicker` 每个 input 全量 `replaceChildren` + 逐个 `createElement` →
   一次建列表，搜索只切显隐（事件委托），必要时防抖。
3. `setInterval(tick, 250)` + async 在主线程冻结期间会排队堆积 →
   改 `setTimeout` 链（上次完成再排下次）。

### P3 — 次要 / 待验证

1. `status` 250 ms 轮询改 Rust 侧 `emit` 事件推送：收益小，优先级低。
2. MouseShow overlay 录制期每帧全屏重绘开销：见 §五（mouseshow 性能 review）。
3. `[profile.release] opt-level = "s"` 偏体积：对卡顿主因影响极小，不作为方向。

## 五、mouseshow 性能 review

范围 `mouseshow/src/`（lib.rs / overlay.rs / scene.rs / input.rs / config.rs）。
取证方式：临时集成测试实测 GDI 对象计数（`GetGuiResources`）、release 二进制实测
常驻 CPU、`thread::sleep` 精度实测；取证代码跑完即删，未入库。

### 5.1 实测数据

| 指标 | 结果 |
|---|---|
| 反复 `Marks::start`→`stop` 的 GDI 对象数 | **before=0 → 2 → 4 → 6（每次固定泄漏 2 个）** |
| idle 常驻 CPU（无输入，release） | fps=60 实测 0.31–0.62% 单核；fps=240 实测 1.25% 单核（CPU 时间测量粒度量化到 15.6ms，量级可信、精确值待精测） |
| `thread::sleep` 精度（未设 timer resolution） | 2000µs→实测 2031–2565µs；4167µs→4364–4697µs；16667µs→16755–17258µs |

sleep 精度说明本机定时器分辨率不是瓶颈：input 的 500Hz 轮询（实测 ~435Hz）
与 render 的 60fps 名义节拍**可达**，config 里 fps 上限 240 实际约 217fps。
"轮询频率名不副实"的怀疑被证伪，不需要为此改设计。

### 5.2 发现清单

#### P0 — 每次 start/stop 泄漏 2 个 GDI 对象（实测实锤）

`overlay.rs` `Drop for Overlay` 直接 `DeleteObject(self.hbmp)`，但 hbmp 此刻仍被
`SelectObject(mem_dc, hbmp)` 选入 DC——**被选入 DC 的位图无法删除**（GDI 硬规则，
`DeleteObject` 静默返回 FALSE）；`mem_dc` 本身也从未 `DeleteDC`。每次录制
（TakeKit `session.rs` → `marks::start`）泄漏一个全屏 DIB section（W×H×4 字节：
1080p≈8.3MB、1440p≈14.7MB、4K≈33MB）+ 一个 DC 句柄，进程退出才回收。
长驻 GUI 反复录制 = 内存线性增长，GDI 句柄数也线性增长（上限 10000/进程）。
`probe_overlay()`（doctor）走同一构造路径，调用一次泄漏一份。

修复方向（改 mouseshow submodule）：Drop 中先反选再删——
`SelectObject(mem_dc, GetStockObject(NULL_BITMAP))` → `DeleteObject(hbmp)` →
`DeleteDC(mem_dc)`。Drop 实际运行在 overlay 线程（`run_loop` 栈上 `ov` 析构），
显式 `DestroyWindow` 也在正确线程；窗口随线程退出本也会销毁，可顺带显式化。
修复后应在 TakeKit 侧留一个回归测试：连续 start/stop 3 次断言 GDI 对象数不增长
（注意无头环境会失败，需 `#[ignore]` 手动跑）。

#### P1 — 每帧无条件重绘，静止帧纯浪费

`run_loop` 每帧 `dirty = prev ∪ current` → `clear_rect` + `scene.render` +
`UpdateLayeredWindowIndirect`，**画面没有任何变化时也照做**。例如：
- 按住鼠标不动（trail held、无 ripple）：bounds 恒定，每帧 clear + 全轨迹
  stroke 重建 + 重传同一矩形；
- 拖拽跨屏时 dirty 接近全屏：`blit` 逐像素 R/B swap 每帧全屏（1080p≈8.3MB、
  4K≈33MB）+ ULW 上传，@60fps 即数百 MB/s 内存带宽 + DWM 每帧全屏合成，
  叠加进录制预算（本项目性能敏感，录制期是主场景）。

scene 目前没有"是否有东西在变"的概念——事件到达、ripple 动画进行中、released
trail 淡出中，三者任一为真才需要画。方向：scene 提供 `is_animating(now)` +
"本帧消费过事件"标记，两者皆假时跳过 clear/render/present，只 sleep（事件驱动，
可用带超时的 channel recv 替代纯 sleep，超时取"到下一个动画边界"）。

#### P1 — trail 每帧全轨迹重建光栅化

`Scene::render` 对每个活动 trail 每帧从 `points[0]` 重建 PathBuilder 并 stroke
整条折线（上限 4096 点，`bounds()` 同样每帧 O(points)）。held 期间画面其实只在
新点到达时变化，且旧像素已正确留在 pixmap 中（append-only）却每帧 clear 重画。
方向：增量绘制——新点只画新增线段并只上传新段包围盒；fade 阶段才整体重画。
与 5.2-P1 合并实施。微优化可一并处理：`handle` 的 `sqrt` 改平方比较、
`remove(0)` 改 `VecDeque`（两者量级小，顺手做）。

#### P2 — `blit` 逐像素 swap 无批量优化

`present_rect` 内层循环对每像素做 4 次字节操作（RGBA→BGRA）。全屏拖拽时这是
每帧 8–33MB 的标量循环。方向：按 `u64`/`u128` 批量 swap（对齐后），或整段
pixmap 预生成 BGRA 副本。量级：标量 ~4ns/像素 vs 批量 ~0.5ns/像素。

#### P2 — idle 空转（实测 0.3% 单核）

无特效时 `ov.hide()` 让合成成本为零，但主循环仍按 fps 醒来（60 次/秒
try_recv+tick+bounds+sleep），input 线程固定 435Hz 轮询。实测合计 0.3% 单核
（fps=60）。方向（低优先级）：idle 时改用带超时的 `recv_timeout` 事件驱动，
超时对齐"下一个动画边界"，input 轮询保持（它是 500Hz 名义、成本低）。
仅在超长录制（数小时）且 CPU 预算极端时值得做。

#### 待决策 — 录制中 show/hide 顶层窗口与 DESIGN §7 红线的张力

`run_loop` 在特效起落时 `SW_SHOWNOACTIVATE`/`SW_HIDE` overlay 窗口——**发生在
录制期间**（每次涟漪出现/消失一次）。DESIGN §7 红线："录制中不要创建、显示或
隐藏顶层窗口"（session.rs 模块注释同样强调新建/显示顶层窗口会让 DDA 会话失效、
输出截断到不足 1 秒）。当前实现依赖一个未写下的假设：**已存在窗口的 show/hide
对 DDA 无害，只有新建窗口有害**。用户实测未报截断问题，暂按现状。
hide 的设计理由是"idle 时 DWM 完全不合成，成本字面为零"（overlay.rs 注释）；
DESIGN §4.6 提的 opacity 方案在 UpdateLayeredWindow 窗口上不能用
`SetLayeredWindowAttributes`（与 ULW 互斥），只能用"每帧 ULW 更新为全 alpha=0"
替代 hide——窗口常驻可见但全透明，DWM 仍参与合成（有持续成本）。
**权衡需要用户拍板**：A 保持 show/hide（零 idle 成本，依赖未验证假设）；
B 常驻透明 ULW（符合红线字面，idle 有合成成本，需实测量级）；
C 先做 DDA 下两种模式的截断实测再定（推荐：符合"先取证"，成本一轮录屏）。

#### 非问题（检查后不改）

- input 500Hz 轮询用 `GetAsyncKeyState` 而非 `WH_MOUSE_LL`：注释给出的理由
  （反病毒误报、系统输入路径延迟）成立，成本实测可忽略。
- `RegisterClassW` 重复注册被忽略：进程内二次失败无害。
- `Style` 预计算所有几何常量：render 路径零解析，设计良好。
- overlay 的 `WS_EX_TRANSPARENT|NOACTIVATE|TOOLWINDOW|TOPMOST` 组合：符合
  DESIGN §4.6/§7 的 Z 序与不抢焦点要求。

## 六、验证方法

1. 代码已有插桩（`TAKEKIT_PROBE_LOG`，带毫秒时间戳）：给每个 command 加
   入口/出口打点，跑真实操作，把"感觉卡"的动作对应到确切毫秒。
2. 每项优化前后各跑一轮同一脚本，量化对比。
3. 打点确认 §三 的映射关系——本表是代码路径推断，未逐项复现 UI 操作。

## 七、实施记录（ledger）

执行方式：executing-plans 纪律（连续执行、每任务验证、Ruling 记录），计划即本文档
§四/§五。Ruling 格式：`决定 — 理由 — 代价（若错）`。

### Rulings

1. 不套用 sdd-workspace/task-start brief 脚本 — 计划非 writing-plans 格式，工作模式为
   本工作区未提交状态直接推进（用户既定） — 代价：无 brief 文件，验证靠本文档与套件。
2. P1d（设备缓存）与 P2（JS 优化）合并为同一任务 — 同文件同区域，分开改互相冲突 — 无。
3. P3（status 事件推送）不实施 — status 的 snapshot 为微秒级，轮询成本可忽略；引入
   事件/状态同步的风险大于收益，§四 原文已标"收益小，优先级低" — 代价：保留 250ms 轮询。
4. 无新行为的重构（stop 并行化等）以"既有套件绿 + 编译 + 用户手动验证"为门，不强造
   难测的单测；新行为（缓存、GDI 修复、blit 纯函数）严格 TDD RED→GREEN — TDD 对不可测
   时序壳层无能为力，强测反而测实现细节 — 代价：重构等价性靠 review 兜底。
5. mouseshow 渲染优化只做"GDI 泄漏修复 + blit 批量 swap"；静止帧跳过 / trail 增量 /
   idle 事件驱动推迟 — 拖拽期成本未实测（§六 验证方法），先重构渲染循环违反"先取证"，
   残影/漏画 bug 的代价高于收益 — 代价：拖拽期 CPU 暂不下降，待实测基线后二轮。
6. dda 缓存为进程级、无 TTL — 预热价值随 TTL 衰减；DDA 失效时已有源级失败上报路径 —
   代价：极端场景（录制中独占全屏切换）缓存过期需手动重试。
7. mouseshow 改动前 `git switch main`（当前游离 HEAD，10:2x 教训） — 代价：无。
8. 【事件】实施期间用户清场了 mouseshow submodule（reset --hard + 清未跟踪文件，
   丢弃了一个未提交的 lib 重构版并一度删掉 tests/gdi_leak.rs） — 当前基线为
   ba363dd 干净状态；§五 review 的关键符号（overlay.rs Drop/blit、scene.rs
   points/remove(0)/sqrt/PathBuilder）逐一复核仍在，结论不受影响 —
   代价：gdi_leak.rs 曾重写一次；后续在 submodule 留文件需知会用户。
9. dda/pick 缓存保持"锁内探测"（不做短锁查/无锁探） — DDA 并发会话有 GPU
   上限，并行探测会互相挤占导致缓存假阴性（误判 gdigrab 回退）；锁内探测使
   并发探测天然串行 — 代价：并发不同 rect 时后者最多多等一轮（与无缓存持平）。
10. 主会话内完成 T7 fresh review（subagent 超时 4 次后用户指示改主会话），
    发现并修复：picker 启动初期空态闪现（init 并行 env_info/refreshSources +
    sourcesLoaded 标记）、ScreenRect 全路径收敛为 import — 其余确认无问题。

### 任务状态

| # | 任务 | 状态 |
|---|---|---|
| T1 | P0 GUI 慢命令 async 化（env_info/list_devices/start/stop/status + spawn_blocking） | done（clippy 0 / tests 8/8；另加 preheat_engine 命令与 EnvInfo.screen 字段，为 T3 铺路） |
| T2 | P1a encoder::pick 进程级缓存（TDD） | done（RED E0425 → GREEN，tests 10/10） |
| T3 | P1b dda_available 缓存 + preheat_engine 命令 + EnvInfo.screen（TDD） | done（RED E0425 → GREEN，tests 11/11；JS 端调用并入 T5） |
| T4 | P1c stop() ffprobe 并行化（重构，套件为门） | done（tests 11/11 / clippy 0） |
| T5 | P1d+P2 JS：Promise.all / picker 缓存优先渲染 / renderPicker 显隐切换 / setTimeout 链 / preheat 调用 | done（node --check 0，无 renderPicker/groupLabel 残留） |
| T6 | mouseshow：GDI 泄漏修复（TDD，tests/gdi_leak.rs #[ignore] 本地验）+ blit 批量 swap（TDD 纯函数） | done（RED GDI 0→6 → GREEN gdi_leak ok；swap_rb_copy 2 tests；clippy 0） |
| T7 | 终验：fmt/test/clippy 全套 + fresh subagent 整体 review + 修复 + 文档回填 | done（fmt 本轮触碰文件干净（take.rs 历史基线不碰）；tests 11/11、clippy 0；release build 成功；review 主会话完成并修 2 项） |
| T8 | 用户测试反馈：GUI 每次 spawn ffmpeg 闪现 console 窗口 | done（GUI 子系统的父进程 spawn console 子进程会新建控制台；ffmpeg.rs 集中 `command()` 构造器给全部 5 个 spawn 点加 `CREATE_NO_WINDOW`，含 encoder/DDA/设备列表/时长探测与录制本体；tests 11/11、clippy 0、release 21:08 重建） |
| T9 | 用户测试反馈×2：start 报缺 `out_root`（JS 发 camelCase `outRoot`，serde 精确匹配）+ 参数区五项只读 | done（JS 改传 `out_root`；输出/编码/帧率/质量/点击标记改为可编辑控件——input/select/toggle，样式融入 params 区，录制中禁用；`StartRequest` 加 `#[serde(default)] encoder: Option<Encoder>`；源 fps 在 start 时统一取控件值（改帧率对已添加源也生效）；`node --check` / tests 11/11 / clippy 0 / release 21:27 重建） |
| T10 | 用户设计确认：帧率/质量下沉为源属性（调研后拍板：编码/输出/标记保全局，音频与视频编码正交记 backlog） | done（`Source` 加 `#[serde(default)] quality: Option<u8>` + TDD `effective_quality`（None 回落全局，tests 12/12）；`Settings.fps`/`Manifest.fps` 删除（per-source 后语义消失，sources 自带元数据；CLI 走 `Source::new` 默认零改动）；源行内嵌 `[30▾] [Q20▾]` 小下拉（fps: 15/24/30/60、Q: 16/20/24，录制中禁用）；按 ui-ux-pro-max 指导打磨：select 加 `aria-label`（Critical：紧凑控件语义）、控件面 `--tk-canvas` 与行面区分（双主题可发现性）、focus 走 `.search` 先例 2px outline；`node --check` / clippy 0 / release 21:53 重建） |
| T11 | 用户：输出 input 改路径选择器 | done（加 `rfd 0.17` 依赖 + 同步 command `pick_folder`（模态 Win32 对话框自带消息泵，同步即合理）；输出字段改 input + `…` 浏览按钮（保留手输相对路径，选中回填绝对路径；`aria-label`/title"选择输出目录"；录制中随 `.params button` 一并禁用）；tests 12/12、clippy 0、release 22:07 重建） |
| T12 | 用户：输出默认 `%TEMP%` + 点击标记改图形 switch + **完全重建 UI**（判定上次小调整效果不明显） | done（设计方向经 ui-ux-pro-max 两次检索：首轮 `--design-system` 输出营销落地页/游戏风向 → 判定与桌面工具不适配，按 skill 要求改写重试；`--stack winui` 出可用性要点（accessible name/标题层级）、`--domain style` 命中 Minimalism & Swiss（Best For: professional tools）。**style.css 全量重写**（中性色阶+单一 accent、小标题大写+字距、4/8 节奏、160ms 统一过渡、双主题独立 authored、reduced-motion 全关、统一 `:focus-visible`）；**index.html 语义化重建**（h2 层级、sr-only、switch markup、空态两行层级）；main.js 同步（switch change、%TEMP% 初始化）；Rust `env_info` 加 `default_out_root`（`temp_dir()` trim 尾分隔符）。按项目纪律重定向 WebView2 数据目录启动 + gdigrab 抓屏**肉眼确认渲染**（深色主题/%TEMP%/switch/排版层级全部符合设计）后清理进程与临时目录；tests 12/12、clippy 0、release 22:21 重建） |
| T13 | 用户：`点击标记` 命名不达意（并说明本意该处是倒计时/停止提示的开关）；`编码`→`视频编码`；`输出`→`输出目录` | done（澄清：该 switch 真实功能 = mouseshow 点击涟漪 + 拖动轨迹（`Config{ripple,trail}`），label 改 **鼠标效果** + `title` 给完整说明；`编码`→`视频编码`（编码器确为纯视频通道，音频将独立）、`输出`→`输出目录`；启动 + 抓屏肉眼复核三处文案与布局（浅色主题下验证通过，双主题均已确认渲染）；用户所述本意（参数区 `倒计时` + `收尾面板` 两开关）对应 DESIGN.md §4.4/§4.6 全屏提示层，属未实现设计，待用户排期） |
| T14 | 环境：NVIDIA 驱动 591.86 → 617.42（用户安装）+ 修复 `encoder::pick` 探测尺寸 bug；另：`nvidia-app` MCP 加入用户级配置 | done（`nvidia-app` MCP（remote `http://127.0.0.1:13508/mcp`，已 connected）的 `nvapp_client_get_driver_status` 确认可升 617.42（≥610 满足 nvenc API 13.1），用户安装到位。**bug**：驱动更新后 NVENC 实测可用（ddagrab 探测 exit=0），但 `works()` 用 64×64 探测源，新驱动 NVENC 以 `Frame Dimension less than the minimum supported value` 拒绝 → 可用编码器被误判为不可用、静默回落 libx264（软编）。尺寸实测：96×96/128×128 ✗，192×108/256×256/512×288 ✓ → 探测源改 **256×256**，模块文档写明该约束。验证：GUI env `encoder:"h264_nvenc"` ✓、真实 5s 录制 144 帧/3440×1440 ✓、tests 12/12、clippy 0、release 23:12:57。**顺带发现**：同抓屏路径下 nvenc 0.973x vs x264-ultrafast 0.994x → 瓶颈在 `ddagrab + hwdownload`（GPU→CPU→GPU 往返），非编码器；候选优化「NVENC 直吃 GPU 帧、免 hwdownload 往返」记 backlog） |
| T15 | 用户：录制开始前把需要录制的窗口放到前台（规则已定：**逐个前置**，互不覆盖由用户自己保证） | pending（实现：按源列表顺序对每个窗口源 `ShowWindow(SW_RESTORE)` + `SetWindowPos(HWND_TOP, SWP_NOACTIVATE)`（提 Z 序、不夺焦点、不需前台权限），再尽力 `SetForegroundWindow` 给焦点；置于隐藏主窗口前后皆可。理由：ddagrab 抓 DWM 合成结果，遮挡/最小化 = 录到遮挡物。已写入 DESIGN.md §4.6 时序与"讲究"第 3 条；并入提示层特性实施） |
