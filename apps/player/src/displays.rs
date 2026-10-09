/// A physical monitor's desktop rectangle, in native pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayMonitor {
    pub index: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// Windows display scaling, e.g. 1.5 for 150%.
    pub scale: f32,
}

#[cfg(windows)]
pub fn enumerate() -> Vec<DisplayMonitor> {
    use std::{mem::size_of, ptr};
    use windows_sys::Win32::{
        Foundation::{BOOL, LPARAM, RECT},
        Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW},
        UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    };

    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(data as *mut Vec<DisplayMonitor>);
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info.monitorInfo) != 0 {
            let rect = info.monitorInfo.rcMonitor;
            let end = info
                .szDevice
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.szDevice.len());
            let device = String::from_utf16_lossy(&info.szDevice[..end]);
            // Windows exposes names such as `\\.\DISPLAY2`; this number is
            // the one users see in Display Settings. Fall back to enumeration
            // order only for an unusual driver-provided name.
            let index = device.trim_end_matches(|c: char| c.is_ascii_digit()).len();
            let index = device[index..]
                .parse::<u32>()
                .ok()
                .and_then(|number| number.checked_sub(1))
                .unwrap_or(monitors.len() as u32);
            monitors.push(DisplayMonitor {
                index,
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
                // MONITORINFOF_PRIMARY is defined as 1 in WinUser.h.
                primary: info.monitorInfo.dwFlags & 1 != 0,
                scale: {
                    let (mut dpi_x, mut dpi_y) = (96_u32, 96_u32);
                    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0 {
                        dpi_x as f32 / 96.0
                    } else {
                        1.0
                    }
                },
            });
        }
        1
    }

    let mut monitors = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(collect),
            &mut monitors as *mut Vec<DisplayMonitor> as LPARAM,
        );
    }
    monitors
}

#[cfg(not(windows))]
pub fn enumerate() -> Vec<DisplayMonitor> {
    // MapForge is Windows-first. Other development platforms keep using the
    // existing preview windows instead of guessing at native display APIs.
    Vec::new()
}
