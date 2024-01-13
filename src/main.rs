#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::fs::File;
use std::rc::Rc;
use std::sync::Mutex;
use std::path::PathBuf;
use std::time::Duration;
use std::task::Poll::Ready;
use std::io::Write;
use anyhow::{Result, Context};
use futures_util::{stream::SplitSink, poll, StreamExt};
use log::info;
use serde::{Serialize, Deserialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, tungstenite::Message};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use tray_item::{IconSource, TrayItem};
use simplelog::{
    CombinedLogger, LevelFilter, ColorChoice, TermLogger,
    WriteLogger, TerminalMode
};
use rand::prelude::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
use windows::core::{PCWSTR, HSTRING};
use winrt_notification::Toast;

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
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
enum FitMode {
    Stretch,
    #[default]
    Fit,
    Fill,
}

const SETTINGS_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/settings.html.min"));
const BACKGROUND_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/background.html.min"));

enum TrayMessage {
    Quit,
    Settings,
    Refresh,
    OpenCurrent,
}

// TODO: Maybe just make all of this just one "Update" message
enum UiMessage {
    UpdateFit,
    UpdateRunOnBoot,
    UpdateBackgroundColour,
    SubscribeTo(usize),
}

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
async fn main() -> Result<()> {
    let instance = single_instance::SingleInstance::new("walltaker-engine")?;
    if !instance.is_single() {
        return Ok(());
    }

    let config_path = directories::BaseDirs::new()
        .unwrap()
        .config_dir()
        .join("walltaker-engine/walltaker-engine.json");

    let config: Config = if let Ok(file) = File::open(&config_path) {
        serde_json::from_reader(file)?
    } else {
        // Default configuration
        let mut cfg = Config::default();
        cfg.notifications = true;
        cfg.debug_logs = true;
        cfg.background_colour = String::from("#000000");
        cfg
    };
    let config: Rc<Mutex<Config>> = Mutex::new(config).into();

    if config.lock().unwrap().debug_logs {
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

    info!("Parsed config: {config:#?}");

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)?;
    }
    let hwnds = unsafe { hwnd::find_hwnds() }?;

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut tray = TrayItem::new("Walltaker Engine",
                        IconSource::Resource("tray-icon"))?;
    tray_items![tx, tray,
        "Open Current", TrayMessage::OpenCurrent;
        "Refresh",      TrayMessage::Refresh;
    ];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Settings", TrayMessage::Settings;];
    tray.inner_mut().add_separator()?;
    tray_items![tx, tray, "Quit", TrayMessage::Quit;];

    let (ws_stream, _) = tokio_tungstenite::connect_async(
        "wss://walltaker.joi.how/cable").await?;
    let (mut write, mut read) = ws_stream.split();

    let mut bg_webviews = Vec::new();
    for hwnd in hwnds {
        let webview = Rc::new(webview::WebView::create(Some(hwnd), true, (100, 100))?);
        webview.navigate_html(BACKGROUND_HTML)?;
        set_bg_colour(&webview, &config.lock().unwrap().background_colour)?;
        set_fit(&config.lock().unwrap().fit_mode, &webview)?;
        
        bg_webviews.push(webview);
    }

    let _config = Rc::clone(&config);

    let (ui_tx, ui_rx) = std::sync::mpsc::sync_channel(50);
    let settings = webview::WebView::create(None, true, (420, 420))?;
    let _config = Rc::clone(&config);
    let _ui_tx = ui_tx.clone();
    settings.bind("saveSettings", move |request| {
        if let Some(new_cfg) = request.get(0) {
            let new_settings: Config = serde_json::from_value(new_cfg.clone())?;
            let mut config = _config.lock().unwrap();
        
            let _ = _ui_tx.send(UiMessage::UpdateFit);

            // This is theoretically really, really slow but these vecs will only
            // ever contain like, 5 elements tops. So it doesn't really matter.
            let added = new_settings.links.iter()
                .filter(|i| !config.links.contains(i));
            let _removed = config.links.iter()
                .filter(|i| !new_settings.links.contains(i));
            // TODO: support live unsubscribing

            for link in added {
                let _ = _ui_tx.send(UiMessage::SubscribeTo(*link));
            }

            let _ = _ui_tx.send(UiMessage::UpdateBackgroundColour);
            let _ = _ui_tx.send(UiMessage::UpdateRunOnBoot);

            log::info!("Settings updated {new_settings:#?}");

            *config = new_settings;
            return Ok(serde_json::Value::String(String::from("ok")));
        }

        Err(webview::Error::WebView2Error(
            webview2_com::Error::CallbackError(String::from("Called wrong. wtf?"))))
    })?;
    let _config = Rc::clone(&config);
    settings.bind("loadSettings", move |_request| {
        let cfg = &*_config;
        Ok(serde_json::to_value(cfg)?)
    })?;
    settings.resize(420, 420)?;
    settings.navigate_html(SETTINGS_HTML)?;
    settings.show();

    let mut current_url = None;
    loop {
        /* Read UI message */
        if let Ok(message) = ui_rx.try_recv() {
            match message {
                UiMessage::SubscribeTo(link) => walltaker::subscribe_to(&mut write, link).await?,
                UiMessage::UpdateRunOnBoot => run_on_boot(config.lock().unwrap().run_on_boot)?,
                UiMessage::UpdateBackgroundColour => for view in &bg_webviews {
                    set_bg_colour(&view, &config.lock().unwrap().background_colour)?;
                },
                UiMessage::UpdateFit => for view in &bg_webviews {
                    set_fit(&config.lock().unwrap().fit_mode, &view)?;
                },
            }
        }
        
        /* Read Walltaker websocket messages */
        if let Ready(Some(message)) = poll!(read.next()) {
            use walltaker::Incoming;

            let msg = message?.to_string();
            match serde_json::from_str(&msg).context(msg)? {
                Incoming::Ping { .. } => { },

                Incoming::Welcome => {
                    info!("Connected to Walltaker");

                    for link in &config.lock().unwrap().links {
                        walltaker::subscribe_to(&mut write, *link).await?;
                    }

                    if let Some(link) = config.lock().unwrap().links.choose(&mut rand::thread_rng()) {
                        // Not the best but it works and whatnot
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        info!("Checking link {link} for initial wallpaper");
                        walltaker::check(&mut write, *link).await?;
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
                        current_url = Some(url_path);

                        let element = 
                            if ext == "webm" {
                                "video"
                            } else {
                                "image"
                            };

                        for view in &bg_webviews {
                            view.eval(&format!("
                                document.getElementById('{element}').src = '{url}';
                            "))?;
                        }

                        if config.lock().unwrap().notifications {
                            let set_by = message.set_by
                                .unwrap_or_else(|| String::from("Anonymous"));

                            let text = format!("{} changed your wallpaper via link {}! ❤️",
                                set_by, message.id);

                            notification(&text);
                        }
                    }
                }
            }
        }

        /* Read tray messages */
        if let Ok(message) = rx.try_recv() {
            match message {
                TrayMessage::Quit => {
                    let mut cfg = File::create(config_path)?;
                    write!(cfg, "{}", serde_json::to_string(&*config.lock().unwrap())?)?;
                    log::info!("settings saved");
                    std::process::exit(0);
                },
    
                TrayMessage::Refresh => {
                    if let Some(link) = config.lock().unwrap().links.choose(&mut rand::thread_rng()) {
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
    
                TrayMessage::Settings => {
                    settings.show();
                },
            }
        }

        settings.handle_messages()?;
        for view in &bg_webviews {
            view.handle_messages()?;
        }
    }
}

fn set_fit(mode: &FitMode, to: &Rc<webview::WebView>) -> webview::Result<()> {
    to.eval(match mode {
        FitMode::Stretch => "setStretch();",
        FitMode::Fill => "setFill();",
        FitMode::Fit => "setFit();",
    })?;

    Ok(())
}

fn set_bg_colour(view: &Rc<webview::WebView>, color: &str) -> anyhow::Result<()> {
    view.eval(&format!("document.body.style.backgroundColor = '{}';", color))?;
    
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

fn run_on_boot(should: bool) -> anyhow::Result<()> {
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