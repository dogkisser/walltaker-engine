use std::fs::File;
use std::rc::Rc;
use std::sync::Mutex;
use std::{path::PathBuf, sync::Arc};
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

mod hwnd;
mod webview;
mod walltaker;

type Writer = Arc<tokio::sync::Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
struct Config {
    links: Vec<usize>,
    log: bool,
}

const SETTINGS_HTML: &str = include_str!("../res/settings.html");

const HTML: &str = r#"
    <!doctype html>
    <html>
        <body>
            <img id="image"></img>
            <video id="video" autoplay preload>
            </video>
        </body>
    </html>

    <script>
window.onload = () => {
    document.getElementById('video').addEventListener('loadeddata', () => {
        document.getElementById('video').style.display = 'block';
        document.getElementById('image').style.display = 'none';
    }, false);

    document.getElementById('image').onload = () => {
        document.getElementById('video').style.display = 'none';
        document.getElementById('image').style.display = 'block';
    };
};
    </script>

    <style>
        html, body {
            overflow: hidden;
            width: 100vw;
            height: 100vh;
            padding: 0;
            margin: 0;

            background-color: black;
        }

        #video {
            display: none;
            min-width: 100%; 
            min-height: 100%; 
            height: 100%;
            width: auto;

            position: absolute;
            top: 50%;
            left: 50%;
            transform: translate(-50%,-50%);
        }

        #image {
            display: none;
            max-width: 100%;
            max-height: 100%;
            height: 100%;
            margin: auto;
        }
    </style>
"#;

enum TrayMessage {
    Quit,
    Settings,
    Refresh,
    OpenCurrent,
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
    let config_path = "./walltaker-engine.json";

    let config: Config = serde_json::from_reader(File::open(&config_path)?)?;
    let config: Rc<Mutex<Config>> = Mutex::new(config).into();

    if config.lock().unwrap().log {
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
    let hwnd = hwnds[0];

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
    let (write, mut read) = ws_stream.split();
    let write = Arc::new(tokio::sync::Mutex::new(write));

    let webview = webview::WebView::create(Some(hwnd), false)?;
    webview.navigate_html(HTML)?;

    let settings = webview::WebView::create(None, true)?;
    let _config = std::rc::Rc::clone(&config);
    let _write = Arc::clone(&write);
    settings.bind("saveSettings", move |request| {
        if let Some(new_cfg) = request.get(0) {
            let new_settings: Config = serde_json::from_value(new_cfg.clone())?;
            let mut config = _config.lock().unwrap();
            
            // This is theoretically really, really slow but these vecs will only
            // ever contain like, 5 elements tops. So it doesn't really matter.
            let added = new_settings.links.iter()
                .filter(|i| !config.links.contains(i));
            let removed = config.links.iter()
                .filter(|i| !new_settings.links.contains(i));
            // TODO: support live unsubscribing

            for link in added {
                let link = *link;
                let _write = Arc::clone(&_write);
                tokio::spawn(async move {
                    let _ = walltaker::subscribe_to(&_write, link).await;
                });
            }

            log::info!("Settings updated {new_settings:#?}");

            *config = new_settings;
            return Ok(serde_json::Value::String(String::from("ok")));
        }

        Err(webview::Error::WebView2Error(
            webview2_com::Error::CallbackError(String::from("Called wrong. wtf?"))))
    })?;
    let _config = std::rc::Rc::clone(&config);
    settings.bind("loadSettings", move |request| {
        let cfg = &*_config;
        Ok(serde_json::to_value(cfg)?)
    })?;
    settings.navigate_html(SETTINGS_HTML)?;
    settings.show();

    let mut current_url = None;
    loop {
        /* Read Walltaker websocket messages */
        if let Ready(Some(message)) = poll!(read.next()) {
            use walltaker::Incoming;

            let msg = message?.to_string();
            match serde_json::from_str(&msg).context(msg)? {
                Incoming::Ping { .. } => { },

                Incoming::ConfirmSubscription { identifier } => {
                    info!("Successfully subscribed to {identifier}");
                },

                Incoming::Welcome => {
                    info!("Connected to Walltaker");

                    for link in &config.lock().unwrap().links {
                        walltaker::subscribe_to(&write, *link).await?;
                    }

                    if let Some(link) = config.lock().unwrap().links.choose(&mut rand::thread_rng()) {
                        info!("Checking link {link} for initial wallpaper");
                        // Not the best but it works and whatnot
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        walltaker::check(&write, *link).await?;
                    }
                },
                // Wallpaper change
                Incoming::Message { message, .. } => {
                    if let Some(url) = message.post_url {
                        info!("Wallpaper changed to {url}");
                        let url_path = PathBuf::from(&url);
                        let ext = url_path.extension().unwrap().to_string_lossy().to_lowercase();
                        current_url = Some(url_path);

                        if ext == "webm" {
                            webview.eval(&format!("
                                document.getElementById('video').src = '{url}';
                            "))?;
                        } else {
                            webview.eval(&format!("
                                document.getElementById('image').src = '{url}';
                            "))?;
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
                        walltaker::check(&write, *link).await?;
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

        webview.handle_messages()?;
        settings.handle_messages()?;
    }
}

pub fn open(url: &str) {
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