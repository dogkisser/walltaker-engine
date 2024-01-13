/// Shamelessly stolen from 
/// <https://github.com/wravery/webview2-rs>
/// I'm thrilled to not have to do this work myself.
use std::{
    collections::HashMap,
    fmt, mem, ptr,
    sync::{mpsc, Arc, Mutex}, rc::Rc,
};

use serde::Deserialize;
use serde_json::Value;
#[allow(clippy::wildcard_imports)]
use webview2_com::{*, Microsoft::Web::WebView2::Win32::*};
use windows::Win32::{
    System::LibraryLoader::GetModuleHandleA,
    UI::WindowsAndMessaging::{
        IMAGE_ICON, LR_SHARED, WM_SETICON, ICON_SMALL,
        LoadImageA, SendMessageA,
    },
};
#[allow(clippy::wildcard_imports)]
use windows::{
    core::*,
    Win32::{
        Foundation::{E_POINTER, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM},
        Graphics::Gdi,
        System::{LibraryLoader, Threading, WinRT::EventRegistrationToken},
        UI::WindowsAndMessaging::{self,
            MSG, WINDOW_LONG_PTR_INDEX, WNDCLASSW, MINMAXINFO, SWP_NOZORDER, SWP_NOMOVE,
            WINDOW_EX_STYLE,
            SetWindowPos
        },
    },
};

pub mod settings;
pub mod webviews;

/// TODO: This function generally needs better error management.
#[derive(Debug)]
pub enum Error {
    WebView2(webview2_com::Error),
    Windows(windows::core::Error),
    Json(serde_json::Error),
    Lock,
}

impl std::error::Error for Error { }

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<webview2_com::Error> for Error {
    fn from(err: webview2_com::Error) -> Self {
        Self::WebView2(err)
    }
}

impl From<windows::core::Error> for Error {
    fn from(err: windows::core::Error) -> Self {
        Self::Windows(err)
    }
}

impl From<HRESULT> for Error {
    fn from(err: HRESULT) -> Self {
        Self::Windows(windows::core::Error::from(err))
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl<'a, T: 'a> From<std::sync::PoisonError<T>> for Error {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        Self::Lock
    }
}

impl<'a, T: 'a> From<std::sync::TryLockError<T>> for Error {
    fn from(_: std::sync::TryLockError<T>) -> Self {
        Self::Lock
    }
}

pub type Result<T> = std::result::Result<T, Error>;

struct Window(HWND);

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}

#[derive(Clone)]
pub struct FrameWindow {
    window: Arc<HWND>,
    size: Arc<Mutex<SIZE>>,
}

impl FrameWindow {
    fn new() -> Self {
        let hwnd = {
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                lpszClassName: w!("WalltakerEngine"),
                ..Default::default()
            };

            unsafe {
                WindowsAndMessaging::RegisterClassW(&window_class);

                let hwnd = WindowsAndMessaging::CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("WalltakerEngine"),
                    w!("Walltaker Engine"),
                    WindowsAndMessaging::WS_OVERLAPPEDWINDOW,
                    WindowsAndMessaging::CW_USEDEFAULT,
                    WindowsAndMessaging::CW_USEDEFAULT,
                    WindowsAndMessaging::CW_USEDEFAULT,
                    WindowsAndMessaging::CW_USEDEFAULT,
                    None,
                    None,
                    LibraryLoader::GetModuleHandleW(None).unwrap_or_default(),
                    None,
                );


                let handle = GetModuleHandleA(PCSTR::null()).unwrap();
                let icon_handle = LoadImageA(handle, s!("icon"), IMAGE_ICON, 0, 0, LR_SHARED)
                    .unwrap();
                SendMessageA(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(icon_handle.0));

                hwnd
            }
        };

        FrameWindow {
            window: Arc::new(hwnd),
            size: Arc::new(Mutex::new(SIZE { cx: 300, cy: 300 })),
        }
    }
}

struct WebViewController(ICoreWebView2Controller);

type WebViewSender = mpsc::Sender<Box<dyn FnOnce(WebView) + Send>>;
type WebViewReceiver = mpsc::Receiver<Box<dyn FnOnce(WebView) + Send>>;
type BindingCallback = Box<dyn FnMut(Vec<Value>) -> Result<Value>>;
type BindingsMap = HashMap<String, BindingCallback>;

#[derive(Clone)]
pub struct WebView {
    controller: Rc<WebViewController>,
    webview: Rc<ICoreWebView2>,
    tx: WebViewSender,
    rx: Rc<WebViewReceiver>,
    thread_id: u32,
    min_w: i32,
    min_h: i32,
    html: Arc<Mutex<String>>,
    bindings: Rc<Mutex<BindingsMap>>,
    frame: Option<FrameWindow>,
    parent: Arc<HWND>,
}

impl Drop for WebViewController {
    fn drop(&mut self) {
        unsafe { self.0.Close() }.unwrap();
    }
}

#[derive(Debug, Deserialize)]
struct InvokeMessage {
    id: u64,
    method: String,
    params: Vec<Value>,
}

impl WebView {
    pub fn create(parent: Option<HWND>, debug: bool, min_size: (i32, i32)) -> Result<WebView> {
        #[allow(clippy::single_match_else)]
        let (parent, frame) = match parent {
            Some(hwnd) => (hwnd, None),
            None => {
                let frame = FrameWindow::new();
                (*frame.window, Some(frame))
            }
        };

        let environment = {
            let (tx, rx) = mpsc::channel();

            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(|environmentcreatedhandler| unsafe {
                    let data_folder = directories::BaseDirs::new()
                        .unwrap()
                        .cache_dir()
                        .join("walltaker-engine");
                    let data_folder = HSTRING::from(data_folder.as_os_str());
        
                    let options: ICoreWebView2EnvironmentOptions =
                        CoreWebView2EnvironmentOptions::default().into();

                    CreateCoreWebView2EnvironmentWithOptions(
                            PCWSTR::null(),
                            &data_folder,
                            &options,
                            &environmentcreatedhandler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, environment| {
                    error_code?;
                    tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .expect("send over mpsc channel");
                    Ok(())
                }),
            )?;

            rx.recv()
                .map_err(|_| Error::WebView2(webview2_com::Error::SendError))?
        }?;

        let controller = {
            let (tx, rx) = mpsc::channel();

            CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    environment
                        .CreateCoreWebView2Controller(parent, &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, controller| {
                    error_code?;
                    tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .expect("send over mpsc channel");
                    Ok(())
                }),
            )?;

            rx.recv()
                .map_err(|_| Error::WebView2(webview2_com::Error::SendError))?
        }?;

        let size = get_window_size(parent);
        let mut client_rect = RECT::default();
        unsafe {
            let _ = WindowsAndMessaging::GetClientRect(parent, std::ptr::addr_of_mut!(client_rect));
            controller.SetBounds(RECT {
                left: 0,
                top: 0,
                right: size.cx,
                bottom: size.cy,
            })?;
            controller.SetIsVisible(true)?;
        }

        let webview = unsafe { controller.CoreWebView2()? };

        if !debug {
            unsafe {
                let settings = webview.Settings()?;
                settings.SetIsStatusBarEnabled(false)?;
                settings.SetIsZoomControlEnabled(false)?;
                settings.SetAreDefaultContextMenusEnabled(false)?;
                settings.SetAreDevToolsEnabled(false)?;
            }
        }

        if let Some(frame) = frame.as_ref() {
            *frame.size.lock()? = size;
        }

        let (tx, rx) = mpsc::channel();
        let rx = Rc::new(rx);
        let thread_id = unsafe { Threading::GetCurrentThreadId() };

        let webview = WebView {
            controller: Rc::new(WebViewController(controller)),
            webview: Rc::new(webview),
            tx,
            rx,
            thread_id,
            min_w: min_size.0,
            min_h: min_size.1,
            html: Mutex::new(String::new()).into(),
            bindings: Rc::new(Mutex::new(HashMap::new())),
            frame,
            parent: Arc::new(parent),
        };

        // Inject the invoke handler.
        webview
            .init(r"window.external = { invoke: s => window.chrome.webview.postMessage(s) };")?;

        let bindings = webview.bindings.clone();
        let bound = webview.clone();
        unsafe {
            let mut token_ = EventRegistrationToken::default();
            webview.webview.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_webview, args| {
                    if let Some(args) = args {
                        let mut message = PWSTR(ptr::null_mut());
                        if args.WebMessageAsJson(&mut message).is_ok() {
                            let message = CoTaskMemPWSTR::from(message);
                            if let Ok(value) =
                                serde_json::from_str::<InvokeMessage>(&message.to_string())
                            {
                                if let Ok(mut bindings) = bindings.try_lock() {
                                    if let Some(f) = bindings.get_mut(&value.method) {
                                        match (*f)(value.params) {
                                            Ok(result) => bound.resolve(value.id, 0, &result),
                                            Err(err) => bound.resolve(
                                                value.id,
                                                1,
                                                &Value::String(format!("{err:#?}")),
                                            ),
                                        };
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                })),
                &mut token_,
            )?;
        }

        if webview.frame.is_some() {
            WebView::set_window_webview(parent, Some(Box::new(webview.clone())));
        }

        Ok(webview)
    }

    pub fn handle_messages(&self) -> Result<()> {
        if let Some(frame) = self.frame.as_ref() {
            let hwnd = *frame.window;
            unsafe {
                Gdi::UpdateWindow(hwnd);
            }
        }

        let mut msg = MSG::default();
        let h_wnd = HWND::default();

        while let Ok(f) = self.rx.try_recv() {
            (f)(self.clone());
        }

        unsafe {
            let result = WindowsAndMessaging::GetMessageW(&mut msg, h_wnd, 0, 0).0;

            match (result, msg.message) {
                (-1, _) => Err(windows::core::Error::from_win32().into()),
                (0, _) | (_, WindowsAndMessaging::WM_APP) => Ok(()),
                _ => {
                    WindowsAndMessaging::TranslateMessage(&msg);
                    WindowsAndMessaging::DispatchMessageW(&msg);
                    Ok(())
                },
            }
        }
    }

    pub fn resize(&self, w: i32, h: i32) -> Result<()> {
        unsafe {
            SetWindowPos(*self.parent, HWND(-1), -1, -1, w, h, SWP_NOZORDER | SWP_NOMOVE)?;
        }

        Ok(())
    }

    pub fn terminate(self) {
        self.dispatch(|_webview| unsafe {
            WindowsAndMessaging::PostQuitMessage(0);
        });

        if self.frame.is_some() {
            WebView::set_window_webview(self.get_window(), None);
        }
    }

    pub fn init(&self, js: &str) -> Result<&Self> {
        let webview = self.webview.clone();
        let js = String::from(js);
        AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                let js = CoTaskMemPWSTR::from(js.as_str());
                webview
                    .AddScriptToExecuteOnDocumentCreated(*js.as_ref().as_pcwstr(), &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(|error_code, _id| error_code),
        )?;
        Ok(self)
    }

    pub fn navigate_html(&self, html: &str) -> Result<&Self> {
        *self.html.lock().unwrap() = String::from(html);

        let webview = self.webview.as_ref();
        let html = self.html.lock().unwrap();
        let (tx, rx) = mpsc::channel();

        if !html.is_empty() {
            let handler =
                NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
                    tx.send(()).expect("send over mpsc channel");
                    Ok(())
                }));
            let mut token = EventRegistrationToken::default();
            unsafe {
                webview.add_NavigationCompleted(&handler, &mut token)?;
                let html = CoTaskMemPWSTR::from(html.as_str());
                webview.NavigateToString(*html.as_ref().as_pcwstr())?;
                let result = webview2_com::wait_with_pump(rx);
                webview.remove_NavigationCompleted(token)?;
                result?;
            }
        }


        Ok(self)
    }

    pub fn eval(&self, js: &str) -> Result<&Self> {
        let webview = self.webview.clone();
        let js = String::from(js);
        ExecuteScriptCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                let js = CoTaskMemPWSTR::from(js.as_str());
                webview
                    .ExecuteScript(*js.as_ref().as_pcwstr(), &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(|error_code, _result| error_code),
        )?;
        Ok(self)
    }

    pub fn get_window(&self) -> HWND {
        *self.parent
    }

    pub fn show(&self) {
        let hwnd = *self.parent;
        unsafe { WindowsAndMessaging::ShowWindow(hwnd, WindowsAndMessaging::SW_SHOW) };
    }

    pub fn dispatch<F>(&self, f: F) -> &Self
    where
        F: FnOnce(WebView) + Send + 'static,
    {
        self.tx.send(Box::new(f)).expect("send the fn");

        unsafe {
            let _ = WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                WindowsAndMessaging::WM_APP,
                WPARAM::default(),
                LPARAM::default(),
            );
        }
        self
    }

    pub fn bind<F>(&self, name: &str, f: F) -> Result<&Self>
    where
        F: FnMut(Vec<Value>) -> Result<Value> + 'static,
    {
        self.bindings
            .lock()?
            .insert(String::from(name), Box::new(f));

        let js = String::from(
            r"
            (function() {
                var name = '",
        ) + name
            + r"';
                var RPC = window._rpc = (window._rpc || {nextSeq: 1});
                window[name] = function() {
                    var seq = RPC.nextSeq++;
                    var promise = new Promise(function(resolve, reject) {
                        RPC[seq] = {
                            resolve: resolve,
                            reject: reject,
                        };
                    });
                    window.external.invoke({
                        id: seq,
                        method: name,
                        params: Array.prototype.slice.call(arguments),
                    });
                    return promise;
                }
            })()";

        self.init(&js)
    }

    pub fn resolve(&self, id: u64, status: i32, result: &Value) -> &Self {
        let result = result.to_string();

        self.dispatch(move |webview| {
            let method = match status {
                0 => "resolve",
                _ => "reject",
            };
            let js = format!(
                r#"
                window._rpc[{id}].{method}({result});
                window._rpc[{id}] = undefined;"#
            );

            webview.eval(&js).expect("eval return script");
        })
    }

    fn set_window_webview(hwnd: HWND, webview: Option<Box<WebView>>) -> Option<Box<WebView>> {
        unsafe {
            match SetWindowLong(
                hwnd,
                WindowsAndMessaging::GWLP_USERDATA,
                match webview {
                    Some(webview) => Box::into_raw(webview) as _,
                    None => 0_isize,
                },
            ) {
                0 => None,
                ptr => Some(Box::from_raw(ptr as *mut _)),
            }
        }
    }

    fn get_window_webview(hwnd: HWND) -> Option<Box<WebView>> {
        unsafe {
            let data = GetWindowLong(hwnd, WindowsAndMessaging::GWLP_USERDATA);

            if data == 0 { None } else {
                let webview_ptr = data as *mut WebView;
                let raw = Box::from_raw(webview_ptr);
                let webview = raw.clone();
                mem::forget(raw);
                
                Some(webview)
            }
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    let Some(webview) = WebView::get_window_webview(hwnd) else {
        return unsafe { WindowsAndMessaging::DefWindowProcW(hwnd, msg, w_param, l_param) }
    };

    let frame = webview
        .frame
        .as_ref()
        .expect("should only be called for owned windows");

    match msg {
        WindowsAndMessaging::WM_SIZE => {
            let size = get_window_size(hwnd);
            unsafe {
                webview
                    .controller
                    .0
                    .SetBounds(RECT {
                        left: 0,
                        top: 0,
                        right: size.cx,
                        bottom: size.cy,
                    })
                    .unwrap();
            }
            *frame.size.lock().expect("lock size") = size;
            LRESULT::default()
        }

        WindowsAndMessaging::WM_GETMINMAXINFO => {
            let mmi = l_param.0 as *mut MINMAXINFO;
            
            unsafe {
                (*mmi).ptMinTrackSize.x = webview.min_w;
                (*mmi).ptMinTrackSize.y = webview.min_h;
            }
            LRESULT::default()
        },

        WindowsAndMessaging::WM_CLOSE => {
            unsafe {
                // intelligent design™️
                let _ = WindowsAndMessaging::ShowWindow(hwnd, WindowsAndMessaging::SW_HIDE);
            }
            LRESULT::default()
        }

        WindowsAndMessaging::WM_DESTROY => {   
            webview.terminate();
            LRESULT::default()
        }

        _ => unsafe { WindowsAndMessaging::DefWindowProcW(hwnd, msg, w_param, l_param) },
    }
}

fn get_window_size(hwnd: HWND) -> SIZE {
    let mut client_rect = RECT::default();
    let _ = unsafe { WindowsAndMessaging::GetClientRect(hwnd, std::ptr::addr_of_mut!(client_rect)) };
    SIZE {
        cx: client_rect.right - client_rect.left,
        cy: client_rect.bottom - client_rect.top,
    }
}

#[allow(non_snake_case)]
#[cfg(target_pointer_width = "64")]
unsafe fn SetWindowLong(window: HWND, index: WINDOW_LONG_PTR_INDEX, value: isize) -> isize {
    WindowsAndMessaging::SetWindowLongPtrW(window, index, value)
}

#[allow(non_snake_case)]
#[cfg(target_pointer_width = "64")]
unsafe fn GetWindowLong(window: HWND, index: WINDOW_LONG_PTR_INDEX) -> isize {
    WindowsAndMessaging::GetWindowLongPtrW(window, index)
}