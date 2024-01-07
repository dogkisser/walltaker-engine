//! This module handles the hacky hack(s) required to get a video playing as
//! the wallpaper.
use log::info;
use windows::{
    core::{s, PCSTR},
    Win32::{
        Foundation::*,
        UI::WindowsAndMessaging::*,
        Graphics::Gdi::{HMONITOR, HDC, EnumDisplayMonitors, HBRUSH},
        System::LibraryLoader::GetModuleHandleA,
    },
};

/// This function creates a HWND between the wallpaper and desktop icons
/// for each monitor. Based on this:
/// <https://www.codeproject.com/Articles/856020/Draw-Behind-Desktop-Icons-in-Windows-plus>
pub unsafe fn find_hwnds() -> anyhow::Result<Vec<HWND>> {
    let progman = FindWindowA(s!("Progman"), PCSTR::null());
    anyhow::ensure!(progman.0 != 0, "No progman process");

    // 0x052C asks Progman to create a new window, called WorkerW, between the
    // wallpaper and desktop icons. There's no WINAPI constant for it because
    // this feature is undocumented.
    SendMessageTimeoutA(
        progman,
        0x052C,
        WPARAM(0),
        LPARAM(0),
        SMTO_NORMAL,
        1000,
        None);

    // Now we need to find the window it created.
    let mut workerw_hwnd = HWND(0);
    EnumWindows(Some(enum_windows_proc),
        LPARAM(std::ptr::addr_of_mut!(workerw_hwnd) as isize))?;
    anyhow::ensure!(workerw_hwnd.0 != 0, "Couldn't find WorkerW");
    info!("WorkerW HWND: {:#X?}", workerw_hwnd.0);

    // Register a window class with a transparent background. This sidesteps
    // the user's wallpaper being a harsh white while until a video is set.
    let class = WNDCLASSA {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndclass_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(GetModuleHandleA(PCSTR::null())?.0),
        hIcon: HICON(0),
        hCursor: HCURSOR(0),
        hbrBackground: HBRUSH(0),
        lpszMenuName: s!(""),
        lpszClassName: s!("Walltaker Engine")
    };
    RegisterClassA(std::ptr::addr_of!(class));

    // This pushes the WorkerW HWND as the first element of the Vec so I don't
    // have to bother creating a struct etc. to move that extra information in.
    let mut hwnds = Vec::from(&[workerw_hwnd]);
    let ptr = std::ptr::addr_of_mut!(hwnds) as isize;
    // Create that new window for each monitor
    EnumDisplayMonitors(HDC(0), None, Some(enum_monitors_proc), LPARAM(ptr));
    // The WorkerW HWND is removed at the end :)
    hwnds.swap_remove(0);

    anyhow::ensure!(!hwnds.is_empty(), "Couldn't create HWNDs");

    Ok(hwnds)
}

// This function HAS to be defined (or the application hangs, interestingly)
// but it's just a stub.
unsafe extern "system" fn wndclass_proc(
    _: HWND,
    _: u32,
    _: WPARAM,
    _: LPARAM
) -> LRESULT
{
    LRESULT(1)
}

// Called by ``EnumWindows``; iterates every open window on the system,
// checking whether it has a child called `SHELLDLL_DefView`, the container for
// the desktop icons window. The window "next" to this one is our WorkerW
// created by Progman.
unsafe extern "system" fn enum_windows_proc(hwnd: HWND, out: LPARAM) -> BOOL {
    let wind = FindWindowExA(hwnd, HWND(0), s!("SHELLDLL_DefView"),
        PCSTR::null());

    if wind.0 != 0 {
        let out: &mut isize = &mut *(out.0 as *mut isize);
        let target = FindWindowExA(HWND(0), hwnd, s!("WorkerW"),
            PCSTR::null()).0;

        *out = target;
    }

    true.into()
}

/// Called by ``EnumDisplayMonitors``; for every connected monitor, spawn a new
/// window to fit it.
unsafe extern "system" fn enum_monitors_proc(
    _: HMONITOR,
    _: HDC,
    rect: *mut RECT,
    out: LPARAM,
) -> BOOL {
    let hwnds: &mut Vec<HWND> = &mut *(out.0 as *mut _);
    let workerw_hwnd = hwnds[0];

    let RECT { left: x, top: y, right, bottom } = *rect;
    let width = right - x;
    let height = bottom - y;

    info!("Creating window at {x}:{y} size {width}:{height}");

    let next_hwnd = CreateWindowExA(
        WS_EX_NOACTIVATE,
        s!("Walltaker Engine"),
        s!(""),
        WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        workerw_hwnd,
        HMENU(0),
        HINSTANCE(0),
        None,
    );

    hwnds.push(next_hwnd);

    true.into()
}