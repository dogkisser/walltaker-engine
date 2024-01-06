use vlc::{Instance, Media, MediaPlayer, MediaPlayerVideoEx};
use windows::Win32::{Foundation::{HWND, RECT}, UI::{Shell::DWPOS_STRETCH, WindowsAndMessaging::GetWindowRect}};

pub struct Video {
    instance: Instance,
    intended_aspect_ratios: Vec<String>,
    media_players: Vec<MediaPlayer>,
}

impl Video {
    pub fn new(hwnds: &[HWND]) -> Self {
        // None of --loop, --repeat, -L, or -R, neither --input-repeat=-1, work.
        // Anymore, at least.
        // There's no API to loop videos.
        // I can't do it programatically at all, because vlc-rs doesn't support
        // passing user data in callbacks (despite VLC supporting it).
        // This is the pinnacle of software. These people are engineers.
        let instance = Instance::with_args(Some(vec![
            String::from("--input-repeat=99999999")
        ])).unwrap();

        let mut s = Self {
            instance,
            intended_aspect_ratios: vec![String::from("16:9")],
            media_players: Vec::new(),
        };

        for hwnd in hwnds {
            let media_player = MediaPlayer::new(&s.instance).unwrap();
            media_player.set_hwnd(hwnd.0 as *mut std::ffi::c_void);

            s.media_players.push(media_player);
        }

        s
    }

    pub fn set_video(&mut self, to: &str) {
        self.intended_aspect_ratios.clear();
        let media = Media::new_path(&self.instance, to).unwrap();

        for media_player in &self.media_players {
            media_player.set_media(&media);
        }
    }

    /* Stretch is the only supported ulterior mode. Otherwise the intended video
     * resolution is used, scaled ("Fit"). */
    pub fn set_aspect_ratio(&mut self, to: i32) -> anyhow::Result<()> {
        if to == DWPOS_STRETCH.0 {
            for media_player in &self.media_players {
                let res = hwnd_res(HWND(media_player.get_hwnd().unwrap() as isize))?;
                media_player.set_aspect_ratio(Some(&res));
            }
        } else {
            for media_player in &self.media_players {
                media_player.set_aspect_ratio(None);
            }
        }

        Ok(())
    }

    pub fn play(&self) {
        for media_player in &self.media_players {
            media_player.play().unwrap();
        }
    }

    pub fn pause(&self) {
        for media_player in &self.media_players {
            media_player.pause();
        }
        // TODO: I really don't want to have to do this, but I guess .pause() is
        // async so VLC may still send a few more frames to the screen after we
        // change the wallpaper, overwriting it.
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn hwnd_res(hwnd: HWND) -> anyhow::Result<String> {
    let (w, h) = unsafe {
        let mut r = RECT::default();
        GetWindowRect(
            hwnd,
            &mut r,
        )?;

        (r.right - r.left, r.bottom - r.top)
    };

    Ok(format!("{w}:{h}"))
}