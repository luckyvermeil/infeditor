//! Win32 infrastructure for frameless windows (everything from the former
//! `winframe` crate).
//!
//! This layer only depends on the Win32 API and on winit types re-exported
//! through `dioxus-native` (the types are the same origin); it does **not**
//! depend on the blitz rendering stack — the `shell` layer (the popup framework)
//! is built on top of this one.
//!
//! * [`frameless`] — drag/resize/maximize/topmost/DPI/owner attachment/dialog
//!   owner;
//! * [`snap`] — subclassing the wndproc to complete Win+↑ / drag-to-top snap
//!   and answer `WM_NCHITTEST`;
//! * [`softkb`] — touch keyboard (TabTip) detection (throttled).

pub mod frameless;
pub mod snap;
pub mod softkb;
