//! 焦点上下文感知输入法切换。
//! 只在上下文稳定且发生变化时切换一次，保留用户在当前输入框内的手动选择。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
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
const ADOBE_ENGLISH_RETRY_MS: u64 = 120;
const EDITABLE_ENTRY_RETRY_WINDOW_MS: u64 = 260;
const EDITABLE_ENTRY_RETRY_INTERVAL_MS: u64 = 70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanguageTarget {
    Chinese,
    English,
}

/// UIA 元素身份之外还要记录输入语义。微信等自绘应用始终只暴露同一个
/// 顶层窗口，但进入/离开输入区时系统 Caret 状态仍会改变。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SwitchContextKey {
    identity: String,
    editable: bool,
    password: bool,
}

impl From<&FocusContext> for SwitchContextKey {
    fn from(context: &FocusContext) -> Self {
        Self {
            identity: context.identity.clone(),
            editable: context.editable,
            password: context.password,
        }
    }
}

pub struct AutoSwitcher {
    candidate: Option<FocusContext>,
    candidate_since: Instant,
    applied_context: Option<SwitchContextKey>,
    enabled_last: bool,
    adobe_editable_processes: HashSet<u32>,
    adobe_english_attempts: HashMap<u32, Instant>,
    editable_entry: Option<(SwitchContextKey, Instant)>,
    editable_entry_last_attempt: Option<Instant>,
}

impl AutoSwitcher {
    pub fn new() -> Self {
        Self {
            candidate: None,
            candidate_since: Instant::now(),
            applied_context: None,
            enabled_last: crate::tray::smart_switch_enabled(),
            adobe_editable_processes: HashSet::new(),
            adobe_english_attempts: HashMap::new(),
            editable_entry: None,
            editable_entry_last_attempt: None,
        }
    }

    pub fn observe(&mut self, context: FocusContext, chinese_mode: bool) {
        let enabled = crate::tray::smart_switch_enabled();
        if !enabled {
            self.enabled_last = false;
            self.editable_entry = None;
            self.editable_entry_last_attempt = None;
            return;
        }
        if !self.enabled_last {
            self.candidate = None;
            self.applied_context = None;
            self.editable_entry = None;
            self.editable_entry_last_attempt = None;
            self.enabled_last = true;
        }

        let next_key = SwitchContextKey::from(&context);
        let changed = self
            .candidate
            .as_ref()
            .map_or(true, |current| SwitchContextKey::from(current) != next_key);
        if changed {
            let now = Instant::now();
            if next_key.editable && !next_key.password {
                self.editable_entry = Some((next_key.clone(), now));
                self.editable_entry_last_attempt = None;
            } else {
                self.editable_entry = None;
                self.editable_entry_last_attempt = None;
            }
            self.candidate = Some(context);
            self.candidate_since = now;
            // 输入框和搜索框优先响应：一旦检测到 Caret/Edit/Document 焦点，
            // 本轮立即切换。非输入区仍保留短暂防抖，避免切窗口时闪动。
            if !self
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.editable)
            {
                return;
            }
        }

        let settle = Duration::from_millis(crate::config::auto_switch_settle_ms());
        if self
            .candidate
            .as_ref()
            .is_some_and(|candidate| !candidate.editable)
            && self.candidate_since.elapsed() < settle
        {
            return;
        }

        let Some(context) = self.candidate.as_ref() else {
            return;
        };
        let context_key = SwitchContextKey::from(context);
        let process_name = process_name(context.process_id).unwrap_or_default();
        // C4D、剪映和 CapCut 只显示用户手动切换后的状态；即使旧配置曾保存
        // auto/chinese/english，也不向这些自绘消息循环发送输入语言消息。
        let rule = if is_indicator_only_process(&process_name) {
            "ignore"
        } else {
            crate::config::auto_switch_app_rule(&process_name)
                .unwrap_or_else(|| default_rule_for_process(&process_name))
        };

        // Adobe 的 auto 模式在启动窗口阶段不接收跨进程语言消息。只有该进程
        // 确实进入过文字编辑后，才在退出编辑时发一次英文切换。显式设置的
        // 固定中文/英文规则仍然按用户选择执行。
        let adobe_auto = rule == "auto" && is_adobe_design_process(&process_name);
        if adobe_auto {
            if context.editable {
                self.adobe_editable_processes.insert(context.process_id);
            } else if !self.adobe_editable_processes.contains(&context.process_id) {
                return;
            }
        }

        let Some(target) = target_for(rule, context.password, context.editable) else {
            return;
        };

        if chinese_mode
            && self
                .editable_entry
                .as_ref()
                .is_some_and(|(entry_key, _)| entry_key == &context_key)
        {
            self.editable_entry = None;
            self.editable_entry_last_attempt = None;
        }

        let enforce_adobe_english = should_enforce_adobe_english(
            adobe_auto,
            context.editable,
            chinese_mode,
            self.adobe_english_attempts
                .get(&context.process_id)
                .map(Instant::elapsed),
        );
        let retry_editable_chinese = should_retry_editable_chinese(
            target,
            chinese_mode,
            self.editable_entry
                .as_ref()
                .and_then(|(entry_key, started)| {
                    (entry_key == &context_key).then(|| started.elapsed())
                }),
            self.editable_entry_last_attempt.map(|last| last.elapsed()),
        );
        if self.applied_context.as_ref() == Some(&context_key)
            && !enforce_adobe_english
            && !retry_editable_chinese
        {
            return;
        }

        // 默认仍然每个上下文只切换一次。搜索框/TextInputHost 刚取得焦点时
        // 可能尚未建立 IME 窗口，因此只在进入后的 260ms 内短时补发；窗口
        // 结束后绝不持续抢回中文，保留用户手动切换一次即可生效的行为。
        self.applied_context = Some(context_key.clone());

        if adobe_auto && !context.editable {
            self.adobe_english_attempts
                .insert(context.process_id, Instant::now());
        }
        if target == LanguageTarget::Chinese && context.editable {
            self.editable_entry_last_attempt = Some(Instant::now());
        }

        let _ = switch_language(context.focused_hwnd, context.foreground_hwnd, target);
    }
}

fn is_indicator_only_process(process_name: &str) -> bool {
    ["Cinema 4D.exe", "JianyingPro.exe", "CapCut.exe"]
        .iter()
        .any(|candidate| process_name.eq_ignore_ascii_case(candidate))
}

fn is_adobe_design_process(process_name: &str) -> bool {
    ["Photoshop.exe", "Illustrator.exe"]
        .iter()
        .any(|candidate| process_name.eq_ignore_ascii_case(candidate))
}

fn should_enforce_adobe_english(
    adobe_auto: bool,
    editable: bool,
    chinese_mode: bool,
    since_last_attempt: Option<Duration>,
) -> bool {
    adobe_auto
        && !editable
        && chinese_mode
        && since_last_attempt.map_or(true, |elapsed| {
            elapsed >= Duration::from_millis(ADOBE_ENGLISH_RETRY_MS)
        })
}

fn should_retry_editable_chinese(
    target: LanguageTarget,
    chinese_mode: bool,
    since_entry: Option<Duration>,
    since_last_attempt: Option<Duration>,
) -> bool {
    target == LanguageTarget::Chinese
        && !chinese_mode
        && since_entry
            .is_some_and(|elapsed| elapsed <= Duration::from_millis(EDITABLE_ENTRY_RETRY_WINDOW_MS))
        && since_last_attempt.map_or(true, |elapsed| {
            elapsed >= Duration::from_millis(EDITABLE_ENTRY_RETRY_INTERVAL_MS)
        })
}

fn default_rule_for_process(process_name: &str) -> &'static str {
    if is_indicator_only_process(process_name) {
        "ignore"
    } else {
        "auto"
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
        Path::new(&full_path)
            .file_name()?
            .to_str()
            .map(str::to_string)
    }
}

fn switch_language(
    focused_hwnd: HWND,
    foreground_hwnd: HWND,
    target: LanguageTarget,
) -> windows::core::Result<()> {
    let klid = match target {
        LanguageTarget::Chinese => crate::config::auto_switch_chinese_klid(),
        LanguageTarget::English => crate::config::auto_switch_english_klid(),
    };
    let wide: Vec<u16> = klid.encode_utf16().chain(Some(0)).collect();

    unsafe {
        let layout = LoadKeyboardLayoutW(PCWSTR(wide.as_ptr()), KLF_ACTIVATE)?;
        let targets = [focused_hwnd, foreground_hwnd];
        let mut any_sent = false;
        for (index, target_hwnd) in targets.into_iter().enumerate() {
            if target_hwnd.0.is_null() || (index == 1 && target_hwnd == focused_hwnd) {
                continue;
            }
            // 英文布局请求可能让 ImmGetDefaultIMEWnd 随即变为空，因此先保存
            // 当前中文 IME 窗口，确保 Adobe 等自绘应用退出编辑时能立即关闭。
            let ime_before = ImmGetDefaultIMEWnd(target_hwnd);
            let mut result = 0usize;
            let sent = SendMessageTimeoutW(
                target_hwnd,
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(INPUTLANGCHANGE_SYSCHARSET),
                LPARAM(layout.0 as isize),
                SMTO_ABORTIFHUNG,
                350,
                Some(&mut result),
            );
            if sent.0 == 0 {
                continue;
            }
            any_sent = true;

            let ime_after = ImmGetDefaultIMEWnd(target_hwnd);
            for ime_hwnd in [ime_before, ime_after] {
                if !ime_hwnd.0.is_null() {
                    send_ime_control(
                        ime_hwnd,
                        IMC_SETOPENSTATUS,
                        ime_open_status_for_target(target),
                    );
                    if target == LanguageTarget::Chinese {
                        send_ime_control(ime_hwnd, IMC_SETCONVERSIONMODE, IME_CMODE_NATIVE);
                    }
                }
                if ime_after == ime_before {
                    break;
                }
            }
        }
        if !any_sent {
            return Err(windows::core::Error::from_win32());
        }
    }
    Ok(())
}

fn ime_open_status_for_target(target: LanguageTarget) -> isize {
    match target {
        LanguageTarget::Chinese => 1,
        LanguageTarget::English => 0,
    }
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
    use std::time::Duration;

    use windows::Win32::Foundation::HWND;

    use super::{
        default_rule_for_process, ime_open_status_for_target, is_indicator_only_process,
        should_enforce_adobe_english, should_retry_editable_chinese, target_for, LanguageTarget,
        SwitchContextKey,
    };
    use crate::caret_detector::FocusContext;

    fn context(editable: bool, password: bool) -> FocusContext {
        FocusContext {
            identity: "same-wechat-window".to_string(),
            foreground_hwnd: HWND::default(),
            focused_hwnd: HWND::default(),
            process_id: 1,
            editable,
            password,
            readonly_document: false,
            force_mouse_indicator: false,
        }
    }

    #[test]
    fn automatic_rules_follow_context() {
        assert_eq!(
            target_for("auto", false, true),
            Some(LanguageTarget::Chinese)
        );
        assert_eq!(
            target_for("auto", false, false),
            Some(LanguageTarget::English)
        );
        assert_eq!(
            target_for("auto", true, true),
            Some(LanguageTarget::English)
        );
    }

    #[test]
    fn application_rules_take_precedence() {
        assert_eq!(
            target_for("chinese", true, false),
            Some(LanguageTarget::Chinese)
        );
        assert_eq!(
            target_for("english", false, true),
            Some(LanguageTarget::English)
        );
        assert_eq!(target_for("ignore", false, true), None);
        assert_eq!(ime_open_status_for_target(LanguageTarget::Chinese), 1);
        assert_eq!(ime_open_status_for_target(LanguageTarget::English), 0);
    }

    #[test]
    fn adobe_non_editing_area_keeps_retrying_english_only_while_chinese() {
        assert!(should_enforce_adobe_english(true, false, true, None));
        assert!(!should_enforce_adobe_english(
            true,
            false,
            true,
            Some(Duration::from_millis(80))
        ));
        assert!(should_enforce_adobe_english(
            true,
            false,
            true,
            Some(Duration::from_millis(120))
        ));
        assert!(!should_enforce_adobe_english(true, true, true, None));
        assert!(!should_enforce_adobe_english(true, false, false, None));
    }

    #[test]
    fn editable_entry_retries_only_during_short_focus_window() {
        assert!(should_retry_editable_chinese(
            LanguageTarget::Chinese,
            false,
            Some(Duration::from_millis(90)),
            Some(Duration::from_millis(80)),
        ));
        assert!(!should_retry_editable_chinese(
            LanguageTarget::Chinese,
            false,
            Some(Duration::from_millis(300)),
            Some(Duration::from_millis(80)),
        ));
        assert!(!should_retry_editable_chinese(
            LanguageTarget::Chinese,
            true,
            Some(Duration::from_millis(90)),
            Some(Duration::from_millis(80)),
        ));
        assert!(!should_retry_editable_chinese(
            LanguageTarget::English,
            false,
            Some(Duration::from_millis(90)),
            Some(Duration::from_millis(80)),
        ));
    }

    #[test]
    fn sensitive_video_and_3d_apps_are_indicator_only_by_default() {
        assert_eq!(default_rule_for_process("Cinema 4D.exe"), "ignore");
        assert_eq!(default_rule_for_process("JianyingPro.exe"), "ignore");
        assert_eq!(default_rule_for_process("CapCut.exe"), "ignore");
        assert!(is_indicator_only_process("capcut.EXE"));
        assert_eq!(default_rule_for_process("Photoshop.exe"), "auto");
        assert_eq!(target_for("ignore", false, false), None);
        assert_eq!(target_for("ignore", false, true), None);
    }

    #[test]
    fn editable_state_changes_context_for_custom_drawn_apps() {
        let non_input = context(false, false);
        let input = context(true, false);

        assert_ne!(
            SwitchContextKey::from(&non_input),
            SwitchContextKey::from(&input)
        );
        assert_eq!(
            SwitchContextKey::from(&input),
            SwitchContextKey::from(&input)
        );
    }
}
