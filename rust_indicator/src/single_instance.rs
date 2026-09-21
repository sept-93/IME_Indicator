//! 进程级单实例保护。

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

static INSTANCE_MUTEX: AtomicIsize = AtomicIsize::new(0);

/// 返回 false 表示已有一个 IME Indicator 正在运行。
pub fn acquire() -> bool {
    unsafe {
        let Ok(handle) = CreateMutexW(
            None,
            false,
            w!("Local\\IME_Indicator_sept93_single_instance"),
        ) else {
            // 无法创建互斥体时不阻止程序启动，避免系统异常时完全无法使用。
            return true;
        };

        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return false;
        }

        INSTANCE_MUTEX.store(handle.0 as isize, Ordering::Release);
        true
    }
}

/// 重启前主动释放名字；正常退出时 Windows 会自动关闭进程句柄。
pub fn release() {
    let raw = INSTANCE_MUTEX.swap(0, Ordering::AcqRel);
    if raw != 0 {
        unsafe {
            let _ = CloseHandle(HANDLE(raw as *mut _));
        }
    }
}
