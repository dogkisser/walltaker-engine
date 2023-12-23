use anyhow::Context;
use serde::{Serialize, Deserialize};
use windows::Win32::UI::Shell::DWPOS_FILL;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Settings {
    pub subscribed: Vec<usize>,
    pub method: i32,

    #[serde(skip)]
    out: std::path::PathBuf,
}

impl Settings {
    pub fn load_or_new() -> Self {
        let out = directories::BaseDirs::new()
            .unwrap()
            .config_dir()
            .join("walltaker-engine.json");

        let mut t: Self = std::fs::File::open(&out)
            .context("reading file")
            .and_then(|f| serde_json::from_reader(f).context("reading config"))
            .unwrap_or_default();
        t.method = DWPOS_FILL.0;
        t.out = out;

        t
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let out = std::fs::File::create(&self.out)?;
        serde_json::to_writer_pretty(out, &self)?;

        Ok(())
    }
}