# 全屏提示层 · 托盘面板 · 可配置热键 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让录制开始/停止有可见可听的反馈（全屏提示层 + 音频），并让隐藏主窗口后的录制仍然可停（托盘面板 + 可配置全局热键，默认 F9）。

**Architecture:** 三块新增能力，两个阶段。阶段 A 先建"停止通道"——配置持久化、全局热键、托盘图标与 220px 托盘面板，它们不依赖提示层且必须先在位（阶段 B 会隐藏主窗口，没有停止通道就是把用户锁在外面）。阶段 B 再建全屏提示层：一个常驻的透明置顶窗口（启动时创建、期间只切 `show`/`hide`），由它自己跑 `3/2/1/GO` 时序与音频，跑完回调 Rust 去 spawn ffmpeg。Rust 侧把 take 生命周期从 tauri command 里抽成 `take.rs`，让命令、热键、托盘三处共用同一份 `begin`/`stop`。

**Tech Stack:** Tauri 2（`withGlobalTauri`，无 npm、无打包器）、WebView2 直接跑原生 JS/CSS、`tauri-plugin-global-shortcut 2`（Rust 侧）、`serde_json`（配置持久化，已是依赖）、`windows` crate（core 已有的 Win32 绑定）。

**Spec:** `DESIGN.md`（§4.3 录制键、§4.4 参数区、§4.5 状态条、§4.6 提示层、§4.7 托盘面板、§4.8 图标、§4.9 音频、§5 Layout、§6 Depth、§7 Cautions）

## Global Constraints

- 沟通中文；代码、注释、标识符一律英文。
- **不自动 commit。** 本项目规则：只有用户明确发话才提交。计划里每个任务以"汇报"收尾，**没有 commit 步骤**。
- 验证命令（每次改动后跑）：
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets`（要求零 warning）
  - `cargo build --release --workspace`
- `crates/takekit-core/src/take.rs` 是历史文件，**不对它跑 `cargo fmt`**（基线口径），手写风格与现有一致即可。其余新文件正常 `cargo fmt`。
- 前端不使用 npm / 打包器 / 框架：`tauri.conf.json` 的 `withGlobalTauri: true`，JS 通过 `window.__TAURI__.core.invoke` 与 `window.__TAURI__.event.listen` 调用。
- 平台仅 Windows（可自由使用 Win32 API）。
- **界面只允许一个红按钮**（DESIGN §4.3）。托盘迷你面板是唯一允许出现第二个红按钮的地方（§4.7），它不是并存而是替代。
- 间距刻度固定 `4 / 8 / 12 / 16 / 24`；不使用阴影（popover 与全屏提示层是仅有的两处例外）；分层靠色调 + 1px 描边（§6）。
- 图标用 Lucide 内联 SVG，24 单位画板、`stroke-width: 1.75`（§4.8）。
- 音频用 Web Audio 现场合成，音量固定 `0.25`，设备不可用时静默失败（§4.9）。
- **绝不给源加实时预览**（§7.1）：任何预览都要再开一路采集，会和录制抢同一个 DXGI 会话。
- 状态点不得只靠颜色表达，必须配合形状或副行文字（§7.3）。
- 新增 Rust 依赖只有三个：`tauri-plugin-global-shortcut = "2"`（GUI）、`rfd 0.17`（输出目录选择框，GUI）、core 不需要新增（`windows` 已在）。

## File Structure

新增（GUI）：
- `crates/takekit-gui/src/config.rs` —— 配置结构 + 落盘/读取（`%APPDATA%\TakeKit\config.json`）。纯逻辑，可单测。
- `crates/takekit-gui/src/shortcut.rs` —— 热键字符串校验与规范化。纯逻辑，可单测。
- `crates/takekit-gui/src/proto.rs` —— 前后端 IPC 类型（`StartRequest`/`StartResult`/`StatusInfo`/`StopResult`/`EnvInfo`/`WindowItem`）。从 `main.rs` 原样搬出，只改可见性。
- `crates/takekit-gui/src/take.rs` —— take 生命周期：`begin`/`spawn`/`stop`/`cancel`/`status` + `AppState`。命令、热键、托盘三处共用。
- `crates/takekit-gui/src/tray.rs` —— 托盘图标（双色程序生成）+ 图标状态切换 + 左键事件。
- `crates/takekit-gui/src/tray_panel.rs` —— 220px 迷你面板窗口的创建、定位、显示/隐藏。
- `crates/takekit-gui/src/overlay.rs` —— 全屏提示层窗口的创建与 `show`/`hide`，以及它的三个回调命令。
- `crates/takekit-gui/ui/beep.js` —— Web Audio 合成音（主窗口与提示层共用一段）。
- `crates/takekit-gui/ui/tray.html` / `tray.css` / `tray.js` —— 迷你面板。
- `crates/takekit-gui/ui/overlay.html` / `overlay.css` / `overlay.js` —— 全屏提示层。

修改：
- `crates/takekit-gui/src/main.rs` —— `setup` 里创建托盘/面板/提示层并注册插件与热键；命令薄转调到 `take.rs`。
- `crates/takekit-gui/Cargo.toml` —— 加 `tauri-plugin-global-shortcut`。
- `crates/takekit-gui/capabilities/default.json` —— `windows` 扩到 `["main", "tray", "overlay"]`。
- `crates/takekit-gui/ui/index.html` / `style.css` / `main.js` —— 参数区两行 + 倒计时/收尾面板/热键三控件 + 状态条各态。
- `crates/takekit-core/src/source.rs` —— 新增 `bring_to_front`。
- `crates/takekit-core/src/session.rs` —— `Recording::stop()` 拆出可迭代的 `stop_next()`。
- `DESIGN.md` / `docs/perf-review.md` / `.workbuddy/memory/*.md` —— 实施后回填。

## Review Focus

以下六条是 spec 暗示、但没有哪条任务的自测能覆盖的失败模式。每条都挂到拥有该代码的任务上（见各任务内标注 `[RF-n]`）。

1. **隐藏主窗口后音频还在不在。** WebView2 对不可见窗口可能节流 JS 定时器甚至静音，而"窗口隐藏时声音是唯一确认通道"（§4.9）正是整套设计的前提。期望：主窗口隐藏期间提示层的音频照常响。
2. **提示层在屏幕上时不能吃掉鼠标。** 透明置顶窗口若接收鼠标事件，录制期间用户点不到任何东西（连"点一下收掉汇总"以外的操作都做不了）。期望：遮罩不在时鼠标穿透，遮罩在时只在它自己身上生效。
3. **热键被别的程序占用。** 注册失败必须可见且不能把用户的热键配置弄丢——不能静默失败，也不能留在"看起来生效了"的状态。
4. **多显示器 / 非 100% DPI。** 提示层要覆盖整个虚拟屏（含任务栏），托盘面板要贴在托盘图标旁边。期望：在 125%/150% 缩放下两者位置都正确。
5. **倒计时期间抢占最前的全屏应用。** 用户切到全屏游戏/播放器再按 F9 时，提示层是否还在最前。期望：遮罩仍可见（`always_on_top` 在 Windows 上对全屏独占窗口不保证，需要实测结论）。
6. **窗口源置前的边界。** 标题匹配到 0 个窗口（窗口刚关掉）、匹配到多个（重名）、窗口已最小化——期望：0 个时不报错照常录（画面就是桌面现状），多个时全部置前，最小化的被恢复。

---

## 阶段 A —— 停止通道

### Task A1: 配置持久化

**Files:**
- Create: `crates/takekit-gui/src/config.rs`
- Modify: `crates/takekit-gui/src/main.rs`（加 `mod config;`）

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
  #[serde(default)]
  pub struct Config {
      pub hotkey: String,        // default "F9"
      pub countdown_secs: u8,    // default 3
      pub finalize_panel: bool,  // default true
  }
  impl Default for Config
  impl Config {
      pub fn path() -> std::path::PathBuf;
      pub fn load() -> Config;
      pub fn save(&self) -> Result<(), String>;
      pub fn load_from(path: &std::path::Path) -> Config;      // tests
      pub fn save_to(&self, path: &std::path::Path) -> Result<(), String>;  // tests
  }
  ```
- 位置：`path()` = `%APPDATA%\TakeKit\config.json`；`APPDATA` 缺失时退回 `std::env::temp_dir().join("TakeKit")`。

- [ ] **Step 1: 写失败测试**（`config.rs` 内 `#[cfg(test)] mod tests`）

```rust
#[test]
fn round_trip() {
    let path = std::env::temp_dir().join("takekit-cfg-rt").join("config.json");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let cfg = Config { hotkey: "Ctrl+Alt+R".into(), countdown_secs: 5, finalize_panel: false };
    cfg.save_to(&path).unwrap();
    assert_eq!(Config::load_from(&path), cfg);
}

#[test]
fn missing_file_is_default() {
    let path = std::env::temp_dir().join("takekit-cfg-none").join("nope.json");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    assert_eq!(Config::load_from(&path), Config::default());
}

#[test]
fn corrupt_file_is_default() {
    let path = std::env::temp_dir().join("takekit-cfg-bad").join("config.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(Config::load_from(&path), Config::default());
}

#[test]
fn partial_file_fills_in_defaults() {
    let path = std::env::temp_dir().join("takekit-cfg-part").join("config.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"hotkey":"F8"}"#).unwrap();
    let cfg = Config::load_from(&path);
    assert_eq!(cfg.hotkey, "F8");
    assert_eq!(cfg.countdown_secs, 3);
    assert!(cfg.finalize_panel);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p takekit-gui config`
Expected: 编译失败（`Config` 未定义）。

- [ ] **Step 3: 实现**

`save_to` 先 `create_dir_all(parent)` 再写 pretty JSON；`load_from` 在"读失败 / 解析失败"两条路径上都返回 `Config::default()`（配置文件损坏不能让程序起不来）。`save_to` 的错误转 `String`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p takekit-gui config`
Expected: 4 passed。

- [ ] **Step 5: 静态检查**

Run: `cargo clippy -p takekit-gui --all-targets`
Expected: 零 warning。

- [ ] **Step 6: 汇报（不提交）**

---

### Task A2: 热键字符串校验与规范化

**Files:**
- Create: `crates/takekit-gui/src/shortcut.rs`
- Modify: `crates/takekit-gui/Cargo.toml`（加 `tauri-plugin-global-shortcut = "2"`）
- Modify: `crates/takekit-gui/src/main.rs`（加 `mod shortcut;`）

**Interfaces:**
- Consumes: `tauri_plugin_global_shortcut::Shortcut`（实现 `FromStr` 与 `Display`）。
- Produces:
  ```rust
  /// Normalised display form for a shortcut string, or `None` when the string
  /// is not a shortcut the plugin can register. `"f9"` → `Some("…")`.
  pub fn normalize(input: &str) -> Option<String>;
  ```
- 测试**不断言具体输出字符串**（`Shortcut` 的 `Display` 格式不属于我们的契约），只断言"合法 / 非法 / 幂等"。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn accepts_a_plain_key() {
    assert!(normalize("f9").is_some());
    assert!(normalize("F9").is_some());
}

#[test]
fn accepts_a_combo() {
    assert!(normalize("Ctrl+Shift+R").is_some());
    assert!(normalize("ctrl+alt+r").is_some());
}

#[test]
fn rejects_garbage() {
    assert_eq!(normalize("nonsense"), None);
    assert_eq!(normalize(""), None);
    assert_eq!(normalize("Ctrl+NotAKey"), None);
}

#[test]
fn is_idempotent() {
    let once = normalize("ctrl+alt+r").unwrap();
    assert_eq!(normalize(&once), Some(once));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p takekit-gui shortcut`
Expected: 编译失败（crate 依赖未加 / 函数未定义）。

- [ ] **Step 3: 实现**

`input.trim().parse::<Shortcut>().ok().map(|s| s.to_string())`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p takekit-gui shortcut`
Expected: 4 passed。若 `is_idempotent` 失败，说明 `Display` 输出无法被自己解析——此时改为在实现里返回 `input` 的规范化副本而非 `Display`，并在此处记录原因。

- [ ] **Step 5: 静态检查**

Run: `cargo clippy -p takekit-gui --all-targets`
Expected: 零 warning。

- [ ] **Step 6: 汇报（不提交）**

---

### Task A3: take 生命周期抽出 + 全局热键

**Files:**
- Create: `crates/takekit-gui/src/proto.rs`（从 `main.rs` 原样搬出 IPC 类型）
- Create: `crates/takekit-gui/src/take.rs`
- Modify: `crates/takekit-gui/src/main.rs`（命令转调、插件注册、`set_hotkey` 命令）

**Interfaces:**
- Consumes: `config::Config`、`shortcut::normalize`、`takekit_core::session::{Session, Prepared, Recording, Settings}`。
- Produces:
  ```rust
  // proto.rs
  pub struct StartRequest { pub sources: Vec<Source>, pub out_root: String,
                            pub quality: u8, pub marks: bool, pub encoder: Option<encoder::Encoder> }
  pub struct StartResult { pub dir: String, pub engine: String, pub encoder: String }
  pub struct StatusInfo  { pub recording: bool, pub elapsed: f64, pub sources: Vec<LiveSourceInfo> }
  pub struct LiveSourceInfo { pub id: String, pub alive: bool, pub bytes: u64 }
  pub struct StopResult  { pub dir: String, pub duration: Option<f64>,
                           pub sources: Vec<OutcomeInfo>, pub failed: usize }
  pub struct OutcomeInfo { pub id: String, pub seconds: Option<f64>, pub bytes: u64, pub error: Option<String> }
  pub struct EnvInfo     { pub ffmpeg: Option<String>, pub encoder: Option<String>,
                           pub screen: ScreenRect, pub default_out_root: String }
  pub struct WindowItem  { pub title: String, pub x: i32, pub y: i32, pub w: u32, pub h: u32 }

  // take.rs
  pub struct AppState { pub recording: Mutex<Option<Recording>>,
                        pub pending: Mutex<Option<Prepared>> }
  pub fn install_hotkey(app: &tauri::AppHandle, spec: &str) -> Result<String, String>;
  pub fn begin(app: &tauri::AppHandle, req: StartRequest) -> Result<(), String>;
  pub fn stop(app: &tauri::AppHandle) -> Result<StopResult, String>;
  pub fn status(app: &tauri::AppHandle) -> StatusInfo;
  ```
- `install_hotkey` 语义：先注销当前已注册的全部热键，再注册 `spec`；注册失败则**回滚**——重新注册上一个可用的热键，并返回 `Err(原因)`。
- 热键触发：注册 `spec` 时绑定的回调在"未录制"时调 `begin`（用当前界面参数）并在"录制中"时调 `stop`。参数从 `AppState` 另存的一份 `last_request` 取（新增字段 `last_request: Mutex<Option<StartRequest>>`，由 `begin` 写入）。

- [ ] **Step 1: 纯重构——搬出 proto.rs 与 take.rs**

把 `main.rs` 里的 IPC 类型与 `start_recording`/`status`/`stop_recording` 的实现搬进新文件，`main.rs` 的 command 只留薄转调（`take::begin(&app, req)`）。**本步不加任何新行为。**

- [ ] **Step 2: 验证重构没改行为**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: 与前一次相同的 12 passed（core）+ 零 warning。
再跑 `cargo build --release --workspace`，启动 `target/release/takekit-gui.exe`，加一个源、录 5 秒、停止——结果与重构前一致（banner 显示 `已保存 …`）。

- [ ] **Step 3: 注册插件并装载热键**

`main.rs` 的 builder 加 `.plugin(tauri_plugin_global_shortcut::Builder::new().build())`；`setup` 里 `Config::load()` 后调 `take::install_hotkey(app.handle(), &cfg.hotkey)`，失败时 `probe` 记录原因（界面提示见 Task B5）。

- [ ] **Step 4: 加 `set_hotkey` 命令**

```rust
#[tauri::command]
async fn set_hotkey(app: tauri::AppHandle, hotkey: String) -> Result<String, String>;
```
先 `shortcut::normalize(&hotkey)` 校验（`None` → `Err("无法识别的按键组合")`），再 `take::install_hotkey`，成功后把新值写进 `Config` 并存盘。

- [ ] **Step 5: 验证热键真的工作** `[RF-3]`

Run: `cargo build --release --workspace`，带 `TAKEKIT_PROBE_LOG` 启动，加一个源，按 F9 → PROBE 日志出现 `hotkey:begin`，界面进入录制；再按 F9 → `hotkey:stop`，banner 出现 `已保存`。
再验证占用场景：先声明 `Ctrl+Shift+Q` 给别的程序后用 `set_hotkey` 试注册（或用一个已在别处注册的组合），Expected：返回错误、日志记下原因、**F9 仍然可用**（回滚生效）。

- [ ] **Step 6: 汇报（不提交）**

---

### Task A4: 托盘图标

**Files:**
- Create: `crates/takekit-gui/src/tray.rs`
- Modify: `crates/takekit-gui/src/main.rs`（`setup` 调 `tray::setup`）

**Interfaces:**
- Produces:
  ```rust
  pub const ID: &str = "main";
  pub fn setup(app: &tauri::AppHandle) -> tauri::Result<()>;
  pub fn set_recording(app: &tauri::AppHandle, recording: bool);
  fn dot(rgba: [u8; 4]) -> tauri::image::Image<'static>;
  ```
- 图标**程序生成**，不用资源文件：32×32 RGBA，画一个实心圆（半径 11，中心 16,16，边缘 1px 内做简单 alpha 过渡即可）。
- 两个颜色：灰 = 未录制，红 = 录制中。红值取自 `ui/style.css` 里 `--tk-rec` 的字面量（实现时读该文件取值后写成 Rust 常量）。
- 左键 `Up` 事件 → `crate::tray_panel::toggle(app)`（Task A5 提供；本步先接一个 `probe("tray:click")` 占位，A5 换成真调用）。

- [ ] **Step 1: 实现 `dot()` + `setup()`**

`TrayIconBuilder::with_id(ID).icon(dot(GRAY)).tooltip("TakeKit").on_tray_icon_event(..)` 后 `.build(app)?`。

- [ ] **Step 2: 验证托盘出现且可点**

Run: `cargo build --release --workspace`，启动，看通知区域出现一个圆点图标；左键点击 → PROBE 日志出现 `tray:click`。

- [ ] **Step 3: 接上录制状态**

在 `take::begin` 成功后与 `take::stop` 收尾完成后分别调 `tray::set_recording(app, true/false)`。

- [ ] **Step 4: 验证颜色切换**

Run: 启动 → 图标灰；F9 开始 → 图标变红；F9 停止 → 收尾完成后转灰。

- [ ] **Step 5: 汇报（不提交）**

---

### Task A5: 托盘迷你面板

**Files:**
- Create: `crates/takekit-gui/src/tray_panel.rs`、`ui/tray.html`、`ui/tray.css`、`ui/tray.js`
- Modify: `crates/takekit-gui/src/tray.rs`（左键接 `toggle`）、`main.rs`（`setup` 建窗口、命令转调）、`capabilities/default.json`

**Interfaces:**
- Produces:
  ```rust
  pub const LABEL: &str = "tray";
  pub fn create(app: &tauri::AppHandle) -> tauri::Result<tauri::WebviewWindow>;
  pub fn toggle(app: &tauri::AppHandle);
  fn position_near_tray(app: &tauri::AppHandle, win: &tauri::WebviewWindow) -> tauri::Result<()>;
  ```
- 窗口：`label = "tray"`、`inner_size(220, 132)`、`decorations(false)`、`resizable(false)`、`always_on_top(true)`、`skip_taskbar(true)`、`shadow(true)`（popover 允许，§6）、`visible(false)`、`url = "tray.html"`。
- 定位：`app.tray_by_id(tray::ID).and_then(|t| t.rect())` 拿托盘图标矩形，把面板放在它上方居中（越界则贴屏幕边缘）。`rect()` 不可用时退回屏幕右下角。坐标用**物理像素**（`PhysicalPosition`），矩形带 `scale_factor` 时先换算。
- 收起：`WindowEvent::Focused(false)` → `hide()`。
- 面板内容（§4.7）：REC 点 + 等宽计时 / 每源一行（状态点 + `id` + `MB`，**点不能只靠颜色**，§7.3）/ `停止` 键 / `显示主窗口`。计时与状态 250ms 轮询 `invoke("status")`。`停止` → `invoke("stop_recording")`（收尾进行中禁用）。`显示主窗口` → `invoke("show_main")`。
- 新增命令：
  ```rust
  #[tauri::command] fn show_main(app: tauri::AppHandle);
  ```

- [ ] **Step 1: 建窗口 + 定位 + 收起**

`capabilities/default.json` 的 `windows` 改成 `["main", "tray", "overlay"]`（`overlay` 为 Task B2 预留）。

- [ ] **Step 2: 写面板页面**

`tray.html` 用与主窗口同一套 CSS 变量（把 `style.css` 里的 `:root` 变量复制进 `tray.css`，不跨文件 `@import`——`style.css` 里有主窗口专属布局，引进来会互相污染）。

- [ ] **Step 3: 验证显示、定位、收起** `[RF-4]`

Run: 启动 → 点托盘图标 → 面板出现在图标上方；点别处 → 收起；把系统缩放改成 150% 再试一次，Expected：位置仍然贴着托盘。

- [ ] **Step 4: 验证停止与恢复**

Run: 开始录制 → 主窗口手动隐藏（`show_main` 的反向：用托盘面板的停止键前先 `app.get_webview_window("main").hide()`，或等 Task B3 接上流程）→ 托盘面板显示计时在走 → 点 `停止` → 收尾完成 → 点 `显示主窗口` → 主窗口回来且 banner 显示 `已保存`。这一步是"隐藏主窗口后仍可停"的核心验证。

- [ ] **Step 5: 汇报（不提交）**

---

## 阶段 B —— 全屏提示层

### Task B1: 窗口源置前

**Files:**
- Modify: `crates/takekit-core/src/source.rs`
- Modify: `crates/takekit-gui/src/take.rs`（`begin` 里接入）

**Interfaces:**
- Produces:
  ```rust
  /// Bring every window whose title contains `needle` to the top of the Z order,
  /// restoring it first if minimized. Returns how many windows matched.
  pub fn bring_to_front(needle: &str) -> usize;
  ```
- 实现要点：`EnumWindows` 回调里匹配标题（`to_lowercase().contains(needle.to_lowercase())`，**不做尺寸过滤**——最小化窗口的客户区为零但仍需被恢复）；对每个命中调 `ShowWindow(hwnd, SW_RESTORE)` 然后 `SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)`。用 `HWND_TOP` 而非 `SetForegroundWindow`：我们要的是 Z 序（可见），不是键盘焦点，而且 `SetWindowPos` 不需要调用方是前台进程（主窗口隐藏后就可能不是了）。

- [ ] **Step 1: 写失败测试（无副作用的那条）**

```rust
#[test]
fn no_match_brings_nothing_to_front() {
    assert_eq!(bring_to_front("takekit-z-order-probe-xyz"), 0);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p takekit-core bring_to_front`
Expected: 编译失败（函数未定义）。

- [ ] **Step 3: 实现**

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p takekit-core bring_to_front`
Expected: 1 passed。

- [ ] **Step 5: 写 `#[ignore]` 集成测试（真实窗口的 Z 序）** `[RF-6]`

```rust
/// Needs an interactive desktop: opens Notepad, checks it really moves to the
/// top of the Z order, and closes it again. Run with
/// `cargo test -p takekit-core --ignored the_matching_window_moves_to_the_top -- --nocapture`.
#[test]
#[ignore]
fn the_matching_window_moves_to_the_top() { ... }
```

步骤（测试体内）：启动 `notepad.exe`；轮询 `list_windows("")` 最多 3 秒，找出比启动前多出来的那个标题；用测试内的 `fn z_order() -> Vec<String>`（`GetTopWindow(None)` + `GetWindow(h, GW_HWNDNEXT)` 循环收集标题）记下索引 `before`；调 `bring_to_front(&title)` 断言返回 `1`；再取 `z_order()` 断言该标题索引 `< before`；最后杀掉 notepad 进程。

- [ ] **Step 6: 手工跑这条测试**

Run: `cargo test -p takekit-core --ignored the_matching_window_moves_to_the_top -- --nocapture`
Expected: passed（会看到记事本被弹到最前再关掉）。

- [ ] **Step 7: 在 `begin` 里接入**

`take::begin` 在 `Session::prepare` 成功之后、显示提示层之前，对每个 `Source` 的 `Capture::Window { title, .. }` 调 `bring_to_front(title)`，并把 `(title, count)` 记进 PROBE 日志（`front:记事本=1`）。`count == 0` 只记日志，不算失败——窗口可能刚被关掉，画面就是桌面现状。

- [ ] **Step 8: 验证遮挡场景** `[RF-6]`

Run: 打开两个窗口 A、B，让 B 完全盖住 A；把 A 加成窗口源；按 F9 走完倒计时；Expected：A 在录制开始前被提到最前，且录下来的第一帧是 A 的内容（用 `ffmpeg -i <take>/<id>.mp4 -frames:v 1 out.png` 抽帧肉眼确认）。

- [ ] **Step 9: 汇报（不提交）**

---

### Task B2: 提示层窗口骨架

**Files:**
- Create: `crates/takekit-gui/src/overlay.rs`、`ui/overlay.html`、`ui/overlay.css`、`ui/overlay.js`
- Modify: `crates/takekit-gui/src/main.rs`（`setup` 建窗口）

**Interfaces:**
- Produces:
  ```rust
  pub const LABEL: &str = "overlay";
  pub fn create(app: &tauri::AppHandle) -> tauri::Result<tauri::WebviewWindow>;
  pub fn show(app: &tauri::AppHandle);
  pub fn hide(app: &tauri::AppHandle);
  ```
- 窗口：`label = "overlay"`、位置与尺寸 = `takekit_core::source::virtual_screen_rect()`（物理像素，`PhysicalPosition`/`PhysicalSize`）、`transparent(true)`、`decorations(false)`、`always_on_top(true)`、`skip_taskbar(true)`、`shadow(false)`、`focused(false)`、`resizable(false)`、`visible(false)`。
  - **实现时修正（2026-10-19 实机）**：原计划的 `additional_browser_args("--autoplay-policy=no-user-gesture-required")` **不能加**。该 setter 是**整体替换** wry 的默认参数串而非追加，加了它就会丢掉 wry 默认的 `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection`，WebView2 直接拒绝创建（`HRESULT 0x8007139F`），而 `create` 的 `?` 会把整个 app 带崩（实测探针停在 `tray:panel-ready`，再无 `overlay:ready`）。而 B3 要的 autoplay **本来就有**：wry 的 `WebViewAttributes::autoplay` 默认 `true`，默认就传 `--autoplay-policy=no-user-gesture-required`（`wry-0.57.0/src/lib.rs:758,881,1788-1791`，tauri 全程未覆盖）。结论：这一行删掉，什么都不用写。
- **偏离 §4.6 的一处决定（已与用户确认方向，实现时按此）**：spec 写"整个进程只改 `opacity`"。改用 `show()`/`hide()`——`opacity` 方案下窗口始终存在，透明窗口仍会吃鼠标事件，得再维护一份 `set_ignore_cursor_events` 状态；`show`/`hide` 简单且已被 §7 实测证明不破坏 DDGrab（19s/570 帧零丢帧）。窗口仍在启动时创建，只在期间切换可见性，创建延迟这件事的处理与 spec 一致。
- DOM：`<div id="veil" class="veil"><div id="big"></div><div id="list"></div></div>`。
- CSS：`.veil { position: fixed; inset: 0; background: rgba(0,0,0,.62); display: flex; flex-direction: column; align-items: center; justify-content: center; }`；`#big { font: 500 96px/1.1 ui-monospace, "Cascadia Mono", Consolas, monospace; color: #fff; }`；`#list { font: 400 16px/1.6 ...; color: #fff; }`；`.go { color: var(--tk-ok) }`。`--tk-ok` 的字面量从 `ui/style.css` 取。
- 验证钩子：`TAKEKIT_OVERLAY_DEMO=1` 时 `create` 之后立刻 `show`，并由 `overlay.js` 把 `#big` 设成 `GO`；`hide` 由 Rust 侧 5 秒后自动调用（原写 3 秒，实现取 5 秒：截图要留余量，且同一轮运行里就能把"隐藏后不吃鼠标"那半也验到）。这是 PROBE 性质的脚手架，与 `ui_ping` 同批处理。

- [ ] **Step 1: 实现 create/show/hide + 页面骨架**

- [ ] **Step 2: 验证遮罩覆盖整个虚拟屏** `[RF-4]`

Run: `TAKEKIT_OVERLAY_DEMO=1` 启动 → 3 秒内用 `ffmpeg -f gdigrab -frames:v 1 -i desktop shot.png` 抓帧；Expected：整屏（含第二显示器的任务栏）被 62% 黑压暗，中央是绿色 `GO`。改 150% 缩放再跑一次，Expected：边界仍无缝隙。

- [ ] **Step 3: 验证鼠标不被吃掉** `[RF-2]`

Run: 不加 `TAKEKIT_OVERLAY_DEMO`，正常启动 → 在提示层应该覆盖的位置点击、拖动窗口；Expected：一切照常（窗口未显示时完全不参与鼠标）。
再让它显示（`TAKEKIT_OVERLAY_DEMO=1`）后点桌面，Expected：点击落在遮罩上（不透到下面的窗口）。

- [ ] **Step 4: 验证它压得住全屏应用** `[RF-5]`

Run: 打开一个真正全屏的应用（`Win+Shift+Enter` 让浏览器全屏，或一个全屏播放器），按 F9。Expected：遮罩仍然可见、计时仍在走；若被顶掉，记录这个边界并在 `DESIGN.md` §4.6 写下"全屏独占应用期间提示层不可见，此时靠托盘图标与声音判断"。

- [ ] **Step 5: 汇报（不提交）**

---

### Task B3: 倒计时 · 音频 · spawn 编排 · Esc 取消

**Files:**
- Create: `ui/beep.js`
- Modify: `crates/takekit-gui/src/overlay.rs`（三个回调命令）、`crates/takekit-gui/src/take.rs`（`begin` 拆成 `begin` + `spawn`）、`ui/overlay.js`、`ui/index.html` + `ui/main.js`（引入 `beep.js`，开始按钮改走新流程）

**Interfaces:**
- Produces:
  ```rust
  // overlay.rs
  #[tauri::command] fn overlay_countdown_done(app: tauri::AppHandle);  // 倒计时到 spawn 点
  #[tauri::command] fn overlay_hide(app: tauri::AppHandle);            // 提示层请求收起
  #[tauri::command] fn overlay_cancel(app: tauri::AppHandle);          // Esc

  // take.rs
  pub fn begin(app: &tauri::AppHandle, req: StartRequest) -> Result<(), String>;
  pub fn spawn(app: &tauri::AppHandle) -> Result<StartResult, String>;
  pub fn cancel(app: &tauri::AppHandle);

  // beep.js（挂到 window）
  window.beep = (hz, ms) => { /* AudioContext + Oscillator + Gain(0.25) */ };
  ```
- `proto.rs` 的 `StatusInfo` 增补一个字段：`pub error: Option<String>`；`AppState`（Task A3 定义）补 `pub last_error: Mutex<Option<String>>`。`begin`/`spawn` 开头清空它，失败时写入，`status` 填进 `StatusInfo.error`（B5 的 `failed` 态展示）。
- 命令层照现状用 `tauri::async_runtime::spawn_blocking` 包住 `begin`/`spawn`/`stop`：`Session::prepare` 会阻塞两秒，放主线程上会冻住渲染。
- 流程（事件驱动，不做跨 `await` 握手）：
  1. 主窗口 `invoke("start_recording", {req})` → `take::begin`：
     `Session::prepare`（失败即返回 `Err`，此时主窗口还在，界面直接 banner + `beep(220, 300)`）
     → 存进 `AppState.pending`，同时把 `req` 存进 `last_request`
     → 逐个窗口源 `bring_to_front` → `tray::set_recording(app, true)`
     → `overlay::show(app)` + `app.emit_to("overlay", "overlay:countdown", { secs })`
     → 隐藏主窗口 → 立刻返回 `Ok(())`（目录只有 spawn 之后才知道，前端在 begin 阶段不消费它）
  2. `overlay.js` 收到 `overlay:countdown`：从 `secs` 递减渲染 `3/2/1`，每个数字 `beep(440, 120)`；到 0 显示 `GO`（`.go`）并 `beep(880, 1000)`；到 `secs + 0.2` 秒 `invoke("overlay_countdown_done")`；到 `secs + 1.0` 秒 `invoke("overlay_hide")`。
  3. `take::spawn`：从 `pending` 取 `Prepared` 调 `.start()` 存入 `recording`；失败则 `overlay::hide` + 显示主窗口 + 写进 `last_error`（B5 的 `failed` 态展示）。
  4. `overlay_hide` → `overlay::hide`。
  5. `Esc`：`overlay.js` 的 `keydown` → `invoke("overlay_cancel")` → `take::cancel`：丢掉 `pending`、`tray::set_recording(false)`、`overlay::hide`、显示主窗口。
- 计时用 `performance.now()` 起一个 `requestAnimationFrame` 循环推进，不用一堆 `setTimeout`（避免隐藏窗口的定时器节流把它们串成一坨）`[RF-1]`。

- [x] **Step 1: 写 `beep.js`**

- [x] **Step 2: 验证音频与隐藏窗口** `[RF-1]`

Run: 启动 → 加源 → F9。Expected：主窗口在倒计时开始的那一刻隐藏，`3`/`2`/`1`/`GO` 每下落都有声音，声音在窗口隐藏之后**仍然在响**（这是本任务的核心风险点）。
若隐藏后无声：改由 `overlay` 之外的通道发声（把 `beep.js` 引到主窗口、由 Rust `emit_to("main", "beep", …)` 驱动），并在计划文件与 `DESIGN.md` §4.9 记下这个结论。

**实测结论（2026-10-19，两条都与计划不同，都是真问题）**：

1. **声音的瓶颈不是"隐藏后能否响"，而是第一个 beep 的启动延迟**。遮罩窗口可见，音频一直能响；但 `AudioContext` 若在第一个 beep 里才创建，设备初始化 + `resume()` 的异步耗时会**让第一个音晚几十到几百毫秒**——用户按声音计时，表现为"3→2 明显短"，而 JS 时钟测出来是准的（探针 `audio=` 字段就是为这个加的）。解法：页面加载即建上下文 + `begin()` 在探针前发 `overlay:wake`（探针最长 ~2.5s，正好是免费的预热窗口）。
2. **Esc 不能走 `overlay.js` 的 keydown**：遮罩 `focused(false)`（不能抢焦点，否则录窗口源时目标窗口失焦变灰），按键落在前台窗口，JS 永远收不到。改由 Rust 30ms 轮询 `GetAsyncKeyState`（`source::escape_pressed`）。`keybd_event` 注入的 Esc 能被该轮询看到（与 `RegisterHotKey` 不同）。
3. 另修：**取消后必须显式停表**（Rust 发 `overlay:stop`）——只隐藏窗口不够，Chromium 对刚隐藏的窗口仍供帧一两秒，2/1/GO 的提示音会照常播完。
4. 另修：**t0 取第一帧而非事件时刻**——否则首帧延迟（~190ms）会吃掉第一个数字的显示时间（视觉版的同一个病）。

- [x] **Step 3: 验证 spawn 时序**

Run: 带 `TAKEKIT_PROBE_LOG` 启动录一次，Expected：日志里 `countdown:done` 与 `spawn:ok` 的间隔 < 200ms，且录到的文件时长 ≥ 倒计时结束后的时长（GO 期间已经在录）。

**实测**：`overlay:done-invoked → spawn:ok` = 11–17ms；take 4.666s / 650KB / failed:0（GO 期间确已在录）。

- [x] **Step 4: 验证 Esc 取消**

Run: F9 后在 `GO` 之前按 Esc。Expected：遮罩消失、主窗口回来、没有产生任何 take 目录、托盘图标转灰。

**实测**：注入 Esc → `cancelled` + `tray:idle`，无新 take 文件、无余音；倒计时中再按 F9 取消同样通过（`cancelled` 计数 2）。

- [x] **Step 5: 汇报（不提交）**

**用户插入项（2026-10-19，已一并完成）**：① 倒计时开关（`set_countdown`，放在"鼠标效果"右边，§4.10 的 `countdown_secs` 0=关）；② `secs==0` 直启路径（DESIGN §4.6，B6 的前半段因此提前完成）；③ 热键停止后主窗口回前台。

---

### Task B4: 收尾流程（STOP + 逐源清单 + 已保存）

**Files:**
- Modify: `crates/takekit-core/src/session.rs`（`stop()` 拆出 `stop_next()`）
- Modify: `crates/takekit-gui/src/take.rs`（`stop` 改成逐源推进并 emit）、`ui/overlay.js`（收尾渲染）、`crates/takekit-gui/src/tray.rs`（接入转灰）

**Interfaces:**
- Produces:
  ```rust
  // core
  impl Recording {
      /// Finalise the next source. `None` once every source is done.
      pub fn stop_next(&mut self) -> Option<Outcome>;
      pub fn stop(self) -> Result<Finished>;   // 保持现有签名，改为基于 stop_next 的循环
      pub fn dir(&self) -> &Path;              // 已存在（main.rs 现在就在用它取目录）
  }
  // gui，事件 payload（前缀是 take，不是 overlay：提示层与主窗口状态条都订阅）
  "take:finalize" { dir: String, elapsed: f64 }
  "take:source"   { id: String, seconds: Option<f64>, bytes: u64, error: Option<String> }
  "take:done"     { dir: String, duration: Option<f64>, bytes: u64, failed: usize }
  ```
- 流程：
  1. `take::stop`：`tray::set_recording(app, false)`；若 `Config.finalize_panel` 为真 → `overlay::show` + `emit("take:finalize", {dir, elapsed})` + `beep(660, 200)`；否则跳过遮罩，直接进第 2 步（Task B6）。
  2. 循环 `recording.stop_next()`，每出一个源就 `emit("take:source", …)` —— 提示层用它追加清单，主窗口状态条用它把 `正在写入… n/m` 的分子往上加。
  3. 全部完成 → `emit("take:done", …)` + `beep(880, 150)`。
  4. 若开了收尾面板：`overlay.js` 显示 `已保存` + 目录，`+2500ms` 后 `invoke("overlay_hide")`；任意键或点击立即 `invoke("overlay_hide")`。`overlay_hide` 里恢复主窗口（`show_main`）。
  5. 若没开：直接恢复主窗口。
- 遮罩上的计时在 `finalize` 那一刻冻结（`elapsed` 就是为此放进 payload 的）。

- [ ] **Step 1: 写 core 的 `stop_next`**

保持 `stop()` 的行为不变（CLI 依赖它），把它改成基于 `stop_next()` 的循环。`dir()` 已存在，不用补。

- [ ] **Step 2: 验证 core 行为未变**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: 仍是 12 passed + 零 warning。
再跑 CLI 录一次（`cargo run -p takekit -- record …`，参数照 README），Expected：产出与改动前一致（时长、文件数、目录结构）。

- [ ] **Step 3: 接上 GUI 的逐源 emit 与遮罩渲染**

- [ ] **Step 4: 验证逐源清单可见** `[RF-1]`

Run: 加 2 个源（一个桌面 + 一个摄像头）开始录制，5 秒后 F9 停止。Expected：`STOP` 在按下的同一帧出现；随后清单逐行追加（id、时长、分辨率、大小）；全部完成后中央变绿色 `已保存` + 目录路径；2.5 秒后遮罩消失、主窗口回来、托盘转灰。

- [ ] **Step 5: 验证提前关闭**

Run: 在 `已保存` 出现后立刻按任意键。Expected：遮罩立刻收起（不用等满 2.5 秒），主窗口正常回来。

- [ ] **Step 6: 汇报（不提交）**

---

### Task B5: 参数区 + 状态条 + 配置联通

**Files:**
- Modify: `ui/index.html`、`ui/style.css`、`ui/main.js`、`crates/takekit-gui/src/main.rs`（`config_get`/`config_set`/`reveal_logs` 命令）

**Interfaces:**
- Produces:
  ```rust
  #[tauri::command] fn config_get() -> Config;
  #[tauri::command] async fn save_session(session: SessionConfig) -> Result<(), String>;
  #[tauri::command] async fn set_hotkey(app: tauri::AppHandle, hotkey: String) -> Result<String, String>;
  #[tauri::command] fn reveal_logs(dir: String);
  ```
- ⚠️ **已漂移**：C1 之后实际落成的是 `save_session(SessionConfig)`（只收会话字段：
  `sources/out_root/encoder/marks`）而不是全量 `config_set(Config)`——全量写回会让
  前端的旧快照覆盖刚改的热键/窗口位置。热键走独立的 `set_hotkey`。B5 按实际形状写。
- 参数区布局（§4.4，两行；高度从现在的单行改成 `96px`）：
  - 第一行：`输出目录`（flex:1，带 `…` 按钮）｜`视频编码`｜`鼠标效果`
  - 第二行：`倒计时 [ 3 ] 秒`｜`收尾面板`（switch）｜`全局热键 [ F9 ]`
- 热键控件：`<button id="hotkey">`，点击进入捕获态（`aria-pressed="true"`，文案 `按下组合键…`），`keydown` 里收集修饰键 + 主键拼成字符串（用 `e.code` 去掉 `Key`/`Digit` 前缀），Esc 放弃；提交后 `config_set`，失败则 banner 报错并回滚显示。
- 状态条（§4.5）各态：`idle`（`N 个源就绪`）、`recording`（REC 点 + 计时 + 右侧 `停止`）、`finalizing`（`正在写入… 2/3`，分子来自订阅 `take:source`）、`done`（`已完成 00:01:24 · 29.6 MB`，3 秒后回 idle）、`failed`（原因单行省略 + 悬停全文 + 右侧 `查看日志` → `reveal_logs`）。`failed` 的原因取 `StatusInfo.error`（Task B3 写入）。`countdown` 态不需要实现：那时主窗口已隐藏——倒计时期间唯一可见的是提示层，取消走 Esc（Task B3）。
- 录制中参数区整块 `opacity: .55` + `pointer-events: none` + `aria-disabled="true"`，但**输出目录仍可选中复制**（`user-select: text`）（§4.4）。
- 窗口高度 < 500px 时参数区折叠成一行摘要（`…\Temp · NVENC · 30fps · Q20`），点击展开成浮层（§4.4）。最小高度 440px 小于 500px，所以这条会真的被走到，不能省。
- 图标按 §4.8：停止键的方形、`查看日志` 的文件夹、失败态的警告角标都用 Lucide 内联 SVG（24 单位画板、`stroke-width: 1.75`），不引运行时库。

- [ ] **Step 1: 重排参数区并加上三控件**

- [ ] **Step 2: 接上 config 读写** `[RF-3]`

`init()` 里 `config_get` 回填三个控件；三个控件 change 时 `config_set`。热键保存失败时 Expected：banner 显示原因，按钮文案回到**上一个生效值**而不是用户刚按的组合。

- [ ] **Step 3: 状态条各态**

- [ ] **Step 4: 验证** 

Run: 抓屏确认浅色/深色两套主题下参数区两行不挤、`96px` 高度不与源列表打架（窗口 900×640 与最小 640×440 各看一次）。
Run: 改热键为 `Ctrl+Alt+R` → 关掉程序重开 → Expected：控件显示 `Ctrl+Alt+R`，且按它真的能开始录制。

- [ ] **Step 5: 汇报（不提交）**

---

### Task B6: 关掉两条开关之后的路径

**Files:**
- Modify: `crates/takekit-gui/src/take.rs`、`ui/main.js`

**Interfaces:**
- 无新类型；只加分支。`countdown_secs == 0` 与 `finalize_panel == false` 两条路径分别由 `begin`/`stop` 读取 `Config` 决定。

- [ ] **Step 1: 倒计时 0 秒路径**（§4.6）

`begin` 里若 `countdown_secs == 0`：置前 → 显示主窗口 `hide()` → `tray::set_recording(true)` → `beep(440, 120)` → **不显示遮罩**、不 emit countdown → 等 50ms（`std::thread::sleep`，给 DWM 处理窗口隐藏）→ 直接 `take::spawn`。`AppState.pending` 在此路径下即用即清。

- [ ] **Step 2: 收尾面板关闭路径**（§4.6）

`stop` 里若 `finalize_panel == false`：不显示遮罩，直接恢复主窗口，改由状态条显示 `正在写入… n/m`，全部完成后 `已完成 … · … MB`，3 秒后回 idle。

- [ ] **Step 3: 验证 0 秒路径**

Run: 把倒计时设为 0，按 F9。Expected：主窗口立刻隐藏、一声短音、无 `3/2/1/GO`、托盘图标随即变红；录下来的文件开头有 0.3–0.5 秒的桌面原样（这是 §4.6 写明的代价，不是 bug）。

- [ ] **Step 4: 验证无收尾面板路径**

Run: 取消勾选 `收尾面板`，录 5 秒停止。Expected：没有任何全屏遮罩出现，主窗口立刻回来，状态条从 `正在写入… n/m` 走到 `已完成 …`，3 秒后回 idle。

- [ ] **Step 5: 汇报（不提交）**

---

### Task B7: 文档与台账回填

**Files:**
- Modify: `DESIGN.md`、`docs/perf-review.md`、`.workbuddy/memory/<当天>.md`、`README.md`（若有功能清单/用法章节）

**Interfaces:** 无代码接口。

- [ ] **Step 1: 更新 `DESIGN.md`**

- §4.6 生命周期表：`opacity` → `show`/`hide`（附 Task B2 里那条理由）。
- §4.6 倒计时/收尾时序：把实现中确定的实际值（spawn 点、遮罩收起点）与文档对齐，不一致处标明实际值。
- §4.9：音频实际由哪个窗口发出（B3 的实测结论），以及 `--autoplay-policy` 这个前提。
- §4.7：托盘面板的实现要点（定位来源、失焦收起）。
- §5：现在的窗口尺寸是 `900×640` / 最小 `640×440`（UI 重建时定的），与 §5 写的 `720×420` / `560×380` 不一致——按实际值回填，或与用户确认后改回。同一节里"唯一滚动区是源列表""间距刻度""左右内边距 16"这几条要在实现中核对一遍。

- [ ] **Step 2: 追加 `docs/perf-review.md` 台账**

按现有格式追加 T16（停止通道）与 T17（提示层）两行，含验证结论（倒计时时序实测值、音频是否在隐藏窗口下工作、置前的实测结果）。

- [ ] **Step 3: 追加 workbuddy 日志**

在 `.workbuddy/memory/<当天>.md` 末尾追加一条（append-only，不回改已有段落），记本轮做了什么、验证结论、未提交状态。

- [ ] **Step 4: 汇报（不提交）**

---

## 收尾

全部任务完成后，把这份计划里的"偏离 spec 的三处"（提示层 `show`/`hide`、音频发声窗口、热键插件引入）连同实测结论一并汇总给用户，等用户发话再提交。
