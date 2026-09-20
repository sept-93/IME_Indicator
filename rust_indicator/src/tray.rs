
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, TrackPopupMenu,
    TranslateMessage, CW_USEDEFAULT, HICON, MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    WM_COMMAND, WM_DESTROY, WM_NULL, WM_RBUTTONUP, WM_USER, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP,
};
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromFile, GdipCreateHICONFromBitmap, GdipDisposeImage,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
    REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const WM_TRAYICON: u32 = WM_USER + 1;
const IDM_RESTART: u32 = 1001;
const IDM_CONFIG: u32 = 1002;
const IDM_ABOUT: u32 = 1003;
const IDM_EXIT: u32 = 1004;
const IDM_STARTUP: u32 = 1005;
const IDM_SMART_SWITCH: u32 = 1006;
const STARTUP_VALUE_NAME: windows::core::PCWSTR = w!("IME Indicator");
const STARTUP_RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const SETTINGS_KEY: windows::core::PCWSTR = w!("Software\\IME Indicator");
const SMART_SWITCH_VALUE_NAME: windows::core::PCWSTR = w!("SmartSwitchEnabled");

static SMART_SWITCH_ENABLED: AtomicBool = AtomicBool::new(true);
static MENU_OPEN: AtomicBool = AtomicBool::new(false);

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
            ).unwrap();

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
            let path_str = path.to_str()?.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
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

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            match lparam.0 as u32 {
                WM_RBUTTONUP => {
                    show_context_menu(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
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
        let result = RegSetValueExW(
            key,
            SMART_SWITCH_VALUE_NAME,
            0,
            REG_DWORD,
            Some(&bytes),
        )
        .ok();
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
        MessageBoxW(None, PCWSTR(body.as_ptr()), w!("输入指示器"), MB_ICONERROR | MB_OK);
    }
}

fn open_config() {
    unsafe {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
        
        let path = crate::config::get_config_path();
        let path_w = path.to_str().unwrap().encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        
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
        let about_w: Vec<u16> = about_content.encode_utf16().chain(std::iter::once(0)).collect();
        let title = w!("关于 输入指示器");
        MessageBoxW(None, PCWSTR(about_w.as_ptr()), title, MB_ICONINFORMATION | MB_OK);
    }
}

fn restart_app() {
    unsafe {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
        use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
        
        let mut path = [0u16; 512];
        let len = GetModuleFileNameW(None, &mut path);
        if len > 0 {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path.as_ptr()),
                None,
                None,
                SW_SHOW,
            );
        }
    }
}
