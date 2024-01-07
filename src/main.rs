#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::pedantic, clippy::style)]
// Clippy clearly hasn't met WinAPI
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::wildcard_imports,
)]
use std::{task::Poll::Ready, sync::Arc, path::{Path, PathBuf}};
use anyhow::anyhow;
use tokio::{sync::{RwLock, Mutex}, net::TcpStream};
use rand::seq::SliceRandom;
use tokio_tungstenite::{
    tungstenite::Message,
    WebSocketStream, MaybeTlsStream,
};
use tray_item::{IconSource, TrayItem};
use log::{info, warn, debug};
use futures_util::{StreamExt, stream::SplitSink, poll};
use simplelog::{
    CombinedLogger, LevelFilter, ColorChoice, TermLogger,
    WriteLogger, Config, TerminalMode
};
use windows::{
    core::{HSTRING, PWSTR, PCWSTR},
    Win32::{
        Foundation::*,
        UI::{
            WindowsAndMessaging::*,
            Controls::Dialogs::*,
        },
    },
};

mod wallpaper;
mod walltaker;
mod gui;
mod settings;
mod hwnd;

type Writer = Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;

enum TrayMessage {
    Quit,
    Settings,
    Refresh,
    OpenCurrent,
    SaveCurrent,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let instance = single_instance::SingleInstance::new("walltaker-engine")?;
    if !instance.is_single() {
        gui::popup("Walltaker Engine is already running.");
        return Ok(());
    }

    CombinedLogger::init(vec![
        TermLogger::new(LevelFilter::Debug, Config::default(),
            TerminalMode::Mixed, ColorChoice::Auto),
        WriteLogger::new(LevelFilter::Debug, Config::default(),
            std::fs::File::create("walltaker-engine.log")?),
    ])?;

    match app().await {
        Ok(()) => { },
        Err(e) => gui::popup(&e
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
            .join("; ")),
    }

    Ok(())
}

async fn app() -> anyhow::Result<()> {
    let settings = Arc::new(RwLock::new(settings::Settings::load_or_new()));
    info!("Loaded settings: {settings:#?}");

    let bg_hwnds = unsafe { hwnd::find_hwnds()? };
    info!("WorkerW HWNDs: {bg_hwnds:#X?}");
    let wallpaper = Arc::new(Mutex::new(wallpaper::Wallpaper::new(&bg_hwnds)?));

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
        gui::notification(
            "Walltaker Engine is now running. You can find it in the system tray.",
            None,
        );
    }

    let mut current_url = None;

    /* Event loop */
    loop {
        /* Read Walltaker websocket messages */
        if let Ready(Some(message)) = poll!(read.next()) {
            use walltaker::Incoming;

            let msg = message?.to_string();
            match serde_json::from_str(&msg)? {
                Incoming::Ping { .. } => { },

                Incoming::ConfirmSubscription { identifier } => {
                    info!("Successfully subscribed to {identifier}");
                },

                Incoming::Welcome => {
                    info!("Connected to Walltaker");

                    let subscribed = &settings.read().await.subscribed;
                    for id in subscribed {
                        info!("Requesting subscription to {id}");
                        walltaker::subscribe_to(&write, *id).await?;
                    }

                    /* If at least one ID is already set in the config file,
                     * immediately request the latest set wallpaper
                     * so we can change it immediately to start. */
                    if let Some(id) = settings.read().await.subscribed.last() {
                        let id = *id;
                        let write = Arc::clone(&write);
                        info!("Refreshing {id} for initial wallpaper");

                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                        walltaker::subscribe_to(&write, id).await.unwrap();
                        walltaker::check(&write, id).await.unwrap();
                    }
                },

                Incoming::Message { message, .. } => {
                    current_url = Some(PathBuf::from(&message.post_url));
                    let settings = settings.read().await;

                    let out_path = save_file(&message.post_url).await?;
                    wallpaper.lock().await.set(&out_path, settings.method)?;
                    
                    if settings.notifications {
                        let set_by = message.set_by
                            .unwrap_or_else(|| String::from("Anonymous"));

                        let text = format!("{} changed your wallpaper via link {}! ❤️",
                            set_by, message.id);
                        let icon = std::path::Path::new(&out_path);

                        gui::notification(&text, Some(icon));
                    }
                }
            }
        }

        /* Read tray messages */
        if let Ok(message) = rx.try_recv() {
            match message {
                TrayMessage::Quit => {
                    wallpaper.lock().await.reset()?;
                    std::process::exit(0);
                },
    
                TrayMessage::Refresh => {
                    let lock = settings.read().await;
                    let subscribed = &lock.subscribed;
                    let id = subscribed.choose(&mut rand::thread_rng());
    
                    if let Some(id) = id {
                        walltaker::check(&write, *id).await?;
                    }
                },
    
                TrayMessage::SaveCurrent => {
                    let lock = wallpaper.lock().await;
                    let path = lock.current_media();
                    if path.is_empty() || current_url.is_none() {
                        continue;
                    }

                    let url = current_url.clone().unwrap();

                    save_current_wallpaper(path, &url)?;
                },
                
                TrayMessage::OpenCurrent => {
                    if let Some(ref current_url) = current_url {
                        let md5 = current_url
                            .file_stem()
                            .ok_or_else(|| anyhow!("current_url has no stem!"))?
                            .to_string_lossy();
                        
                        let url = format!("https://e621.net/posts?md5={md5}");
                        gui::open(&url);
                    }
                },
    
                TrayMessage::Settings => {
                    let c_settings = Arc::clone(&settings);
                    let write = Arc::clone(&write);
                    
                    let wallpaper = Arc::clone(&wallpaper);
                    tokio::spawn(gui::spawn_settings(c_settings, wallpaper, write));
                },
            }
        }

        /* Read WinAPI messages */
        unsafe {
            let mut msg = MSG::default();
            let addr = std::ptr::addr_of_mut!(msg);

            if PeekMessageA(addr, HWND(0), 0, 0, PM_REMOVE).0 == 1 {
                TranslateMessage(&msg);
                DispatchMessageA(&msg);
            }
        }
    }
}

fn save_current_wallpaper(path: &str, name: &Path) -> anyhow::Result<()> {
    // TODO: Hack. Just pass name as a pathbuf
    let ext = name
        .extension()
        .ok_or_else(|| anyhow::anyhow!("No extension in passed file??"))?
        .to_string_lossy();

    // The pre-filled placeholder filename.
    let mut placeholder = Vec::<u16>::with_capacity(2048);
    placeholder.append(&mut name.to_string_lossy().encode_utf16().collect::<Vec<u16>>());
    placeholder.push('\0' as u16);
    let ptr = PWSTR::from_raw(placeholder.as_mut_ptr());

    let filter = HSTRING::from(format!("{} Files (*.{ext})\0*.{ext}\0",
        ext.to_uppercase()));
    let filter = PCWSTR::from_raw(filter.as_ptr());

    let mut cfg = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFile: ptr,
        lpstrFilter: filter,
        nMaxFile: 1024,
        Flags: OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };

    if (unsafe { GetSaveFileNameW(std::ptr::addr_of_mut!(cfg)).0 } == 1) {
        let save_to = unsafe { cfg.lpstrFile.to_string()? };
        debug!("Saving current wallpaper to: {save_to}");

        // Result ignored; no need to crash and burn if this fails.
        _ = std::fs::copy(path, save_to);
    } else {
        let err = unsafe {
            windows::Win32::UI::Controls::Dialogs::CommDlgExtendedError()
        };

        warn!("Couldn't save file: {err:?}");
    }

    Ok(())
}

/// Saves the file at url to the disk in a cache directory, returning its path.
async fn save_file(url: &str) -> anyhow::Result<std::path::PathBuf> {
    let base_dirs = directories::BaseDirs::new().unwrap();
    let out_dir = base_dirs.cache_dir().join("wallpaper-engine");

    let ext = Path::new(url)
        .extension()
        .ok_or_else(|| anyhow!("URL has no extension part"))?;

    let out_path = out_dir.join("out").with_extension(ext);

    std::fs::create_dir_all(out_dir)?;
    let mut out_file = std::fs::File::create(&out_path)?;
    let media_stream = reqwest::get(url).await?;
    let mut content = std::io::Cursor::new(media_stream.bytes().await?);

    std::io::copy(&mut content, &mut out_file)?;

    Ok(out_path)
}