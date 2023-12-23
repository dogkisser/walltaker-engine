#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::pedantic, clippy::style)]
// Clippy clearly hasn't met WinAPI
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
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
use anyhow::anyhow;
use futures_util::{StreamExt, SinkExt, stream::SplitSink};
use windows::{
    core::{s, w, PCSTR, HSTRING},
    Win32::{
        Foundation::{WPARAM, LPARAM, HWND, BOOL, HINSTANCE},
        UI::{
            WindowsAndMessaging::{
                LB_GETCOUNT, LB_GETTEXTLEN, LB_GETTEXT, BM_SETCHECK,
                SMTO_NORMAL, MB_OK, MB_ICONERROR, WM_INITDIALOG, WM_COMMAND,
                LB_ADDSTRING, LB_DELETESTRING, LB_GETCURSEL, LB_ERR, WM_CLOSE,
                FindWindowA, SendMessageTimeoutA, EnumWindows, FindWindowExA,
                MessageBoxW, DialogBoxParamW, GetDlgItemInt, SetDlgItemTextA,
                SendDlgItemMessageA, SendMessageA, GetDlgItem, EndDialog,
            },
            Controls::{IsDlgButtonChecked, BST_CHECKED},
            Shell::{
                DWPOS_CENTER, DWPOS_TILE, DWPOS_STRETCH, DWPOS_FILL,
                DESKTOP_WALLPAPER_POSITION
            }
        },
    },
};

mod wallpaper;
mod walltaker;
mod settings;

enum TrayMessage {
    Quit,
    Settings,
    Refresh,
}

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

async fn app() -> anyhow::Result<()> {
    let settings = Arc::new(RwLock::new(settings::Settings::load_or_new()));
    println!("Loaded settings: {settings:?}");

    let bg_hwnd = unsafe { find_hwnd()? }
        .ok_or_else(|| anyhow!("Couldn't find workerW HWND."))?;
    println!("WorkerW HWND: 0x{:X}", bg_hwnd.0);
    let mut wallpaper = wallpaper::Wallpaper::new((bg_hwnd.0 as *mut usize)
        .cast())?;

    /* Set up websocket */
    let (ws_stream, _) = tokio_tungstenite::connect_async(
        "wss://walltaker.joi.how/cable").await?;
    let (write, mut read) = ws_stream.split();
    let write = Arc::new(Mutex::new(write));

    /* Set up system tray */
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut tray = TrayItem::new("Walltaker Engine",
                        IconSource::Resource("tray-icon"))?;

    let tx_ = tx.clone();
    tray.inner_mut().add_menu_item_with_id("Refresh", move || {
        tx_.send(TrayMessage::Refresh).unwrap();
    })?;
    let tx_ = tx.clone();
    tray.inner_mut().add_menu_item_with_id("Settings", move || {
        tx_.send(TrayMessage::Settings).unwrap();
    })?;
    tray.inner_mut().add_separator()?;
    tray.add_menu_item("Quit", move || {
        tx.send(TrayMessage::Quit).unwrap();
    })?;

    Toast::new(Toast::POWERSHELL_APP_ID)
        .title("Walltaker Engine")
        .text1("Walltaker Engine is now running. You can find it in the system tray.")
        .show()
        .unwrap();

    /* Event loop */
    loop {
        /* Read Walltaker websocket messages */
        if let Some(message) = read.next().await {
            use walltaker::Incoming;

            let msg = message?.to_string();
            println!("Raw msg: {msg}");

            match serde_json::from_str(&msg)? {
                Incoming::ConfirmSubscription { identifier } => {
                    println!("Successfully subscribed to {identifier}");
                },

                Incoming::Ping { .. } => {
                    println!("Keepalive"); 
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
                        println!("Immediately setting wallpaper with ID {id}");
                        write.lock()
                            .await
                            .send(tungstenite::Message::text(msg))
                            .await?;
                    }
                },

                Incoming::Message { message, .. } => {
                    let out_path = save_file(message.post_url).await?;
                    wallpaper.set(&out_path, settings.read().await.method)?;

                    // A toast!
                    let set_by = message.set_by
                        .unwrap_or_else(|| String::from("Anonymous"));
                    
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

        /* Read system tray events */
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
                    
                        write.lock()
                            .await
                            .send(tungstenite::Message::text(&msg))
                            .await?;
                    }
                },

                TrayMessage::Settings => {
                    let c_settings = Arc::clone(&settings);
                    let write = Arc::clone(&write);

                    tokio::spawn(spawn_settings(c_settings, write));
                },
            }
        }
    }
}

async fn spawn_settings(
    settings: Arc<RwLock<settings::Settings>>,
    write: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>,
)
{
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
        let added = new.subscribed.iter().filter(|i| !settings_reader.subscribed.contains(i));
        let removed = settings_reader.subscribed.iter().filter(|i| !new.subscribed.contains(i));

        // TODO: ability to unsubscribe
        _ = removed;

        for item in added {
            let msg = walltaker::subscribe_message(*item).unwrap();
            write.lock().await.send(tungstenite::Message::text(msg)).await.unwrap();
        }

        new
    };

    let mut settings = settings.write().await;
    *settings = new.clone();
    settings.save().unwrap();

    println!("Config saved");
}

async fn save_file(url: String) -> anyhow::Result<String> {
    let base_dirs = directories::BaseDirs::new().unwrap();

    let ext = std::path::Path::new(&url).extension().unwrap();
    let out_path = base_dirs.cache_dir().join("out").with_extension(ext);

    let mut out_file = std::fs::File::create(&out_path)?;
    let media_stream = reqwest::get(&url).await?;
    let mut content = std::io::Cursor::new(media_stream.bytes().await?);

    std::io::copy(&mut content, &mut out_file)?;

    Ok(out_path.to_string_lossy().to_string())
}

unsafe extern "system" fn subscriptions_proc(hwnd: HWND, message: u32, w_param: WPARAM, settings: LPARAM) -> isize {
    match (message, (w_param.0 >> 16 & 0xffff) as u32, (w_param.0 & 0xFFFF)) /* HIWORD, LOWORD */ { 
        (WM_INITDIALOG, _, _) => {
            let settings: &settings::Settings = &*(settings.0 as *const _);

            for i in &settings.subscribed {
                let s = std::ffi::CString::new(i.to_string()).unwrap();
                SendDlgItemMessageA(hwnd, 1002, LB_ADDSTRING, WPARAM(0), LPARAM(s.as_ptr() as isize));  
            }

            let target = match DESKTOP_WALLPAPER_POSITION(settings.method) {
                DWPOS_TILE    => 1006,
                DWPOS_FILL    => 1007,
                DWPOS_CENTER  => 1008,
                DWPOS_STRETCH => 1009,
                _ => panic!("invalid settings.method"),
            };

            SendDlgItemMessageA(hwnd, target, BM_SETCHECK, WPARAM(BST_CHECKED.0 as usize), LPARAM(0));
        },

        /* IDC_ADD */
        (WM_COMMAND, _, 1003) => {
            let id = GetDlgItemInt(hwnd, 1000, None, false);

            if id != 0 {
                let s = std::ffi::CString::new(id.to_string()).unwrap();
                SendDlgItemMessageA(hwnd, 1002, LB_ADDSTRING, WPARAM(0), LPARAM(s.as_ptr() as isize));
            }

            let _ = SetDlgItemTextA(hwnd, 1000, s!(""));
        },

        /* IDC_REMOVE */
        (WM_COMMAND, _, 1005) => {
            let dlg = GetDlgItem(hwnd, 1002);
            let selected = SendMessageA(dlg, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
            // Nothing selected
            if selected == LB_ERR as isize {
                return 1;
            }

            SendMessageA(dlg, LB_DELETESTRING, WPARAM(selected as usize), LPARAM(0));
        },

        /* IDC_RADIO_* */
        x@(WM_COMMAND, _, 1006..=1009) => {
            let method = match x.2 {
                1006 => DWPOS_TILE,
                1007 => DWPOS_FILL,
                1008 => DWPOS_CENTER,
                1009 => DWPOS_STRETCH,
                _ => unreachable!(),
            };

            let _ = wallpaper::Wallpaper::set_method(method.0);
        },

        (WM_CLOSE, _, _) => {
            let item_count = SendDlgItemMessageA(hwnd, 1002, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
            let mut out: Box<settings::Settings> = Box::default();
            
            // Terrible, horrible, not safe, very dangerous code
            for i in 0..item_count {
                let len = SendDlgItemMessageA(hwnd, 1002, LB_GETTEXTLEN, WPARAM(i), LPARAM(0)).0 as usize;
                let mut buf: Vec<u8> = Vec::with_capacity(len + 1);
                let ptr = buf.as_mut_ptr() as isize;
                
                let read_in = SendDlgItemMessageA(hwnd, 1002, LB_GETTEXT, WPARAM(i), LPARAM(ptr)).0 as usize;
                buf.set_len(read_in);
                let num = std::str::from_utf8(&buf).unwrap().parse().unwrap();

                out.subscribed.push(num);
            }

            for (button_id, value) in [1006      , 1007      , 1008        , 1009         ].iter().zip(
                                      [DWPOS_TILE, DWPOS_FILL, DWPOS_CENTER, DWPOS_STRETCH])
            {
                if IsDlgButtonChecked(hwnd, *button_id) == 1 {
                    out.method = value.0;
                }
            }
            
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

unsafe fn find_hwnd() -> anyhow::Result<Option<HWND>> {
    let progman = FindWindowA(s!("Progman"), PCSTR::null());
    
    // The ability to send a window 0x052C is undocumented.
    SendMessageTimeoutA(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, None);

    let mut hwnd = HWND(0);
    EnumWindows(Some(enum_proc), LPARAM(std::ptr::addr_of_mut!(hwnd) as isize))?;

    match hwnd.0 {
        0 => Ok(None),
        _ => Ok(Some(hwnd)),
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, out: LPARAM) -> BOOL {
    let wind = FindWindowExA(hwnd, HWND(0), s!("SHELLDLL_DefView"), PCSTR::null());

    if wind.0 != 0 {
        let out: &mut isize = &mut *(out.0 as *mut isize);
        let target = FindWindowExA(HWND(0), hwnd, s!("WorkerW"), PCSTR::null()).0;

        *out = target;
    }

    true.into()
}