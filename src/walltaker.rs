use serde::{Serialize, Deserialize};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Welcome,
    Ping { message: u64, },
    ConfirmSubscription { identifier: String },
    #[serde(untagged)]
    Message {
        identifier: String,
        message:    WallpaperUpdate,
    }
}

#[derive(Deserialize)]
pub struct WallpaperUpdate {
    pub id:       usize,
    pub post_url: String,
    pub set_by:   Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "command")]
pub enum Outgoing {
    #[serde(rename = "subscribe")]
    Subscribe { identifier: String, },
    #[serde(untagged)]
    Check {
        data: String,
        identifier: String,
        command: String,
    },
}

#[derive(Serialize)]
struct Action {
    action: String,
}

#[derive(Serialize)]
pub struct Identifier {
    pub channel: String,
    pub id: usize,
}

pub fn subscribe_message(to: usize) -> anyhow::Result<String> {
    let inner = Identifier { channel: String::from("LinkChannel"), id: to };
    let inner = serde_json::to_string(&inner)?;

    let msg = Outgoing::Subscribe { identifier: inner };
    Ok(serde_json::to_string(&msg)?)
}

pub fn check_message(id: usize) -> anyhow::Result<String> {
    let inner = Identifier { channel: String::from("LinkChannel"), id };
    let inner = serde_json::to_string(&inner)?;

    let data = Action { action: String::from("check"), };
    let data = serde_json::to_string(&data)?;

    let msg = Outgoing::Check { data, identifier: inner, command: String::from("message"), };

    Ok(serde_json::to_string(&msg)?)
}