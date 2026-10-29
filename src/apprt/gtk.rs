//! Port of `src/apprt/gtk.zig`.
//!
//! STATUS: partial — the `App`/`Surface` re-exports wait for `App.zig`,
//! `Surface.zig` and `class/*.zig` to be ported.
//!
//! The GTK apprt is only built with the `gtk` Cargo feature. This module
//! itself always exists (`apprt::rt` names it on Linux); its ported children
//! each start with `#![cfg(feature = "gtk")]`.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "gtk/adw_version.rs"]
pub mod adw_version;
#[path = "gtk/app.rs"]
pub mod app;
#[path = "gtk/build/mod.rs"]
pub mod build;
#[path = "gtk/cgroup.rs"]
pub mod cgroup;
#[path = "gtk/class.rs"]
pub mod class;
#[path = "gtk/desktop_notifications.rs"]
pub mod desktop_notifications;
#[path = "gtk/ext.rs"]
pub mod ext;
#[path = "gtk/flatpak.rs"]
pub mod flatpak;
#[path = "gtk/gsettings.rs"]
pub mod gsettings;
#[path = "gtk/gtk_version.rs"]
pub mod gtk_version;
#[path = "gtk/ipc/mod.rs"]
pub mod ipc;
#[path = "gtk/key.rs"]
pub mod key;
#[path = "gtk/media.rs"]
pub mod media;
#[path = "gtk/portal.rs"]
pub mod portal;
#[path = "gtk/post_fork.rs"]
pub mod post_fork;
#[path = "gtk/pre_exec.rs"]
pub mod pre_exec;
#[path = "gtk/scale.rs"]
pub mod scale;
#[path = "gtk/surface.rs"]
pub mod surface;
#[path = "gtk/weak_ref.rs"]
pub mod weak_ref;
#[path = "gtk/winproto.rs"]
pub mod winproto;
// @generated-mods-end

// The required comptime API for any apprt.
// TODO(port-dedupe): `App` (`gtk/App.zig`) and `Surface` (`gtk/Surface.zig`)
// once ported.
#[cfg(feature = "gtk")]
pub use self::flatpak::resources_dir;

// The exported API, custom for the apprt.
#[cfg(feature = "gtk")]
pub use self::weak_ref::WeakRef;
