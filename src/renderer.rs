//! Port of `src/renderer.zig`.
//!
//! STATUS: partial — `GenericRenderer` (generic.zig) and its
//! `DerivedConfig` are stand-ins in [`standin`] until generic.zig is
//! ported (TODO(port-dedupe)).
//!
//! Renderer implementation and utilities. The renderer is responsible for
//! taking the internal screen state and turning into some output format,
//! usually for a screen.
//!
//! The renderer is closely tied to the windowing system which usually
//! has to prepare the window for the given renderer using system-specific
//! APIs. The renderers in this package assume that the renderer is already
//! setup (OpenGL has a context, Vulkan has a surface, etc.)

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "renderer/backend.rs"]
pub mod backend;
#[path = "renderer/cell.rs"]
pub mod cell;
#[path = "renderer/cursor.rs"]
pub mod cursor;
#[path = "renderer/display_link.rs"]
pub mod display_link;
#[path = "renderer/dmabuf.rs"]
pub mod dmabuf;
#[path = "renderer/generic.rs"]
pub mod generic;
#[path = "renderer/image.rs"]
pub mod image;
#[path = "renderer/link.rs"]
pub mod link;
#[path = "renderer/message.rs"]
pub mod message;
#[path = "renderer/metal.rs"]
pub mod metal;
#[path = "renderer/opengl.rs"]
pub mod opengl;
#[path = "renderer/options.rs"]
pub mod options;
#[path = "renderer/overlay.rs"]
pub mod overlay;
#[path = "renderer/row.rs"]
pub mod row;
#[path = "renderer/shadertoy.rs"]
pub mod shadertoy;
#[path = "renderer/size.rs"]
pub mod size;
#[path = "renderer/state.rs"]
pub mod state;
#[path = "renderer/thread.rs"]
pub mod thread;
// @generated-mods-end

pub use self::backend::Backend;
pub use self::cursor::style as cursor_style;
pub use self::cursor::Style as CursorStyle;
pub use self::message::Message;
#[cfg(target_os = "macos")]
pub use self::metal::Metal;
#[cfg(not(target_os = "macos"))]
pub use self::opengl::OpenGL;
pub use self::options::Options;
pub use self::overlay::Overlay;
pub use self::size::{CellSize, Coordinate, GridSize, Padding, ScreenSize, Size};
pub use self::state::State;
pub use self::thread::Thread;
pub use crate::lib;

/// The generic renderer, parameterized by graphics API.
///
/// TODO(port-dedupe): replace with `generic::Renderer`.
pub use self::standin::GenericRenderer;

/// The graphics API of this build (Zig: chosen by `build_config.renderer`).
#[cfg(target_os = "macos")]
pub type GraphicsApiImpl = Metal;
/// The graphics API of this build (Zig: chosen by `build_config.renderer`).
#[cfg(not(target_os = "macos"))]
pub type GraphicsApiImpl = OpenGL;

/// The implementation to use for the renderer. This is comptime chosen
/// so that every build has exactly one renderer implementation.
pub type Renderer = GenericRenderer<GraphicsApiImpl>;

/// The app-scoped render device from which surface renderers are created.
#[cfg(target_os = "macos")]
pub type Device = metal::device::Device;
/// The app-scoped render device from which surface renderers are created.
#[cfg(not(target_os = "macos"))]
pub type Device = opengl::device::Device;

crate::lib_enum! {
    /// The health status of a renderer. These must be shared across all
    /// renderers even if some states aren't reachable so that our API users
    /// can use the same enum for all renderers.
    pub enum Health: c {
        Healthy,
        Unhealthy,
    }
}

/// Stand-ins for types whose Zig files are not ported yet.
pub mod standin {
    use std::fmt;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::Instant;

    use super::message::{SearchMatch, SearchMatches};
    use super::{Health, Options, Size, State, Thread};
    use crate::apprt;
    use crate::config::config::{AlphaBlending, CustomShaderAnimation};
    use crate::config::path::RepeatablePath;
    use crate::config::Config;
    use crate::crash::sentry::CrashSurface;
    use crate::datastruct::blocking_queue::Timeout as QueueTimeout;
    use crate::global::xev;
    use crate::lib::tinyio::{Mutex, TinyIo};

    /// Error from initializing a derived config.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InitError {
        OutOfMemory,
    }

    impl fmt::Display for InitError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("OutOfMemory")
        }
    }

    impl std::error::Error for InitError {}

    /// The error type of the stand-in renderer's fallible methods (Zig:
    /// inferred error sets).
    pub type Error = Box<dyn std::error::Error + Send + Sync>;

    /// The interface `generic.zig` expects of a graphics API. Zig checks
    /// optional hooks with `@hasDecl`; here they have no-op defaults.
    ///
    /// TODO(port-dedupe): move to `generic.rs` with the generic renderer.
    pub trait GraphicsApi: Sized {
        /// Frames exported to the apprt (Zig: `void` when the API does not
        /// export frames).
        type ExportedFrame: Send;

        /// Whether the API exports frames (`ExportedFrame != void`).
        const EXPORTS_FRAMES: bool;

        /// Release the API's resources.
        fn deinit(&mut self);

        fn thread_enter(&mut self, _surface: *mut apprt::Surface) -> Result<(), Error> {
            Ok(())
        }

        fn thread_exit(&mut self) {}

        /// Zig: `loopEnter(self)`. The renderer pointer stands in for
        /// `@fieldParentPtr("api", self)`.
        fn loop_enter(&mut self, _renderer: *mut GenericRenderer<Self>) {}

        fn loop_exit(&mut self) {}
    }

    /// A health value that can be read and written atomically (Zig:
    /// `std.atomic.Value(Health)`).
    #[derive(Debug)]
    pub struct AtomicHealth(AtomicU8);

    impl AtomicHealth {
        pub const fn new(v: Health) -> AtomicHealth {
            AtomicHealth(AtomicU8::new(v as u8))
        }

        pub fn load(&self) -> Health {
            match self.0.load(Ordering::SeqCst) {
                0 => Health::Healthy,
                _ => Health::Unhealthy,
            }
        }

        pub fn store(&self, v: Health) {
            self.0.store(v as u8, Ordering::SeqCst);
        }
    }

    /// A thread-safe, single-slot "latest wins" queue. The render thread
    /// calls `push` with the latest frame; the apprt calls `take` in its
    /// snapshot handler to grab the most recent frame. Old frames are
    /// dropped and released. For a terminal this is correct — we never
    /// want to queue up frames behind a slow compositor.
    #[derive(Debug)]
    pub struct LatestFrame<F> {
        pub latest: std::sync::Mutex<Option<F>>,
    }

    impl<F> Default for LatestFrame<F> {
        fn default() -> Self {
            LatestFrame {
                latest: std::sync::Mutex::new(None),
            }
        }
    }

    impl<F> LatestFrame<F> {
        pub fn push(&self, value: F) {
            // Old frames are released on drop.
            *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        }

        pub fn take(&self) -> Option<F> {
            self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
        }
    }

    /// The parts of the generic renderer that the graphics API wrappers
    /// (`OpenGL.zig`, `Metal.zig`, their `Frame.zig`) and `Thread.zig`
    /// use, with the same names. Frame building/drawing is not modelled
    /// (`update_frame`/`draw_frame` only do the bookkeeping that does not
    /// need the terminal).
    ///
    /// TODO(port-dedupe): replace with `generic::Renderer`.
    pub struct GenericRenderer<G: GraphicsApi> {
        /// This mutex must be held whenever any state used in `drawFrame` is
        /// being modified, and also when it's being accessed in `drawFrame`.
        pub draw_mutex: Mutex,

        /// The configuration we need derived from the main config.
        pub config: RendererDerivedConfig,

        /// The mailbox for communicating with the window.
        pub surface_mailbox: apprt::surface::Mailbox,

        /// The latest frame exported for the apprt.
        pub latest_frame: LatestFrame<G::ExportedFrame>,

        /// The size of everything.
        pub size: Size,

        /// True if the window is focused
        pub focused: bool,

        /// Flag to indicate that our focus state changed for custom
        /// shaders to update their state.
        pub custom_shader_focused_changed: bool,

        /// True if the window is visible.
        pub visible: bool,

        /// Search matches from the search thread.
        pub search_matches: Option<SearchMatches>,
        pub search_selected_match: Option<SearchMatch>,
        pub search_matches_dirty: bool,

        /// The current font grid.
        pub font_grid: *mut SharedGrid,

        /// Graphics API state.
        pub api: G,

        /// The most recent health of the renderer.
        pub health: AtomicHealth,

        /// How well the apprt can present our frames.
        pub presentation_health: AtomicHealth,

        /// Whether the display is realized (see `displayRealized`).
        pub display_realized: bool,

        /// Whether custom shaders are configured.
        pub has_custom_shaders: bool,

        /// Kitty animation clock origin and next frame deadline (ms on that
        /// clock).
        pub kitty_animation_clock: Option<Instant>,
        pub kitty_animation_next_ms: Option<u64>,
    }

    /// A point in the future when the renderer needs to be driven
    /// again to keep animating, and what kind of drive it needs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AnimationWake {
        /// Delay in milliseconds until the wake is due.
        pub delay_ms: u64,
        pub kind: AnimationWakeKind,
    }

    /// Zig: `AnimationWake.Kind`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum AnimationWakeKind {
        /// A redraw alone suffices, no updateFrame. Much cheaper
        /// than `update`.
        #[default]
        Draw,

        /// Frame data must be updated first: updateFrame, then draw.
        Update,
    }

    impl<G: GraphicsApi> GenericRenderer<G> {
        /// The cadence of continuous (draw-only) animation wakes,
        /// i.e. 120fps, and the floor for any animation wake delay.
        pub const DRAW_INTERVAL_MS: u64 = 8;

        /// Create a renderer around an initialized graphics API.
        pub fn init_with_api(api: G, options: Options) -> Self {
            let has_custom_shaders = !options.config.custom_shaders.value.is_empty();
            GenericRenderer {
                draw_mutex: Mutex::new(),
                config: options.config,
                surface_mailbox: options.surface_mailbox,
                latest_frame: LatestFrame::default(),
                size: options.size,
                focused: true,
                custom_shader_focused_changed: false,
                visible: true,
                search_matches: None,
                search_selected_match: None,
                search_matches_dirty: false,
                font_grid: options.font_grid,
                api,
                health: AtomicHealth::new(Health::Healthy),
                presentation_health: AtomicHealth::new(Health::Healthy),
                display_realized: true,
                has_custom_shaders,
                kitty_animation_clock: None,
                kitty_animation_next_ms: None,
            }
        }

        pub fn deinit(&mut self) {
            self.api.deinit();
        }

        /// Callback called by renderer.Thread when it begins.
        pub fn thread_enter(&mut self, surface: *mut apprt::Surface) -> Result<(), Error> {
            self.api.thread_enter(surface)
        }

        /// Callback called by renderer.Thread when it exits.
        pub fn thread_exit(&mut self) {
            {
                self.draw_mutex.lock_uncancelable(TinyIo::INIT);
                self.display_realized = false;
                self.release_gpu_resources();
                self.draw_mutex.unlock(TinyIo::INIT);
            }
            self.api.thread_exit();
        }

        /// Called by renderer.Thread when it starts the main loop.
        pub fn loop_enter(&mut self, _thr: *mut Thread) -> Result<(), Error> {
            let this: *mut Self = self;
            self.api.loop_enter(this);
            Ok(())
        }

        /// Called by renderer.Thread when it exits the main loop.
        pub fn loop_exit(&mut self) {
            self.api.loop_exit();
        }

        /// Push the latest completed frame, replacing if one previously
        /// existed. Called on the render thread.
        pub fn push_frame(&mut self, frame: G::ExportedFrame) {
            if !G::EXPORTS_FRAMES {
                return;
            }
            self.latest_frame.push(frame);
        }

        /// Take the latest completed frame for the apprt to composite.
        pub fn take_frame(&mut self) -> Option<G::ExportedFrame> {
            if !G::EXPORTS_FRAMES {
                return None;
            }
            self.latest_frame.take()
        }

        /// Called when the window moves to a different display.
        pub fn set_macos_display_id(&mut self, _id: u32, _draw_now: *mut xev::Async) -> Result<(), Error> {
            Ok(())
        }

        /// The soonest animation wake this renderer needs, if any.
        pub fn animation_wake(&self) -> Option<AnimationWake> {
            // Custom shaders animate by redrawing on a fixed cadence,
            // gated by configuration and focus.
            let shader_delay: Option<u64> = if !self.has_custom_shaders {
                None
            } else {
                match self.config.custom_shader_animation {
                    CustomShaderAnimation::False => None,
                    CustomShaderAnimation::Always => Some(Self::DRAW_INTERVAL_MS),
                    CustomShaderAnimation::True => self.focused.then_some(Self::DRAW_INTERVAL_MS),
                }
            };

            // Kitty animations tick during updateFrame.
            let kitty_delay: Option<u64> = match (self.kitty_animation_next_ms, self.kitty_animation_clock) {
                (Some(next), Some(base)) => {
                    let now_ms = base.elapsed().as_millis() as u64;
                    Some(next.saturating_sub(now_ms).max(Self::DRAW_INTERVAL_MS))
                }
                _ => None,
            };

            // An update wake includes a draw, so it wins ties.
            if let Some(k) = kitty_delay {
                if shader_delay.is_none_or(|s| k <= s) {
                    return Some(AnimationWake {
                        delay_ms: k,
                        kind: AnimationWakeKind::Update,
                    });
                }
            }

            shader_delay.map(|s| AnimationWake {
                delay_ms: s,
                kind: AnimationWakeKind::Draw,
            })
        }

        /// Returns true if the display link is running.
        pub fn has_vsync(&self) -> bool {
            false
        }

        /// Callback when the focus changes for the terminal this is rendering.
        pub fn set_focus(&mut self, focus: bool) -> Result<(), Error> {
            debug_assert!(self.focused != focus);
            self.focused = focus;
            self.custom_shader_focused_changed = true;
            Ok(())
        }

        pub fn set_presentation_health(&mut self, health: Health) {
            self.presentation_health.store(health);
        }

        pub fn presentation_health(&self) -> Health {
            self.presentation_health.load()
        }

        /// Callback when the window is visible or occluded.
        pub fn set_visible(&mut self, visible: bool) {
            self.visible = visible;
            if !visible {
                self.draw_mutex.lock_uncancelable(TinyIo::INIT);
                self.release_gpu_resources();
                self.draw_mutex.unlock(TinyIo::INIT);
            }
        }

        /// Release the GPU resources we hold while the surface is not
        /// visible. Caller must lock the draw mutex.
        pub fn release_gpu_resources(&mut self) {}

        pub fn set_font_grid(&mut self, grid: *mut SharedGrid) {
            self.draw_mutex.lock_uncancelable(TinyIo::INIT);
            self.font_grid = grid;
            self.draw_mutex.unlock(TinyIo::INIT);
        }

        /// Update the frame data from the terminal state.
        pub fn update_frame(&mut self, _state: &State, _cursor_blink_visible: bool) -> Result<(), Error> {
            Ok(())
        }

        /// Draw the frame to the screen.
        pub fn draw_frame(&mut self, _sync: bool) -> Result<(), Error> {
            Ok(())
        }

        /// Callback from the graphics API when a frame is completed.
        pub fn frame_completed(&mut self, health: Health) {
            if self.health.load() != health {
                self.health.store(health);
                let _ = self.surface_mailbox.push(
                    apprt::surface::Message::RendererHealth(health),
                    QueueTimeout::Forever,
                );
            }
        }

        pub fn change_config(&mut self, config: &mut RendererDerivedConfig) -> Result<(), Error> {
            self.draw_mutex.lock_uncancelable(TinyIo::INIT);
            self.has_custom_shaders = !config.custom_shaders.value.is_empty();
            self.config = std::mem::take(config);
            self.draw_mutex.unlock(TinyIo::INIT);
            Ok(())
        }

        pub fn set_screen_size(&mut self, size: Size) {
            self.draw_mutex.lock_uncancelable(TinyIo::INIT);
            self.size = size;
            self.draw_mutex.unlock(TinyIo::INIT);
        }
    }

    /// The crash-reporting view of the surface behind a mailbox.
    ///
    /// TODO(port-dedupe): Zig stores `surface_mailbox.surface` (`*Surface`)
    /// in the crash thread state; until the core `Surface` is ported there
    /// is nothing to report.
    pub fn crash_surface(_mailbox: &apprt::surface::Mailbox) -> *const dyn CrashSurface {
        struct NoCrashSurface;
        impl CrashSurface for NoCrashSurface {
            fn crash_dimensions(&self) -> crate::crash::sentry::SurfaceDimensions {
                crate::crash::sentry::SurfaceDimensions::default()
            }
        }
        static NO_CRASH_SURFACE: NoCrashSurface = NoCrashSurface;
        &NO_CRASH_SURFACE as &dyn CrashSurface as *const dyn CrashSurface
    }

    /// TODO(port-dedupe): replace with `Renderer::DerivedConfig`
    /// (`renderer/generic.zig`).
    #[derive(Debug, Clone)]
    pub struct RendererDerivedConfig {
        pub blending: AlphaBlending,
        pub custom_shaders: RepeatablePath,
        pub custom_shader_animation: CustomShaderAnimation,
    }

    impl Default for RendererDerivedConfig {
        fn default() -> Self {
            RendererDerivedConfig {
                blending: AlphaBlending::Native,
                custom_shaders: RepeatablePath::default(),
                custom_shader_animation: CustomShaderAnimation::True,
            }
        }
    }

    impl RendererDerivedConfig {
        pub fn init(config: &Config) -> Result<RendererDerivedConfig, InitError> {
            Ok(RendererDerivedConfig {
                blending: config.alpha_blending,
                custom_shaders: config.custom_shader.clone(),
                custom_shader_animation: config.custom_shader_animation,
            })
        }

        pub fn deinit(&mut self) {}
    }

    /// The type used for sending messages to the renderer thread.
    pub type ThreadMailbox = super::thread::Mailbox;

    /// Zig: `Thread.DerivedConfig`.
    pub type ThreadDerivedConfig = super::thread::DerivedConfig;

    pub type SharedGrid = crate::font::SharedGrid;
    pub type SharedGridSet = crate::font::SharedGridSet;
    pub type SharedGridSetKey = crate::font::shared_grid_set::Key;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_h_health() {
        crate::lib::check_ghostty_h_enum::<Health>(
            "GHOSTTY_RENDERER_HEALTH_",
            include_str!("../include/ghostty.h"),
        )
        .unwrap();
    }
}
