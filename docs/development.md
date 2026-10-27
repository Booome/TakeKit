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
- `takekit-core` 有 8 个 `#[ignore]` 实机测试（会开真窗口），`cargo test -p takekit-core -- --ignored` 才跑——**别在用户正用机器时跑**。
- `mouseshow` 的 `tests/gdi_leak.rs` 同样是 `#[ignore = "needs an interactive desktop"]`：**改过 overlay 创建/销毁就手动跑一次** `cargo test --test gdi_leak -- --ignored`（在 `mouseshow/` 里）。
  这条测试确实抓到过「同进程第二次录制报 `Class already exists`」（2026-10-26）——ignore 的测试没有固定运行场合，等于没有。
- 基线（2026-10-28 实测）：cli 9 / core 66（+8 ignored）/ gui 94 / media 37。以 `cargo test` 实际输出为准。
- `docs/tools/check-contracts.py` 四方核对（命令 / 事件 / i18n / **控制面端点↔MCP 工具↔CLI 子命令**），CI 有门禁；改动任何一层的名字前先看它。
- **功能级三端一致性**：`node scripts/check-three-surface.mjs`——真跑 MCP（schema → stdio 调用 → 面板 → 配置）和 CLI 参数。名字对了不等于字段通了：`sound` 曾在 schema 里声明、在 HTTP 转发前被白名单丢掉，`check-contracts.py` 看不见这种漂移。
- 🔴 **每个 GUI 检查跑的是哪个二进制，开局就印出来**（`scripts/lib/build.mjs`，2026-10-28）：
  `TK_GUI` / `TK_CLI` 显式指定优先，否则取 `target/{release,debug}` 里**较新的那个**，并打印
  `using target/release/takekit-gui.exe (built …Z)`；比最新源码还旧就打 ⚠。这条不是装饰：
  此前默认写死 `target/debug`，一个两天前的 debug 二进制让 `check-midtake` 报
  `a-window-that-closes-is-stopped-without-the-panel` FAIL（`probe-source:vanished=false`），
  换 `target/release` 同一棵树 15/15 —— **测错二进制时，结论长得跟代码有 bug 一模一样**。
- **界面语言×主题**：`node scripts/check-ui-matrix.mjs`——zh/en × light/dark × 9 屏（源列表、卡片、最小化窗口的卡片、裁剪源卡片、裁剪工作区、裁剪被丢弃的卡片与工作区、添加源、设置），量横向滚动、文本溢出、WCAG AA 对比度，截图落 `target/checks/ui-matrix/`。它就是抓出「`.field button.browse` 选择器从未匹配、深色下 `…` 对比度 1.02」的那一个。
  - 它还钉住**尺寸列的口径**：裁剪放不下时，列表 / 卡片 / 工作区三处都必须报「整幅」。这条来自一次真事故——后端 `CropBox::rect_in` 会丢掉放不下的裁剪、整帧原样录，而列表当时照算式印 `1720×1392 → -2280×1392`，报的是一个永远不会存在的文件。**算术正确 ≠ 说得对**，凡是由算式得出的展示值，都要问一句「后端实际会给什么」。
  - 以及**行内空隙**（`row-summary-tight`）：帧率/画质格必须按文字自适应，且离尺寸列只隔 12px。定宽列装不满的那截空白不会溢出、不会截断、不会有滚动条，只有截图看得出来——用户先看到，测试才补上（2026-10-23）。
  - 以及**右侧三列是一条信息线**（`row-speaker-last` + `row-trailing-columns-line-up`）：喇叭必须在帧率之后、在最右；行从右往左排，所以任何宽度可变的格子落在数字左边都会让整列按行横移——不能出声的行因此保留一个同宽的**空槽**（`row-sound-slot`）而非少一格。这两条是同一个教训的两面：**右边的列只有在宽度不随行变化时才成列**。
  - 还会**自己造一个最小化的窗口**（WinForms 表单 + 最小化，脚本里 `probe` 那段）来测「没画面就不给裁」与「恢复窗口后能裁」；行标题则靠塞一个假标题（`STALE …`）来验证「显示的是窗口现在的名字」。这两条都是 2026-10-23 用户报的问题，靠读代码看不出来。
  - 以及**喇叭的作用域**（`row-speaker-scope`）：window 与 desktop 行必须有喇叭，mic 与 camera 行必须没有。判断种类用 `row.dataset.kind`（产品代码为测试加的语义钩子），不匹配图标 SVG——图标一改，断言就悄悄失效，那是最糟的一种绿。
  - 以及**暂停态只有一个颜色**（`paused-look-is-one-colour` / `paused-badge-carries-the-mark` / `paused-hint-agrees-with-the-badge`，2026-10-27 加）：把 `state.paused` 摆成真、`render()` 之后，(徽章字色 == 计时字色 == 继续键底色) 三处必须相等，徽章必须带那个用 `::before` 画的 ▮▮（不用 U+23F8，字体一缺就是豆腐块），状态条 hint 必须与徽章说同一件事。
  - 🔴 这组断言写了两遍才立住，**两次都是测太快**：`.pause` 有 160ms 的 background 过渡，`render()` 之后立刻读 computed style 拿到的是**过渡起点**（报 `rgb(255,255,255)`，截图却明明是琥珀）；改成 `sleep(260)` 又红，因为面板 tick 链每 ~1 秒 `invoke("status")` 再 `paintTransport()`，把脚本摆的 `state.paused` 用真实状态（false）覆盖回去，`body` 上根本没有 `paused` 类。**正解不是加长等待，是等到「条件成立」再采样**：12 次重断言 + 每次 190ms，`body.className` 里出现 `paused` 才 break 并当场截图。截图放在循环之后同样会拍到下一个 tick 之后的空闲态，必须移进循环内。
- **录制中加源的真路**：`node scripts/check-midtake.mjs`——15 条断言，唯一**真录一段**的检查。它走的路是：控制面 `POST /record/start`（面板没参与）→ 面板必须自己认出「正在录」→ `rec-btn` 显示停止、每行带起点 → 点 `#add-btn` 列表必须出来 → 点一个源必须**真加入这一条 take**（`status.sources` 1→2、多出一行带起点）→ **关掉那个窗口必须在不靠面板的情况下被停掉**（见下）→ 停下后 `take.json` 两个 `start` 都在、后加的那个 `> 0`、没被切开的源不带 `-part`。
  - 最后那条（2026-10-27）验的是 Rust 侧 `take::start_source_watch`，而它**不能靠「`alive:false` 出现」判**：采集自己在窗口消失时也可能放弃，两条路径的终态一模一样。所以先 `Page.setWebLifecycleState({state:"frozen"})` 把页面冻住（附着 DevTools 本身就在阻止隐藏页面被节流，不冻就永远是面板的 250ms tick 先赢），再用**控制面** `GET /state` 轮询，最后要求探针里有 `source:vanished`——那行只有 watch 会写。
  - 为什么另起一个检查：`check-ui-matrix` 里那条 `add-source-opens-while-recording` 是**把 `state.recording` 设成 `true` 再点按钮**，它证明渲染，不证明门。2026-10-24 就正因如此，一个「录制中点了添加源没列表」的面板通过了 151/151——`openPicker` 里留着 `if (state.recording) return`，而这条测试替它把状态摆好了。**凡是「状态由别处产生」的功能，测试都得从产生那一端开始。**
  - 同一次还查出一个真 bug：`state.recording` 只有 `toggleRecording` 会写，所以控制面 / CLI / MCP 起的那条 take 面板一直以为自己是空闲的（栏里写着「开始录制」，没有任何一行有起点，此时选中的源会进「下一次」而不是「这一次」）。修法是在 `ctl:changed` 里用 `status` 对账（`adoptStatus`）。
- **暂停这条路**：`node scripts/check-pause.mjs`——13 条断言，真录一段带暂停的素材（`/record/start` → 1.2 秒 → `/record/pause` → 2 秒 → `/record/resume` → 1.2 秒 → `/record/stop`），源是一个每 30 ms 换一次颜色的窗口（`TopMost`，否则被面板盖住）。验的是：暂停被接受（`paused:true`）、`warning` 为 `null`、**一次暂停恰好两个文件**、每个文件的 `probed` 与记录相差 ≤ 0.15 秒、记录里的每个文件名都真在目录里、`paused` 入账 > 1.5 秒、**两个文件都带编号**（`-part1` / `-part2`）、源的顶层 `file` 指向第一段。对应的反面在 `check-midtake` 里：**没被切开的源一个编号都不带**——编号只用来对齐一组，没有组就不该出现。
  - 后四条（2026-10-27 新增）走 CDP 连**托盘 popover**（`TAKEKIT_TRAY_DEMO=1`，端口 9251）：卡片要写「已暂停」、要报出**这段已经多久**并明说「不计入录制」、点一次/(dot、clock、state) 三处必须变同一个琥珀色、继续键必须在（能走回去）。它是**第二个窗口**，`check-ui-matrix` 同样看不到它。
  - 🔴 两处「测太快」的代价（用户 2026-10-27 直接指出报了 `这段 NaN:NaN`）：① `status` 命令的载荷是 **snake_case**（`pause_len`），而面板里的 `state` 对象是 camelCase（`pauseLen`）——我按后者取了字段，`undefined` 一进 `shortClock` 就成 `NaN:NaN`。② 采样只等 700ms，读到的是 `0:00`——**一个永不递增的计数器也会通过同一条正则**，所以断言改成「解析出秒数 ≥ 1」并把等待放到 1.4 秒。正则匹到形状不等于数对。
  - 🔴 它的诚实边界（2026-10-27 实测）：**这个 fixture 复现不了那个 bug**。把修复回退，它照样 7/7——WGC 在暂停期间对这块窗口不投递帧。它守住的是暂停链路的其余部分，**帧风暴的决定性测试是 `takekit-media` 的 `frames_that_arrive_during_a_pause_are_dropped`**（手工喂帧，去掉守卫就报 12 段）。别把「脚本全绿」当成「那个 bug 有覆盖」。
  - 这个 bug 的形状值得记住：暂停期间采集不停，还在重绘的窗口每帧都把时间戳交在冻结时刻之后；writer 把它写下去、并且因为队列已被清空而为它开新文件，恢复时 `cut_at` 又是同一时刻——**一帧一个文件**。5 秒暂停留下 16 个 0 秒段，首段被反复重开只剩 1 帧（`probed 0.0333` vs `3.33`），面板于是报 `1 个文件的实测长度与 take.json 不符`。**静态窗口永远碰不到**，所以它躲过了所有截图检查。
- **托盘 popover 的布局**：`node scripts/check-tray-layout.mjs`——13 条断言（4 个组合 × 两行两列 / 无裁切 / 底部内边距 / 窗口=卡片）。它是**第二个窗口**，面板在屏时不打开、只有点托盘图标才出现，所以 `check-ui-matrix` 一条都看不到它：2026-10-27 连着两个版本都在这上面翻车，第一个把第二排按键切掉一半，第二个（本意是分两行）让四个键挤成一行、标签在 50px 里折成两行。
  - 手法：`TAKEKIT_TRAY_DEMO=1` 启动后 1.5 秒自己 `raise` 一次 popover（与 `TAKEKIT_OVERLAY_DEMO` 同批的 PROBE 脚手架——不能点就先给个入口），CDP 读 `getBoundingClientRect` 判断结构，`Page.captureScreenshot` 逐组合落 `target/checks/tray-layout/`。
  - 它顺带钉住一条 Windows 坑：`set_size` 在 tao 里走 undecorated-with-shadow 分支，会把**调用前**测到的框偏移加进去。新建的无边框窗口偏移是 39px（30px 隐藏标题栏 + 真实 9px 边框），首次 resize 之后才缩到 9px，于是「请求 152」落地成 182——而页面报的高度没变，`fit` 又因为「没变化」跳过，那 30px 就一直留着。现在 `resize` 是 ask → measure → ask（同一个高度再请求一次），不再给框建模。
  - **两行两列用 grid 写死，不用 flex 折行**：四个 `flex-basis: 0` 的键在任何宽度下都「放得下」，折行永远不会发生——折行是结果，不是约束。
- **收尾面板（遮罩）本身**：`node scripts/check-stop-screen.mjs`——20 条断言。遮罩是**第三个文档**，`check-ui-matrix` 一条都看不到它（面板不在屏上时它才在），而它 2026-10-27 连着改过五件事：寿命 2 秒 → 1 分钟、可点击退出、删掉第二声提示音、**清单改成预留槽位**、**清单改成四列表格**（详见下两条）。验的是：页面完整（含新挂的 `i18n.js`，脚本一坏连倒计时一起垮）、提示文案跟随语言、提示条在窗口内且不与清单重叠、**点击真的到 Rust**（读探针 `overlay:dismiss-click`）、倒计时期间点击不算数、`take:done` 才是拉起它的那个事件、**逐条落行时 STOP 与计时一动不动**（`drift=[0,0,0,0]`，修复前是 `[-12.8,-28.6,-44.4,-60.2]`）、**四条行的三列数值落在同一条竖线上**（`names 0px apart, numbers 0px apart per column [0,0,0]`，旧 flex 布局是 `[114.1,179.7,179.8]`）、空格子有内容（`—` / `m:ss` / `GB`）、失败行从时长列一路占到行尾。
  - 🔴 **右对齐的列，那条竖线是「右边界」**：第一版断言把三列的 `left` 混在一起比，得到 192px 的「不齐」——布局其实是对的，`1:24` 与 `01:02:00` 起点本来就该差 4 个字符。量右对齐的列只能比 `right`，按列分组比，别跨列混算。
  - 🔴 **`subgrid` 在这台 WebView2 上不生效**：写 `grid-template-columns: subgrid` 不会报错，行内直接退化成 auto 列——比不做还歪（实测 232px）。所以 `#list` 与 `.row` 各写一次轨道，共用一个 `--tk-cols` 自定义属性。
  - 手法：`TAKEKIT_OVERLAY_DEMO=1` 抬起遮罩（顺便拍照，`target/checks/stop-screen/`，含「四个槽只填了一个」的中间态与一张四列表格的成品），CDP 连 `overlay.html`。
  - 🔴 **「Tauri 事件没有从页面注入的口子」这个判断是错的**（2026-10-27 推翻）：页面里调 `window.__TAURI__.event.emit(name, payload)` 会经 Rust **广播回来**，落在**同一个页面的 `listen` 回调**上——这正是真实停止走的同一条路，所以测的就是线上跑的那个 adapter，不是复制品。上一版只能手摆 DOM，等于测自己写的复制品；现在能真发 `take:finalize` / `take:source` / `take:done`。只有「谁拉起它」这类**调用点在 Rust** 的事实仍需静态断言，两半都要。
  - 静态断言别用「距离」当判据：`armed-by-done-only` 原来写成「`take:finalize` 之后 600 字符内出现 `stopScreen = false`」，给那个 handler 补注释就把断言写红了（代码没坏）。现在按 handler 切块（`listen("x"` 到下一个 `\n});`）再查内容。
- **`TK_GUI` 指向出货件**：六个脚本都认这个环境变量（`TK_GUI=target/release/takekit-gui.exe`）。debug 绿 ≠ 出货绿。
  - 🔴 **2026-10-27 发现这条有过一半是假的**：`check-midtake.mjs` 与 `check-three-surface.mjs` 的 `GUI` 常量
    **硬编码成 `target/debug`，不读 `TK_GUI`**。于是所有「`TK_GUI=... node scripts/check-midtake.mjs`」
    其实一直在跑 debug 二进制——命令看着对、日志照样绿，被验证的东西根本不是准备出货的那个。
    发现方式很典型：新加的断言（Rust 侧源失效监视）对 release 是绿的、对这轮跑出来是红的，
    于是去追「为什么探针一条都没有」，最后发现**二进制里压根没这段代码**（`grep -c "source:watch-on" target/debug/...` = 0）。
    **教训不是「记得改脚本」，是「说自己在测出货件之前，先证明跑的是出货件」**——一条
    `grep -c <新加的探针字符串> <binary>` 就能证，比读脚本快。
- **`control.json` 的 pid 要对得上**（2026-10-27 踩到的坑）：脚本里 `rmSync` 清测试目录失败时，上一轮留下的 `control.json` 会活下来，于是 `POST /record/start` 打到**上一个实例**的端口——本次进程一条 `begin:` 探针都没有。`check-ui-matrix` 与 `check-midtake` 现在都把 `control.pid === gui.pid` 写进 `panel-start`，清不掉目录就直接退出并说明原因。**「所有请求都成功但产品毫无反应」= 先确认对面是谁。**
  - 而 `rmSync` 会失败的原因查清了：宿主的 safe-delete 守卫**也注进了 node 进程的 fs**（不只 PowerShell 的 `Remove-Item`），批量删超过 50 个文件一律 `SAFE_DELETE_BULK_CONFIRM_REQUIRED` 拒绝，**重试无用**。所以「清目录」这个动作本身要改掉：只删真正会导致假阳性的那几个文件（`check-ui-matrix` 是 `appdata/TakeKit/control.json` + `probe-handle.txt`，其后的脚本是 `probe.log`），截图和 `wv2/` 交给覆盖与复用。别用「手动 `node -e` 能删」当反证——那次命令沙箱被绕过，守卫没生效。
- **隔离运行**（不碰用户真实配置）与 WebView2 双缓解：见 `docs/webview2-agent-sandbox.md` §4（唯一出处，配方与两条铁律都在那里）。

### 2.1 界面矩阵钉住的规则（2026-10-23，从 README 搬来）

`check-ui-matrix.mjs` 与 `check-three-surface.mjs` 的断言数**跟着「屏幕 × 语言 × 主题」增长**，
别把某一次的数字抄进文档当常量（2026-10-25 有 123 这个数，同一批断言在加屏幕后就变了；
`check-three-surface.mjs` 也从 14 条涨到 17 条 —— 2026-10-27 数 `record()` 调用，包含本轮新增的
`cli-addlive-sound-flag`）。判据是脚本自己那行汇总全绿（`N/N`）+ `low=0 spill=0 hScroll=0`。
输出里 `trunc` 是省略号截断（设计意图，不算失败），`low` 是实测对比度
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
| `tray/both-buttons-open-the-popover` | 托盘图标的左键与右键都接 `tray_panel::toggle`，且没有原生菜单插进来。图标在系统通知区、不在任何 webview 里，UIA 也点不到（§5.2），所以只能静态验：匹配 `MouseButton::Left \| MouseButton::Right` + `show_menu_on_left_click(false)` |
| `settings/shortcuts-have-their-own-group` | 两颗 chip 在**同一个**「快捷键」组里（组名从 chip 上方最近的 `.set-label` 读，和页面自己的读法一致），且该组里没有 switch。分组错了截图看不出来——两颗 chip 照样都在页面上，只是挂在错的标签下面 |
| `hotkey/a-refused-key-is-reported` | 用 pause 键去抢 toggle 键自己的组合，`RegisterHotKey` 必然拒绝；然后 `hotkey_status` 必须**记住**这次拒绝。这条防的不是注册失败（那由系统保证），是**失败被吞掉**——配置里照样写着 `F9`，界面照样画着 `F9`，用户在需要它的那一刻才发现没生效 |
| `hotkey/the-card-shows-a-refused-key` | 上半段的失败要真的走到卡片上：chip 有 `.warn`、下面那行换成「`{key}` 没能注册」、`title` 带着注册器的原文（`HotKey already registered: …`）。构法是**把 chip 读的那个 state 改成被拒的组合**——这正是启动时 `applySaved()` 把配置值写进去、而注册早就失败了的真实状态。查完把两个键都恢复，否则检查本身就在制造「一个组合两条绑定」 |
| `paused-look-is-one-colour` + `paused-badge-carries-the-mark` + `paused-hint-agrees-with-the-badge` | 暂停态只说一个颜色：徽章字色 == 计时字色 == 继续键底色，徽章带 `::before` 画的 ▮▮，状态条 hint 与徽章同一件事。2026-10-27 之前计时仍是灰色 `--tk-text-3`、暂停键是白描边，琥珀只落在徽章上——**三处各说各话**。⚠️ 采样必须等 `body` 上真出现 `paused` 类：160ms 过渡 + 每 ~1 秒的 `status` 轮询都会让你读到过渡起点或空闲态 |
| `tray/icon-*`（Rust 单测，在 `tray.rs`） | 托盘图标三态靠**形状**分辨，不靠颜色：灰点=空闲、红点=录制、**琥珀双竖条**=暂停。任务栏只给 16px，琥珀点与红点在那个尺寸上无从区分。断言直接读像素（中心透明、两柱有色、包围盒近似正方形），`#e0a11e` 是给深色任务栏手调的值，不等于 `--tk-warn` |
| `naming/window-id-is-the-app` | 窗口源的 id 是 app 名，**两扇门都算**：面板 picker 走后端 `window.app`（`uniqueId(app)`），控制面走 `unique_id(existing, &base)`，两处都从同一个 `w.app` 取材。只改一处，同一个窗口按添加路径不同会得到两个文件名——这正是两套 id 生成器对齐时要消灭的那种静默分叉。段名 `-part2` 由 `takekit-media` 的单测 `a_segment_is_named_apart_from_a_second_source` 钉（它挡的是「一个 mic 停一次」与「两个 mic 不停」写出同一组名字）；**切开的源第一段也要带编号**由 `a_split_numbers_every_part_including_the_first` 钉（同一个测试文件里，真建临时目录真改名：一段不动、两段从 `win.mp4` 变 `win-part1.mp4`、重复调用不产生 `win-part1-part1.mp4`） |

两脚本都以 `target/checks/` 为隔离 APPDATA，**跑之前先关掉正在运行的面板**（单实例会让第二个进程
直接退出，脚本拿不到控制端口）。新增界面加进 `SCREENS`，新增语言/主题扩 `LANGS` / `THEMES`。

**这条现在是脚本自己拦（2026-10-27）**：五个起面板的脚本都先 `await` `scripts/lib/panel-guard.mjs` 的
`assertNoPanel()` —— 有面板在跑就直接退出（码 2）并打印 pid 与 `taskkill` 命令，而不是等 50 秒后报
`panel-start port undefined pid undefined`（那句话离真因隔了三步）。它**先等最多 10 秒**再判定：
`taskkill` 返回时进程对象还在，上一个检查刚收工、下一个立刻起面板时每次都撞上，报的是「已死进程」——
`check-midtake && check-pause` 连跑踩了这个（2026-10-27，`ours 41304` 但 `tasklist` 已经空）。4 秒不够：
前一个面板的 **WebView2 子进程**比 `/T` 杀掉的父进程活得久，五个检查连着跑仍复现，所以拉到 10 秒。
  - 🔴 **等不是根治，重试才是**（2026-10-27 二次复发）：`assertNoPanel` 只能证明 `tasklist` 里没有进程，
    不能证明上一个进程**已经放手**。在「`taskkill` 返回」和「最后一个句柄关闭」之间有一道缝：
    新进程要么输给 `-sim` 互斥量、要么打不开旧进程子窗口还占着的 WebView2 profile 目录，于是**静默退出**，
    `control.json` 一个字节都不写。所以四个要控制端口的脚本改用 `startPanel(launch, appDir)`：
    起进程 → 等 `control.json`（pid 必须是自己）→ 进程若已退出或 20 秒没出来就**重来一次**（最多 3 次）。
    判据从「等够了没有」换成「起来了没有」。半分钟的空等从此不会发生，假红也不会。**为什么不能给脚本一份自己的实例**：
`tauri-plugin-single-instance` 在 Windows 上用 `{identifier}-sim` 命名互斥量（`platform_impl/windows.rs`
L66-75），identifier 来自 `tauri.conf.json`，而 builder 只开放 `dbus_id`（Linux 专用）——**没有运行时换 key 的口子**。
隔离 APPDATA 只隔离了配置文件与 WebView2 profile，隔离不了这个互斥量。
守卫**故意不杀**：一个开着的面板揣着别人配好的源列表（`config.json` 不存 sources），检查脚本没有资格把它扔掉。

**开发约定**：沟通中文；代码、注释、标识符一律英文；`docs/` 下文档用中文。新行为走 TDD（先写失败测试）。不自动 commit / push——只有用户明确发话才提交。**开发期间 agent 可以自行结束正在运行的 `takekit-gui`**（用户 2026-10-27 授权，不必逐次确认）——单实例互斥量是全局的，不结束它任何 GUI 检查都跑不起来。

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
       → take:done → 遮罩**不立刻收**（`finalize_panel` 开时）：
            set_clickable(true)（撤掉 WS_EX_TRANSPARENT，页面才能收到点击）
            state.stop_screen = true + 起一个 1 分钟看门狗线程（STOP_SCREEN_SECS）
            主窗口**先不回来**——它在遮罩后面，提前显示只会白闪一帧
       → 点击（页面 pointerdown → overlay_dismiss）/ 看门狗 / 下一次 begin
            → dismiss_stop_screen()：收遮罩 → show_main_window()
       ⇒ 幂等：三条路都走同一个 swap(false)，所以只会关一次
       ⇒ 收尾写入期间（take:finalize ~ take:done）不可点、也没有第二次提示音
       ⇒ 写入期 `begin` 仍拒绝（`err.finalizing`），但**遮罩停留期不再拒绝**：
          begin 会先把遮罩收掉（不带面板），失败时再把面板叫回来
  → main.js showSaved()：failed>0 才弹浮窗；随后 refreshJournal()
```

**界面的四条不变量**（2026-10-27，`scripts/check-ui-matrix.mjs` 有断言）

1. **运输栏跟着 take，不跟着页签**：不录制时除「录制」页签外整条不显示；录制中浮在**所有**页签，
   但只带计时、暂停、停止——源数量属于它数的那张列表，在别的页签上隐藏。
2. **暂停紧挨停止**：同一个 `.transport` 组，同高（38px）、8px 间距、靠右。它是次级按钮
   （描边 + 文字），停止仍是唯一的红键。录制中暂停态时，徽章 / 计时 / 继续键三处同时转琥珀。
3. **一个状态，三个窗口都得说**：暂停这件事面板、托盘 popover、托盘图标各有一个表达面，
   而且都在**自己的尺寸约束**里做——面板有宽度，popover 有高度，任务栏只有 16px。
   所以图标不用换色而用换形状（见上表 `tray/icon-*`），popover 多一行「这段 0:12 不计入录制」。
   **只改一处就是用户说的「提示不充分」**（2026-10-27）。
4. **所有可滚区域共用一个 `.scroll`**（`.source-list` / `.take-list` / `.picker-body` / `.pane-scroll`）：
   同样的 10px 槽、3px 透明边距的 4px 圆角滑块、透明轨道。**加新的可滚容器就挂这个类**，
   不要再写一份 `::-webkit-scrollbar`。

**录制中动态加源（2026-10-24）**

```
面板「添加源」/ 托盘「添加源」/ `ctl add-live` / MCP `takekit_record_add_source`
  → POST /record/sources（或 tauri 命令 add_live_source）
  → Recording::add_source(Source)
       ├─ take.json 先落一行（start=null，added_at=现在）：中途崩了也看得出这条源存在过
       ├─ 复用 take 的同一个 TakeClock 起采集/编码器；writer 用 Origin::FirstSample
       │   （文件从自己的第一帧起算，不补黑：一小时处加的源补黑要白编 10 万帧，
       │    而且那段时间实时帧只会在有界队列里排队被丢掉）
       ├─ 第一帧的 take 时间经通道回传 → 写进 start、立刻重写 take.json → 返回给界面
       └─ 失败只回滚这一条源（清掉刚写的行），整条 take 照旧录
  → 停止时每份文件 ffprobe 复核：probed 与 duration 差 > 一帧半 → 打 `len.mismatch`
```

**take.json 的每条源都有起点**：`start`（该文件在本条 take 时间轴上的秒数）+ `duration`（文件
长度）。`start + duration ≈ manifest.duration`，这是可以机械核对的恒等式；`start_clock` 说明
`start` 怎么来的——`frames` = 首帧自己的时间戳（Windows 后端，准），`spawn` = ffmpeg 子进程
启动时刻（该后端看不到帧，误差 = 启动延迟）。`added_at` 只在录制中加入的源上出现，它和 `start`
的差就是设备唤醒花了多久。

**暂停与分段（2026-10-24）**

```
面板「暂停」/ 托盘 / ctl pause / MCP takekit_pause_recording
  → POST /record/pause
  → Recording::pause()
       ├─ TakeClock::pause()：钟一停，之后盖的时间戳全落在同一点
       └─ 各 writer 在循环顶部看到 is_paused()：
            先 try_recv 排空队列（这些帧是暂停前的，属于本段）
            → close_segment(frozen) 收尾当前文件
            → 之后只丢帧，不写盘；采集线程不停（这是恢复零延迟的原因）
  → POST /record/resume
  → TakeClock::resume()：把这段暂停时长记入 paused_total
       └─ writer 用**切点**（不是"发现钟又走了"的时刻）开下一段；
          窗口没重绘时用持有的最后一帧开（WGC 是变化驱动的，不重绘就没有新帧）
  → 停止时 ffprobe 逐段复核；`paused` 与 `segments` 写进 take.json
```

**为什么不用"停止后再切文件"**：MP4 只能切在关键帧上，而 `open_for` 没有显式设 GOP（默认
几秒一个），`-c copy` 的边界会被挪到最近的 IDR；精确切只能重编码。录制时就切，段边界落在
文件第一帧，天然是 IDR，零误差，也不占双倍磁盘。

**只有 Windows 后端能暂停**。ffmpeg 子进程各自计时（`start_clock: spawn`），挂起进程只会让
设备缓冲溢出、恢复后突发涌入，那不是暂停而是把时间轴搞乱——所以那里明确拒绝。

ffmpeg 后端的数据流差异：`begin` 里每个窗口源 `bring_to_front` → `Session::prepare`
带最长 2 秒的编码器/引擎探测 → 每源 spawn 一个 ffmpeg 子进程（详见 `source.rs` 的
`input_args`）。

---

## 4. 实现契约（改前必读，踩过坑才定下的）

0. **一条 take 只有一条时间轴**。`TakeClock` 在第一个采集打开之前建立，之后**所有**源（包括
   录制中途加的）都用它，每份文件的 `start` 必须来自它自己的**第一帧**而不是用户点击的时刻。
   一个源一个钟，每个偏移就只对它自己成立，`take.json` 也就不再是可核对的东西。

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

`docs/tools/` 下**除 `check-contracts.py` 外**整目录在 `.gitignore` 里（Win32/UIA 驱动脚本，随环境漂移；
`check-contracts.py` 是纯静态分析、跨平台且 CI 要跑，`.gitignore` 里为它开了 `!` 例外）：
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
   ~~修法见 §11.2，未做~~ → **已修（2026-10-27）**，见 §11.2 与下面「2026-10-27 技术债清理」。
3. ~~待拍板 5 项~~ ✅（2026-10-23 全部拍板并落地：单实例二次启动聚焦已有窗口；提示层创建失败降级为
   "无提示层 + 记探针"；历史「清空」连磁盘文件一起删（删不动的行保留）；CLI `--marks` 与
   UI「鼠标效果」文案对齐；ffmpeg 路径可配置（设置行 + CLI `--ffmpeg` + 控制面 `ffmpeg_path`）；
   另修历史行双击开两个资源管理器窗口的节流）。
4. 未实测：150% 缩放、多显示器双屏真机、深色主题下历史页浮窗/徽章观感。
5. ~~B4 收尾流程~~ ✅（2026-10-21：`take:finalize|source|done` 事件流 + 提示层逐源清单 +
   状态条 `正在写入… n/m`，「已保存」横幅按口径不做；`finalize_panel` 开关已进设置）。
6. ~~状态条 failed 态~~ ✅（2026-10-21：菱形 + 原因单行 + 查看日志）。
7. ~~光标/多屏对齐~~ ✅（2026-10-21：`WithCursor` 实测光标进画；picker 逐屏 + rect→Monitor）。

### 2026-10-27 技术债清理（用户点名「逐个做了」）

七条一起清的，新代码都有测试或检查钉着。**仍留一条开放项**（最后一行）。

| 项 | 落点 | 钉住它的东西 |
|---|---|---|
| 关窗丢最后一次编辑 | `persist()` 防抖 300ms→0（同 tick 合并）+ `flushPersist()`；Rust `CloseRequested` 不再直接 `exit`，先 emit `app:quitting`，前端 flush 完回 `quit_now`，**400ms 兜底**（webview 卡死也必须能关） | 靠 `config.json` 的 mtime/内容人工验；无自动化 |
| 倒计时挂住无法退出 | `take::start_countdown_watchdog`：`secs + 5s` 后 pending 还在 → `drop_pending` + `err.countdown-lost` | 未做自动化（要能人为让 overlay JS 不响应） |
| DDA 判定永不过期 | `ffmpeg::dda_forget()`，在 DDA 选中的 take **spawn 失败**时清该 rect 的 verdict | `forgetting_a_verdict_leaves_no_answer_behind` |
| 热键注册失败无入口 | `proto::HotkeyStatus` + `AppState.hotkey_error` + `hotkey_status` 命令；设置页 chip 转琥珀 + hint 换成「`{key}` 没能注册」 | `hotkey/a-refused-key-is-reported`、`hotkey/the-card-shows-a-refused-key`（用 pause 抢 toggle 的组合造真实拒绝） |
| 源失效判断依赖隐藏面板的 250ms 轮询 | `take::start_source_watch`：Rust 侧 500ms `try_lock` 轮询，失效即 `stop_source` + emit `source:stopped`；前端 `forgetSources()` 只负责移出清单 | `check-midtake::a-window-that-closes-is-stopped-without-the-panel`（冻页面 + 控制面轮询 + 要求 `source:vanished` 探针） |
| 音频重投后永久错位 | `AudioWriter::write` 的 `place()` 三分支 + `skew_base` 基线 | `a_redelivered_chunk_is_dropped_and_a_real_hole_is_filled`、`a_standing_clock_offset_is_learned_not_repaired` |
| ~~窗口源 pid 校验~~ | **早已实现**（`resolve_window` / `owns_window` / `window_is_alive`），backlog 条目过期 | — |

🔴 **本轮最值钱的一条教训**：`place()` 的判定**不能直接用绝对 skew**。WASAPI 时间戳与字节累计
不在同一绝对基准上，设备时钟偏 200ms 就会让每个 chunk 都被判成空洞、文件被静音撑爆——比原来的
bug 更糟。判据必须是**相对首个可测量偏差的变化量**。这条只有在写第二条单测（50 个连续偏移）时
才暴露出来。

🔴 **第二条**：`check-midtake` / `check-three-surface` 其实一直不读 `TK_GUI`，过去所有「对 release
跑过」的说法对这两项是假的（详见 §2「`TK_GUI` 指向出货件」）。新加的断言对 release 是绿的、对这轮
跑出来是红的，才把这件事翻出来。

**开放项**：`docs/development.md` §7 #4 与 §11.1 的「未实测」（150% 缩放 / 双屏真机 / 深色历史页
观感）一直没跑，属硬件条件受限，不是没时间。

**性能 review**（2026-10-23）：录制热路径已过一遍。帧通道原为无界 `mpsc::channel`，与"队列满则丢帧"
的注释矛盾 —— 写入端落后时帧会在内存无限堆积（4K BGRA 帧约 33 MB）。已改为有界 `sync_channel` +
`offer()`（满则丢帧、writer 断开才停采集），视频队深 3、音频队深 64
（`takekit-media/src/frame.rs`）；KeepLatest 采样顺修积压时取最新帧而非队头帧（`newest_of_burst`）。
其余热点（250ms tick 链、`spare` 复用、`copy_frame`）已是优化后形态，无改动。
