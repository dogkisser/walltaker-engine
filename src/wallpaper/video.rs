use vlc::{Instance, Media, MediaPlayer};

pub struct Video {
    instance: Instance,
    media_player: MediaPlayer,
}

impl Video {
    pub fn new(hwnd: *mut std::ffi::c_void) -> Self {
        // None of --loop, --repeat, -L, or -R, neither --input-repeat=-1, work.
        // Anymore, at least.
        // There's no API to loop videos.
        // I can't do it programatically at all, because vlc-rs doesn't support
        // passing user data in callbacks (despite VLC supporting it).
        // This is the pinnacle of software. These people are engineers.
        let instance = Instance::with_args(Some(vec![
            String::from("--input-repeat=99999999")
        ])).unwrap();
        let media_player = MediaPlayer::new(&instance).unwrap();

        media_player.set_hwnd(hwnd);

        Self {
            instance,
            media_player,
        }
    }

    pub fn set_video(&mut self, to: &str) {
        let media = Media::new_path(&self.instance, to).unwrap();
        self.media_player.set_media(&media);
    }

    pub fn play(&self) {
        self.media_player.play().unwrap();
    }

    pub fn pause(&self) {
        self.media_player.pause();
        // TODO: I really don't want to have to do this, but I guess .pause() is
        // async so VLC may still send a few more frames to the screen after we
        // change the wallpaper, overwriting it.
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}