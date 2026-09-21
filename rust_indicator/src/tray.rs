use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, LRESULT, TRUE, WPARAM};
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromFile, GdipCreateHICONFromBitmap, GdipDisposeImage,
};
use windows::Win32::Graphics::Gdi::{HBRUSH, COLOR_WINDOW};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
    GetCursorPos, GetMessageW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage, CBS_DROPDOWNLIST,
    CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CW_USEDEFAULT, HICON, HMENU, LBS_NOTIFY,
    LB_ADDSTRING, LB_GETCURSEL, LB_RESETCONTENT, MSG, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_NULL, WM_RBUTTONUP, WM_USER, WNDCLASSW,
    WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const WM_TRAYICON: u32 = WM_USER + 1;
const IDM_RESTART: u32 = 1001;
const IDM_CONFIG: u32 = 1002;
const IDM_ABOUT: u32 = 1003;
const IDM_EXIT: u32 = 1004;
const IDM_STARTUP: u32 = 1005;
const IDM_SMART_SWITCH: u32 = 1006;
const IDM_APP_RULES: u32 = 1007;
const IDC_APP_LIST: u32 = 2001;
const IDC_RULE_COMBO: u32 = 2002;
const IDC_SAVE_RULE: u32 = 2003;
const IDC_REFRESH_APPS: u32 = 2004;
const STARTUP_VALUE_NAME: windows::core::PCWSTR = w!("IME Indicator");
const STARTUP_RUN_KEY: windows::core::PCWSTR =
    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const SETTINGS_KEY: windows::core::PCWSTR = w!("Software\\IME Indicator");
const SMART_SWITCH_VALUE_NAME: windows::core::PCWSTR = w!("SmartSwitchEnabled");

static SMART_SWITCH_ENABLED: AtomicBool = AtomicBool::new(true);
static MENU_OPEN: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct RunningApp {
    exe_name: String,
    display_name: String,
}

struct AppRuleEditorState {
    hwnd: HWND,
    list: HWND,
    combo: HWND,
    apps: Vec<RunningApp>,
}

thread_local! {
    static APP_RULE_EDITOR: RefCell<Option<AppRuleEditorState>> = const { RefCell::new(None) };
}

pub fn initialize_smart_switch(default_enabled: bool) {
    let enabled = read_smart_switch_enabled().unwrap_or(default_enabled);
    SMART_SWITCH_ENABLED.store(enabled, Ordering::Release);
}

pub fn smart_switch_enabled() -> bool {
    SMART_SWITCH_ENABLED.load(Ordering::Acquire)
}

pub struct TrayManager {
    hwnd: HWND,
}

impl TrayManager {
    pub fn new(icon: HICON) -> Self {
        unsafe {
            let h_instance = GetModuleHandleW(None).unwrap();
            let class_name = w!("IMETrayWindowClass");

            let wnd_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: h_instance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };

            RegisterClassW(&wnd_class);

            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                w!("IME Indicator Tray"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                h_instance,
                None,
            )
            .unwrap();

            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAYICON,
                hIcon: icon,
                ..Default::default()
            };

            // 设置提示文字
            let tip = w!("输入指示器 (IME Indicator)");
            let tip_slice = tip.as_wide();
            let len = tip_slice.len().min(nid.szTip.len() - 1);
            nid.szTip[..len].copy_from_slice(&tip_slice[..len]);

            let _ = Shell_NotifyIconW(NIM_ADD, &nid);

            Self { hwnd }
        }
    }

    pub fn load_icon_from_file(path: &Path) -> Option<HICON> {
        unsafe {
            let path_str = path
                .to_str()?
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<u16>>();
            let mut bitmap = std::ptr::null_mut();

            if GdipCreateBitmapFromFile(PCWSTR(path_str.as_ptr()), &mut bitmap).0 == 0 {
                let mut hicon = HICON::default();
                if GdipCreateHICONFromBitmap(bitmap, &mut hicon).0 == 0 {
                    let _ = GdipDisposeImage(bitmap as _);
                    return Some(hicon);
                }
                let _ = GdipDisposeImage(bitmap as _);
            }
            None
        }
    }

    pub fn run_message_loop(&self) {
        unsafe {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn destroy(&self) {
        unsafe {
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAYICON => match lparam.0 as u32 {
            WM_RBUTTONUP => {
                show_context_menu(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        },
        WM_COMMAND => {
            let id = wparam.0 as u32 & 0xFFFF;
            match id {
                IDM_EXIT => {
                    PostQuitMessage(0);
                }
                IDM_RESTART => {
                    restart_app();
                    PostQuitMessage(0);
                }
                IDM_CONFIG => {
                    open_config();
                }
                IDM_APP_RULES => {
                    show_app_rule_editor();
                }
                IDM_ABOUT => {
                    show_about();
                }
                IDM_STARTUP => {
                    if let Err(error) = set_startup_enabled(!is_startup_enabled()) {
                        show_error(&format!("更新开机自启失败：\n{}", error));
                    }
                }
                IDM_SMART_SWITCH => {
                    if let Err(error) = set_smart_switch_enabled(!smart_switch_enabled()) {
                        show_error(&format!("更新智能切换失败：\n{}", error));
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_context_menu(hwnd: HWND) {
    // TrackPopupMenu 会运行嵌套消息循环；重复右键可能重入 WndProc。只允许一个菜单实例，
    // 并且所有 Win32 返回值都按可取消操作处理，避免用户点空白处时 unwrap 导致闪退。
    if MENU_OPEN.swap(true, Ordering::AcqRel) {
        return;
    }
    let Ok(menu) = CreatePopupMenu() else {
        MENU_OPEN.store(false, Ordering::Release);
        return;
    };
    let startup_flag = if is_startup_enabled() {
        windows::Win32::UI::WindowsAndMessaging::MF_CHECKED
    } else {
        windows::Win32::UI::WindowsAndMessaging::MF_UNCHECKED
    };
    let smart_switch_flag = if smart_switch_enabled() {
        windows::Win32::UI::WindowsAndMessaging::MF_CHECKED
    } else {
        windows::Win32::UI::WindowsAndMessaging::MF_UNCHECKED
    };
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING,
        IDM_CONFIG as usize,
        w!("编辑配置 (Config)"),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING,
        IDM_APP_RULES as usize,
        w!("应用规则 (Apps)"),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING | smart_switch_flag,
        IDM_SMART_SWITCH as usize,
        w!("智能切换 (Smart)"),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING | startup_flag,
        IDM_STARTUP as usize,
        w!("开机自启 (Startup)"),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING,
        IDM_RESTART as usize,
        w!("重启程序 (Restart)"),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING,
        IDM_ABOUT as usize,
        w!("关于 (About)"),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_SEPARATOR,
        0,
        None,
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::AppendMenuW(
        menu,
        windows::Win32::UI::WindowsAndMessaging::MF_STRING,
        IDM_EXIT as usize,
        w!("退出 (Exit)"),
    );

    let mut pos = windows::Win32::Foundation::POINT::default();
    if GetCursorPos(&mut pos).is_err() {
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(menu);
        MENU_OPEN.store(false, Ordering::Release);
        return;
    }

    // 必须设置前台窗口，否则菜单点击外部不会消失
    let _ = SetForegroundWindow(hwnd);

    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        pos.x,
        pos.y,
        0,
        hwnd,
        None,
    );

    // Windows 文档建议菜单关闭后向所属窗口投递一条无操作消息，确保点击外部时
    // 菜单状态完整退出，否则下一次右键可能重入或留下空白弹框。
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(menu);
    MENU_OPEN.store(false, Ordering::Release);
}

fn read_smart_switch_enabled() -> Option<bool> {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
        .is_err()
        {
            return None;
        }
        let mut value = 0u32;
        let mut byte_len = std::mem::size_of::<u32>() as u32;
        let result = RegQueryValueExW(
            key,
            SMART_SWITCH_VALUE_NAME,
            None,
            None,
            Some((&mut value as *mut u32).cast::<u8>()),
            Some(&mut byte_len),
        );
        let _ = RegCloseKey(key);
        if result.is_err() {
            return None;
        }
        Some(value != 0)
    }
}

fn set_smart_switch_enabled(enabled: bool) -> windows::core::Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let bytes = u32::from(enabled).to_ne_bytes();
        let result = RegSetValueExW(key, SMART_SWITCH_VALUE_NAME, 0, REG_DWORD, Some(&bytes)).ok();
        let _ = RegCloseKey(key);
        result?;
        SMART_SWITCH_ENABLED.store(enabled, Ordering::Release);
        Ok(())
    }
}

fn is_startup_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            STARTUP_RUN_KEY,
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
        .is_err()
        {
            return false;
        }
        let enabled = RegQueryValueExW(key, STARTUP_VALUE_NAME, None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        enabled
    }
}

fn set_startup_enabled(enabled: bool) -> windows::core::Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            STARTUP_RUN_KEY,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;

        let result = if enabled {
            let exe_path = std::env::current_exe()?;
            let command = format!("\"{}\"", exe_path.display());
            let value: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
            let bytes = std::slice::from_raw_parts(
                value.as_ptr().cast::<u8>(),
                value.len() * std::mem::size_of::<u16>(),
            );
            RegSetValueExW(key, STARTUP_VALUE_NAME, 0, REG_SZ, Some(bytes)).ok()
        } else {
            RegDeleteValueW(key, STARTUP_VALUE_NAME).ok()
        };
        let _ = RegCloseKey(key);
        result
    }
}

fn show_error(message: &str) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let body: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            w!("输入指示器"),
            MB_ICONERROR | MB_OK,
        );
    }
}

fn show_app_rule_editor() {
    unsafe {
        let mut existing = None;
        APP_RULE_EDITOR.with(|state| {
            existing = state.borrow().as_ref().map(|editor| editor.hwnd);
        });
        if let Some(hwnd) = existing {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            return;
        }

        let Ok(h_instance) = GetModuleHandleW(None) else {
            show_error("无法创建应用规则窗口。");
            return;
        };
        let class_name = w!("IMEIndicatorAppRulesClass");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(app_rule_window_proc),
            hInstance: h_instance.into(),
            lpszClassName: class_name,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 as usize + 1) as *mut _),
            ..Default::default()
        };
        let _ = RegisterClassW(&window_class);

        let Ok(hwnd) = CreateWindowExW(
            Default::default(),
            class_name,
            w!("应用规则 - 输入指示器"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            470,
            None,
            None,
            h_instance,
            None,
        ) else {
            show_error("无法创建应用规则窗口。");
            return;
        };

        let _ = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("从当前已打开的应用中选择："),
            WS_CHILD | WS_VISIBLE,
            20,
            18,
            360,
            24,
            hwnd,
            None,
            h_instance,
            None,
        );
        let Ok(list) = CreateWindowExW(
            Default::default(),
            w!("LISTBOX"),
            None,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_BORDER
                | WS_VSCROLL
                | WINDOW_STYLE(LBS_NOTIFY as u32),
            20,
            45,
            590,
            285,
            hwnd,
            HMENU(IDC_APP_LIST as usize as *mut _),
            h_instance,
            None,
        ) else {
            let _ = DestroyWindow(hwnd);
            show_error("无法创建应用列表。");
            return;
        };
        let Ok(combo) = CreateWindowExW(
            Default::default(),
            w!("COMBOBOX"),
            None,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
            20,
            350,
            290,
            220,
            hwnd,
            HMENU(IDC_RULE_COMBO as usize as *mut _),
            h_instance,
            None,
        ) else {
            let _ = DestroyWindow(hwnd);
            show_error("无法创建规则选择框。");
            return;
        };

        for label in [
            "自动识别输入框",
            "固定中文",
            "固定英文",
            "忽略此应用",
            "删除自定义规则",
        ] {
            let wide: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            let _ = SendMessageW(
                combo,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
        }
        let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));

        let _ = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            w!("刷新列表"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            330,
            348,
            125,
            32,
            hwnd,
            HMENU(IDC_REFRESH_APPS as usize as *mut _),
            h_instance,
            None,
        );
        let _ = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            w!("保存并应用"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            475,
            348,
            135,
            32,
            hwnd,
            HMENU(IDC_SAVE_RULE as usize as *mut _),
            h_instance,
            None,
        );
        let _ = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("“自动识别输入框”只补充控件识别；固定规则会覆盖输入框/非输入区判断。"),
            WS_CHILD | WS_VISIBLE,
            20,
            392,
            590,
            24,
            hwnd,
            None,
            h_instance,
            None,
        );

        APP_RULE_EDITOR.with(|state| {
            *state.borrow_mut() = Some(AppRuleEditorState {
                hwnd,
                list,
                combo,
                apps: Vec::new(),
            });
        });
        refresh_running_apps();
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

unsafe extern "system" fn app_rule_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match wparam.0 as u32 & 0xffff {
                IDC_SAVE_RULE => save_selected_app_rule(),
                IDC_REFRESH_APPS => refresh_running_apps(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            APP_RULE_EDITOR.with(|state| {
                let should_clear = state
                    .borrow()
                    .as_ref()
                    .is_some_and(|editor| editor.hwnd == hwnd);
                if should_clear {
                    *state.borrow_mut() = None;
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn refresh_running_apps() {
    let apps = enumerate_running_apps();
    APP_RULE_EDITOR.with(|state| {
        let mut state = state.borrow_mut();
        let Some(editor) = state.as_mut() else { return };
        unsafe {
            let _ = SendMessageW(editor.list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
            for app in &apps {
                let wide: Vec<u16> = app.display_name.encode_utf16().chain(Some(0)).collect();
                let _ = SendMessageW(
                    editor.list,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(wide.as_ptr() as isize),
                );
            }
        }
        editor.apps = apps;
    });
}

fn save_selected_app_rule() {
    let selection = APP_RULE_EDITOR.with(|state| {
        let state = state.borrow();
        let editor = state.as_ref()?;
        unsafe {
            let app_index = SendMessageW(editor.list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
            let rule_index = SendMessageW(editor.combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
            if app_index < 0 || rule_index < 0 {
                return None;
            }
            let app = editor.apps.get(app_index as usize)?.exe_name.clone();
            Some((app, rule_index as usize))
        }
    });
    let Some((app, rule_index)) = selection else {
        show_error("请先选择一个应用和规则。");
        return;
    };
    let mode = ["auto", "chinese", "english", "ignore", "remove"]
        .get(rule_index)
        .copied()
        .unwrap_or("auto");
    if let Err(error) = crate::config::update_application_setting(&app, mode) {
        show_error(&format!("保存应用规则失败：\n{}", error));
        return;
    }

    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONINFORMATION, MB_YESNO,
        };
        let body = format!("已保存 {} 的规则。\n\n现在重启程序使配置生效吗？", app);
        let body_w: Vec<u16> = body.encode_utf16().chain(Some(0)).collect();
        if MessageBoxW(
            None,
            PCWSTR(body_w.as_ptr()),
            w!("输入指示器"),
            MB_ICONINFORMATION | MB_YESNO,
        ) == IDYES
        {
            restart_app();
            PostQuitMessage(0);
        }
    }
}

fn enumerate_running_apps() -> Vec<RunningApp> {
    let mut apps = Vec::<RunningApp>::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_running_app),
            LPARAM((&mut apps as *mut Vec<RunningApp>) as isize),
        );
    }
    let mut seen = HashSet::new();
    apps.retain(|app| seen.insert(app.exe_name.to_lowercase()));
    apps.sort_by_key(|app| app.display_name.to_lowercase());
    apps
}

unsafe extern "system" fn collect_running_app(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let title_len = GetWindowTextLengthW(hwnd);
    if title_len <= 0 {
        return TRUE;
    }
    let mut process_id = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id == 0 || process_id == GetCurrentProcessId() {
        return TRUE;
    }
    let Some(exe_name) = process_name_from_id(process_id) else {
        return TRUE;
    };
    let mut title = vec![0u16; title_len as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut title);
    if copied <= 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&title[..copied as usize]);
    let apps = &mut *(lparam.0 as *mut Vec<RunningApp>);
    apps.push(RunningApp {
        display_name: format!("{}  —  {}", exe_name, title),
        exe_name,
    });
    TRUE
}

fn process_name_from_id(process_id: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        Path::new(&path).file_name()?.to_str().map(str::to_string)
    }
}

fn open_config() {
    unsafe {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

        let path = crate::config::get_config_path();
        let path_w = path
            .to_str()
            .unwrap()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<u16>>();

        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path_w.as_ptr()),
            None,
            None,
            SW_SHOW,
        );
    }
}

fn show_about() {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
        let about_content = include_str!("../assets/about.txt");
        let about_w: Vec<u16> = about_content
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let title = w!("关于 输入指示器");
        MessageBoxW(
            None,
            PCWSTR(about_w.as_ptr()),
            title,
            MB_ICONINFORMATION | MB_OK,
        );
    }
}

fn restart_app() {
    unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

        let mut path = [0u16; 512];
        let len = GetModuleFileNameW(None, &mut path);
        if len > 0 {
            crate::single_instance::release();
            ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), None, None, SW_SHOW);
        }
    }
}
