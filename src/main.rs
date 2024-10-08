#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::pedantic)]
#![allow(clippy::too_many_lines)]
use anyhow::{Result, Context};
use buttplug::{core::connector::new_json_ws_client_connector, client::{ButtplugClient, ScalarValueCommand}};
use futures_util::{stream::SplitSink, poll, StreamExt};
use log::info;
use serde::{Serialize, Deserialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, tungstenite::Message};
use tray_item::{IconSource, TrayItem};
use rand::prelude::*;
use tauri_winrt_notification::Toast;
use std::{
    fs::File,
    rc::Rc,
    path::{PathBuf, Path},
    time::Duration,
    task::Poll::Ready,
    io::Write,
};
use windows::{core::{PCWSTR, HSTRING}, Win32::UI::WindowsAndMessaging::MB_OK};
use windows::Win32::{
    UI::{WindowsAndMessaging, Shell::ShellExecuteW, HiDpi},
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
#[allow(clippy::struct_excessive_bools)]
struct Config {
    links: Vec<usize>,
    fit_mode: FitMode,
    notifications: bool,
    background_colour: String,
    run_on_boot: bool,
    debug_logs: bool,
    vibrate_for: u16,
    vibration_intensity: u8,
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
const BUTTPLUG_URL: &str = "ws://127.0.0.1:12345";
const WALLTAKER_WS_URL: &str = match option_env!("WALLTAKER_ENGINE_WS_URL") {
    Some(x) => x, None => "wss://walltaker.joi.how/cable",
};

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

macro_rules! popup {
    ($style:expr, $($t:expr),*) => {
        unsafe {
            WindowsAndMessaging::MessageBoxW(
                HWND(0),
                &HSTRING::from(format!($($t)*)),
                windows::core::w!("Walltaker Engine Error"),
                $style);
            }
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
        popup!(MB_OK, "Unfortunately, Walltaker Engine has crashed.\n{e}");
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

    let (tx, rx) = std::sync::mpsc::sync_channel(5);
    let mut tray = TrayItem::new("Walltaker Engine", IconSource::Resource("icon"))?;
    tray_items![tx, tray,
        "Open Current", TrayMessage::OpenCurrent;
        "Refresh",      TrayMessage::Refresh;
    ];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Settings", TrayMessage::Settings;];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Quit", TrayMessage::Quit;];

    let mut should_play_audio = true;
    let mut bg_webviews = Vec::new();
    for hwnd in hwnds {
        let webview = webview::WebView::create(Some(hwnd), should_play_audio, (100, 100))?;
        webview.navigate_html(BACKGROUND_HTML)?;
        set_bg_colour(&webview, &config.lock().await.background_colour)?;
        set_fit(&config.lock().await.fit_mode, &webview)?;
        
        bg_webviews.push(webview);
        should_play_audio = false;
    }

    let (settings, ui_rx) = webview::webviews::settings::create_settings_webview(&config)?;

    // We do a little hacking
    if config.lock().await.links.is_empty() {
        let tx = tx.clone();

        _ = Toast::new(Toast::POWERSHELL_APP_ID)
            .title("Walltaker Engine")
            .text1("Walltaker Engine is now running! Open me from the tray to set your link(s).")
            .on_activated(move || { _ = tx.send(TrayMessage::Settings); Ok(()) })
            .show();
    }

    let buttplug_connector = new_json_ws_client_connector(BUTTPLUG_URL);
    let buttplug = ButtplugClient::new("Walltaker Engine");

    if let Err(e) = buttplug.connect(buttplug_connector).await {
        log::info!("Couldn't connect to Intiface: {}", e);
    }

    let (ws_stream, _) = tokio_tungstenite::connect_async(WALLTAKER_WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();
    let mut current_url = None;
    loop {
        /* Read UI message */
        if let Ok(message) = ui_rx.try_recv() {
            use crate::webview::webviews::settings::UiMessage;

            match message {
                UiMessage::TestNotification =>
                    notification(&*config.lock().await, &buttplug, None, 0).await,
                UiMessage::SubscribeTo(link) => walltaker::subscribe_to(&mut write, link).await?,
                UiMessage::UnsubscribeFrom(link) =>
                    walltaker::unsubscribe_from(&mut write, link).await?,
                UiMessage::UpdateSettings => {
                    run_on_boot(config.lock().await.run_on_boot)?;
                    for view in &bg_webviews {
                        set_bg_colour(view, &config.lock().await.background_colour)?;
                        set_fit(&config.lock().await.fit_mode, view)?;
                    }
                },
            }
        }
        
        /* Read Walltaker websocket messages */
        if let Ready(Some(message)) = poll!(read.next()) {
            let new_link = read_walltaker_message(
                &*config.lock().await,
                &buttplug,
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

        tokio::time::sleep(Duration::from_millis(20)).await;
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
    buttplug: &ButtplugClient,
    writer: &mut Writer,
    bg_webviews: &[webview::WebView],
    message: &Message
) -> Result<Option<PathBuf>>
{
    use walltaker::Incoming;

    let msg = message.to_string();
    log::debug!("Recv: {msg}");

    let Ok(decm) = serde_json::from_str(&msg) else {
        log::warn!("message couldn't be decoded (skipping): {msg}");
        return Ok(None);
    };
    match decm {
        Incoming::Ping { .. } => { },
        Incoming::Disconnect { reason, reconnect } => {
            if !reconnect {
                popup!(MB_OK, "Walltaker told Walltaker Engine to disconnect: {reason}");
                std::process::exit(0);
            }

            log::warn!("Server issued disconect: {reason}");

        },

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

                // just awful
                let element =           if ext == "webm" { "video" } else { "image" };
                let the_other_element = if ext == "webm" { "image" } else { "video" };
                
                for view in bg_webviews {
                    view.eval(&format!("
                        document.getElementById('{element}').src = '{url}';
                        document.getElementById('{the_other_element}').src = '';
                    "))?;
                }

                notification(config, buttplug, message.set_by, message.id).await;

                return Ok(Some(url_path));
            }
        }
    }

    Ok(None)
}

async fn notification(
    config: &Config,
    buttplug: &ButtplugClient,
    set_by: Option<String>,
    id: usize
) {
    if config.notifications {
        let set_by = set_by
            .unwrap_or_else(|| String::from("Anonymous"));

        let notif = format!("{set_by} changed your wallpaper via link {id}! ❤️");

        _ = Toast::new(Toast::POWERSHELL_APP_ID)
            .title("Walltaker Engine")
            .text1(&notif)
            .show();
    }

    if config.vibrate_for != 0 {
        vibrate(config, buttplug).await;
    }
}

async fn vibrate(config: &Config, buttplug: &ButtplugClient) {
    let intensity = f64::from(config.vibration_intensity) / 100.;
    let length = u64::from(config.vibrate_for);
    for device in buttplug.devices() {
        log::info!("Vibing {} for {}/{}",
            device.name(),
            intensity, length,
        );
        if let Err(e) = device.vibrate(&ScalarValueCommand::ScalarValue(intensity)).await {
            log::warn!("Couldn't make connected device vibrate: {:?}", e);
        }

        // is this the best way to do this?
        tokio::time::sleep(std::time::Duration::from_millis(length)).await;
        let _ = device.stop().await;
    }
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
            WindowsAndMessaging::SW_SHOW,
        )
    };
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
