// UI strings for both languages the panel ships.
//
// Flat keys (`tab.sources`) rather than a tree: a key is greppable from the
// markup that uses it, and a missing translation is one line to fix instead of
// a walk down a nested object.
//
// The markup carries the Chinese text as its own fallback (so a page still
// reads correctly if this file fails to load) and names the key in
// `data-i18n`; `applyI18n()` rewrites those nodes. Anything built in JS asks
// `t()` directly.
//
// Rust never sends prose: user-facing failures arrive as `{ code, detail }`
// and `uiErrorText()` turns them into a sentence in the current language. That
// is the only way the same failure can read correctly in both.

const I18N = {
  zh: {
    "tabs.aria": "面板",
    "tab.sources": "录制",
    "tab.history": "历史",
    "tab.log": "日志",
    "tab.settings": "设置",

    "section.sources": "源",

    "sources.add": "添加源",
    "sources.emptyTitle": "还没有源",
    "sources.emptyHint": "添加一个窗口、桌面或摄像头开始录制。",

    "history.clear": "清空…",
    "history.clearArmed": "再点一次删除文件",
    "log.clear": "清空…",
    "log.clearArmed": "再点一次清空",
    "history.emptyTitle": "还没有录制过",
    "history.emptyHint": "按 F9 或点「开始录制」，录完的那一段会出现在这里。",

    "log.emptyTitle": "没有需要处理的事",
    "log.emptyHint": "源失效、编码失败这类问题会记在这里。",

    "params.out": "输出目录",
    "params.outPick": "选择输出目录",
    "params.backend": "采集方式",
    "params.backendWindows": "Windows API（后台窗口可录）",
    "params.backendFfmpeg": "ffmpeg",
    "params.encoder": "视频编码",
    "params.encoderAuto": "自动",
    "params.encoderAutoWith": "自动（{name}）",
    "params.ffmpegPath": "ffmpeg 路径",
    "params.ffmpegPathHint": "留空则在 PATH 里找；手动指定后按指定的来",
    "params.ffmpegPick": "选择 ffmpeg 程序",
    "params.marks": "鼠标效果",
    "params.marksHint": "点击涟漪与拖动轨迹叠加到录制画面上",
    "params.countdown": "倒计时",
    "params.raise": "窗口置前",
    "params.raiseHint": "最小化的窗口开录时都会自动恢复显示",
    "params.countdownHint": "关掉后按 F9 直接开录，开头 0.3–0.5 秒不进画面",
    "params.finalizePanel": "收尾面板",
    "params.finalizePanelHint": "停止时的全屏收尾清单；关掉后进度只在状态条显示",
    "params.hotkey": "快捷键",
    "params.hotkeyHint": "点击后按新的组合键",

    "hotkey.capturing": "按下组合键…",
    "hotkey.cancelNote": "Esc 取消",

    "bar.rec": "开始录制",
    "bar.stop": "停止",
    "bar.hintSources": "{n} 个源",
    "bar.hintRecording": "录制中",
    "bar.sourceStopped": "已停止",
    "bar.writing": "正在写入… {n}/{m}",
    "bar.done": "已完成 {secs} · {mb} MB",
    "bar.pause": "暂停",
    "bar.resume": "继续",
    "bar.pausedFor": "已暂停 {at}",
    "bar.pauseNeedsWin": "暂停需要 Windows 后端：现在每个源是独立的 ffmpeg 子进程，各自计时，停不住",
    "bar.viewLogs": "查看日志",

    "notice.close": "关闭",

    "picker.search": "搜索窗口、摄像头或声音…",
    "picker.desktop": "桌面",
    "picker.windows": "窗口",
    "picker.cameras": "摄像头",
    "picker.mics": "麦克风",
    "picker.sound": "声音",
    "picker.systemAudio": "系统声音",
    "picker.systemAudioMeta": "扬声器播放的一切",
    "picker.micMeta": "麦克风",
    "picker.desktopAll": "整个桌面",
    "picker.allDisplays": "全部显示器",
    "picker.none": "没有匹配项",
    "picker.bgCapable": "被遮挡时仍可录",
    "picker.noSignal": "无信号 · 先启动宿主程序",
    "picker.added": "已添加 · {meta}",
    "picker.addedTitle": "已在清单中",

    "label.desktop": "整个桌面",
    "label.monitor": "显示器 {n}",
    "label.systemAudio": "系统声音",
    "label.windowSound": "窗口声音",

    "row.fpsLabel": "帧率",
    "row.qualityLabel": "画质",
    "row.editLabel": "编辑源：{name}",
    "row.remove": "移除",
    "row.sizeUnknown": "尺寸未知",
    "row.startTitle": "这个文件在本次录制时间轴上的起点（take.json 的 start）",
    "row.startPending": "还没出画面",
    "row.cropDropped": "裁剪超出画面，按整幅录制",
    "row.soundOn": "声音已录进文件，点击静音",
    "row.soundOff": "只有画面，点击录声音",
    "row.soundDesktop": "桌面录的是整台电脑的声音",

    "editor.close": "关闭",
    "editor.crop": "裁剪",
    "editor.sound": "静音",
    "editor.soundHint": "打开后这个文件只有画面，没有声音",
    "editor.cropOpen": "调整裁剪…",
    "editor.cancel": "取消",
    "editor.save": "保存",
    "editor.size": "输出 {w} × {h}",
    "editor.sizeWhole": "输出整幅（裁剪超出画面）",
    "editor.sizeUnknown": "尺寸未知",
    "editor.loading": "正在抓取画面…",
    "editor.noStill": "这个源暂时抓不到静帧",
    "editor.restoreWindow": "恢复窗口",
    "preview.minimized": "窗口已最小化，系统不再绘制它，没有画面可对照。恢复窗口后才能裁剪。",
    "preview.recycled": "这个句柄现在是别的进程的窗口了，请重新添加这个源",
    "preview.camera": "摄像头没有静帧画面",
    "preview.audio": "声音源没有画面，只有声音",

    "crop.title": "裁剪",
    "crop.oneToOne": "1 px = 源 1 px",
    "crop.back": "返回",
    "crop.source": "源 {w} × {h}",
    "crop.dropped": "整幅",
    "crop.droppedHint": "裁剪超出画面，这个文件会按整幅录制",
    "crop.noSource": "没有可用的画面",
    "crop.zoomIn": "放大",
    "crop.zoomOut": "缩小",
    "crop.fit": "适应",
    "crop.actual": "1:1",
    "crop.reset": "重置视图",
    "crop.pan": "平移",
    "crop.hint": "滚轮缩放 · 空格拖动 · 方向键微调",
    "crop.top": "上",
    "crop.bottom": "下",
    "crop.left": "左",
    "crop.right": "右",
    "crop.output": "输出",
    "crop.clear": "恢复全幅",
    "crop.handle.nw": "左上角",
    "crop.handle.n": "上边",
    "crop.handle.ne": "右上角",
    "crop.handle.e": "右边",
    "crop.handle.se": "右下角",
    "crop.handle.s": "下边",
    "crop.handle.sw": "左下角",
    "crop.handle.w": "左边",

    "day.today": "今天",
    "day.yesterday": "昨天",

    "take.unknown": "未知",
    "take.new": "新",
    "take.delete": "删除",
    "take.deleteArmed": "再点一次删除",
    "take.deleteLabel": "删除这次录制：{when}",
    "log.openDir": "打开目录",
    "log.delete": "删除",
    "log.deleteArmed": "再点一次删除",
    "log.deleteLabel": "删除这条日志：{when}",

    "notice.pruned": "已移除 {n} 个不可用的源（窗口已关闭或不在当前桌面）",
    "notice.dropped": "已停止并移除 {n} 个不可用的源（窗口已关闭或不在当前桌面）",
    "notice.noSources": "没有可用的源",
    "notice.noFfmpeg": "找不到 ffmpeg，现在还不能录制",
    "notice.noFfmpegDetail": "安装 ffmpeg 并加入 PATH，重启面板后生效。",
    "notice.takePartial": "录制完成，但 {n} 路没有录到（已保存 {secs} · {mb} MB）",
    "notice.lenMismatch": "{n} 个文件的实测长度与 take.json 不符（文件可用，记录已标注）",
    "warn.len-mismatch": "容器时长与清单记录对不上",

    "settings.interface": "界面",
    "settings.capture": "采集与编码",
    "settings.recording": "录制行为",
    "settings.language": "语言",
    "settings.theme": "主题",
    "settings.themeAuto": "跟随系统",
    "settings.themeLight": "浅色",
    "settings.themeDark": "深色",
    "settings.control": "Agent 控制",
    "settings.controlHint": "允许本机的 AI Agent 通过本地接口操作面板；关闭后接口下线",
    "settings.note": "各项设置立即生效，并记住到下次启动。",

    "tray.stateIdle": "未在录制",
    "tray.stateRecording": "录制中",
    "tray.stateStopping": "正在写入…",
    "tray.add": "添加源",
    "tray.stop": "停止",
    "tray.show": "显示主窗口",
    "tray.empty": "没有正在录制的源",
    "tray.stopped": "已停止",
    "tray.stopFailed": "停止失败",

    "err.starting": "正在准备录制",
    "err.already-recording": "已经在录制中",
    "err.countdown": "倒计时进行中，请稍候",
    "err.finalizing": "上一条还在收尾，请稍候",
    "err.no-sources": "没有可用的源",
    "err.no-ffmpeg": "找不到 ffmpeg（请安装并加入 PATH）",
    "err.prepare-failed": "准备失败：{detail}",
    "err.overlay": "无法显示倒计时遮罩，已取消",
    "err.spawn-failed": "启动失败：{detail}",
    "err.nothing-pending": "没有待启动的录制",
    "err.not-recording": "当前没有在录制",
    "err.stop-failed": "收尾失败：{detail}",
    "err.add-source-failed": "加入录制失败：{detail}",
    "err.pause-failed": "暂停失败：{detail}",
    "err.hotkey-unknown": "无法识别的按键组合：{detail}",
    "err.hotkey-conflict": "快捷键 {detail} 注册失败（可能已被其他程序占用）",
    "err.save": "没能保存设置：{detail}",
    "err.internal": "内部错误：{detail}",
    "err.take-gone": "目录已不在：{detail}",
    "err.reveal-failed": "打开目录失败：{detail}",
    "err.not-a-take": "这不是录制目录：{detail}",
    "err.delete-failed": "删除失败：{detail}",
    "err.log-entry-gone": "该日志条目已不存在（可能已被删除）",
    "err.cam-no-signal": "{detail} 已注册但没有画面——虚拟摄像头需要先启动它的宿主程序（如 Mikey 客户端），再重新录制",
    "err.cam-not-mf": "{detail} 是 DirectShow 虚拟摄像头，Windows 后端看不到它——请改用 ffmpeg 后端录制",
    "err.device-gone": "找不到设备 {detail}——可能已被拔出或改名，请重新选择",
    "err.cam-open": "{detail} 打不开——如果是虚拟摄像头，请先启动它的宿主程序（如 Mikey 客户端）；否则可能正被其他程序占用",
    "err.ffmpeg-died": "录制进程没有写出任何画面",
  },

  en: {
    "tabs.aria": "Panel",
    "tab.sources": "Record",
    "tab.history": "History",
    "tab.log": "Log",
    "tab.settings": "Settings",

    "section.sources": "Sources",

    "sources.add": "Add source",
    "sources.emptyTitle": "No sources yet",
    "sources.emptyHint": "Add a window, the desktop or a camera to start recording.",

    "history.clear": "Clear…",
    "history.clearArmed": "Click again to delete the files",
    "log.clear": "Clear…",
    "log.clearArmed": "Click again to clear",
    "history.emptyTitle": "Nothing recorded yet",
    "history.emptyHint":
      "Press F9 or click Start recording — a finished take appears here.",

    "log.emptyTitle": "Nothing to fix",
    "log.emptyHint": "Lost sources and encoding failures are recorded here.",

    "params.out": "Output folder",
    "params.outPick": "Choose output folder",
    "params.backend": "Capture method",
    "params.backendWindows": "Windows API (records background windows)",
    "params.backendFfmpeg": "ffmpeg",
    "params.encoder": "Video encoder",
    "params.encoderAuto": "Auto",
    "params.encoderAutoWith": "Auto ({name})",
    "params.ffmpegPath": "ffmpeg path",
    "params.ffmpegPathHint": "Empty = look in PATH; a path here wins",
    "params.ffmpegPick": "Choose the ffmpeg executable",
    "params.marks": "Mouse effects",
    "params.marksHint": "Draw click ripples and drag trails into the recording",
    "params.countdown": "Countdown",
    "params.raise": "Raise windows",
    "params.raiseHint": "Minimized windows are always brought back at record time",
    "params.countdownHint": "Off: F9 starts instantly; the first 0.3–0.5 s stays out of frame",
    "params.finalizePanel": "Stop screen",
    "params.finalizePanelHint": "The full-screen stop checklist; off = progress on the status bar only",
    "params.hotkey": "Hotkey",
    "params.hotkeyHint": "Click, then press a new shortcut",

    "hotkey.capturing": "Press shortcut…",
    "hotkey.cancelNote": "Esc to cancel",

    "bar.rec": "Start recording",
    "bar.stop": "Stop",
    "bar.hintSources": "{n} sources",
    "bar.hintRecording": "Recording",
    "bar.sourceStopped": "stopped",
    "bar.writing": "Writing… {n}/{m}",
    "bar.done": "Done {secs} · {mb} MB",
    "bar.pause": "Pause",
    "bar.resume": "Resume",
    "bar.pausedFor": "Paused {at}",
    "bar.pauseNeedsWin": "Pause needs the Windows backend: each source is an ffmpeg child with a clock of its own",
    "bar.viewLogs": "View log",

    "notice.close": "Dismiss",

    "picker.search": "Search windows, cameras or sound…",
    "picker.desktop": "Desktop",
    "picker.windows": "Windows",
    "picker.cameras": "Cameras",
    "picker.mics": "Microphones",
    "picker.sound": "Sound",
    "picker.systemAudio": "System audio",
    "picker.systemAudioMeta": "everything the speakers play",
    "picker.micMeta": "microphone",
    "picker.desktopAll": "Entire desktop",
    "picker.allDisplays": "All displays",
    "picker.none": "No matches",
    "picker.bgCapable": "keeps recording when covered",
    "picker.noSignal": "no signal — start its host app",
    "picker.added": "Added · {meta}",
    "picker.addedTitle": "Already in the list",

    "label.desktop": "Entire desktop",
    "label.monitor": "Display {n}",
    "label.systemAudio": "System audio",
    "label.windowSound": "window sound",

    "row.fpsLabel": "Frame rate",
    "row.qualityLabel": "Quality",
    "row.editLabel": "Edit source: {name}",
    "row.remove": "Remove",
    "row.sizeUnknown": "Size unknown",
    "row.startTitle": "Where this file begins on the take's timeline (take.json's `start`)",
    "row.startPending": "no frame yet",
    "row.cropDropped": "The crop is larger than the picture; the whole frame is recorded",
    "row.soundOn": "Sound is in the file — click to mute",
    "row.soundOff": "Picture only — click to record sound",
    "row.soundDesktop": "the desktop records all system sound",

    "editor.close": "Close",
    "editor.crop": "Crop",
    "editor.sound": "Mute",
    "editor.soundHint": "On means this file carries picture only, no sound",
    "editor.cropOpen": "Adjust crop…",
    "editor.cancel": "Cancel",
    "editor.save": "Save",
    "editor.size": "Output {w} × {h}",
    "editor.sizeWhole": "Output whole frame (crop too large)",
    "editor.sizeUnknown": "Size unknown",
    "editor.loading": "Taking a still…",
    "editor.noStill": "No still available for this source",
    "editor.restoreWindow": "Restore window",
    "preview.minimized": "This window is minimized, so Windows is not drawing it and there is nothing to crop against. Restore it first.",
    "preview.recycled": "This handle belongs to another process's window now — add the source again",
    "preview.camera": "A camera has no still preview",
    "preview.audio": "A sound source has no picture",

    "crop.title": "Crop",
    "crop.oneToOne": "1 px = 1 source px",
    "crop.back": "Back",
    "crop.source": "Source {w} × {h}",
    "crop.dropped": "whole frame",
    "crop.droppedHint": "The crop is larger than the picture, so this file records the whole frame",
    "crop.noSource": "No picture available",
    "crop.zoomIn": "Zoom in",
    "crop.zoomOut": "Zoom out",
    "crop.fit": "Fit",
    "crop.actual": "1:1",
    "crop.reset": "Reset view",
    "crop.pan": "Pan",
    "crop.hint": "Wheel zooms · Space pans · Arrows nudge",
    "crop.top": "Top",
    "crop.bottom": "Bottom",
    "crop.left": "Left",
    "crop.right": "Right",
    "crop.output": "Output",
    "crop.clear": "Reset to full",
    "crop.handle.nw": "Top-left corner",
    "crop.handle.n": "Top edge",
    "crop.handle.ne": "Top-right corner",
    "crop.handle.e": "Right edge",
    "crop.handle.se": "Bottom-right corner",
    "crop.handle.s": "Bottom edge",
    "crop.handle.sw": "Bottom-left corner",
    "crop.handle.w": "Left edge",

    "day.today": "Today",
    "day.yesterday": "Yesterday",

    "take.unknown": "unknown",
    "take.new": "New",
    "take.delete": "Delete",
    "take.deleteArmed": "Click again to delete",
    "take.deleteLabel": "Delete this recording: {when}",
    "log.openDir": "Open folder",
    "log.delete": "Delete",
    "log.deleteArmed": "Click again to delete",
    "log.deleteLabel": "Delete this log entry: {when}",

    "notice.pruned": "Removed {n} unusable source(s) — the window closed or left this desktop",
    "notice.dropped": "Stopped and removed {n} unusable source(s) — the window closed or left this desktop",
    "notice.noSources": "No usable sources",
    "notice.noFfmpeg": "ffmpeg was not found, so recording is unavailable",
    "notice.noFfmpegDetail": "Install ffmpeg and add it to PATH, then restart the panel.",
    "notice.takePartial": "Take finished, but {n} source(s) recorded nothing (saved {secs} · {mb} MB)",
    "notice.lenMismatch": "{n} file(s) are not the length take.json says (usable, but flagged)",
    "warn.len-mismatch": "the container's length disagrees with the manifest",

    "settings.interface": "Interface",
    "settings.capture": "Capture & encoding",
    "settings.recording": "Recording",
    "settings.language": "Language",
    "settings.theme": "Theme",
    "settings.themeAuto": "System",
    "settings.themeLight": "Light",
    "settings.themeDark": "Dark",
    "settings.control": "Agent control",
    "settings.controlHint": "Let local AI agents drive the panel over a loopback API; off closes the door",
    "settings.note": "All settings apply immediately and are remembered.",

    "tray.stateIdle": "Not recording",
    "tray.stateRecording": "Recording",
    "tray.stateStopping": "Writing…",
    "tray.stop": "Stop",
    "tray.add": "Add source",
    "tray.show": "Show panel",
    "tray.empty": "No sources recording",
    "tray.stopped": "stopped",
    "tray.stopFailed": "Stop failed",

    "err.starting": "Already preparing a take",
    "err.already-recording": "A take is already running",
    "err.countdown": "Counting down — one moment",
    "err.finalizing": "The last take is still being finished",
    "err.no-sources": "No usable sources",
    "err.no-ffmpeg": "ffmpeg was not found — install it and add it to PATH",
    "err.prepare-failed": "Could not prepare the take: {detail}",
    "err.overlay": "Could not show the countdown, so nothing started",
    "err.spawn-failed": "Could not start recording: {detail}",
    "err.nothing-pending": "Nothing left to start",
    "err.not-recording": "Not recording",
    "err.stop-failed": "Could not finish the take: {detail}",
    "err.add-source-failed": "Could not add the source: {detail}",
    "err.pause-failed": "Could not pause: {detail}",
    "err.hotkey-unknown": "That key combination cannot be used: {detail}",
    "err.hotkey-conflict": "Could not register {detail} — another app may already use it",
    "err.save": "Could not save the setting: {detail}",
    "err.internal": "Internal error: {detail}",
    "err.take-gone": "That folder is gone: {detail}",
    "err.reveal-failed": "Could not open the folder: {detail}",
    "err.not-a-take": "That is not a take folder: {detail}",
    "err.delete-failed": "Could not delete: {detail}",
    "err.log-entry-gone": "That log entry is already gone",
    "err.cam-no-signal": "{detail} is registered but sends no video — start its host app (e.g. the Mikey client), then record again",
    "err.cam-not-mf": "{detail} is a DirectShow virtual camera that the Windows backend cannot see — record it on the ffmpeg backend",
    "err.device-gone": "No device named {detail} — it may be unplugged or renamed; pick it again",
    "err.cam-open": "{detail} could not be opened — if it is a virtual camera, start its host app (e.g. the Mikey client); otherwise it may be in use by another app",
    "err.ffmpeg-died": "the recording process wrote no frames",
  },
};

// Rust seeds the language before the page loads (see ui_script in main.rs):
// without it the page would paint Chinese and switch a frame later, which
// reads as a flicker rather than as a setting.
let uiLang = window.__TAKEKIT_LANG__ === "en" ? "en" : "zh";

/// The sentence for `key`, with `{name}` placeholders filled from `vars`.
/// A key missing from a translation falls back to Chinese, then to the key
/// itself — a half-translated build still says something usable.
function t(key, vars) {
  const table = I18N[uiLang] || I18N.zh;
  let text = table[key];
  if (text === undefined) text = I18N.zh[key];
  if (text === undefined) return key;
  if (!vars) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name) =>
    vars[name] === undefined ? whole : String(vars[name]),
  );
}

/// Rewrite every node that names a string, under `root` or the whole document.
function applyI18n(root) {
  const scope = root || document;
  for (const el of scope.querySelectorAll("[data-i18n]")) {
    el.textContent = t(el.dataset.i18n);
  }
  for (const el of scope.querySelectorAll("[data-i18n-title]")) {
    el.title = t(el.dataset.i18nTitle);
  }
  for (const el of scope.querySelectorAll("[data-i18n-ph]")) {
    el.placeholder = t(el.dataset.i18nPh);
  }
  for (const el of scope.querySelectorAll("[data-i18n-aria]")) {
    el.setAttribute("aria-label", t(el.dataset.i18nAria));
  }
}

/// Switch language and repaint the static text. Callers re-render the parts
/// they built in JS (lists, hints, chips) right after this.
function setUiLang(lang) {
  uiLang = lang === "en" ? "en" : "zh";
  document.documentElement.lang = uiLang === "en" ? "en" : "zh-CN";
  applyI18n();
}

/// A failure from Rust is `{ code, detail }`, never prose. Translate the code
/// and append the technical half.
function uiErrorText(err) {
  if (err && typeof err === "object" && typeof err.code === "string") {
    return t(err.code, { detail: err.detail == null ? "" : String(err.detail) });
  }
  return String(err);
}

/// A bare error string from the core (`Outcome.error` and friends) may be a
/// `diagnose` code with its device name (`err.cam-no-signal|Mikey Cam`).
/// Translate those too; anything else is already the sentence.
function codeText(raw) {
  const s = String(raw ?? "");
  const at = s.indexOf("|");
  if (at > 0) {
    const code = s.slice(0, at);
    if (/^err\.[a-z-]+$/.test(code)) return t(code, { detail: s.slice(at + 1) });
  }
  return s;
}

/// How a shortcut is shown to a person: `control+alt+KeyR` (what the plugin
/// round-trips) is not what anyone calls that key.
function prettyHotkey(spec) {
  return String(spec)
    .split("+")
    .map((raw) => {
      const part = raw.trim();
      const lower = part.toLowerCase();
      if (lower === "control" || lower === "ctrl") return "Ctrl";
      if (lower === "shift") return "Shift";
      if (lower === "alt") return "Alt";
      if (lower === "super" || lower === "meta" || lower === "command") return "Win";
      if (/^key[a-z]$/i.test(part)) return part.slice(3).toUpperCase();
      if (/^digit\d$/i.test(part)) return part.slice(5);
      return part;
    })
    .join("+");
}

// Both pages load their scripts at the end of <body>, so every node is already
// parsed: apply now, before the browser paints, instead of waiting for the
// async config read.
setUiLang(uiLang);
