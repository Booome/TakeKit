# WebView2 与 agent 沙箱：实测结论与操作规程

> 写于 2026-10-19。全部结论来自本机实测 + 官方文档 + 同类案例，不是推测。
> 读者：在本机（或任何注入了 DLL 的 agent/安全软件环境里）运行、验证 TakeKit 的人。

---

## 0. 一句话

在 agent 沙箱里跑 TakeKit 面板需要**两个**缓解，缺一个都起不来或活不久：
**`WEBVIEW2_USER_DATA_FOLDER`**（否则建不出 WebView2 环境）和
**`TAKEKIT_WEBVIEW_NO_SANDBOX=1`**（否则渲染进程被注入 DLL 打崩）。
两者都只需在验证时设置，**正式构建与用户手动运行都不需要**。

## 1. 环境事实（先接受，再绕过）

| 事实 | 证据 |
|---|---|
| 沙箱往进程树注入 `tsbx.dll` | `Get-Process takekit-gui` 的模块表里直接看到 |
| 沙箱给每条命令注入 `HTTP_PROXY/HTTPS_PROXY`（指向命令结束后就销毁的随机端口） | `Start-Process -Redirect*` 因大小写双变量键冲突报错，反证其存在 |
| 命令验证层拦 `Add-Type` / `Start-Process`+嵌套 `powershell -File` / `Remove-Item -Recurse` | 本次多条命令被拦的实测 |
| **命令结束后，沙箱会延迟回收它启动的进程树** | 见 §5：轮询中的实例活 60–240s，无人轮询时 10–60s 内死亡 |

## 2. 两个缓解各自解决什么

### 2.1 `WEBVIEW2_USER_DATA_FOLDER=<可写目录>` — 不起作用的兜底

不设置时，WebView2 默认把用户数据目录建在 exe 同级目录（或 `%LOCALAPPDATA%`），
沙箱阻断该写入 → 第一个 WebView2 窗口（托盘面板之后的 overlay）创建失败 →
`setup` 的 `?` 把整个 app 带崩。探针特征：**停在 `tray:panel-ready`，没有 `overlay:ready`**。

官方依据（WebView2 IDL 文档）："WebView2 creation can fail if the executable is
running in a directory that the process doesn't have permission to create a new
folder in."

### 2.2 `TAKEKIT_WEBVIEW_NO_SANDBOX=1` — 渲染进程崩溃

只设 UVDF 时：app 能起来、overlay 页能加载完，但**主页面加载中途整个进程原生死亡**
（无 Rust panic、退出码 1、事件日志无记录——是 SEH/FAST_FAIL，不是逻辑错误）。

根因与社区案例完全同型：

| 案例 | 崩溃码 | 机制 |
|---|---|---|
| 天锐绿盾 DLP（CSDN 实录） | `0x80000003` FAIL_FAST | Detours 注入触发渲染进程**代码完整性自检**失败 |
| MacType（Tauri foliole） | `SBOX_ERROR_CANNOT_SETUP_INTERCEPTION_THUNK` | 注入 DLL 改了 ntdll 入口，sandbox thunk 装不上 |
| Appeon 社区 | `-1073740760` STATUS_INVALID_IMAGE_HASH | 渲染进程被注入未签名 DLL |
| **TakeKit 本机（10-18 记录）** | `0x80000003 @ msedge.dll` | 同一机制：未签名的 `tsbx.dll` |

微软官方《防止安全工具阻止 WebView2 应用》对此有明文：*"DLL injection into
renderer or GPU processes breaks Chromium's sandboxing model, and commonly causes
renderer crashes"*，并建议安全软件改用 Edge 安全连接器。

**本机的解法**（与 DLP 案例一致，且经 240s 稳定性实测）：

```
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS =
  --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity
  --autoplay-policy=no-user-gesture-required
  --no-sandbox
```

- `--no-sandbox`：禁 Chromium 自身沙箱，注入不再冲突；
- `--disable-features=RendererCodeIntegrity`：关掉渲染进程的模块签名自检——**这才是对未签名注入 DLL 执行 FAIL_FAST 的那个 feature**，必须与前者同时上（10-18 只上 `--no-sandbox` 曾一度够用，但本次间歇性死亡靠它才根治，240s 实测稳定）。

### 2.3 为什么用环境变量而不是 `additional_browser_args`

这是本次最贵的教训，单独一节：

`WebviewWindowBuilder::additional_browser_args`（以及 wry 的对应参数）是
**整体替换** wry 的默认参数串。而 WebView2 有一条硬约束（微软员工在
[WebView2Feedback #159](https://github.com/MicrosoftEdge/WebView2Feedback/issues/159)
明确回复 "This is expected behavior"）：

> **同一用户数据目录下的多个 WebView2 环境，创建选项（含 AdditionalBrowserArguments）必须一致，否则第二个失败，HRESULT = 0x8007139F（ERROR_INVALID_STATE）。**

TakeKit 进程里有三个窗口（托盘面板 → overlay → 主窗口）。只要给**其中一个**
单独传参数，就构成"参数不一致" → 第二个环境创建失败 → `?` 带崩 app。
**B2 时代码注释里那个 0x8007139F 就是这个根因**（当时挂在 overlay 上），
只是当时误判成"丢了 `--disable-features`"—— defaults 丢了只是难看，**参数不一致才致命**。

`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 是**进程级**环境变量，创建任何环境时
都会被读取并统一替换 → 三个窗口天然同参数，从结构上消除该陷阱。
实现见 `main.rs::webview_browser_args()`（在 `main()` 第一句调用，早于任何窗口创建）。

## 3. 实测矩阵（2026-10-19，release 构建）

| UVDF | no-sandbox | 结果 |
|---|---|---|
| ✗ | ✗ | 死在 `overlay::create`（无 `overlay:ready`） |
| ✓ | ✗ | 起到主页加载中途原生死亡（×2，复现稳定） |
| ✓ | ✓（完整参数串） | 主窗口建成、全页加载、`ui:init-done`、**240s 稳定存活** |
| ✗ | ✓ | 仍死在 `overlay::create`（UVDF 不可省） |

结论：**两个都必需**；no-sandbox 必须带完整参数串（见 §2.2），
单独 `--no-sandbox` 会触发 §2.3 的不一致陷阱把主窗口搞死。

## 4. 操作规程（验证时怎么起）

```powershell
$root = "$env:TEMP\tk_run"
$env:APPDATA = "$root\APPDATA"                       # 隔离配置/历史，不碰用户真实数据
$env:TAKEKIT_PROBE_LOG = "$root\probe.log"           # 探针（见 docs/development.md §5.1）
$env:WEBVIEW2_USER_DATA_FOLDER = "$root\wv2"         # 缓解①
$env:TAKEKIT_WEBVIEW_NO_SANDBOX = "1"                # 缓解②（RendererCodeIntegrity 自动并入）
Start-Process -FilePath "target\release\takekit-gui.exe" -WorkingDirectory (Get-Location) -PassThru
```

铁律：

1. **启动与交互必须在同一个工具调用内完成**。命令结束后沙箱会回收进程树
   （见 §5），跨调用驱动会反复撞上"进程已死"。
2. **每次启动前清场**：`Get-Process takekit-gui | Stop-Process -Force`。
   残留实例占着 F9，新实例 `hotkey:unavailable`，之后所有 F9 都打到旧实例上——
   表现为"按了没反应"而探针毫无痕迹。
3. 启动后先确认探针里有 `hotkey:ready F9` 再按 F9。
4. 抓屏：倒计时期间用 **.NET 中心裁剪**（900×500，~100ms）；`Graphics.CopyFromScreen`
   全屏编码要 ~4s（错过整个倒计时），**ffmpeg gdigrab 在遮罩升起时不可用**
   （会挂住）。倒计时结束后再全屏 shot 没有时间压力。
5. PowerShell 5.1 按 ANSI 读无 BOM 的 .ps1——**脚本里不要出现中文**（`docs/development.md` 陷阱清单同类）。

## 5. 已知残余风险

- **间歇性早死**：双缓解全开时仍观察到 1 次"初始化成功后 15s–2min 内死亡"
  （同配置后续 4 次全部稳定 60–240s）。无 Rust panic、无事件日志，疑似注入 DLL
  与渲染管线的竞态。若复现，优先怀疑 UDF 被上次崩溃现场污染（换新目录再试）。
- **UIA 内容树取不到**：之前会话能用 `FromHandle + Descendants` 拿到 40 个元素
  驱动面板；本环境实测（聚焦 + 真实点击后）只有 16 个空 Pane。`docs/development.md` §5.2 的
  UIA 点击法当前不可用，需要点具体控件时先重新验证此法。
- 沙箱代理在命令结束后死亡：GUI 进程里残留的 `HTTP_PROXY` 指向死端口。
  WebView2 的自带网络请求可能因此挂起（未观察到影响页面加载，记录备查）。

## 6. 官方与社区依据

- [WebView2 browser flags（WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS）](https://learn.microsoft.com/microsoft-edge/webview2/concepts/webview-features-flags)
- [ICoreWebView2Environment（环境选项一致性约束原文）](https://learn.microsoft.com/microsoft-edge/webview2/reference/win32/icorewebview2environment)
- [防止安全工具阻止 WebView2 应用（DLL 注入的官方声明）](https://learn.microsoft.com/microsoft-edge/webview2/concepts/measures)
- WebView2Feedback #159（多环境参数不一致 → 0x8007139F，微软确认是预期行为）
- WebView2Feedback #4218（0x8007139F 随机复现与排查）
- CSDN《WebView2 应用突然白屏崩溃？可能是 DLP 加密软件的钩子注入导致的 FAIL_FAST》
- Campfirium《WebView2 反复白屏，没想到锅在 MacType》（`--no-sandbox` A/B 100% 有效）

## 7. 什么时候可以去掉这些开关

只要运行环境不往进程树注入 DLL（用户双击、正常桌面），两个开关都不需要——
代码里它们是显式的 opt-in（`TAKEKIT_WEBVIEW_NO_SANDBOX`），默认关闭。
**不要**为了让某台机器"跑得更安静"而把它们设成默认。
