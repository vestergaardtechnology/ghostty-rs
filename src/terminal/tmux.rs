//! Port of `src/terminal/tmux.zig`.
//!
//! STATUS: partial — `Viewer` re-export pending (tmux/viewer.zig not yet ported).
//!
//! Types and functions related to tmux protocols.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "tmux/control.rs"]
pub mod control;
#[path = "tmux/layout.rs"]
pub mod layout;
#[path = "tmux/output.rs"]
pub mod output;
#[path = "tmux/viewer.rs"]
pub mod viewer;
// @generated-mods-end

pub use control::Notification as ControlNotification;
pub use control::Parser as ControlParser;
pub use layout::Layout;
// TODO: `pub use viewer::Viewer;` once tmux/viewer.zig is ported.
