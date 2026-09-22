use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, COLORREF, HWND, LPARAM, LRESULT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetSysColorBrush, SetBkMode, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    COLOR_WINDOW, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HBRUSH, HDC, HFONT,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromFile, GdipCreateHICONFromBitmap, GdipDisposeImage,
};
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
use windows::Win32::UI::Controls::{
    ImageList_Create, ImageList_Destroy, ImageList_ReplaceIcon, ImageList_SetBkColor,
    InitCommonControlsEx, CLR_NONE, HIMAGELIST, ICC_LISTVIEW_CLASSES, ILC_COLOR32, ILC_MASK,
    INITCOMMONCONTROLSEX, LVCF_WIDTH, LVCOLUMNW, LVIF_IMAGE, LVIF_TEXT, LVITEMW,
    LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETIMAGELIST, LVNI_SELECTED, LVSIL_SMALL,
    LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_NOCOLUMNHEADER, LVS_REPORT, LVS_SHOWSELALWAYS,
    LVS_SINGLESEL,
};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW,
    EnumChildWindows, EnumWindows, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, ShowWindow, TrackPopupMenu,
    TranslateMessage, BM_GETCHECK, BM_SETCHECK, BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON,
    CW_USEDEFAULT, HICON, HMENU, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORSTATIC,
    WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETFONT, WM_USER, WNDCLASSW, WS_CHILD,
    WS_EX_CLIENTEDGE, WS_GROUP, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use std::cell::RefCell;
use std::collections::HashSet;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
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
const IDC_SAVE_RULE: u32 = 2003;
const IDC_REFRESH_APPS: u32 = 2004;
const IDC_RULE_FIRST: u32 = 2100;
const APP_RULE_LABELS: [&str; 5] = [
    "自动识别输入框",
    "固定中文",
    "固定英文",
    "删除自定义规则",
    "仅显示状态",
];
const APP_RULE_MODES: [&str; 5] = ["auto", "chinese", "english", "remove", "ignore"];
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
    exe_path: PathBuf,
}

struct AppRuleEditorState {
    hwnd: HWND,
    list: HWND,
    rule_buttons: Vec<HWND>,
    font: HFONT,
    title_font: HFONT,
    image_list: Option<HIMAGELIST>,
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
            WM_LBUTTONUP => {
                show_app_rule_editor();
                LRESULT(0)
            }
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
        let common_controls = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&common_controls);
        let class_name = w!("IMEIndicatorAppRulesClass");
        let window_icon = LoadIconW(h_instance, PCWSTR(1 as _)).unwrap_or_default();
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(app_rule_window_proc),
            hInstance: h_instance.into(),
            lpszClassName: class_name,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 as usize + 1) as *mut _),
            hIcon: window_icon,
            ..Default::default()
        };
        let _ = RegisterClassW(&window_class);

        const EDITOR_WIDTH: i32 = 400;
        const EDITOR_HEIGHT: i32 = 535;
        let editor_x = ((GetSystemMetrics(SM_CXSCREEN) - EDITOR_WIDTH) / 2).max(0);
        let editor_y = ((GetSystemMetrics(SM_CYSCREEN) - EDITOR_HEIGHT) / 2).max(0);

        let Ok(hwnd) = CreateWindowExW(
            Default::default(),
            class_name,
            w!("应用规则 - 输入指示器"),
            WS_OVERLAPPEDWINDOW,
            editor_x,
            editor_y,
            EDITOR_WIDTH,
            EDITOR_HEIGHT,
            None,
            None,
            h_instance,
            None,
        ) else {
            show_error("无法创建应用规则窗口。");
            return;
        };

        let header = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("应用规则"),
            WS_CHILD | WS_VISIBLE,
            16,
            10,
            240,
            24,
            hwnd,
            None,
            h_instance,
            None,
        )
        .ok();
        let _ = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("选择已打开的应用并设置处理方式。"),
            WS_CHILD | WS_VISIBLE,
            16,
            35,
            352,
            20,
            hwnd,
            None,
            h_instance,
            None,
        );
        let Ok(list) = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("SysListView32"),
            None,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOCOLUMNHEADER),
            16,
            58,
            352,
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
        let column = LVCOLUMNW {
            mask: LVCF_WIDTH,
            cx: 328,
            ..Default::default()
        };
        let _ = SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            WPARAM(0),
            LPARAM((&column as *const LVCOLUMNW) as isize),
        );
        let _ = SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            WPARAM(0),
            LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize),
        );
        let _ = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("处理方式"),
            WS_CHILD | WS_VISIBLE,
            16,
            356,
            120,
            18,
            hwnd,
            None,
            h_instance,
            None,
        );

        let mut rule_buttons = Vec::new();
        let positions = [
            (16, 372, 135),
            (151, 372, 105),
            (256, 372, 105),
            (16, 394, 120),
            (136, 394, 150),
        ];
        for (index, label) in APP_RULE_LABELS.iter().enumerate() {
            let group = if index == 0 {
                WS_GROUP
            } else {
                WINDOW_STYLE(0)
            };
            let Ok(button) = CreateWindowExW(
                Default::default(),
                w!("BUTTON"),
                PCWSTR(
                    label
                        .encode_utf16()
                        .chain(Some(0))
                        .collect::<Vec<_>>()
                        .as_ptr(),
                ),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | group
                    | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
                positions[index].0,
                positions[index].1,
                positions[index].2,
                24,
                hwnd,
                HMENU((IDC_RULE_FIRST + index as u32) as usize as *mut _),
                h_instance,
                None,
            ) else {
                let _ = DestroyWindow(hwnd);
                show_error("无法创建规则选项。");
                return;
            };
            rule_buttons.push(button);
        }
        let _ = SendMessageW(rule_buttons[0], BM_SETCHECK, WPARAM(1), LPARAM(0));

        let _ = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            w!("刷新列表"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            160,
            426,
            96,
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
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            264,
            426,
            104,
            32,
            hwnd,
            HMENU(IDC_SAVE_RULE as usize as *mut _),
            h_instance,
            None,
        );
        let _ = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("提示：仅显示状态不会自动切换或探测控件。"),
            WS_CHILD | WS_VISIBLE,
            16,
            470,
            352,
            18,
            hwnd,
            None,
            h_instance,
            None,
        );

        // 未显式设置字体时，原生 LISTBOX/COMBOBOX 会退回难看的等宽系统字体。
        // 给全部子控件统一使用 Windows 默认界面字体，并立即重绘。
        let font = create_ui_font(-12, FW_NORMAL.0 as i32);
        let title_font = create_ui_font(-13, FW_NORMAL.0 as i32);
        let _ = EnumChildWindows(hwnd, Some(set_default_gui_font), LPARAM(font.0 as isize));
        if let Some(header) = header {
            let _ = SendMessageW(header, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1));
        }

        APP_RULE_EDITOR.with(|state| {
            *state.borrow_mut() = Some(AppRuleEditorState {
                hwnd,
                list,
                rule_buttons,
                font,
                title_font,
                image_list: None,
                apps: Vec::new(),
            });
        });
        refresh_running_apps();
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

unsafe fn create_ui_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        w!("Microsoft YaHei UI"),
    )
}

unsafe extern "system" fn set_default_gui_font(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let _ = SendMessageW(hwnd, WM_SETFONT, WPARAM(lparam.0 as usize), LPARAM(1));
    TRUE
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
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = HDC(wparam.0 as *mut _);
            let _ = SetBkMode(hdc, TRANSPARENT);
            LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            APP_RULE_EDITOR.with(|state| {
                let mut state = state.borrow_mut();
                if state.as_ref().is_some_and(|editor| editor.hwnd == hwnd) {
                    if let Some(editor) = state.take() {
                        if let Some(image_list) = editor.image_list {
                            let _ = ImageList_Destroy(image_list);
                        }
                        let _ = DeleteObject(editor.font);
                        let _ = DeleteObject(editor.title_font);
                    }
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
            let _ = SendMessageW(editor.list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
            let (new_image_list, image_indices) = create_app_image_list(&apps);
            let image_handle = new_image_list.map_or(0, |images| images.0);
            let _ = SendMessageW(
                editor.list,
                LVM_SETIMAGELIST,
                WPARAM(LVSIL_SMALL as usize),
                LPARAM(image_handle),
            );
            let old_image_list = editor.image_list.take();
            editor.image_list = new_image_list;
            if let Some(old_image_list) = old_image_list {
                let _ = ImageList_Destroy(old_image_list);
            }

            for (index, app) in apps.iter().enumerate() {
                let mut wide: Vec<u16> = app.display_name.encode_utf16().chain(Some(0)).collect();
                let item = LVITEMW {
                    mask: LVIF_TEXT | LVIF_IMAGE,
                    iItem: index as i32,
                    iSubItem: 0,
                    pszText: PWSTR(wide.as_mut_ptr()),
                    iImage: image_indices.get(index).copied().unwrap_or(-1),
                    ..Default::default()
                };
                let _ = SendMessageW(
                    editor.list,
                    LVM_INSERTITEMW,
                    WPARAM(0),
                    LPARAM((&item as *const LVITEMW) as isize),
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
            let app_index = SendMessageW(
                editor.list,
                LVM_GETNEXTITEM,
                WPARAM(usize::MAX),
                LPARAM(LVNI_SELECTED as isize),
            )
            .0;
            if app_index < 0 {
                return None;
            }
            let rule_index = editor.rule_buttons.iter().position(|button| {
                SendMessageW(*button, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1
            })?;
            let app = editor.apps.get(app_index as usize)?.exe_name.clone();
            Some((app, rule_index))
        }
    });
    let Some((app, rule_index)) = selection else {
        show_error("请先选择一个应用和规则。");
        return;
    };
    let mode = APP_RULE_MODES.get(rule_index).copied().unwrap_or("auto");
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
    let Some((exe_name, exe_path)) = process_info_from_id(process_id) else {
        return TRUE;
    };
    let apps = &mut *(lparam.0 as *mut Vec<RunningApp>);
    apps.push(RunningApp {
        display_name: process_display_name(&exe_name),
        exe_name,
        exe_path,
    });
    TRUE
}

fn process_display_name(exe_name: &str) -> String {
    Path::new(exe_name)
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(exe_name)
        .to_string()
}

fn process_info_from_id(process_id: u32) -> Option<(String, PathBuf)> {
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
        let path = PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
        let exe_name = path.file_name()?.to_str()?.to_string();
        Some((exe_name, path))
    }
}

unsafe fn create_app_image_list(apps: &[RunningApp]) -> (Option<HIMAGELIST>, Vec<i32>) {
    let images = ImageList_Create(28, 28, ILC_COLOR32 | ILC_MASK, apps.len().max(1) as i32, 4);
    if images.0 == 0 {
        return (None, vec![-1; apps.len()]);
    }
    let _ = ImageList_SetBkColor(images, COLORREF(CLR_NONE as u32));

    let mut image_indices = Vec::with_capacity(apps.len());
    for app in apps {
        let path: Vec<u16> = app
            .exe_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut file_info = SHFILEINFOW::default();
        let loaded = SHGetFileInfoW(
            PCWSTR(path.as_ptr()),
            Default::default(),
            Some(&mut file_info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if loaded != 0 && !file_info.hIcon.0.is_null() {
            image_indices.push(ImageList_ReplaceIcon(images, -1, file_info.hIcon));
            let _ = DestroyIcon(file_info.hIcon);
        } else {
            image_indices.push(-1);
        }
    }
    (Some(images), image_indices)
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

#[cfg(test)]
mod tests {
    use super::{process_display_name, APP_RULE_LABELS, APP_RULE_MODES};

    #[test]
    fn destructive_and_indicator_only_rules_have_matching_positions() {
        assert_eq!(APP_RULE_LABELS[3], "删除自定义规则");
        assert_eq!(APP_RULE_MODES[3], "remove");
        assert_eq!(APP_RULE_LABELS[4], "仅显示状态");
        assert_eq!(APP_RULE_MODES[4], "ignore");
    }

    #[test]
    fn application_list_uses_only_the_executable_name() {
        assert_eq!(process_display_name("Photoshop.exe"), "Photoshop");
        assert_eq!(process_display_name("Tabbit Browser.exe"), "Tabbit Browser");
    }

    #[test]
    fn extensionless_process_names_are_kept() {
        assert_eq!(process_display_name("微信"), "微信");
    }
}
