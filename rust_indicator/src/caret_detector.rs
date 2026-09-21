//! 文本光标位置检测模块 - 多级检测策略

use std::collections::HashSet;
use std::path::Path;

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
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
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
    "SearchHost.exe",
    "SearchApp.exe",
    "StartMenuExperienceHost.exe",
];

/// 对这些应用不做 Caret/MSAA/UIA 或应用内编辑模式探测，只读取系统 IME 状态。
/// Cinema 4D 的自绘消息循环对跨进程辅助功能查询非常敏感，频繁探测会造成卡顿。
const INDICATOR_ONLY_APPS: &[&str] = &["Cinema 4D.exe"];

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
];

/// Photoshop/Illustrator 的画布文字光标完全由应用绘制，Windows 没有 Caret 对象。
/// 在这些应用内按 T 进入文字工具后进入兼容输入模式，Esc 或 Ctrl+Enter 退出。
const DESIGN_TEXT_SHORTCUT_APPS: &[&str] = &["Photoshop.exe", "Illustrator.exe"];

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
    t_down: bool,
    escape_down: bool,
    enter_down: bool,
    left_down: bool,
    v_down: bool,
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
            t_down: false,
            escape_down: false,
            enter_down: false,
            left_down: false,
            v_down: false,
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
            let design_text_mode =
                self.update_design_text_mode(&process_name, process_id, has_caret);

            // 微信等自绘应用的 UIA GetFocusedElement 会卡住约 3 秒，并且最终只返回
            // 顶层窗口。Win32/MSAA Caret 已能可靠反映聊天框和搜索框是否可输入。
            if process_matches(&process_name, CARET_FAST_PATH_APPS) {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: has_caret || design_text_mode,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: design_text_mode && !has_caret,
                };
            }

            let Some(automation) = self.automation.as_ref() else {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: has_caret || design_text_mode,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: design_text_mode && !has_caret,
                };
            };
            let Ok(focused) = automation.GetFocusedElement() else {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: has_caret || design_text_mode,
                    password: false,
                    readonly_document: false,
                    force_mouse_indicator: design_text_mode && !has_caret,
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
            let editable = standard_editable || design_text_mode;
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
                force_mouse_indicator: design_text_mode && !has_caret,
            }
        }
    }

    fn update_design_text_mode(
        &mut self,
        process_name: &str,
        process_id: u32,
        has_caret: bool,
    ) -> bool {
        let (t_now, t_since_last) = key_sample(0x54); // T
        let (escape_now, escape_since_last) = key_sample(VK_ESCAPE.0 as i32);
        let (enter_now, enter_since_last) = key_sample(VK_RETURN.0 as i32);
        let (left_now, left_since_last) = key_sample(VK_LBUTTON.0 as i32);
        let (v_now, v_since_last) = key_sample(0x56); // V
        let (ctrl_now, _) = key_sample(VK_CONTROL.0 as i32);

        // 同时使用高位按键状态和低位“自上次查询后按过”标志，避免短按 T/Esc
        // 刚好落在两次 30ms 轮询之间，造成 Illustrator 偶发无法进入或退出。
        let t_pressed = t_since_last || (t_now && !self.t_down);
        let escape_pressed = escape_since_last || (escape_now && !self.escape_down);
        let enter_pressed = enter_since_last || (enter_now && !self.enter_down);
        let left_pressed = left_since_last || (left_now && !self.left_down);
        let v_pressed = v_since_last || (v_now && !self.v_down);
        self.t_down = t_now;
        self.escape_down = escape_now;
        self.enter_down = enter_now;
        self.left_down = left_now;
        self.v_down = v_now;

        if process_id == 0 {
            return false;
        }

        if process_matches(process_name, DESIGN_TEXT_SHORTCUT_APPS) {
            let was_active = self.design_text_processes.contains(&process_id);
            if escape_pressed || (ctrl_now && enter_pressed) {
                self.design_text_processes.remove(&process_id);
                // 第一次 Esc 结束文字编辑但保留文字工具；第二次 Esc 完全退出工具。
                if escape_pressed && !was_active {
                    self.design_text_tools.remove(&process_id);
                }
            } else if t_pressed && !has_caret {
                // T 只选择文字工具；等用户真正点击画布文字位置后才进入中文。
                self.design_text_tools.insert(process_id);
            } else if self.design_text_tools.contains(&process_id) && left_pressed {
                if crate::cursor_detector::is_standard_arrow_cursor() {
                    // 点击图层、工具栏等普通界面立即离开输入态，保证快捷键使用英文。
                    self.design_text_processes.remove(&process_id);
                } else {
                    // 文字工具选中时，点击画布的文字光标位置才进入中文输入态。
                    self.design_text_processes.insert(process_id);
                }
            } else if !was_active && v_pressed {
                self.design_text_tools.remove(&process_id);
            }
            return self.design_text_processes.contains(&process_id);
        }

        false
    }

    /// 核心：按配置管线检测光标位置
    pub fn get_caret_pos(&mut self) -> Option<CaretPos> {
        self.detect()
    }

    /// 多级检测：按配置的 methods 顺序依次尝试
    fn detect(&mut self) -> Option<CaretPos> {
        if foreground_process_matches(INDICATOR_ONLY_APPS) {
            self.last_source = DetectionSource::None;
            return None;
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

fn foreground_process_matches(candidates: &[&str]) -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        process_name(process_id)
            .is_some_and(|name| process_matches(&name, candidates))
    }
}

fn process_matches(process_name: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| process_name.eq_ignore_ascii_case(candidate))
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
    use super::{
        process_matches, CARET_COMPAT_APPS, CARET_FAST_PATH_APPS, DESIGN_TEXT_SHORTCUT_APPS,
        INDICATOR_ONLY_APPS,
    };

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
        assert!(process_matches("Photoshop.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("Illustrator.exe", CARET_FAST_PATH_APPS));
        assert!(process_matches("Cinema 4D.exe", CARET_FAST_PATH_APPS));
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
