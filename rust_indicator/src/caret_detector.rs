//! 文本光标位置检测模块 - 多级检测策略


use std::path::Path;

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::CUIAutomation;
use windows::Win32::UI::Accessibility::IUIAutomation;
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
/// 它们已有可靠的 Win32/MSAA Caret，因此直接使用 Caret 状态判断输入区。
const CARET_FAST_PATH_APPS: &[&str] = &["Weixin.exe"];

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
        }
    }

    /// 获取当前焦点元素的可编辑状态与稳定身份。UIA 查询失败时使用 Win32 焦点窗口
    /// 和是否存在真实文本光标作为保守回退。
    pub fn focus_context(&self, has_caret: bool) -> FocusContext {
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

            let fallback_identity = format!(
                "{}:{}",
                foreground_hwnd.0 as usize,
                focused_hwnd.0 as usize
            );

            let process_name = process_name(process_id).unwrap_or_default();

            // 微信等自绘应用的 UIA GetFocusedElement 会卡住约 3 秒，并且最终只返回
            // 顶层窗口。Win32/MSAA Caret 已能可靠反映聊天框和搜索框是否可输入。
            if process_matches(&process_name, CARET_FAST_PATH_APPS) {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: has_caret,
                    password: false,
                    readonly_document: false,
                };
            }

            let Some(automation) = self.automation.as_ref() else {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: has_caret,
                    password: false,
                    readonly_document: false,
                };
            };
            let Ok(focused) = automation.GetFocusedElement() else {
                return FocusContext {
                    identity: fallback_identity,
                    foreground_hwnd,
                    focused_hwnd,
                    process_id,
                    editable: has_caret,
                    password: false,
                    readonly_document: false,
                };
            };

            let control_type = focused.CurrentControlType().unwrap_or_default();
            let password = focused.CurrentIsPassword().map_or(false, |v| v.as_bool());
            let value_pattern = focused.GetCurrentPattern(UIA_ValuePatternId)
                .ok()
                .and_then(|p| p.cast::<IUIAutomationValuePattern>().ok());
            let value_is_readonly = value_pattern.as_ref()
                .and_then(|vp| vp.CurrentIsReadOnly().ok())
                .map(|v| v.as_bool());
            let caret_compat = process_matches(&process_name, CARET_COMPAT_APPS)
                || crate::config::auto_switch_extra_input_apps()
                    .iter()
                    .any(|candidate| process_name.eq_ignore_ascii_case(candidate));
            let editable = if password {
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
            let readonly_document = control_type == UIA_DocumentControlTypeId && !editable;

            let native_hwnd = focused.CurrentNativeWindowHandle().unwrap_or_default();
            let identity = if let Some(runtime_id) = uia_runtime_id(&focused) {
                format!(
                    "{}:{}:{}:{:?}",
                    foreground_hwnd.0 as usize,
                    native_hwnd.0 as usize,
                    control_type.0,
                    runtime_id,
                )
            } else {
                let automation_id = focused.CurrentAutomationId()
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
            }
        }
    }

    /// 核心：按配置管线检测光标位置
    pub fn get_caret_pos(&mut self) -> Option<CaretPos> {
        self.detect()
    }

    /// 多级检测：按配置的 methods 顺序依次尝试
    fn detect(&mut self) -> Option<CaretPos> {
        for name in crate::config::caret_methods() {
            let Some(method) = DetectionSource::from_name(name) else { continue };
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
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
        use windows::core::GUID;
        use windows::core::VARIANT;

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
                append_error(&mut self.last_uia_error, &format!("MSAA:Err:{:X}", result.unwrap_err().code().0 as u32));
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
                    append_error(&mut self.last_uia_error, &format!("MSAA:Loc:{:X}", e.code().0 as u32));
                    None
                }
            }
        }
    }
}

fn process_matches(process_name: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| process_name.eq_ignore_ascii_case(candidate))
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
    use super::{process_matches, CARET_COMPAT_APPS, CARET_FAST_PATH_APPS};

    #[test]
    fn wechat_uses_caret_fast_path() {
        assert!(process_matches("weixin.exe", CARET_FAST_PATH_APPS));
    }

    #[test]
    fn rich_text_apps_use_scoped_caret_fallback() {
        assert!(process_matches("chrome.exe", CARET_COMPAT_APPS));
        assert!(process_matches("Tabbit Browser.exe", CARET_COMPAT_APPS));
        assert!(process_matches("Feishu.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("eCloud.exe", CARET_COMPAT_APPS));
        assert!(!process_matches("FlClash.exe", CARET_COMPAT_APPS));
    }
}

/// UIA RuntimeId 是元素在当前桌面会话内的稳定身份；相比控件坐标，它不会因
/// 同一编辑器滚动或窗口移动而变化。
fn uia_runtime_id(element: &windows::Win32::UI::Accessibility::IUIAutomationElement) -> Option<Vec<i32>> {
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
        let values = std::slice::from_raw_parts(
            data.cast::<i32>(),
            (upper - lower + 1) as usize,
        )
        .to_vec();
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
        SendMessageW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        SystemParametersInfoW, WM_GETFONT,
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
