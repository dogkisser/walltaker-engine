use crate::settings::Settings;
use std::sync::Arc;
use log::debug;
use tokio::sync::{RwLock, Mutex};
use windows::{
    core::{s, w, HSTRING},
    Win32::{
        Foundation::*,
        UI::{
            WindowsAndMessaging::*,
            Controls::{
                IsDlgButtonChecked, BST_CHECKED, EM_SETCUEBANNER,
            },
            Shell::*,
        },
    },
};
use winrt_notification::{Toast, IconCrop};

pub async fn spawn_settings(
    settings: Arc<RwLock<Settings>>,
    wallpaper: Arc<Mutex<crate::wallpaper::Wallpaper>>,
    write: crate::Writer,
) -> anyhow::Result<()>
{
    let new = {
        let settings_reader = settings.read().await;
        let new = {
            let t: Settings = (*settings_reader).clone();
            let ptr = std::ptr::addr_of!(t) as isize;
            unsafe {
                DialogBoxParamW(
                    HINSTANCE(0),
                    w!("IDD_MAIN"),
                    HWND(0),
                    Some(subscriptions_proc),
                    LPARAM(ptr))
            }
        };

        let new: &Settings = unsafe { &*(new as *const _) };

        // This is theoretically really, really slow but these vecs will only
        // ever contain like, 5 elements tops. So it doesn't really matter.
        let added = new.subscribed.iter()
            .filter(|i| !settings_reader.subscribed.contains(i));
        let removed = settings_reader.subscribed.iter()
            .filter(|i| !new.subscribed.contains(i));

        // TODO: ability to unsubscribe
        _ = removed;

        for id in added {
            crate::walltaker::subscribe_to(&write, *id).await?;
        }

        let _ = wallpaper.lock().await.set_method(new.method);

        new
    };

    let mut settings = settings.write().await;
    *settings = new.clone();
    settings.save()?;

    debug!("Config saved");
    Ok(())
}

unsafe extern "system" fn subscriptions_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    settings: LPARAM
) -> isize
{
    /* message, hiword, loword */
    match (message, (w_param.0 >> 16 & 0xffff) as u32, (w_param.0 & 0xFFFF)) { 
        (WM_INITDIALOG, _, _) => {
            let settings: &Settings = &*(settings.0 as *const _);

            for i in &settings.subscribed {
                let s = std::ffi::CString::new(i.to_string()).unwrap();
                SendDlgItemMessageA(
                    hwnd,
                    1002,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(s.as_ptr() as isize));  
            }

            let target = match DESKTOP_WALLPAPER_POSITION(settings.method) {
                DWPOS_TILE    => 1006,
                DWPOS_FILL    => 1007,
                DWPOS_FIT     => 1008,
                DWPOS_STRETCH => 1009,
                _ => panic!("invalid settings.method"),
            };
            SendDlgItemMessageA(hwnd,
                target,
                BM_SETCHECK,
                WPARAM(BST_CHECKED.0 as usize),
                LPARAM(0));

            let placeholder = w!("Walltaker ID");
            SendDlgItemMessageA(hwnd,
                1000,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(placeholder.as_ptr() as isize));

            if settings.notifications {
                SendDlgItemMessageA(hwnd,
                    1010,
                    BM_SETCHECK,
                    WPARAM(BST_CHECKED.0 as usize),
                    LPARAM(0));
            }
        },

        /* IDC_ADD */
        (WM_COMMAND, _, 1003) => {
            let id = GetDlgItemInt(hwnd, 1000, None, false);

            if id != 0 {
                let s = std::ffi::CString::new(id.to_string()).unwrap();
                SendDlgItemMessageA(hwnd,
                    1002,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(s.as_ptr() as isize));
            }

            let _ = SetDlgItemTextA(hwnd, 1000, s!(""));
        },

        /* IDC_REMOVE */
        (WM_COMMAND, _, 1005) => {
            let selected = SendDlgItemMessageA(hwnd,
                1002,
                LB_GETCURSEL,
                WPARAM(0),
                LPARAM(0)).0;

            // Nothing selected
            if selected == LB_ERR as isize {
                return 1;
            }

            SendDlgItemMessageA(hwnd,
                1002,
                LB_DELETESTRING,
                WPARAM(selected as usize),
                LPARAM(0));
        },

        /* IDC_RADIO_* */
        // (WM_COMMAND, _, x@1006..=1009) => {
        //     // let method = match x {
        //     //     1006 => DWPOS_TILE,
        //     //     1007 => DWPOS_FILL,
        //     //     1008 => DWPOS_FIT,
        //     //     1009 => DWPOS_STRETCH,
        //     //     _ => unreachable!(),
        //     // };
        // },

        /* IDC_NOTIFICATIONS */
        (WM_COMMAND, _, 1010) => { },

        (WM_CLOSE, _, _) => {
            let mut out: Box<Settings> = Box::default();
            let item_count = SendDlgItemMessageA(
                hwnd,
                1002,
                LB_GETCOUNT,
                WPARAM(0),
                LPARAM(0)).0 as usize;
    
            for i in 0..item_count {
                let len = SendDlgItemMessageA(
                    hwnd,
                    1002,
                    LB_GETTEXTLEN,
                    WPARAM(i),
                    LPARAM(0)).0 as usize;
                let mut buf: Vec<u8> = Vec::with_capacity(len + 1);
                let ptr = buf.as_mut_ptr() as isize;
                
                let read_in = SendDlgItemMessageA(
                    hwnd,
                    1002,
                    LB_GETTEXT,
                    WPARAM(i),
                    LPARAM(ptr)).0 as usize;
                buf.set_len(read_in);
                let num = std::str::from_utf8(&buf).unwrap().parse().unwrap();

                out.subscribed.push(num);
            }

            for (button_id, value) in [1006 , 1007 , 1008 , 1009 ].iter().zip(
                                      [DWPOS_TILE, DWPOS_FILL,
                                       DWPOS_FIT, DWPOS_STRETCH])
            {
                if IsDlgButtonChecked(hwnd, *button_id) == 1 {
                    out.method = value.0;
                }
            }

            out.notifications = IsDlgButtonChecked(hwnd, 1010) == 1;
            
            EndDialog(hwnd, Box::leak(out) as *const _ as isize).unwrap();
        },

        _ => {
            return 0;
        },
    }
    
    1
}

/// Sends a toast
pub fn notification(text: &str, icon: Option<&std::path::Path>) {
    let mut toast = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("Walltaker Engine")
        .text1(text);

    if let Some(icon) = icon {
        toast = toast.icon(icon, IconCrop::Circular, "Walltaker Engine");
    }

    // Result ignored; it doesn't really matter if this doesn't go through.
    // There's nothing we can do about it anyway.
    _ = toast.show();
}

/// Spawns an error message window
pub fn popup(saying: &str) {
    unsafe {
        MessageBoxW(
            HWND(0),
            &HSTRING::from(saying),
            w!("Walltaker Engine Error"),
            MB_OK | MB_ICONERROR);
        }
}