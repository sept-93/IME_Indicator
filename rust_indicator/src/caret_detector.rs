//! 文本光标位置检测模块 - 多级检测策略

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::CUIAutomation;
use windows::Win32::UI::Accessibility::IUIAutomation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LBUTTON, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    GUITHREADINFO,
};

// ============================================================================
// 常量定义
// ============================================================================

/// MSAA OBJID_CARET 常量
const OBJID_CARET: u32 = 0xFFFFFFF8u32;

/// IID_IAccessible GUID: {618736e0-3c3d-11cf-810c-00aa00389b71}
const IID_IACCESSIBLE: u128 = 0x618736e0_3c3d_11cf_810c_00aa00389b71;

/// 这些自绘应用不暴露可靠的 UIA 子控件；查询焦点元素会阻塞数秒。
/// 直接使用 Caret 或应用专用状态判断，避免 UIA 卡顿。
const CARET_FAST_PATH_APPS: &[&str] = &[
    "Weixin.exe",
    "WXWork.exe",
    "WeCom.exe",
    "Photoshop.exe",
    "Illustrator.exe",
    "Cinema 4D.exe",
    "JianyingPro.exe",
    "CapCut.exe",
    "SearchHost.exe",
    "SearchApp.exe",
    "StartMenuExperienceHost.exe",
    "wps.exe",
    "et.exe",
    "wpp.exe",
    "WINWORD.EXE",
    "EXCEL.EXE",
    "POWERPNT.EXE",
];

/// 对这些应用不做 Caret/MSAA/UIA 或应用内编辑模式探测，只读取系统 IME 状态。
/// 这些自绘消息循环对跨进程辅助功能查询敏感，只读取系统输入法状态。
const INDICATOR_ONLY_APPS: &[&str] = &["Cinema 4D.exe", "JianyingPro.exe", "CapCut.exe"];

/// 浏览器和富文本应用常把真正的编辑区暴露为 Custom/Text/Group，而不是标准 Edit。
/// 仅对这些已验证应用允许用当前线程的真实 Caret 补足 UIA，避免重新放宽到所有
/// 自绘程序（eCloud、FlClash 等会在非输入区保留假的 Caret）。
const CARET_COMPAT_APPS: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "Tabbit Browser.exe",
    "Feishu.exe",
    "Lark.exe",
    "wps.exe",
    "et.exe",
    "wpp.exe",
    "WINWORD.EXE",
    "EXCEL.EXE",
    "POWERPNT.EXE",
];

/// Photoshop/Illustrator 的画布文字光标完全由应用绘制，Windows 没有 Caret 对象。
/// 在这些应用内按 T 进入文字工具后进入兼容输入模式，Esc 或 Ctrl+Enter 退出。
const DESIGN_TEXT_SHORTCUT_APPS: &[&str] = &["Photoshop.exe", "Illustrator.exe"];

/// Adobe 设计软件的启动/画布窗口不接受 MSAA 探测；原生重命名框只用
/// GetGUIThreadInfo，画布文字则由应用专用状态机识别。
const GUI_ONLY_CARET_APPS: &[&str] = &["Photoshop.exe", "Illustrator.exe"];

/// WPS 与 Microsoft Office 的文档/表格画布经常不公开标准 Edit 控件。
const OFFICE_EDIT_APPS: &[&str] = &[
    "wps.exe",
    "et.exe",
    "wpp.exe",
    "WINWORD.EXE",
    "EXCEL.EXE",
    "POWERPNT.EXE",
];

const OFFICE_SPREADSHEET_APPS: &[&str] = &["et.exe", "EXCEL.EXE"];

/// 粘贴文字或图片时，这些富文本应用可能短暂撤销系统 Caret，随后仍回到
/// 原输入框。短暂缺失不能被解释成“离开输入区”。
const TRANSIENT_CARET_APPS: &[&str] = &[
    "Weixin.exe",
    "WXWork.exe",
    "WeCom.exe",
    "Feishu.exe",
    "Lark.exe",
];
const CARET_DROPOUT_GRACE_MS: u64 = 500;
const PASTE_CARET_GRACE_MS: u64 = 3000;

// UIA/IME 查询偶尔会让主检测循环停顿几十到数百毫秒。单独锁存 Esc 的
// 按下沿，避免 Photoshop/Illustrator 已经退出文字编辑，但主循环漏掉按键。
static ESCAPE_LATCHED: AtomicBool = AtomicBool::new(false);
static ESCAPE_WATCHER_STARTED: AtomicBool = AtomicBool::new(false);
static LEFT_CLICK_SAMPLES: Mutex<VecDeque<ClickSample>> = Mutex::new(VecDeque::new());

#[derive(Clone, Copy)]
struct ClickSample {
    at: Instant,
    process_id: u32,
    x: i32,
    y: i32,
}

pub fn start_escape_watcher(running: Arc<AtomicBool>) {
    if ESCAPE_WATCHER_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    thread::spawn(move || {
        let mut was_down = false;
        let mut left_was_down = false;
        while running.load(Ordering::SeqCst) {
            let down = unsafe { (GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000) != 0 };
            if down && !was_down {
                ESCAPE_LATCHED.store(true, Ordering::SeqCst);
            }
            was_down = down;

            // 主检测循环通常只每 100ms 读取一次状态，快速双击的两次按下可能
            // 被合并成一次。5ms 采样独立记录每个按下沿，确保 Photoshop 画布
            // 中双击已有文字时不会漏掉第二次点击。
            let left_down = unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 };
            if left_down && !left_was_down {
                let mut point = POINT::default();
                if unsafe { GetCursorPos(&mut point).is_ok() } {
                    let foreground = unsafe { GetForegroundWindow() };
                    let mut process_id = 0;
                    unsafe {
                        GetWindowThreadProcessId(foreground, Some(&mut process_id));
                    }
                    if let Ok(mut samples) = LEFT_CLICK_SAMPLES.lock() {
                        if samples.len() >= 12 {
                            samples.pop_front();
                        }
                        samples.push_back(ClickSample {
                            at: Instant::now(),
                            process_id,
                            x: point.x,
                            y: point.y,
                        });
                    }
                }
            }
            left_was_down = left_down;
            thread::sleep(Duration::from_millis(5));
        }
        ESCAPE_WATCHER_STARTED.store(false, Ordering::SeqCst);
    });
}

fn take_click_samples(process_id: u32) -> Vec<ClickSample> {
    let Ok(mut queued) = LEFT_CLICK_SAMPLES.lock() else {
        return Vec::new();
    };
    let mut matching = Vec::new();
    while let Some(sample) = queued.pop_front() {
        if sample.at.elapsed() <= Duration::from_millis(650) && sample.process_id == process_id {
            matching.push(sample);
        }
    }
    matching
}

fn consume_click_samples(
    last_left_click: &mut Option<(Instant, u32, i32, i32)>,
    click_samples: impl IntoIterator<Item = ClickSample>,
) -> bool {
    let mut double_click = false;
    for sample in click_samples {
        if last_left_click.as_ref().is_some_and(|(at, pid, x, y)| {
            *pid == sample.process_id
                && sample
                    .at
                    .checked_duration_since(*at)
                    .is_some_and(|elapsed| elapsed <= Duration::from_millis(500))
                && (sample.x - *x).abs() <= 8
                && (sample.y - *y).abs() <= 8
        }) {
            double_click = true;
        }
        *last_left_click = Some((sample.at, sample.process_id, sample.x, sample.y));
    }
    if double_click {
        // 一次双击只消费成一个切换事件。第二击不再作为下一次双击的首击。
        *last_left_click = None;
    }
    double_click
}

// ============================================================================
// 类型定义
// ============================================================================

/// 光标位置信息 (x, y, height)
pub type CaretPos = (i32, i32, i32);

/// 自动切换所需的稳定焦点上下文。identity 只在应用或焦点元素变化时改变，
/// 不随同一输入框内的光标移动而变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusContext {
    pub identity: String,
    pub foreground_hwnd: HWND,
    pub focused_hwnd: HWND,
    pub process_id: u32,
    pub editable: bool,
    pub password: bool,
    pub readonly_document: bool,
    pub force_mouse_indicator: bool,
    pub input_double_click: bool,
}

/// 检测来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionSource {
    GuiInfo,
    MsaaCaret,
    None,
}

impl DetectionSource {
    /// 从配置名解析（配置里用 snake_case）
    fn from_name(s: &str) -> Option<Self> {
        match s {
            "gui_info" => Some(DetectionSource::GuiInfo),
            "msaa_caret" => Some(DetectionSource::MsaaCaret),
            _ => None,
        }
    }
}

// ============================================================================
// CaretDetector 实现
// ============================================================================

/// 文本光标检测器
pub struct CaretDetector {
    automation: Option<IUIAutomation>,
    pub last_source: DetectionSource,
    pub last_uia_error: String,
    design_text_processes: HashSet<u32>,
    design_text_tools: HashSet<u32>,
    design_native_processes: HashSet<u32>,
    design_caret_suppressed: HashSet<u32>,
    office_edit_processes: HashSet<u32>,
    office_caret_suppressed: HashSet<u32>,
    design_text_cursor_handles: HashMap<u32, isize>,
    pending_design_native: Option<(u32, Instant)>,
    pending_design_canvas: Option<(u32, Instant, Option<isize>, bool)>,
    pending_design_tool_check: Option<(u32, Instant)>,
    pending_office_edit: Option<(u32, Instant)>,
    transient_editable_seen: HashMap<u32, (usize, usize, Instant)>,
    paste_started: HashMap<u32, Instant>,
    pending_chat_inputs: HashMap<u32, Instant>,
    design_text_tool_armed_at: HashMap<u32, Instant>,
    known_design_text_cursors: HashMap<u32, HashSet<isize>>,
    known_design_nontext_cursors: HashMap<u32, HashSet<isize>>,
    t_down: bool,
    b_down: bool,
    escape_down: bool,
    enter_down: bool,
    left_down: bool,
    v_down: bool,
    last_left_click: Option<(Instant, u32, i32, i32)>,
    input_double_click: bool,
}

impl CaretDetector {
    /// 创建新的检测器
    pub fn new() -> Self {
        // 初始化 COM 和 UI Automation
        let automation = unsafe {
            // 初始化 COM (忽略错误，可能已经初始化)
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            // 创建 UI Automation 实例
            CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL).ok()
        };

        Self {
            automation,
            last_source: DetectionSource::None,
            last_uia_error: String::new(),
            design_text_processes: HashSet::new(),
            design_text_tools: HashSet::new(),
            design_native_processes: HashSet::new(),
            design_caret_suppressed: HashSet::new(),
            office_edit_processes: HashSet::new(),
            office_caret_suppressed: HashSet::new(),
            design_text_cursor_handles: HashMap::new(),
            pending_design_native: None,
            pending_design_canvas: None,
            pending_design_tool_check: None,
            pending_office_edit: None,
            transient_editable_seen: HashMap::new(),
            paste_started: HashMap::new(),
            pending_chat_inputs: HashMap::new(),
            design_text_tool_armed_at: HashMap::new(),
            known_design_text_cursors: HashMap::new(),
            known_design_nontext_cursors: HashMap::new(),
            t_down: false,
            b_down: false,
            escape_down: false,
            enter_down: false,
            left_down: false,
            v_down: false,
            last_left_click: None,
            input_double_click: false,
        }
    }

    /// 获取当前焦点元素的可编辑状态与稳定身份。UIA 查询失败时使用 Win32 焦点窗口
    /// 和是否存在真实文本光标作为保守回退。
    pub fn focus_context(&mut self, has_caret: bool) -> FocusContext {
        use windows::Win32::UI::Accessibility::{
            IUIAutomationValuePattern, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
            UIA_ValuePatternId,
        };

        unsafe {
            let foreground_hwnd = GetForegroundWindow();
            let mut process_id = 0u32;
            let thread_id = GetWindowThreadProcessId(foreground_hwnd, Some(&mut process_id));
            let mut gui_info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let focused_hwnd = if GetGUIThreadInfo(thread_id, &mut gui_info).is_ok()
                && !gui_info.hwndFocus.0.is_null()
            {
                gui_info.hwndFocus
            } else {
                foreground_hwnd
            };

            let fallback_identity =
                format!("{}:{}", foreground_hwnd.0 as usize, focused_hwnd.0 as usize);

            let process_name = process_name(process_id).unwrap_or_default();
            if process_is_indicator_only(&process_name) {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: false,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: false,
                    input_double_click: false,
                };
            }
            let special_edit_mode = self.update_application_edit_mode(
                &process_name,
                process_id,
                has_caret,
                focused_hwnd,
                gui_info.hwndCaret,
            );
            let input_double_click = self.input_double_click;

            // 微信等自绘应用的 UIA GetFocusedElement 会卡住约 3 秒，并且最终只返回
            // 顶层窗口。Win32/MSAA Caret 已能可靠反映聊天框和搜索框是否可输入。
            if process_matches(&process_name, CARET_FAST_PATH_APPS) {
                let strict_special_mode = process_matches(&process_name, DESIGN_TEXT_SHORTCUT_APPS)
                    || process_matches(&process_name, OFFICE_EDIT_APPS);
                let editable = if strict_special_mode {
                    special_edit_mode
                } else {
                    has_caret || special_edit_mode
                };
                let editable = self.stabilize_transient_editable(
                    &process_name,
                    process_id,
                    foreground_hwnd,
                    focused_hwnd,
                    editable,
                );
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: special_edit_mode && !has_caret,
                    input_double_click,
                };
            }

            let Some(automation) = self.automation.as_ref() else {
                let editable = self.stabilize_transient_editable(
                    &process_name,
                    process_id,
                    foreground_hwnd,
                    focused_hwnd,
                    has_caret || special_edit_mode,
                );
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: special_edit_mode && !has_caret,
                    input_double_click,
                };
            };
            let Ok(focused) = automation.GetFocusedElement() else {
                let editable = self.stabilize_transient_editable(
                    &process_name,
                    process_id,
                    foreground_hwnd,
                    focused_hwnd,
                    has_caret || special_edit_mode,
                );
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: special_edit_mode && !has_caret,
                    input_double_click,
                };
            };

            let control_type = focused.CurrentControlType().unwrap_or_default();
            let password = focused.CurrentIsPassword().map_or(false, |v| v.as_bool());
            let value_pattern = focused
                .GetCurrentPattern(UIA_ValuePatternId)
                .ok()
                .and_then(|p| p.cast::<IUIAutomationValuePattern>().ok());
            let value_is_readonly = value_pattern
                .as_ref()
                .and_then(|vp| vp.CurrentIsReadOnly().ok())
                .map(|v| v.as_bool());
            let caret_compat = process_matches(&process_name, CARET_COMPAT_APPS)
                || crate::config::auto_switch_extra_input_apps()
                    .iter()
                    .any(|candidate| process_name.eq_ignore_ascii_case(candidate));
            let standard_editable = if password {
                true
            } else if control_type == UIA_EditControlTypeId {
                value_is_readonly != Some(true)
            } else if control_type == UIA_DocumentControlTypeId {
                // 浏览器正文有时会残留可定位的 caret，但只读 Document 仍不能输入。
                // Word/contenteditable 优先使用 ValuePattern；部分 Chromium 富文本区
                // 不提供 ValuePattern，只对兼容应用接受真实 Caret。
                value_is_readonly == Some(false)
                    || (value_is_readonly.is_none() && has_caret && caret_compat)
            } else {
                // 富文本编辑器和飞书输入区经常是 Custom/Text/Group；只在兼容列表中
                // 使用 Caret 回退。其他应用保持严格模式，防止非输入区的黄色假点。
                has_caret && caret_compat
            };
            let editable = self.stabilize_transient_editable(
                &process_name,
                process_id,
                foreground_hwnd,
                focused_hwnd,
                standard_editable || special_edit_mode,
            );
            let readonly_document = control_type == UIA_DocumentControlTypeId && !editable;

            let native_hwnd = focused.CurrentNativeWindowHandle().unwrap_or_default();
            let identity = if let Some(runtime_id) = uia_runtime_id(&focused) {
                format!(
                    "{}:{}:{}:{:?}",
                    foreground_hwnd.0 as usize, native_hwnd.0 as usize, control_type.0, runtime_id,
                )
            } else {
                let automation_id = focused
                    .CurrentAutomationId()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let bounds = focused.CurrentBoundingRectangle().unwrap_or_default();
                format!(
                    "{}:{}:{}:{}:{}:{}:{}:{}",
                    foreground_hwnd.0 as usize,
                    native_hwnd.0 as usize,
                    control_type.0,
                    automation_id,
                    bounds.left,
                    bounds.top,
                    bounds.right,
                    bounds.bottom,
                )
            };

            FocusContext {
                identity,
                foreground_hwnd,
                focused_hwnd,
                process_id,
                editable,
                password,
                readonly_document,
                force_mouse_indicator: special_edit_mode && !has_caret,
                input_double_click,
            }
        }
    }

    fn stabilize_transient_editable(
        &mut self,
        process_name: &str,
        process_id: u32,
        foreground_hwnd: HWND,
        focused_hwnd: HWND,
        editable: bool,
    ) -> bool {
        if !process_matches(process_name, TRANSIENT_CARET_APPS) {
            return editable;
        }

        let foreground = foreground_hwnd.0 as usize;
        let focused = focused_hwnd.0 as usize;
        if editable {
            self.transient_editable_seen
                .insert(process_id, (foreground, focused, Instant::now()));
            return true;
        }

        let last_seen = self.transient_editable_seen.get(&process_id);
        let same_foreground =
            last_seen.is_some_and(|(last_foreground, _, _)| *last_foreground == foreground);
        let same_focus = last_seen.is_some_and(|(_, last_focused, _)| *last_focused == focused);
        let last_seen_elapsed = last_seen.map(|(_, _, seen)| seen.elapsed());
        let paste_elapsed = self.paste_started.get(&process_id).map(Instant::elapsed);
        should_preserve_transient_editable(
            same_foreground,
            same_focus,
            last_seen_elapsed,
            paste_elapsed,
        )
    }

    fn update_application_edit_mode(
        &mut self,
        process_name: &str,
        process_id: u32,
        has_caret: bool,
        focused_hwnd: HWND,
        caret_hwnd: HWND,
    ) -> bool {
        self.input_double_click = false;
        let (t_now, t_since_last) = key_sample(0x54); // T
        let (b_now, b_since_last) = key_sample(0x42); // B
        let (escape_now, escape_since_last) = key_sample(VK_ESCAPE.0 as i32);
        let (enter_now, enter_since_last) = key_sample(VK_RETURN.0 as i32);
        let (left_now, left_since_last) = key_sample(VK_LBUTTON.0 as i32);
        let (v_now, v_since_last) = key_sample(0x56); // V
        let (ctrl_now, ctrl_since_last) = key_sample(VK_CONTROL.0 as i32);

        // 同时使用高位按键状态和低位“自上次查询后按过”标志，避免短按 T/Esc
        // 刚好落在两次 30ms 轮询之间，造成 Illustrator 偶发无法进入或退出。
        let t_pressed = t_since_last || (t_now && !self.t_down);
        let b_pressed = b_since_last || (b_now && !self.b_down);
        let escape_pressed = ESCAPE_LATCHED.swap(false, Ordering::SeqCst)
            || escape_since_last
            || (escape_now && !self.escape_down);
        let enter_pressed = enter_since_last || (enter_now && !self.enter_down);
        let direct_left_pressed = left_since_last || (left_now && !self.left_down);
        let v_pressed = v_since_last || (v_now && !self.v_down);
        let ctrl_active = ctrl_now || ctrl_since_last;
        let v_tool_pressed = v_pressed && !ctrl_active;
        self.t_down = t_now;
        self.b_down = b_now;
        self.escape_down = escape_now;
        self.enter_down = enter_now;
        self.left_down = left_now;
        self.v_down = v_now;

        if process_id == 0 {
            return false;
        }

        if ctrl_active && v_pressed && process_matches(process_name, TRANSIENT_CARET_APPS) {
            self.paste_started.insert(process_id, Instant::now());
        }
        self.paste_started
            .retain(|_, started| started.elapsed() <= Duration::from_millis(PASTE_CARET_GRACE_MS));

        let mut click_samples = take_click_samples(process_id);
        if click_samples.is_empty() && direct_left_pressed {
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point).is_ok() } {
                click_samples.push(ClickSample {
                    at: Instant::now(),
                    process_id,
                    x: point.x,
                    y: point.y,
                });
            }
        }
        let left_pressed = !click_samples.is_empty();
        let double_click = consume_click_samples(&mut self.last_left_click, click_samples);
        self.input_double_click = double_click;
        let standard_arrow = left_pressed && crate::cursor_detector::is_standard_arrow_cursor();
        let current_cursor = crate::cursor_detector::current_cursor_handle();
        let known_text_cursor = current_cursor.is_some_and(|cursor| {
            self.known_design_text_cursors
                .get(&process_id)
                .is_some_and(|known| known.contains(&cursor))
        });
        let text_cursor =
            left_pressed && (crate::cursor_detector::is_text_cursor() || known_text_cursor);
        let native_edit_focus = has_caret
            && (window_class_is_edit_like(focused_hwnd) || window_class_is_edit_like(caret_hwnd));

        if process_matches(process_name, DESIGN_TEXT_SHORTCUT_APPS) {
            self.office_edit_processes.clear();
            self.office_caret_suppressed.clear();
            self.pending_office_edit = None;

            // 切换 Adobe 进程后不沿用另一个窗口/进程的文字工具状态。
            self.design_text_processes.retain(|pid| *pid == process_id);
            self.design_text_tools.retain(|pid| *pid == process_id);
            self.design_native_processes
                .retain(|pid| *pid == process_id);
            self.design_caret_suppressed
                .retain(|pid| *pid == process_id);
            self.design_text_cursor_handles
                .retain(|pid, _| *pid == process_id);
            self.design_text_tool_armed_at
                .retain(|pid, _| *pid == process_id);
            self.known_design_text_cursors
                .retain(|pid, _| *pid == process_id);
            self.known_design_nontext_cursors
                .retain(|pid, _| *pid == process_id);

            if let Some((pid, started, before_cursor, started_as_arrow)) =
                self.pending_design_canvas
            {
                let current_cursor = crate::cursor_detector::current_cursor_handle();
                let learned_text_cursor = current_cursor.is_some_and(|cursor| {
                    self.known_design_text_cursors
                        .get(&process_id)
                        .is_some_and(|known| known.contains(&cursor))
                });
                let known_nontext_cursor = current_cursor.is_some_and(|cursor| {
                    self.known_design_nontext_cursors
                        .get(&process_id)
                        .is_some_and(|known| known.contains(&cursor))
                });
                let unclassified_custom_cursor = current_cursor.is_some()
                    && !crate::cursor_detector::is_standard_arrow_cursor()
                    && !known_nontext_cursor;
                let confirmed_text_cursor = crate::cursor_detector::is_text_cursor()
                    || learned_text_cursor
                    || has_caret
                    || (unclassified_custom_cursor
                        && (started_as_arrow || current_cursor == before_cursor));
                if pid != process_id || started.elapsed() > Duration::from_millis(500) {
                    self.pending_design_canvas = None;
                } else if should_confirm_pending_design_text(
                    started.elapsed(),
                    confirmed_text_cursor,
                ) {
                    // Adobe 在处理完双击后才把画布光标切成文字光标。延迟确认
                    // 可以识别已有文字，同时不会把移动/抓手/笔刷光标当成文字。
                    self.design_text_tools.insert(process_id);
                    self.design_text_processes.insert(process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_caret_suppressed.remove(&process_id);
                    if let Some(cursor) = crate::cursor_detector::current_cursor_handle() {
                        self.design_text_cursor_handles.insert(process_id, cursor);
                        self.known_design_text_cursors
                            .entry(process_id)
                            .or_default()
                            .insert(cursor);
                    }
                    self.pending_design_canvas = None;
                    self.pending_design_native = None;
                }
            }

            let native_was_tracked = self.design_native_processes.contains(&process_id);
            if !has_caret && native_was_tracked {
                // Photoshop 的图层重命名框失焦后偶尔还会短暂重新暴露 Caret。
                // Caret 首次消失就视为编辑已经结束，并保持抑制状态，直到用户
                // 再次明确双击或选择文字工具；否则下一次普通点击会回弹到中文。
                self.design_native_processes.remove(&process_id);
                self.design_text_tools.remove(&process_id);
                self.design_text_tool_armed_at.remove(&process_id);
                self.design_text_cursor_handles.remove(&process_id);
                self.design_caret_suppressed.insert(process_id);
                self.pending_design_native = None;
                self.pending_design_canvas = None;
            }
            if let Some((pid, started)) = self.pending_design_native {
                if pid != process_id || started.elapsed() > Duration::from_millis(450) {
                    self.pending_design_native = None;
                } else if native_edit_focus {
                    self.design_caret_suppressed.remove(&process_id);
                    self.design_native_processes.insert(process_id);
                    self.pending_design_native = None;
                }
            }
            if let Some((pid, started)) = self.pending_design_tool_check {
                if pid != process_id {
                    self.pending_design_tool_check = None;
                } else if started.elapsed() >= Duration::from_millis(120) {
                    let expected = self.design_text_cursor_handles.get(&process_id).copied();
                    let current = crate::cursor_detector::current_cursor_handle();
                    if expected.is_some() && current.is_some() && expected != current {
                        self.design_text_processes.remove(&process_id);
                        self.design_text_tools.remove(&process_id);
                        self.design_caret_suppressed.insert(process_id);
                    }
                    self.pending_design_tool_check = None;
                }
            }

            let was_canvas_active = self.design_text_processes.contains(&process_id);
            let was_native_active = self.design_native_processes.contains(&process_id);
            let was_active = was_canvas_active || was_native_active;
            if escape_pressed || (ctrl_active && enter_pressed) {
                self.design_text_processes.remove(&process_id);
                self.design_text_tools.remove(&process_id);
                self.design_text_tool_armed_at.remove(&process_id);
                self.design_native_processes.remove(&process_id);
                self.design_text_cursor_handles.remove(&process_id);
                self.design_caret_suppressed.insert(process_id);
                self.pending_design_canvas = None;
                // Esc/Ctrl+Enter 一次就结束输入态，下一轮上下文立即回到英文。
                self.last_left_click = None;
                return false;
            } else if enter_pressed && was_native_active {
                // 图层/对象重命名使用 Enter 提交；画布多行文字中的 Enter 不退出。
                self.design_text_tools.remove(&process_id);
                self.design_text_tool_armed_at.remove(&process_id);
                self.design_native_processes.remove(&process_id);
                self.design_text_cursor_handles.remove(&process_id);
                self.design_caret_suppressed.insert(process_id);
                return false;
            } else if left_pressed && !double_click && was_native_active && !native_edit_focus {
                // Photoshop 图层名/Illustrator 对象名的原生编辑框在失焦后仍可能
                // 暂留一帧 Caret。任何下一次单击都已经提交重命名，必须先结束
                // 中文输入态，不能再让下方的 has_caret 分支把它重新激活。
                self.design_text_processes.remove(&process_id);
                self.design_text_tools.remove(&process_id);
                self.design_text_tool_armed_at.remove(&process_id);
                self.design_native_processes.remove(&process_id);
                self.design_text_cursor_handles.remove(&process_id);
                self.design_caret_suppressed.insert(process_id);
                self.pending_design_native = None;
                self.pending_design_canvas = None;
                self.last_left_click = None;
                return false;
            } else if left_pressed && !double_click && was_canvas_active {
                if standard_arrow {
                    // Photoshop 在同一段文字内不同位置会切换多种文字光标句柄，
                    // 不能再把句柄变化当作退出；只有明确点到普通箭头区域才退出。
                    self.design_text_processes.remove(&process_id);
                    self.design_text_tools.remove(&process_id);
                    self.design_text_tool_armed_at.remove(&process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_text_cursor_handles.remove(&process_id);
                    self.design_caret_suppressed.insert(process_id);
                    self.pending_design_tool_check = None;
                    self.last_left_click = None;
                    return false;
                }
            } else if t_pressed && !has_caret {
                // T 只选择文字工具；等用户真正点击画布文字位置后才进入中文。
                self.design_text_tools.insert(process_id);
                self.design_text_tool_armed_at
                    .insert(process_id, Instant::now());
                if should_learn_adobe_cursor(true, standard_arrow) {
                    if let Some(cursor) = current_cursor {
                        self.known_design_text_cursors
                            .entry(process_id)
                            .or_default()
                            .insert(cursor);
                    }
                }
                self.design_native_processes.remove(&process_id);
                self.design_caret_suppressed.insert(process_id);
            } else if !was_active && (b_pressed || v_tool_pressed) {
                self.design_text_tools.remove(&process_id);
                self.design_text_tool_armed_at.remove(&process_id);
                if !standard_arrow {
                    if let Some(cursor) = current_cursor {
                        self.known_design_nontext_cursors
                            .entry(process_id)
                            .or_default()
                            .insert(cursor);
                    }
                }
                self.design_native_processes.remove(&process_id);
                self.design_caret_suppressed.insert(process_id);
            } else if was_canvas_active && (b_pressed || v_tool_pressed) {
                if !standard_arrow {
                    if let Some(cursor) = current_cursor {
                        self.known_design_nontext_cursors
                            .entry(process_id)
                            .or_default()
                            .insert(cursor);
                    }
                }
                self.pending_design_tool_check = Some((process_id, Instant::now()));
            }

            if native_edit_focus {
                // Photoshop/Illustrator 的搜索框和原生名称编辑框会暴露真实
                // Edit/RichEdit/Search Caret。即使画布编辑刚留下抑制标记，
                // 点击这类明确输入控件仍应立即进入中文。
                self.design_text_processes.remove(&process_id);
                self.design_text_tools.remove(&process_id);
                self.design_text_tool_armed_at.remove(&process_id);
                self.design_caret_suppressed.remove(&process_id);
                self.design_native_processes.insert(process_id);
                self.pending_design_native = None;
                self.pending_design_canvas = None;
                return true;
            }

            if left_pressed {
                let text_tool_armed = self
                    .design_text_tool_armed_at
                    .get(&process_id)
                    .is_some_and(|armed| armed.elapsed() <= Duration::from_millis(2500));
                if should_enter_design_canvas_text(
                    double_click,
                    text_tool_armed,
                    text_cursor,
                    standard_arrow,
                ) {
                    self.design_text_tools.insert(process_id);
                    self.design_text_processes.insert(process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_caret_suppressed.remove(&process_id);
                    if let Some(cursor) = crate::cursor_detector::current_cursor_handle() {
                        self.design_text_cursor_handles.insert(process_id, cursor);
                        self.known_design_text_cursors
                            .entry(process_id)
                            .or_default()
                            .insert(cursor);
                    }
                    self.design_text_tool_armed_at.remove(&process_id);
                    self.last_left_click = None;
                } else if double_click {
                    // 点击瞬间仍可能是移动/选择光标，等待 Adobe 完成事件处理后
                    // 再以文字光标或真实 Caret 确认，期间不提前切换中文。
                    self.pending_design_canvas =
                        Some((process_id, Instant::now(), current_cursor, standard_arrow));
                    self.pending_design_native = Some((process_id, Instant::now()));
                    self.design_text_processes.remove(&process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_caret_suppressed.insert(process_id);
                } else if self.design_text_tools.contains(&process_id) && text_cursor {
                    self.design_text_processes.insert(process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_caret_suppressed.remove(&process_id);
                    if let Some(cursor) = crate::cursor_detector::current_cursor_handle() {
                        self.design_text_cursor_handles.insert(process_id, cursor);
                    }
                } else if standard_arrow {
                    // 普通区域点击必须优先于旧 Caret；否则图层重命名结束后
                    // Photoshop 残留的一帧 Caret 会把退出动作重新覆盖成中文。
                    self.design_text_processes.remove(&process_id);
                    self.design_text_tools.remove(&process_id);
                    self.design_text_tool_armed_at.remove(&process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_text_cursor_handles.remove(&process_id);
                    self.design_caret_suppressed.insert(process_id);
                } else {
                    // 移动、抓手、笔刷等工具均使用自定义非箭头光标。若没有
                    // 明确文字光标，本次画布点击只能退出或保持非编辑状态。
                    self.design_text_processes.remove(&process_id);
                    self.design_text_tools.remove(&process_id);
                    self.design_text_tool_armed_at.remove(&process_id);
                    self.design_native_processes.remove(&process_id);
                    self.design_text_cursor_handles.remove(&process_id);
                    self.design_caret_suppressed.insert(process_id);
                }
            }
            return self.design_text_processes.contains(&process_id)
                || self.design_native_processes.contains(&process_id);
        }

        self.design_text_processes.clear();
        self.design_text_tools.clear();
        self.design_native_processes.clear();
        self.design_caret_suppressed.clear();
        self.design_text_cursor_handles.clear();
        self.design_text_tool_armed_at.clear();
        self.known_design_nontext_cursors.clear();
        self.pending_design_native = None;
        self.pending_design_canvas = None;
        self.pending_design_tool_check = None;

        if process_matches(process_name, OFFICE_EDIT_APPS) {
            self.office_edit_processes.retain(|pid| *pid == process_id);
            self.office_caret_suppressed
                .retain(|pid| *pid == process_id);
            let spreadsheet = process_matches(process_name, OFFICE_SPREADSHEET_APPS);

            if !has_caret {
                self.office_caret_suppressed.remove(&process_id);
            }
            if let Some((pid, started)) = self.pending_office_edit {
                if pid != process_id || started.elapsed() > Duration::from_millis(450) {
                    self.pending_office_edit = None;
                } else if has_caret {
                    self.office_caret_suppressed.remove(&process_id);
                    self.office_edit_processes.insert(process_id);
                    self.pending_office_edit = None;
                }
            }

            if escape_pressed || (spreadsheet && enter_pressed) {
                self.office_edit_processes.remove(&process_id);
                self.office_caret_suppressed.insert(process_id);
                self.pending_office_edit = None;
                return false;
            }

            if left_pressed {
                if double_click {
                    // 单元格、已有文字与形状文字均以双击作为明确编辑信号。
                    self.office_caret_suppressed.remove(&process_id);
                    self.office_edit_processes.insert(process_id);
                    self.pending_office_edit = None;
                    self.last_left_click = None;
                } else if spreadsheet {
                    // 表格单击只是选中单元格，必须退出编辑；第二次点击组成双击时
                    // 再进入中文，避免旧 Caret 让整个表格一直保持中文。
                    self.office_edit_processes.remove(&process_id);
                    self.office_caret_suppressed.insert(process_id);
                    self.pending_office_edit = None;
                } else if has_caret && !self.office_caret_suppressed.contains(&process_id) {
                    self.office_edit_processes.insert(process_id);
                } else if !spreadsheet && !standard_arrow {
                    self.office_edit_processes.insert(process_id);
                    self.pending_office_edit = Some((process_id, Instant::now()));
                } else {
                    // 单击表格非编辑区或办公软件普通界面立即恢复英文。
                    self.office_edit_processes.remove(&process_id);
                    self.office_caret_suppressed.insert(process_id);
                    self.pending_office_edit =
                        (!spreadsheet).then_some((process_id, Instant::now()));
                }
            } else if has_caret && !self.office_caret_suppressed.contains(&process_id) {
                self.office_edit_processes.insert(process_id);
            }

            return self.office_edit_processes.contains(&process_id);
        }

        self.office_edit_processes.clear();
        self.office_caret_suppressed.clear();
        self.pending_office_edit = None;
        if process_matches(process_name, TRANSIENT_CARET_APPS) {
            if left_pressed {
                if text_cursor {
                    self.pending_chat_inputs.insert(process_id, Instant::now());
                } else {
                    self.pending_chat_inputs.remove(&process_id);
                }
            }
            let predicted_input = self
                .pending_chat_inputs
                .get(&process_id)
                .is_some_and(|started| started.elapsed() <= Duration::from_millis(600));
            return has_caret || predicted_input;
        }
        // 普通输入框也必须跨检测轮次保留第一击；否则两次点击分别落在
        // 相邻轮询中时，第一击会在这里被清空，永远无法形成双击。
        false
    }

    /// 核心：按配置管线检测光标位置
    pub fn get_caret_pos(&mut self) -> Option<CaretPos> {
        self.detect()
    }

    /// 多级检测：按配置的 methods 顺序依次尝试
    fn detect(&mut self) -> Option<CaretPos> {
        let foreground_process = foreground_process_name().unwrap_or_default();
        if process_is_indicator_only(&foreground_process) {
            self.last_source = DetectionSource::None;
            return None;
        }
        if process_matches(&foreground_process, GUI_ONLY_CARET_APPS) {
            let pos = self.get_pos_via_gui_info();
            self.last_source = if pos.is_some() {
                DetectionSource::GuiInfo
            } else {
                DetectionSource::None
            };
            return pos;
        }
        for name in crate::config::caret_methods() {
            let Some(method) = DetectionSource::from_name(name) else {
                continue;
            };
            let pos = match method {
                DetectionSource::GuiInfo => self.get_pos_via_gui_info(),
                DetectionSource::MsaaCaret => self.get_pos_via_msaa_caret(),
                DetectionSource::None => None,
            };
            if let Some(pos) = pos {
                self.last_source = method;
                return Some(pos);
            }
        }

        self.last_source = DetectionSource::None;
        None
    }

    /// 通过 GetGUIThreadInfo 获取光标位置
    fn get_pos_via_gui_info(&self) -> Option<CaretPos> {
        unsafe {
            let mut gui_info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };

            if GetGUIThreadInfo(0, &mut gui_info).is_ok() {
                if !gui_info.hwndCaret.0.is_null() {
                    let mut pt = POINT {
                        x: gui_info.rcCaret.left,
                        y: gui_info.rcCaret.top,
                    };
                    let _ = ClientToScreen(gui_info.hwndCaret, &mut pt);
                    // rcCaret 的高度是插入符位图高:有的程序(微信、Tk)只建 1x1 的
                    // 标记插入符,rcCaret.top 只是插入点顶部。行高取位图高与焦点
                    // 窗口字体行高中较大者(IME 候选框定位用的也是字体行高),
                    // Notepad++ 等报真实行高的不受影响。
                    let caret_h = gui_info.rcCaret.bottom - gui_info.rcCaret.top;
                    let font_h = font_height(gui_info.hwndFocus).unwrap_or(caret_h);
                    return Some((pt.x, pt.y, caret_h.max(font_h)));
                }
            }
        }
        None
    }

    /// 通过 MSAA OBJID_CARET 获取光标位置（VS Code 支持，浏览器不提供此对象）
    fn get_pos_via_msaa_caret(&mut self) -> Option<CaretPos> {
        use windows::core::GUID;
        use windows::core::VARIANT;
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};

        // 追加错误信息
        let append_error = |s: &mut String, new: &str| {
            if !s.is_empty() {
                s.push_str(" | ");
            }
            s.push_str(new);
        };

        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                append_error(&mut self.last_uia_error, "MSAA:NoHwnd");
                return None;
            }

            // 使用模块级常量 IID_IACCESSIBLE
            let iid_iaccessible = GUID::from_u128(IID_IACCESSIBLE);

            // 尝试获取 OBJID_CARET 的 IAccessible 接口
            let mut p_acc: Option<IAccessible> = None;
            let result = AccessibleObjectFromWindow(
                hwnd,
                OBJID_CARET,
                &iid_iaccessible,
                &mut p_acc as *mut _ as *mut *mut std::ffi::c_void,
            );

            if result.is_err() {
                append_error(
                    &mut self.last_uia_error,
                    &format!("MSAA:Err:{:X}", result.unwrap_err().code().0 as u32),
                );
                return None;
            } else if p_acc.is_none() {
                append_error(&mut self.last_uia_error, "MSAA:NoAcc");
                return None;
            }

            let acc = p_acc.unwrap();
            // 调用 accLocation 获取位置
            let mut x: i32 = 0;
            let mut y: i32 = 0;
            let mut w: i32 = 0;
            let mut h: i32 = 0;

            // CHILDID_SELF = VARIANT with VT_I4 value 0
            // 使用 from(0i32) 创建 VT_I4 类型的 VARIANT
            let var_child = VARIANT::from(0i32);

            match acc.accLocation(&mut x, &mut y, &mut w, &mut h, &var_child) {
                Ok(_) => {
                    if x != 0 || y != 0 {
                        // 有选区时 caret 对象矩形覆盖整个选区（光标在选区末尾），取右缘
                        return Some((x + w, y, h));
                    } else {
                        append_error(&mut self.last_uia_error, "MSAA:Zero");
                        None
                    }
                }
                Err(e) => {
                    append_error(
                        &mut self.last_uia_error,
                        &format!("MSAA:Loc:{:X}", e.code().0 as u32),
                    );
                    None
                }
            }
        }
    }
}

fn foreground_process_name() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        process_name(process_id)
    }
}

fn process_is_indicator_only(process_name: &str) -> bool {
    process_matches(process_name, INDICATOR_ONLY_APPS)
        || crate::config::auto_switch_app_rule(process_name) == Some("ignore")
}

fn should_enter_design_canvas_text(
    double_click: bool,
    text_tool_armed: bool,
    text_cursor: bool,
    standard_arrow: bool,
) -> bool {
    (double_click && text_cursor) || (text_tool_armed && !standard_arrow)
}

fn should_confirm_pending_design_text(elapsed: Duration, text_cursor: bool) -> bool {
    text_cursor && elapsed >= Duration::from_millis(40) && elapsed <= Duration::from_millis(500)
}

fn should_learn_adobe_cursor(is_text_tool: bool, is_standard_arrow: bool) -> bool {
    is_text_tool && !is_standard_arrow
}

fn should_preserve_transient_editable(
    same_foreground: bool,
    same_focus: bool,
    last_seen_elapsed: Option<Duration>,
    paste_elapsed: Option<Duration>,
) -> bool {
    if !same_foreground {
        return false;
    }
    let short_dropout = same_focus
        && last_seen_elapsed
            .is_some_and(|elapsed| elapsed <= Duration::from_millis(CARET_DROPOUT_GRACE_MS));
    let paste_dropout = paste_elapsed
        .is_some_and(|elapsed| elapsed <= Duration::from_millis(PASTE_CARET_GRACE_MS))
        && last_seen_elapsed
            .is_some_and(|elapsed| elapsed <= Duration::from_millis(PASTE_CARET_GRACE_MS + 500));
    short_dropout || paste_dropout
}

fn process_matches(process_name: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| process_name.eq_ignore_ascii_case(candidate))
}

fn window_class_is_edit_like(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut buffer = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if len <= 0 {
        return false;
    }
    let class_name = String::from_utf16_lossy(&buffer[..len as usize]).to_ascii_lowercase();
    class_name.contains("edit") || class_name.contains("search") || class_name.contains("richedit")
}

fn key_sample(vkey: i32) -> (bool, bool) {
    let state = unsafe { GetAsyncKeyState(vkey) as u16 };
    ((state & 0x8000) != 0, (state & 0x0001) != 0)
}

fn process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        let full_path = String::from_utf16_lossy(&buffer[..len as usize]);
        Path::new(&full_path)
            .file_name()?
            .to_str()
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        consume_click_samples, process_matches, should_confirm_pending_design_text,
        should_enter_design_canvas_text, should_learn_adobe_cursor,
        should_preserve_transient_editable, ClickSample, CARET_COMPAT_APPS, CARET_FAST_PATH_APPS,
        DESIGN_TEXT_SHORTCUT_APPS, GUI_ONLY_CARET_APPS, INDICATOR_ONLY_APPS, OFFICE_EDIT_APPS,
        OFFICE_SPREADSHEET_APPS,
    };

    #[test]
    fn ordinary_input_double_click_survives_separate_poll_cycles() {
        let started = Instant::now();
        let mut last_click = None;

        assert!(!consume_click_samples(
            &mut last_click,
            [ClickSample {
                at: started,
                process_id: 42,
                x: 100,
                y: 80,
            }]
        ));
        assert!(last_click.is_some());
        assert!(consume_click_samples(
            &mut last_click,
            [ClickSample {
                at: started + Duration::from_millis(180),
                process_id: 42,
                x: 103,
                y: 82,
            }]
        ));
        assert!(last_click.is_none());
    }

    #[test]
    fn adobe_custom_canvas_cursors_do_not_imply_text_editing() {
        assert!(!should_enter_design_canvas_text(true, false, false, false));
        assert!(should_enter_design_canvas_text(false, true, false, false));
        assert!(!should_enter_design_canvas_text(false, true, false, true));
        assert!(should_enter_design_canvas_text(true, false, true, false));
        assert!(should_enter_design_canvas_text(false, true, true, false));
        assert!(!should_confirm_pending_design_text(
            Duration::from_millis(20),
            true
        ));
        assert!(should_confirm_pending_design_text(
            Duration::from_millis(80),
            true
        ));
        assert!(!should_confirm_pending_design_text(
            Duration::from_millis(80),
            false
        ));
        assert!(should_learn_adobe_cursor(true, false));
        assert!(!should_learn_adobe_cursor(true, true));
        assert!(!should_learn_adobe_cursor(false, false));
    }

    #[test]
    fn chat_caret_dropout_is_preserved_only_briefly_or_during_paste() {
        assert!(should_preserve_transient_editable(
            true,
            true,
            Some(Duration::from_millis(200)),
            None,
        ));
        assert!(should_preserve_transient_editable(
            true,
            false,
            Some(Duration::from_millis(700)),
            Some(Duration::from_millis(500)),
        ));
        assert!(!should_preserve_transient_editable(
            false,
            true,
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        ));
        assert!(!should_preserve_transient_editable(
            true,
            true,
            Some(Duration::from_millis(1800)),
            None,
        ));
    }

    #[test]
    fn wechat_uses_caret_fast_path() {
        assert!(process_matches("weixin.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("WXWork.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("WeCom.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("SearchHost.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("SearchApp.exe", CARET_FAST_PATH_APPS));
    }

    #[test]
    fn rich_text_apps_use_scoped_caret_fallback() {
        assert!(process_matches("chrome.exe", CARET_COMPAT_APPS));
        assert!(process_matches("Tabbit Browser.exe", CARET_COMPAT_APPS));
        assert!(process_matches("Feishu.exe", CARET_COMPAT_APPS));
        assert!(process_matches("wps.exe", CARET_COMPAT_APPS));
        assert!(process_matches("et.exe", CARET_COMPAT_APPS));
        assert!(process_matches("wpp.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("Photoshop.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("Illustrator.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("Cinema 4D.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("eCloud.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("FlClash.exe", CARET_COMPAT_APPS));
    }

    #[test]
    fn adobe_canvas_apps_support_text_tool_shortcut() {
        assert!(process_matches("Photoshop.exe", DESIGN_TEXT_SHORTCUT_APPS));
        assert!(process_matches(
            "Illustrator.exe",
            DESIGN_TEXT_SHORTCUT_APPS
        ));
        assert!(!process_matches("Cinema 4D.exe", DESIGN_TEXT_SHORTCUT_APPS));
        assert!(process_matches("Cinema 4D.exe", INDICATOR_ONLY_APPS));
        assert!(process_matches("JianyingPro.exe", INDICATOR_ONLY_APPS));
        assert!(process_matches("CapCut.exe", INDICATOR_ONLY_APPS));
        assert!(process_matches("Photoshop.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("Illustrator.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("Cinema 4D.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("Photoshop.exe", GUI_ONLY_CARET_APPS));
        assert!(process_matches("Illustrator.exe", GUI_ONLY_CARET_APPS));
    }

    #[test]
    fn wps_and_microsoft_office_use_edit_mode_tracking() {
        for process in [
            "wps.exe",
            "et.exe",
            "wpp.exe",
            "WINWORD.EXE",
            "EXCEL.EXE",
            "POWERPNT.EXE",
        ] {
            assert!(process_matches(process, OFFICE_EDIT_APPS));
            assert!(process_matches(process, CARET_COMPAT_APPS));
        }
        assert!(process_matches("et.exe", OFFICE_SPREADSHEET_APPS));
        assert!(process_matches("EXCEL.EXE", OFFICE_SPREADSHEET_APPS));
        assert!(!process_matches("wps.exe", OFFICE_SPREADSHEET_APPS));
    }
}

/// UIA RuntimeId 是元素在当前桌面会话内的稳定身份；相比控件坐标，它不会因
/// 同一编辑器滚动或窗口移动而变化。
fn uia_runtime_id(
    element: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Option<Vec<i32>> {
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };

    unsafe {
        let array = element.GetRuntimeId().ok()?;
        if array.is_null() {
            return None;
        }
        let lower = match SafeArrayGetLBound(array, 1) {
            Ok(value) => value,
            Err(_) => {
                let _ = SafeArrayDestroy(array);
                return None;
            }
        };
        let upper = match SafeArrayGetUBound(array, 1) {
            Ok(value) => value,
            Err(_) => {
                let _ = SafeArrayDestroy(array);
                return None;
            }
        };
        if upper < lower {
            let _ = SafeArrayDestroy(array);
            return None;
        }
        let mut data = std::ptr::null_mut();
        if SafeArrayAccessData(array, &mut data).is_err() {
            let _ = SafeArrayDestroy(array);
            return None;
        }
        let values =
            std::slice::from_raw_parts(data.cast::<i32>(), (upper - lower + 1) as usize).to_vec();
        let _ = SafeArrayUnaccessData(array);
        let _ = SafeArrayDestroy(array);
        Some(values)
    }
}

impl Default for CaretDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// 窗口字体的行高(tmHeight)。窗口未设置字体(自绘框架)时用系统 UI 字体
/// (NONCLIENTMETRICS.lfMessageFont,即 tkinter 默认字体对应的 Segoe UI)。
fn font_height(hwnd: HWND) -> Option<i32> {
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, DeleteObject, GetDC, GetTextMetricsW, ReleaseDC, SelectObject, HFONT,
        HGDIOBJ, TEXTMETRICW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageW, SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_GETFONT,
    };

    unsafe {
        let font = SendMessageW(hwnd, WM_GETFONT, None, None);
        let owns_font = font.0 == 0;
        let hfont: HFONT = if !owns_font {
            HFONT(font.0 as *mut _)
        } else {
            let mut ncm = NONCLIENTMETRICSW::default();
            ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .ok()?;
            HFONT(CreateFontIndirectW(&ncm.lfMessageFont).0)
        };
        let hdc = GetDC(hwnd);
        let old = SelectObject(hdc, hfont);
        let mut tm = TEXTMETRICW::default();
        let ok = GetTextMetricsW(hdc, &mut tm).as_bool();
        SelectObject(hdc, old);
        ReleaseDC(hwnd, hdc);
        // CreateFontIndirectW 返回的字体由调用方拥有。这里会在每次光标定位时
        // 执行；若不释放，GDI 句柄耗尽后托盘菜单会出现空白甚至进程卡死。
        if owns_font {
            let _ = DeleteObject(HGDIOBJ(hfont.0));
        }
        ok.then_some(tm.tmHeight)
    }
}
