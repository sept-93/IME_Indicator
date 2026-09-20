//! 焦点上下文感知输入法切换。
//! 只在上下文稳定且发生变化时切换一次，保留用户在当前输入框内的手动选择。

use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::{LoadKeyboardLayoutW, KLF_ACTIVATE};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_INPUTLANGCHANGEREQUEST,
};

use crate::caret_detector::FocusContext;

const WM_IME_CONTROL: u32 = 0x0283;
const IMC_SETCONVERSIONMODE: usize = 0x0002;
const IMC_SETOPENSTATUS: usize = 0x0006;
const IME_CMODE_NATIVE: isize = 0x0001;
const INPUTLANGCHANGE_SYSCHARSET: usize = 0x0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanguageTarget {
    Chinese,
    English,
}

pub struct AutoSwitcher {
    candidate: Option<FocusContext>,
    candidate_since: Instant,
    applied_identity: Option<String>,
}

impl AutoSwitcher {
    pub fn new() -> Self {
        Self {
            candidate: None,
            candidate_since: Instant::now(),
            applied_identity: None,
        }
    }

    pub fn observe(&mut self, context: FocusContext) {
        if !crate::config::auto_switch_enable() {
            return;
        }

        let changed = self.candidate.as_ref()
            .map_or(true, |current| current.identity != context.identity);
        if changed {
            self.candidate = Some(context);
            self.candidate_since = Instant::now();
            return;
        }

        let settle = Duration::from_millis(crate::config::auto_switch_settle_ms());
        if self.candidate_since.elapsed() < settle {
            return;
        }

        let Some(context) = self.candidate.as_ref() else { return };
        if self.applied_identity.as_deref() == Some(context.identity.as_str()) {
            return;
        }

        // 无论切换成功与否，本上下文都只尝试一次，避免不兼容窗口被循环轰炸。
        self.applied_identity = Some(context.identity.clone());

        let process_name = process_name(context.process_id).unwrap_or_default();
        let rule = crate::config::auto_switch_app_rule(&process_name).unwrap_or("auto");
        let Some(target) = target_for(rule, context.password, context.editable) else { return };

        let _ = switch_language(context.focused_hwnd, context.foreground_hwnd, target);
    }
}

fn target_for(rule: &str, password: bool, editable: bool) -> Option<LanguageTarget> {
    match rule {
        "ignore" => None,
        "chinese" => Some(LanguageTarget::Chinese),
        "english" => Some(LanguageTarget::English),
        _ if password => Some(LanguageTarget::English),
        _ if editable => Some(LanguageTarget::Chinese),
        _ => Some(LanguageTarget::English),
    }
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
        Path::new(&full_path).file_name()?.to_str().map(str::to_string)
    }
}

fn switch_language(focused_hwnd: HWND, foreground_hwnd: HWND, target: LanguageTarget) -> windows::core::Result<()> {
    let klid = match target {
        LanguageTarget::Chinese => crate::config::auto_switch_chinese_klid(),
        LanguageTarget::English => crate::config::auto_switch_english_klid(),
    };
    let wide: Vec<u16> = klid.encode_utf16().chain(Some(0)).collect();

    unsafe {
        let layout = LoadKeyboardLayoutW(PCWSTR(wide.as_ptr()), KLF_ACTIVATE)?;
        let target_hwnd = if !focused_hwnd.0.is_null() { focused_hwnd } else { foreground_hwnd };
        let mut result = 0usize;
        let sent = SendMessageTimeoutW(
            target_hwnd,
            WM_INPUTLANGCHANGEREQUEST,
            WPARAM(INPUTLANGCHANGE_SYSCHARSET),
            LPARAM(layout.0 as isize),
            SMTO_ABORTIFHUNG,
            500,
            Some(&mut result),
        );
        if sent.0 == 0 {
            return Err(windows::core::Error::from_win32());
        }

        if target == LanguageTarget::Chinese {
            let ime_hwnd = ImmGetDefaultIMEWnd(target_hwnd);
            if !ime_hwnd.0.is_null() {
                send_ime_control(ime_hwnd, IMC_SETOPENSTATUS, 1);
                send_ime_control(ime_hwnd, IMC_SETCONVERSIONMODE, IME_CMODE_NATIVE);
            }
        }
    }
    Ok(())
}

fn send_ime_control(hwnd: HWND, command: usize, value: isize) {
    unsafe {
        let mut result = 0usize;
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_IME_CONTROL,
            WPARAM(command),
            LPARAM(value),
            SMTO_ABORTIFHUNG,
            500,
            Some(&mut result),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{target_for, LanguageTarget};

    #[test]
    fn automatic_rules_follow_context() {
        assert_eq!(target_for("auto", false, true), Some(LanguageTarget::Chinese));
        assert_eq!(target_for("auto", false, false), Some(LanguageTarget::English));
        assert_eq!(target_for("auto", true, true), Some(LanguageTarget::English));
    }

    #[test]
    fn application_rules_take_precedence() {
        assert_eq!(target_for("chinese", true, false), Some(LanguageTarget::Chinese));
        assert_eq!(target_for("english", false, true), Some(LanguageTarget::English));
        assert_eq!(target_for("ignore", false, true), None);
    }
}
