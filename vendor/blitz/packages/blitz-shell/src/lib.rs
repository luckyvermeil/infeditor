#![cfg_attr(docsrs, feature(doc_cfg))]

//! Event loop, windowing and system integration.
//!
//! ## Feature flags
//!  - `default`: Enables the features listed below.
//!  - `accessibility`: Enables [`accesskit`] accessibility support.
//!  - `hot-reload`: Enables hot-reloading of Dioxus RSX.
//!  - `tracing`: Enables tracing support.

mod application;
mod convert_events;
mod event;
mod net;
mod prepaint;
mod window;

#[cfg(feature = "accessibility")]
mod accessibility;

pub use crate::application::BlitzApplication;
pub use crate::event::{BlitzShellEvent, BlitzShellProxy};
pub use crate::window::{View, WindowConfig};

#[cfg(feature = "data-uri")]
pub use crate::net::DataUriNetProvider;

#[cfg(all(
    feature = "file-dialog",
    any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
use blitz_traits::shell::FileDialogFilter;
use blitz_traits::shell::ShellProvider;
use std::sync::Arc;
use winit::cursor::{Cursor, CursorIcon};
use winit::dpi::{LogicalPosition, LogicalSize};
pub use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
pub use winit::window::Window;
use winit::window::{ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData};

#[derive(Default)]
pub struct Config {
    pub stylesheets: Vec<String>,
    pub base_url: Option<String>,
}

/// Build an event loop for the application
pub fn create_default_event_loop() -> EventLoop {
    let mut ev_builder = EventLoop::builder();
    #[cfg(target_os = "android")]
    {
        use winit::platform::android::EventLoopBuilderExtAndroid;
        ev_builder.with_android_app(current_android_app());
    }

    let event_loop = ev_builder.build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);

    event_loop
}

#[cfg(target_os = "android")]
static ANDROID_APP: std::sync::OnceLock<android_activity::AndroidApp> = std::sync::OnceLock::new();

#[cfg(target_os = "android")]
#[cfg_attr(docsrs, doc(cfg(target_os = "android")))]
/// Set the current [`AndroidApp`](android_activity::AndroidApp).
pub fn set_android_app(app: android_activity::AndroidApp) {
    ANDROID_APP.set(app).unwrap()
}

#[cfg(target_os = "android")]
#[cfg_attr(docsrs, doc(cfg(target_os = "android")))]
/// Get the current [`AndroidApp`](android_activity::AndroidApp).
/// This will panic if the android activity has not been setup with [`set_android_app`].
pub fn current_android_app() -> android_activity::AndroidApp {
    ANDROID_APP.get().unwrap().clone()
}

pub struct BlitzShellProvider {
    window: Arc<dyn Window>,
    proxy: BlitzShellProxy,
}
impl BlitzShellProvider {
    pub fn new(window: Arc<dyn Window>, proxy: BlitzShellProxy) -> Self {
        Self { window, proxy }
    }
}

impl ShellProvider for BlitzShellProvider {
    fn request_redraw(&self) {
        self.window.request_redraw();
    }
    fn set_cursor(&self, icon: Option<CursorIcon>) {
        match icon {
            Some(icon) => {
                self.window.set_cursor_visible(true);
                self.window.set_cursor(Cursor::Icon(icon));
            }
            None => {
                self.window.set_cursor(Cursor::Icon(CursorIcon::Default));
                self.window.set_cursor_visible(false)
            }
        }
    }
    fn set_window_title(&self, title: String) {
        self.window.set_title(&title);
    }
    fn set_ime_enabled(&self, is_enabled: bool) {
        // InfEditor patch 九：`INFEDITOR_IME=0` 逃生门。blitz 的 `Node::focus`
        // 会对真 TextInput 调 `set_ime_enabled(true)`（node.rs 的 focus 路径）
        // ——真 `<input>` 元素布局就绪后每次聚焦都会把**真实 IME** 请进窗口。
        // 真人打字这正是想要的行为；但 GUI 自动化注入的是 SendInput 虚拟键，
        // 真实 IME 会把字母截进组合串、WM_CHAR 从此不再出现（实测：查找弹窗
        // 里 Tab 换框后所有按键 `text=None`，输入全丢）。与 winshell 的
        // `enable_ime` / 组件层显式 Enable 同一开关语义：测试环境全窗禁用。
        if is_enabled && std::env::var_os("INFEDITOR_IME").is_some_and(|v| v == "0") {
            return;
        }
        if is_enabled {
            let _ = self.window.request_ime_update(ImeRequest::Enable(
                // InfEditor patch 六：Enable 必须声明 cursor_area 能力——
                // win32 后端只在 `capabilities.cursor_area()` 为真时接受
                // 候选窗锚点更新，空能力会静默丢弃（warn 后 return）。
                // ImeEnableRequest::new 要求能力与初值成对声明（XOR 校验），
                // 所以这里同时给 0,0 起步的初始锚点，节点 focus 后由
                // set_ime_cursor_area 刷新为真实位置。
                ImeEnableRequest::new(
                    ImeCapabilities::new().with_cursor_area(),
                    ImeRequestData::default().with_cursor_area(
                        LogicalPosition::new(0.0, 0.0).into(),
                        LogicalSize::new(0.0, 0.0).into(),
                    ),
                )
                .unwrap(),
            ));
        } else {
            let _ = self.window.request_ime_update(ImeRequest::Disable);
        }
    }
    fn set_ime_cursor_area(&self, x: f32, y: f32, width: f32, height: f32) {
        let _ = self.window.request_ime_update(ImeRequest::Update(
            ImeRequestData::default().with_cursor_area(
                LogicalPosition::new(x, y).into(),
                LogicalSize::new(width, height).into(),
            ),
        ));
    }

    fn request_window_close(&self) {
        self.proxy.send_event(BlitzShellEvent::CloseWindow {
            window_id: self.window.id(),
        });
    }
    fn set_window_minimized(&self, minimized: bool) {
        self.window.set_minimized(minimized);
    }
    fn set_window_maximized(&self, maximized: bool) {
        self.window.set_maximized(maximized);
    }
    fn is_window_maximized(&self) -> bool {
        self.window.is_maximized()
    }
    fn set_window_decorations(&self, decorations: bool) {
        self.window.set_decorations(decorations);
    }
    fn drag_window(&self) {
        let _ = self.window.drag_window();
    }

    #[cfg(all(
        feature = "clipboard",
        any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )
    ))]
    fn get_clipboard_text(&self) -> Result<String, blitz_traits::shell::ClipboardError> {
        let mut cb = arboard::Clipboard::new().unwrap();
        cb.get_text()
            .map_err(|_| blitz_traits::shell::ClipboardError)
    }

    #[cfg(all(
        feature = "clipboard",
        any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )
    ))]
    fn set_clipboard_text(&self, text: String) -> Result<(), blitz_traits::shell::ClipboardError> {
        let mut cb = arboard::Clipboard::new().unwrap();
        cb.set_text(text.to_owned())
            .map_err(|_| blitz_traits::shell::ClipboardError)
    }

    #[cfg(all(
        feature = "file-dialog",
        any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )
    ))]
    fn open_file_dialog(
        &self,
        multiple: bool,
        filter: Option<FileDialogFilter>,
    ) -> Vec<std::path::PathBuf> {
        let mut dialog = rfd::FileDialog::new();
        if let Some(FileDialogFilter { name, extensions }) = filter {
            dialog = dialog.add_filter(&name, &extensions);
        }
        let files = if multiple {
            dialog.pick_files()
        } else {
            dialog.pick_file().map(|file| vec![file])
        };
        files.unwrap_or_default()
    }
}
