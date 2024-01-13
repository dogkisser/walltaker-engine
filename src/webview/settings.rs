use std::rc::Rc;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::webview::{Error, WebView};

const SETTINGS_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/settings.html.min"));

// TODO: Maybe just make all of this just one "Update" message
pub enum UiMessage {
    UpdateFit,
    UpdateRunOnBoot,
    UpdateBackgroundColour,
    SubscribeTo(usize),
}

pub fn create_settings_webview(
    config: &Rc<Mutex<crate::Config>>,
) -> anyhow::Result<(WebView, mpsc::Receiver<UiMessage>)>
{
    let (ui_tx, ui_rx) = mpsc::sync_channel(50);
    let _config = Rc::clone(&config);
    let _ui_tx = ui_tx.clone();
    
    let settings = WebView::create(None, false, (420, 420))?;
    settings.bind("saveSettings", move |request| {
        if let Some(new_cfg) = request.get(0) {
            let new_settings: crate::Config = serde_json::from_value(new_cfg.clone())?;
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

        Err(Error::WebView2Error(
            webview2_com::Error::CallbackError(String::from("Called wrong. wtf?"))))
    })?;

    let _config = Rc::clone(&config);
    settings.bind("loadSettings", move |_request| {
        let cfg = &*_config;
        Ok(serde_json::to_value(cfg)?)
    })?;

    settings.resize(420, 420)?;
    settings.navigate_html(SETTINGS_HTML)?;

    Ok((settings, ui_rx))
}