#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::pedantic)]
#![allow(clippy::too_many_lines)]
use anyhow::{Result, Context};
use futures_util::{stream::SplitSink, poll, StreamExt};
use log::info;
use serde::{Serialize, Deserialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, tungstenite::Message};
use tray_item::{IconSource, TrayItem};
use rand::prelude::*;
use winrt_notification::Toast;
use std::{
    fs::File,
    rc::Rc,
    path::{PathBuf, Path},
    time::Duration,
    task::Poll::Ready,
    io::Write,
};
use windows::core::{PCWSTR, HSTRING};
use windows::Win32::{
    UI::{WindowsAndMessaging::SW_SHOW, Shell::ShellExecuteW, HiDpi},
    Foundation::HWND,
    System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
};
use simplelog::{
    CombinedLogger, LevelFilter, ColorChoice, TermLogger,
    WriteLogger, TerminalMode
};

mod hwnd;
mod webview;
mod walltaker;

type Writer = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    links: Vec<usize>,
    fit_mode: FitMode,
    notifications: bool,
    background_colour: String,
    run_on_boot: bool,
    debug_logs: bool,
    version: String,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
enum FitMode {
    Stretch,
    #[default]
    Fit,
    Fill,
}

enum TrayMessage {
    Quit,
    Settings,
    Refresh,
    OpenCurrent,
}

const BACKGROUND_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/background.html.min"));

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
async fn main() {
    let instance = single_instance::SingleInstance::new("walltaker-engine").unwrap();
    if !instance.is_single() {
        return;
    }

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).unwrap();
        HiDpi::SetProcessDpiAwareness(HiDpi::PROCESS_PER_MONITOR_DPI_AWARE).unwrap();
    }

    if let Err(e) = _main().await {
        log::error!("Crash: {e:#?}");
    }
}

async fn _main() -> Result<()> {
    let config_path = directories::BaseDirs::new()
        .unwrap()
        .config_dir()
        .join("walltaker-engine/walltaker-engine.json");
    let config = load_config(&config_path)?;
    let config: Rc<tokio::sync::Mutex<Config>> = tokio::sync::Mutex::new(config).into();

    init_logging(config.lock().await.debug_logs)?;

    info!("Parsed config: {config:#?}");
    
    let hwnds = unsafe { hwnd::find_hwnds() }?;

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut tray = TrayItem::new("Walltaker Engine", IconSource::Resource("icon"))?;
    tray_items![tx, tray,
        "Open Current", TrayMessage::OpenCurrent;
        "Refresh",      TrayMessage::Refresh;
    ];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Settings", TrayMessage::Settings;];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Quit", TrayMessage::Quit;];

    let mut bg_webviews = Vec::new();
    for hwnd in hwnds {
        let webview = webview::WebView::create(Some(hwnd), true, (100, 100))?;
        webview.navigate_html(BACKGROUND_HTML)?;
        set_bg_colour(&webview, &config.lock().await.background_colour)?;
        set_fit(&config.lock().await.fit_mode, &webview)?;
        
        bg_webviews.push(webview);
    }

    let (settings, ui_rx) = webview::webviews::settings::create_settings_webview(&config)?;

    let (ws_stream, _) = tokio_tungstenite::connect_async("wss://walltaker.joi.how/cable").await?;
    let (mut write, mut read) = ws_stream.split();

    let mut current_url = None;
    loop {
        /* Read UI message */
        if let Ok(message) = ui_rx.try_recv() {
            use crate::webview::webviews::settings::UiMessage;

            match message {
                UiMessage::SubscribeTo(link) => walltaker::subscribe_to(&mut write, link).await?,
                UiMessage::UpdateRunOnBoot => run_on_boot(config.lock().await.run_on_boot)?,
                UiMessage::UpdateBackgroundColour => for view in &bg_webviews {
                    set_bg_colour(view, &config.lock().await.background_colour)?;
                },
                UiMessage::UpdateFit => for view in &bg_webviews {
                    set_fit(&config.lock().await.fit_mode, view)?;
                },
            }
        }
        
        /* Read Walltaker websocket messages */
        if let Ready(Some(message)) = poll!(read.next()) {
            let new_link = read_walltaker_message(
                &*config.lock().await,
                &mut write,
                &bg_webviews,
                &message?
            ).await?;

            if new_link.is_some() {
                current_url = new_link;
            }
        }

        /* Read tray messages */
        if let Ok(message) = rx.try_recv() {
            match message {
                TrayMessage::Settings => settings.show(),

                TrayMessage::Quit => {
                    let mut cfg = File::create(config_path)?;
                    write!(cfg, "{}", serde_json::to_string(&*config.lock().await)?)?;
                    log::info!("settings saved");
                    std::process::exit(0);
                },
        
                TrayMessage::Refresh => {
                    if let Some(link) = config.lock().await.links.choose(&mut rand::thread_rng()) {
                        walltaker::check(&mut write, *link).await?;
                    }
                },
                
                TrayMessage::OpenCurrent => {
                    if let Some(ref current_url) = current_url {
                        let md5 = current_url
                            .file_stem()
                            .ok_or_else(|| anyhow::anyhow!("current_url has no stem!"))?
                            .to_string_lossy();
                        
                        let url = format!("https://e621.net/posts?md5={md5}");
                        open(&url);
                    }
                },
            }
        }

        settings.handle_messages()?;
        for view in &bg_webviews {
            view.handle_messages()?;
        }
    }
}

fn load_config<P: AsRef<Path>>(from: P) -> Result<Config> {
    let mut config: Config = if let Ok(file) = File::open(&from) {
        serde_json::from_reader(file)?
    } else {
        // Default configuration
        Config {
            notifications: true,
            debug_logs: true,
            background_colour: String::from("#202640"),
            ..Default::default()
        }
    };
    config.version = format!("v{}", env!("CARGO_PKG_VERSION"));

    Ok(config)
}

async fn read_walltaker_message(
    config: &Config,
    writer: &mut Writer,
    bg_webviews: &[webview::WebView],
    message: &Message
) -> Result<Option<PathBuf>>
{
    use walltaker::Incoming;

    let msg = message.to_string();
    match serde_json::from_str(&msg).context(msg)? {
        Incoming::Ping { .. } => { },

        Incoming::Welcome => {
            info!("Connected to Walltaker");

            for link in &config.links {
                walltaker::subscribe_to(writer, *link).await?;
            }

            if let Some(link) = config.links.choose(&mut rand::thread_rng()) {
                // Not the best but it works and whatnot
                tokio::time::sleep(Duration::from_millis(1000)).await;
                info!("Checking link {link} for initial wallpaper");
                walltaker::check(writer, *link).await?;
            }
        },

        Incoming::ConfirmSubscription { identifier } => {
            info!("Successfully subscribed to {identifier}");
        },

        // Wallpaper change
        Incoming::Message { message, .. } => {
            if let Some(url) = message.post_url {
                info!("Changing wallpaper to {url}");
                let url_path = PathBuf::from(&url);
                let ext = url_path.extension().unwrap().to_string_lossy().to_lowercase();

                let element = 
                    if ext == "webm" {
                        "video"
                    } else {
                        "image"
                    };

                for view in bg_webviews {
                    view.eval(&format!("
                        document.getElementById('{element}').src = '{url}';
                    "))?;
                }

                if config.notifications {
                    let set_by = message.set_by
                        .unwrap_or_else(|| String::from("Anonymous"));

                    let notif = format!("{} changed your wallpaper via link {}! ❤️",
                        set_by, message.id);

                    notification(&notif);
                }

                return Ok(Some(url_path));
            }
        }
    }

    Ok(None)
}

fn init_logging(write: bool) -> Result<()> {
    if write {
        CombinedLogger::init(vec![
            TermLogger::new(LevelFilter::Debug, simplelog::Config::default(),
                TerminalMode::Mixed, ColorChoice::Auto),
            WriteLogger::new(LevelFilter::Debug, simplelog::Config::default(),
                std::fs::File::create("walltaker-engine.log")?),
        ])?;
    } else {
        TermLogger::init(LevelFilter::Debug, simplelog::Config::default(),
            TerminalMode::Mixed, ColorChoice::Auto)?;
    }

    Ok(())
}
 
fn set_fit(mode: &FitMode, to: &webview::WebView) -> webview::Result<()> {
    to.eval(match mode {
        FitMode::Stretch => "setStretch();",
        FitMode::Fill => "setFill();",
        FitMode::Fit => "setFit();",
    })?;

    Ok(())
}

fn set_bg_colour(view: &webview::WebView, color: &str) -> Result<()> {
    view.eval(&format!("document.body.style.backgroundColor = '{color}';"))?;
    
    Ok(())
}

fn open(url: &str) {
    unsafe {
        ShellExecuteW(
            HWND(0),
            PCWSTR::null(),
            &HSTRING::from(url),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOW,
        )
    };
}

fn notification(text: &str) {
    _ = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("Walltaker Engine")
        .text1(text)
        .show();
}

fn run_on_boot(should: bool) -> Result<()> {
    let me = std::env::current_exe()?;
    let out = directories::BaseDirs::new()
        .unwrap()
        .data_dir()
        .join("Microsoft/Windows/Start Menu/Programs/Startup/walltaker-engine.exe");

    if should {
        std::fs::copy(me, out)?;
    } else {
        _ = std::fs::remove_file(out);
    }

    Ok(())
}