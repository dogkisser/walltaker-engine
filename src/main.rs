#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::pedantic, clippy::style)]
// Clippy clearly hasn't met WinAPI
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::wildcard_imports,
)]
use std::sync::Arc;
use tokio::{sync::{RwLock, Mutex}, net::TcpStream};
use rand::seq::SliceRandom;
use tokio_tungstenite::{
    tungstenite::{self, Message},
    WebSocketStream, MaybeTlsStream,
};
use winrt_notification::{Toast, IconCrop};
use tray_item::{IconSource, TrayItem};
use futures_util::{StreamExt, SinkExt, stream::SplitSink};
use windows::{
    core::{s, w, PCSTR, HSTRING, PWSTR, PCWSTR},
    Win32::{
        Foundation::*,
        UI::{
            WindowsAndMessaging::*,
            Controls::{
                IsDlgButtonChecked, BST_CHECKED, EM_SETCUEBANNER,
                Dialogs::{
                    OFN_EXPLORER, OPENFILENAMEW, GetSaveFileNameW,
                    OFN_PATHMUSTEXIST, OFN_HIDEREADONLY,
                }},
            Shell::*,
        },
        Graphics::Gdi::{HMONITOR, HDC, EnumDisplayMonitors, HBRUSH},
        System::LibraryLoader::GetModuleHandleA,
    },
};

mod wallpaper;
mod walltaker;
mod settings;

enum TrayMessage {
    Quit,
    Settings,
    Refresh,
    OpenCurrent,
    SaveCurrent,
}

// type Writer = Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let instance = single_instance::SingleInstance::new("walltaker-engine")?;
    if !instance.is_single() {
        popup("Walltaker Engine is already running.");
        return Ok(());
    }

    match app().await {
        Ok(()) => { },
        Err(e) => popup(&e
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
            .join("; ")),
    }

    Ok(())
}

// TODO: Expanding this to support separators would be great but this is already
// an improvement.
macro_rules! tray_items {
    ($tx:ident, $tray:ident, $($text:literal, $variant:expr;)+) => {
        $(
            let tx = $tx.clone();
            $tray.inner_mut().add_menu_item_with_id($text, move || {
                tx.send($variant).unwrap();
            })?;
        )*
    };
}

async fn app() -> anyhow::Result<()> {
    let settings = Arc::new(RwLock::new(settings::Settings::load_or_new()));
    println!("Loaded settings: {settings:?}");

    let bg_hwnds = unsafe { find_hwnds()? };
    println!("WorkerW HWNDS: {bg_hwnds:?}");
    let mut wallpaper = wallpaper::Wallpaper::new(&bg_hwnds)?;

    /* Set up websocket */
    let (ws_stream, _) = tokio_tungstenite::connect_async(
        "wss://walltaker.joi.how/cable").await?;
    let (write, mut read) = ws_stream.split();
    let write = Arc::new(Mutex::new(write));

    /* Set up system tray */
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut tray = TrayItem::new("Walltaker Engine",
                        IconSource::Resource("tray-icon"))?;

    tray_items![tx, tray,
        "Open Current", TrayMessage::OpenCurrent;
        "Save Current", TrayMessage::SaveCurrent;
        "Refresh",      TrayMessage::Refresh;
    ];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Settings", TrayMessage::Settings;];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Quit", TrayMessage::Quit;];

    if settings.read().await.notifications {
        Toast::new(Toast::POWERSHELL_APP_ID)
            .title("Walltaker Engine")
            .text1("Walltaker Engine is now running. You can find it in the system tray.")
            .show()
            .unwrap();
    }

    let mut current_url = None;

    /* Event loop */
    loop {
        /* Read Walltaker websocket messages */
        if let Some(message) = read.next().await {
            use walltaker::Incoming;

            let msg = message?.to_string();
            println!("Raw msg: {msg}");

            match serde_json::from_str(&msg)? {
                Incoming::Ping { .. } => { },

                Incoming::ConfirmSubscription { identifier } => {
                    println!("Successfully subscribed to {identifier}");
                },

                Incoming::Welcome => {
                    println!("Computer says hi");

                    let subscribed = &settings.read().await.subscribed;
                    for link in subscribed {
                        let msg = walltaker::subscribe_message(*link)?;
                        write.lock()
                            .await
                            .send(tungstenite::Message::text(msg))
                            .await?;
                        println!("Requested subscription to {link}");
                    }

                    /* If at least one ID is already set in the config file,
                     * immediately request the latest set wallpaper
                     * so we can change it immediately to start. */
                    if let Some(id) = settings.read().await.subscribed.last() {
                        let msg = walltaker::check_message(*id)?;
                        let write = Arc::clone(&write);

                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(4000)).await;
                            let _ = write.lock()
                                .await
                                .send(tungstenite::Message::text(&msg))
                                .await;
                        });
                    }
                },

                Incoming::Message { message, .. } => {
                    let out_path = save_file(&message.post_url).await?;
                    wallpaper.set(&out_path, settings.read().await.method)?;

                    current_url = Some(message.post_url);

                    let set_by = message.set_by
                        .unwrap_or_else(|| String::from("Anonymous"));
                    
                    if settings.read().await.notifications {
                        // A toast!
                        Toast::new(Toast::POWERSHELL_APP_ID)
                            .icon(std::path::Path::new(&out_path),
                                IconCrop::Circular,
                                "Walltaker Engine Icon")
                            .title("Walltaker Engine")
                            .text1(&format!(
                                "{} changed your wallpaper via link {}! ❤️",
                                set_by, message.id))
                            .show()
                            .unwrap();
                    }
                }
            }
        }

        if let Ok(message) = rx.try_recv() {
            match message {
                TrayMessage::Quit => {
                    wallpaper.reset()?;
                    std::process::exit(0);
                },
    
                TrayMessage::Refresh => {
                    let lock = settings.read().await;
                    let subscribed = &lock.subscribed;
                    let id = subscribed.choose(&mut rand::thread_rng());
    
                    if let Some(id) = id {
                        let msg = walltaker::check_message(*id)?;
                    
                        println!("Refreshing ID {id}");
                        write.lock()
                            .await
                            .send(tungstenite::Message::text(&msg))
                            .await?;
                    }
                },
    
                TrayMessage::SaveCurrent =>
                    save_current_wallpaper(&wallpaper)?,
                
                TrayMessage::OpenCurrent => {
                    if let Some(ref current_url) = current_url {
                        let md5 = current_url
                            .rsplit_once('/')
                            .unwrap()
                            .1
                            .rsplit_once('.')
                            .unwrap()
                            .0;
                        
                        let url = format!("https://e621.net/posts?md5={md5}");
                        unsafe { ShellExecuteW(
                            HWND(0),
                            PCWSTR::null(),
                            &HSTRING::from(url),
                            PCWSTR::null(),
                            PCWSTR::null(),
                            SW_SHOW)
                        };
                    }
                },
    
                TrayMessage::Settings => {
                    let c_settings = Arc::clone(&settings);
                    let write = Arc::clone(&write);
    
                    tokio::spawn(spawn_settings(c_settings, write));
                },
            }
        }

        /* Read winapi events */
        unsafe {
            let mut msg = MSG::default();
            if PeekMessageA(std::ptr::addr_of_mut!(msg), HWND(0), 0, 0, PM_REMOVE).0 == 1 {
                TranslateMessage(&msg);
                DispatchMessageA(&msg);
            }
        }

    }
}

fn save_current_wallpaper(wallpaper: &wallpaper::Wallpaper) -> anyhow::Result<()> {
    let current_file = wallpaper.current_media();
    
    if current_file.is_empty() {
        return Ok(());
    }

    let current_file = std::path::PathBuf::from(current_file);
    let ext = current_file.extension().unwrap().to_string_lossy();
    let placeholder = format!("wallpaper.{ext}");

    let mut out = Vec::<u16>::with_capacity(1024);
    out.append(&mut placeholder.encode_utf16().collect::<Vec<u16>>());
    out.push('\0' as u16);
    let ptr = PWSTR::from_raw(out.as_mut_ptr());

    let filter = HSTRING::from(format!("{} Files (*.{ext})\0*.{ext}\0",
        ext.to_uppercase()));
    let pcw = PCWSTR::from_raw(filter.as_ptr());

    let mut cfg = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFile: ptr,
        lpstrFilter: pcw,
        nMaxFile: 1024,
        Flags: OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };

    if (unsafe { GetSaveFileNameW(std::ptr::addr_of_mut!(cfg)).0 } == 1) {
        let save_to = unsafe { cfg.lpstrFile.to_string()? };
        println!("Out: {save_to}");

        let _ = std::fs::copy(current_file, save_to);
    } else {
        let err = unsafe {
            windows::Win32::UI::Controls::Dialogs::CommDlgExtendedError()
        };

        println!("Couldn't save file: {err:?}");
    }

    Ok(())
}

async fn spawn_settings(
    settings: Arc<RwLock<settings::Settings>>,
    write: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>,
) {
    let new = {
        let settings_reader = settings.read().await;
        let new = {
            let t: settings::Settings = (*settings_reader).clone();
            let ptr = std::ptr::addr_of!(t) as isize;
            unsafe {
                DialogBoxParamW(
                    HINSTANCE(0),
                    w!("IDD_MAIN"),
                    HWND(0),
                    Some(subscriptions_proc),
                    LPARAM(ptr))
            }
        };

        let new: &settings::Settings = unsafe { &*(new as *const _) };

        // This is theoretically really, really slow but these vecs will only
        // ever contain like, 5 elements tops. So it doesn't really matter.
        let added = new.subscribed.iter()
            .filter(|i| !settings_reader.subscribed.contains(i));
        let removed = settings_reader.subscribed.iter()
            .filter(|i| !new.subscribed.contains(i));

        // TODO: ability to unsubscribe
        _ = removed;

        for item in added {
            let msg = walltaker::subscribe_message(*item).unwrap();
            write.lock()
                .await
                .send(tungstenite::Message::text(msg))
                .await
                .unwrap();
        }

        new
    };

    let mut settings = settings.write().await;
    *settings = new.clone();
    settings.save().unwrap();

    println!("Config saved");
}

/// Saves the file at url to the disk in a cache directory, returning its path.
async fn save_file(url: &str) -> anyhow::Result<std::path::PathBuf> {
    let base_dirs = directories::BaseDirs::new().unwrap();

    let ext = std::path::Path::new(&url).extension().unwrap();
    let out_dir = base_dirs
        .cache_dir()
        .join("wallpaper-engine");
    let out_path = out_dir.join("out").with_extension(ext);

    let _ = std::fs::create_dir_all(out_dir);
    let mut out_file = std::fs::File::create(&out_path)?;
    let media_stream = reqwest::get(url).await?;
    let mut content = std::io::Cursor::new(media_stream.bytes().await?);

    std::io::copy(&mut content, &mut out_file)?;

    Ok(out_path)
}

unsafe extern "system" fn subscriptions_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    settings: LPARAM
) -> isize
{
    /* message, hiword, loword */
    match (message, (w_param.0 >> 16 & 0xffff) as u32, (w_param.0 & 0xFFFF)) { 
        (WM_INITDIALOG, _, _) => {
            let settings: &settings::Settings = &*(settings.0 as *const _);

            for i in &settings.subscribed {
                let s = std::ffi::CString::new(i.to_string()).unwrap();
                SendDlgItemMessageA(
                    hwnd,
                    1002,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(s.as_ptr() as isize));  
            }

            let target = match DESKTOP_WALLPAPER_POSITION(settings.method) {
                DWPOS_TILE    => 1006,
                DWPOS_FILL    => 1007,
                DWPOS_FIT     => 1008,
                DWPOS_STRETCH => 1009,
                _ => panic!("invalid settings.method"),
            };
            SendDlgItemMessageA(hwnd,
                target,
                BM_SETCHECK,
                WPARAM(BST_CHECKED.0 as usize),
                LPARAM(0));

            let placeholder = w!("Walltaker ID");
            SendDlgItemMessageA(hwnd,
                1000,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(placeholder.as_ptr() as isize));

            if settings.notifications {
                SendDlgItemMessageA(hwnd,
                    1010,
                    BM_SETCHECK,
                    WPARAM(BST_CHECKED.0 as usize),
                    LPARAM(0));
            }
        },

        /* IDC_ADD */
        (WM_COMMAND, _, 1003) => {
            let id = GetDlgItemInt(hwnd, 1000, None, false);

            if id != 0 {
                let s = std::ffi::CString::new(id.to_string()).unwrap();
                SendDlgItemMessageA(hwnd,
                    1002,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(s.as_ptr() as isize));
            }

            let _ = SetDlgItemTextA(hwnd, 1000, s!(""));
        },

        /* IDC_REMOVE */
        (WM_COMMAND, _, 1005) => {
            let selected = SendDlgItemMessageA(hwnd,
                1002,
                LB_GETCURSEL,
                WPARAM(0),
                LPARAM(0)).0;

            // Nothing selected
            if selected == LB_ERR as isize {
                return 1;
            }

            SendDlgItemMessageA(hwnd,
                1002,
                LB_DELETESTRING,
                WPARAM(selected as usize),
                LPARAM(0));
        },

        /* IDC_RADIO_* */
        (WM_COMMAND, _, x@1006..=1009) => {
            let method = match x {
                1006 => DWPOS_TILE,
                1007 => DWPOS_FILL,
                1008 => DWPOS_FIT,
                1009 => DWPOS_STRETCH,
                _ => unreachable!(),
            };

            let _ = wallpaper::Wallpaper::set_method(method.0);
        },

        /* IDC_NOTIFICATIONS */
        (WM_COMMAND, _, 1010) => { },

        (WM_CLOSE, _, _) => {
            let mut out: Box<settings::Settings> = Box::default();
            let item_count = SendDlgItemMessageA(
                hwnd,
                1002,
                LB_GETCOUNT,
                WPARAM(0),
                LPARAM(0)).0 as usize;
    
            for i in 0..item_count {
                let len = SendDlgItemMessageA(
                    hwnd,
                    1002,
                    LB_GETTEXTLEN,
                    WPARAM(i),
                    LPARAM(0)).0 as usize;
                let mut buf: Vec<u8> = Vec::with_capacity(len + 1);
                let ptr = buf.as_mut_ptr() as isize;
                
                let read_in = SendDlgItemMessageA(
                    hwnd,
                    1002,
                    LB_GETTEXT,
                    WPARAM(i),
                    LPARAM(ptr)).0 as usize;
                buf.set_len(read_in);
                let num = std::str::from_utf8(&buf).unwrap().parse().unwrap();

                out.subscribed.push(num);
            }

            for (button_id, value) in [1006 , 1007 , 1008 , 1009 ].iter().zip(
                                      [DWPOS_TILE, DWPOS_FILL,
                                       DWPOS_FIT, DWPOS_STRETCH])
            {
                if IsDlgButtonChecked(hwnd, *button_id) == 1 {
                    out.method = value.0;
                }
            }

            out.notifications = IsDlgButtonChecked(hwnd, 1010) == 1;
            
            EndDialog(hwnd, Box::leak(out) as *const _ as isize).unwrap();
        },

        _ => {
            return 0;
        },
    }
    
    1
}

fn popup(saying: &str) {
    unsafe {
        MessageBoxW(
            HWND(0),
            &HSTRING::from(saying),
            w!("Walltaker Engine Error"),
            MB_OK | MB_ICONERROR);
        }
}

unsafe fn find_hwnds() -> anyhow::Result<Vec<HWND>> {
    let progman = FindWindowA(s!("Progman"), PCSTR::null());
    anyhow::ensure!(progman.0 != 0, "No progman process");

    // The ability to send a window 0x052C is undocumented.
    SendMessageTimeoutA(
        progman,
        0x052C,
        WPARAM(0),
        LPARAM(0),
        SMTO_NORMAL,
        1000,
        None);
    
    let mut workerw_hwnd = HWND(0);
    EnumWindows(Some(enum_windows_proc),
        LPARAM(std::ptr::addr_of_mut!(workerw_hwnd) as isize))?;
    anyhow::ensure!(workerw_hwnd.0 != 0, "Couldn't find WorkerW");
    println!("WorkerW HWND: 0x{:x}", workerw_hwnd.0);

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

    // This pushes the workerw hwnd as the first element of the Vec so I don't
    // have to bother creating a struct etc. to move that extra information in.
    let mut hwnds = Vec::from(&[workerw_hwnd]);
    let ptr = std::ptr::addr_of_mut!(hwnds) as isize;
    EnumDisplayMonitors(HDC(0), None, Some(enum_monitors_proc), LPARAM(ptr));
    // The workerw hwnd is removed at the end :)
    hwnds.swap_remove(0);

    anyhow::ensure!(!hwnds.is_empty(), "Couldn't create HWNDs");

    Ok(hwnds)
}

unsafe extern "system" fn wndclass_proc(
    _: HWND,
    _: u32,
    _: WPARAM,
    _: LPARAM
) -> LRESULT
{
    LRESULT(1)
}

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

unsafe extern "system" fn enum_monitors_proc(
    _hmonitor: HMONITOR,
    _hdc: HDC,
    rect: *mut RECT,
    out: LPARAM,
) -> BOOL {
    let hwnds: &mut Vec<HWND> = &mut *(out.0 as *mut _);
    let workerw_hwnd = hwnds[0];

    let RECT { left: x, top: y, right, bottom } = *rect;
    let width = right - x;
    let height = bottom - y;

    println!("Creating window at {x}:{y} size {width}:{height}");

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