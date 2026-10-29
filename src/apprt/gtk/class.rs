//! Port of `src/apprt/gtk/class.zig`.
//!
//! STATUS: partial — the `Application`, `Window`, `Surface` and
//! `RenderSurface` re-exports wait for `class/{application,window,surface,
//! render_surface}.zig` (ported concurrently); until then the classes in
//! this directory use the minimal stand-ins in [`port_stubs`]
//! (TODO(port-dedupe)).
//!
//! This files contains all the GObject classes for the GTK apprt
//! along with helpers to work with them.
//!
//! # Conventions for GObject classes in the Rust port
//!
//! Zig defines every class by hand with zig-gobject (`defineClass`,
//! `defineProperty`, `defineSignal`, a `Private` struct and the [`Common`]
//! helpers below). The port builds each class on gtk-rs' `glib::subclass`
//! with the same shape, so that a Zig class reads one-to-one against its
//! Rust port:
//!
//! | Zig | Rust |
//! | --- | --- |
//! | `pub const Foo = extern struct { ... }` + `getGObjectType` | `glib::wrapper! { pub struct Foo(ObjectSubclass<imp::Foo>) @extends ...; }` |
//! | `defineClass(.{ .name = "GhosttyFoo" })` | `impl ObjectSubclass for imp::Foo { const NAME = "GhosttyFoo"; }` (same GType name: templates and CSS refer to it) |
//! | `Private` struct (`self.private()`) | the `imp::Foo` struct with `Cell`/`RefCell` fields (`self.imp()`, see [`Common::private`]) |
//! | `instanceInit` / `initTemplate` | `ObjectSubclass::instance_init` calling `obj.init_template()` |
//! | `Class.init` | `ObjectSubclass::class_init` |
//! | `setTemplateFromResource(gresource.blueprint(..))` | `#[derive(gtk4::CompositeTemplate)]` + `#[template(resource = "...")]` and [`ensure_resources`] before `klass.bind_template()`; each class checks its literal against [`blueprint`] in a test |
//! | `bindTemplateChildPrivate("name")` | `#[template_child] pub name: TemplateChild<T>` |
//! | `bindTemplateCallback("name", &func)` | [`bind_template_callback`] with a [`TemplateArgs`] closure (same argument order as the C callback) |
//! | `properties.foo` + `registerProperties` | `ObjectImpl::properties()` built from `glib::ParamSpec*::builder`, names in a `pub mod properties` of string constants |
//! | `privateShallowFieldAccessor`, `privateObjFieldAccessor`, `privateBoxedFieldAccessor`, `privateStringFieldAccessor` | [`accessor`] helpers on `Cell`/`RefCell` private fields, used from `property`/`set_property` |
//! | `typedAccessor(.{ .getter = f, .setter = g })` | `property`/`set_property` arms calling the Rust getter/setter |
//! | `notifyByPspec(properties.foo.impl.param_spec)` | `obj.notify(properties::FOO)` |
//! | `signals.foo` + `defineSignal` + `impl.emit` | `ObjectImpl::signals()` with `glib::subclass::Signal::builder`, `obj.emit_by_name::<()>(signals::FOO, &[..])` |
//! | `signals.foo.connect(obj, T, cb, ud, .{})` | a typed `connect_foo(&self, f)` method on the wrapper |
//! | pointer signal parameters (`*const Binding.Action`) | `G_TYPE_POINTER` parameters, see [`pointer_value`] / [`pointer_from_value`] |
//! | `defineEnum` | `#[derive(glib::Enum)] #[enum_type(name = "...")]` (nicks are the Zig tag names) |
//! | `defineBoxed` | `#[derive(glib::Boxed)] #[boxed_type(name = "...")]` |
//! | `dispose` + `disposeTemplate` | `ObjectImpl::dispose` calling `self.dispose_template()`; the parent class' dispose is chained by gtk-rs |
//! | `finalize` | `Drop` of the private struct (gtk-rs chains the parent `finalize`) |
//! | `defineVirtualMethod("name")` | a `#[repr(C)]` class struct with function pointer fields plus `unsafe impl IsSubclassable` (see `class/imgui_widget.rs`) |
//! | `ref`/`refSink`/`unref` | `clone()` / [`Common::ref_sink`] / `drop` |
//!
//! A complete, minimal example of a templated widget following these
//! conventions (the shape of `class/debug_warning.rs` plus one property,
//! one signal and one template callback):
//!
//! ```ignore
//! mod imp {
//!     #[derive(Default, gtk4::CompositeTemplate)]
//!     #[template(resource = "/com/mitchellh/ghostty/ui/1.0/example.ui")]
//!     pub struct Example {
//!         // Private data (Zig `Private`)
//!         pub flag: Cell<bool>,
//!         // Template bindings (Zig `bindTemplateChildPrivate("label")`)
//!         #[template_child]
//!         pub label: TemplateChild<gtk4::Label>,
//!     }
//!
//!     #[glib::object_subclass]
//!     impl ObjectSubclass for Example {
//!         const NAME: &'static str = "GhosttyExample";
//!         type Type = super::Example;
//!         type ParentType = libadwaita::Bin;
//!
//!         fn class_init(klass: &mut Self::Class) {
//!             class::ensure_resources();
//!             klass.bind_template();
//!             // Zig: class.bindTemplateCallback("clicked", &clicked)
//!             class::bind_template_callback(klass, "clicked", |args| {
//!                 // (button: *gtk.Button, self: *Self)
//!                 let this: super::Example = args.template();
//!                 this.emit_by_name::<()>(signals::ACTIVATED, &[]);
//!                 None
//!             });
//!         }
//!
//!         fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
//!             obj.init_template();
//!         }
//!     }
//!
//!     impl ObjectImpl for Example {
//!         fn properties() -> &'static [glib::ParamSpec] { /* ParamSpecBoolean "flag" */ }
//!         fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
//!             match pspec.name() {
//!                 properties::FLAG => accessor::get_shallow(&self.flag),
//!                 _ => unreachable!(),
//!             }
//!         }
//!         fn set_property(&self, _: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
//!             match pspec.name() {
//!                 properties::FLAG => accessor::set_shallow(&self.flag, value),
//!                 _ => unreachable!(),
//!             }
//!         }
//!         fn signals() -> &'static [Signal] { /* Signal::builder("activated") */ }
//!         fn dispose(&self) {
//!             self.dispose_template();
//!         }
//!     }
//!     impl WidgetImpl for Example {}
//!     impl BinImpl for Example {}
//! }
//!
//! glib::wrapper! {
//!     pub struct Example(ObjectSubclass<imp::Example>)
//!         @extends libadwaita::Bin, gtk4::Widget,
//!         @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
//! }
//! ```
//!
//! GTK may only be used from one thread: smoke tests instantiate widgets on
//! the shared GTK test thread (`ext::test_gtk::run`).
#![cfg_attr(not(feature = "gtk"), allow(unused))]

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "class/application.rs"]
pub mod application;
#[path = "class/clipboard_confirmation_dialog.rs"]
pub mod clipboard_confirmation_dialog;
#[path = "class/close_confirmation_dialog.rs"]
pub mod close_confirmation_dialog;
#[path = "class/command_palette.rs"]
pub mod command_palette;
#[path = "class/config.rs"]
pub mod config;
#[path = "class/config_errors_dialog.rs"]
pub mod config_errors_dialog;
#[path = "class/debug_warning.rs"]
pub mod debug_warning;
#[path = "class/global_shortcuts.rs"]
pub mod global_shortcuts;
#[path = "class/imgui_widget.rs"]
pub mod imgui_widget;
#[path = "class/inspector_widget.rs"]
pub mod inspector_widget;
#[path = "class/inspector_window.rs"]
pub mod inspector_window;
#[path = "class/key_state_overlay.rs"]
pub mod key_state_overlay;
#[path = "class/overrides.rs"]
pub mod overrides;
#[path = "class/render_surface.rs"]
pub mod render_surface;
#[path = "class/resize_overlay.rs"]
pub mod resize_overlay;
#[path = "class/search_overlay.rs"]
pub mod search_overlay;
#[path = "class/split_tree.rs"]
pub mod split_tree;
#[path = "class/surface.rs"]
pub mod surface;
#[path = "class/surface_child_exited.rs"]
pub mod surface_child_exited;
#[path = "class/surface_scrolled_window.rs"]
pub mod surface_scrolled_window;
#[path = "class/tab.rs"]
pub mod tab;
#[path = "class/title_dialog.rs"]
pub mod title_dialog;
#[path = "class/window.rs"]
pub mod window;
// @generated-mods-end

#[cfg(feature = "gtk")]
pub use self::gtk_impl::*;

#[cfg(feature = "gtk")]
mod gtk_impl {
    use std::cell::{Cell, RefCell};

    use glib::prelude::*;
    use glib::translate::*;
    use gtk4::subclass::prelude::*;

    pub use crate::apprt::gtk::build::gresource::{blueprint, Blueprint};

    // TODO(port-dedupe): `pub use application::Application`,
    // `pub use window::Window`, `pub use surface::Surface` and
    // `pub use render_surface::RenderSurface` once those are ported.
    pub use super::config::Config;

    /// Common methods for all GObject classes we create.
    ///
    /// Zig's `Common(Self, Private)` returns a namespace of helpers that
    /// each class re-exports. In Rust they are a blanket extension trait
    /// over every GObject wrapper type.
    pub trait Common: ObjectType {
        /// Upcast our type to a parent type or interface. This will fail at
        /// compile time if the cast isn't 100% safe. For unsafe casts,
        /// use `downcast` instead. We don't have a helper for that
        /// because its uncommon and unsafe behavior should be noisier.
        fn as_<T: ObjectType>(&self) -> &T
        where
            Self: IsA<T>,
        {
            self.upcast_ref::<T>()
        }

        /// Increase the reference count of the object.
        fn ref_(&self) -> Self {
            self.clone()
        }

        /// If the reference count is 1 and the object is floating, clear the
        /// floating attribute. Otherwise, increase the reference count by 1.
        ///
        /// (gtk-rs never hands out floating references, so in practice this
        /// takes a new reference.)
        fn ref_sink(&self) -> Self {
            // SAFETY: `self` is a valid object; `g_object_ref_sink` returns
            // the same object with a reference we own (wrappers never hold
            // a floating reference).
            unsafe {
                let ptr = glib::gobject_ffi::g_object_ref_sink(
                    self.as_ptr() as *mut glib::gobject_ffi::GObject,
                );
                let obj: glib::Object = from_glib_full(ptr);
                obj.unsafe_cast()
            }
        }

        /// Decrease the reference count of the object.
        fn unref(self) {
            drop(self);
        }

        /// Access the private data of the object. This should be forwarded
        /// via a non-pub const usually. (gtk-rs: `imp()`.)
        fn private(&self) -> &<Self as glib::object::ObjectSubclassIs>::Subclass
        where
            Self: ObjectSubclassIsExt,
        {
            self.imp()
        }

        /// Get the class for the object.
        ///
        /// Every GObject instance starts with a `GTypeInstance` whose first
        /// field points to its class structure, which is what gtk-rs'
        /// `ObjectExt::class` reads.
        fn get_class(&self) -> &glib::Class<Self>
        where
            Self: glib::object::IsClass,
        {
            ObjectExt::class(self)
        }
    }

    impl<T: ObjectType> Common for T {}

    /// Helpers that implement properties reading and writing private
    /// fields (Zig: `privateShallowFieldAccessor`,
    /// `privateObjFieldAccessor`, `privateBoxedFieldAccessor` and
    /// `privateStringFieldAccessor`).
    ///
    /// Reference counting and freeing of the previous value is done by the
    /// Rust types themselves (`Option<Object>` unrefs on replace, boxed
    /// and string values are owned copies), so all non-shallow accessors
    /// share one implementation.
    pub mod accessor {
        use super::*;

        /// A property that reads and writes a private field with only
        /// shallow copies. This is good for primitives such as bools,
        /// numbers, etc.
        pub fn get_shallow<T: ToValue + Copy>(field: &Cell<T>) -> glib::Value {
            field.get().to_value()
        }

        /// See [`get_shallow`].
        pub fn set_shallow<T>(field: &Cell<T>, value: &glib::Value)
        where
            T: for<'a> glib::value::FromValue<'a> + Copy,
        {
            field.set(value.get::<T>().expect("property value type mismatch"));
        }

        /// A property that reads and writes a private object (reference
        /// counted), boxed or string field. Reading the property copies or
        /// refs the value; writing it releases the previous value.
        pub fn get<T: ToValue>(field: &RefCell<T>) -> glib::Value {
            field.borrow().to_value()
        }

        /// See [`get`].
        pub fn set<T>(field: &RefCell<T>, value: &glib::Value)
        where
            T: for<'a> glib::value::FromValue<'a>,
        {
            let v = value.get::<T>().expect("property value type mismatch");
            // Drop the old value outside of the borrow.
            let _old = field.replace(v);
        }
    }

    /// Registers the GResource bundle (UI templates, CSS, icons) before a
    /// class loads its template from it. In Zig the bundle is linked into
    /// the binary and registered by a constructor before any class exists.
    pub fn ensure_resources() {
        #[cfg(ghostty_gresource)]
        crate::apprt::gtk::build::gresource::register();
    }

    /// The arguments a template callback is invoked with.
    ///
    /// The values are in the order of the C callback that Zig binds with
    /// `bindTemplateCallback`: for signal handlers `[emitter, signal
    /// arguments..., self]`, and for `<closure>` functions `[self,
    /// arguments..., self]`. [`TemplateArgs::get`]`(i)` returns the `i`th
    /// parameter of the Zig callback and [`TemplateArgs::template`] the
    /// trailing user-data `self`.
    pub struct TemplateArgs<'a> {
        values: &'a [glib::Value],
    }

    impl<'a> TemplateArgs<'a> {
        pub fn new(values: &'a [glib::Value]) -> Self {
            TemplateArgs { values }
        }

        /// The number of values, including the trailing template object.
        pub fn len(&self) -> usize {
            self.values.len()
        }

        pub fn is_empty(&self) -> bool {
            self.values.is_empty()
        }

        /// The raw `i`th value.
        pub fn value(&self, i: usize) -> &'a glib::Value {
            &self.values[i]
        }

        /// The `i`th value converted to `T`. Panics on a type mismatch, like
        /// a C callback with the wrong signature would misbehave.
        pub fn get<T>(&self, i: usize) -> T
        where
            T: for<'b> glib::value::FromValue<'b>,
        {
            let v = self.values.get(i).unwrap_or_else(|| {
                panic!("template callback argument {i} missing (have {})", self.values.len())
            });
            v.get::<T>()
                .unwrap_or_else(|e| panic!("template callback argument {i}: {e:?}"))
        }

        /// The template object (the callback's `self` user data).
        pub fn template<T: IsA<glib::Object>>(&self) -> T {
            let obj: glib::Object = self.get(self.values.len() - 1);
            obj.downcast::<T>()
                .unwrap_or_else(|o| panic!("template object has type {}", o.type_()))
        }
    }

    /// Bind a function to a template callback symbol (Zig:
    /// `Class.bindTemplateCallback`). The function returns the C return
    /// value as a `glib::Value` (`None` for `void`). Booleans are returned
    /// as `bool` values (Zig returns `c_int` for the same ABI reason).
    pub fn bind_template_callback<C, F>(klass: &mut C, name: &str, func: F)
    where
        C: WidgetClassExt,
        F: Fn(&TemplateArgs<'_>) -> Option<glib::Value> + 'static,
    {
        klass
            .rust_template_scope()
            .add_callback(name, move |values| func(&TemplateArgs::new(values)));
    }

    /// Wraps a reference as a `G_TYPE_POINTER` value, for signals whose Zig
    /// parameter is a plain pointer (e.g. `*const input.Binding.Action`).
    /// The pointer is only valid for the duration of the emission.
    pub fn pointer_value<T>(v: &T) -> glib::Value {
        (v as *const T as glib::Pointer).to_value()
    }

    /// The reverse of [`pointer_value`].
    ///
    /// # Safety
    /// `value` must hold a pointer created by [`pointer_value`] from a `T`
    /// that is still alive (i.e. this is called during the emission).
    pub unsafe fn pointer_from_value<'a, T>(value: &glib::Value) -> &'a T {
        let ptr = value.get::<glib::Pointer>().expect("pointer value");
        &*(ptr as *const T)
    }

    /// GObject wrappers for apprt structs that Zig makes GObject types via
    /// `getGObjectType` in `apprt/structs.zig` and `apprt/surface.zig`.
    pub mod boxed {
        /// `apprt.ClipboardRequest` as the boxed type
        /// `GhosttyClipboardRequest`.
        #[derive(Debug, Clone, PartialEq, Eq, glib::Boxed)]
        #[boxed_type(name = "GhosttyClipboardRequest", nullable)]
        pub struct ClipboardRequest(pub crate::apprt::structs::ClipboardRequest);

        /// `apprt.surface.Message.ChildExited` as the boxed type
        /// `GhosttyApprtChildExited`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Boxed)]
        #[boxed_type(name = "GhosttyApprtChildExited", nullable)]
        pub struct ChildExited(pub crate::apprt::surface::ChildExited);
    }

    /// Minimal stand-ins for the classes and core types that are not
    /// ported yet but that the classes in this directory use. Each mirrors
    /// the Zig names of the methods used.
    ///
    /// The GObject stand-ins register the real GType names
    /// (`GhosttySurface`, `GhosttySplitTree`) because the UI templates refer
    /// to them by name, so they must be removed when the real classes land.
    // TODO(port-dedupe): replace with crate::apprt::gtk::class::{
    // application::Application, surface::Surface, split_tree::SplitTree,
    // window::Window} and crate::surface::Surface (core).
    pub mod port_stubs {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;
        use std::sync::LazyLock;

        use glib::prelude::*;
        use glib::subclass::prelude::*;
        use gtk4::prelude::*;
        use gtk4::subclass::prelude::*;
        use libadwaita::subclass::prelude::*;

        use super::accessor;
        use crate::apprt::gtk::class::config::Config;
        use crate::apprt::gtk::class::overrides::Overrides;
        use crate::apprt::gtk::winproto;
        use crate::apprt::surface::NewSurfaceContext;
        use crate::datastruct::split_tree as dst;

        /// Stand-in for the core `Surface` (`src/Surface.zig`): only what the
        /// GTK classes here read.
        // TODO(port-dedupe): replace with crate::surface::Surface
        #[derive(Debug, Default)]
        pub struct CoreSurface {
            /// `Surface.id`
            pub id: u64,
            inspector_active: Cell<bool>,
        }

        /// `error{...}` of `activateInspector`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct InspectorError;

        impl CoreSurface {
            pub fn new(id: u64) -> Rc<CoreSurface> {
                Rc::new(CoreSurface { id, inspector_active: Cell::new(false) })
            }

            /// `Surface.activateInspector`
            pub fn activate_inspector(&self) -> Result<(), InspectorError> {
                self.inspector_active.set(true);
                Ok(())
            }

            /// `Surface.deactivateInspector`
            pub fn deactivate_inspector(&self) {
                self.inspector_active.set(false);
            }

            /// Whether `Surface.inspector` is set.
            pub fn inspector_active(&self) -> bool {
                self.inspector_active.get()
            }
        }

        /// Stand-in for `class/application.zig`'s `Application`, holding the
        /// application-wide state the classes here read through
        /// `Application.default()`.
        // TODO(port-dedupe): replace with
        // crate::apprt::gtk::class::application::Application
        #[derive(Debug, Clone, Copy)]
        pub struct Application;

        #[derive(Default)]
        struct AppState {
            config: RefCell<Option<Config>>,
            surfaces: RefCell<Vec<glib::WeakRef<Surface>>>,
            winproto: RefCell<Option<winproto::App>>,
        }

        thread_local! {
            static APP: AppState = AppState::default();
        }

        impl Application {
            /// `Application.default()`
            pub fn default() -> Application {
                Application
            }

            /// `getConfig`: the application configuration (a new reference).
            pub fn get_config(&self) -> Option<Config> {
                APP.with(|a| a.config.borrow().clone())
            }

            /// Stand-in hook: set the application configuration.
            pub fn set_config(&self, config: Option<Config>) {
                APP.with(|a| *a.config.borrow_mut() = config);
            }

            /// `core().surfaces[i].gobj()`: all surfaces of the app.
            pub fn surfaces(&self) -> Vec<Surface> {
                APP.with(|a| a.surfaces.borrow().iter().filter_map(|w| w.upgrade()).collect())
            }

            /// Stand-in hook: register a surface with the core app.
            pub fn add_surface(&self, surface: &Surface) {
                APP.with(|a| a.surfaces.borrow_mut().push(surface.downgrade()));
            }

            /// Stand-in hook: install the windowing protocol state.
            pub fn set_winproto(&self, wp: Option<winproto::App>) {
                APP.with(|a| *a.winproto.borrow_mut() = wp);
            }

            /// `winproto()`: runs `f` with the windowing protocol state, or
            /// returns `None` if there is none.
            pub fn with_winproto<R>(&self, f: impl FnOnce(&mut winproto::App) -> R) -> Option<R> {
                APP.with(|a| a.winproto.borrow_mut().as_mut().map(f))
            }
        }

        /// `Surface.Size`
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct Size {
            pub width: u32,
            pub height: u32,
        }

        mod surface_imp {
            use super::*;

            #[derive(Default)]
            pub struct Surface {
                pub config: RefCell<Option<Config>>,
                pub title: RefCell<Option<String>>,
                pub title_override: RefCell<Option<String>>,
                pub pwd: RefCell<Option<String>>,
                pub bell_ringing: Cell<bool>,
                pub core_surface: RefCell<Option<Rc<CoreSurface>>>,
                pub context: Cell<Option<NewSurfaceContext>>,
                pub split_min_size: Cell<Size>,
            }

            #[glib::object_subclass]
            impl ObjectSubclass for Surface {
                const NAME: &'static str = "GhosttySurface";
                type Type = super::Surface;
                type ParentType = libadwaita::Bin;
            }

            impl ObjectImpl for Surface {
                fn properties() -> &'static [glib::ParamSpec] {
                    static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                        vec![
                            glib::ParamSpecObject::builder::<Config>("config").build(),
                            glib::ParamSpecString::builder("title").build(),
                            glib::ParamSpecString::builder("title-override").build(),
                            glib::ParamSpecString::builder("pwd").build(),
                            glib::ParamSpecBoolean::builder("bell-ringing").build(),
                        ]
                    });
                    PROPS.as_ref()
                }

                fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
                    match pspec.name() {
                        "config" => accessor::get(&self.config),
                        "title" => accessor::get(&self.title),
                        "title-override" => accessor::get(&self.title_override),
                        "pwd" => accessor::get(&self.pwd),
                        "bell-ringing" => accessor::get_shallow(&self.bell_ringing),
                        _ => unreachable!(),
                    }
                }

                fn set_property(&self, _: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
                    match pspec.name() {
                        "config" => accessor::set(&self.config, value),
                        "title" => accessor::set(&self.title, value),
                        "title-override" => accessor::set(&self.title_override, value),
                        "pwd" => accessor::set(&self.pwd, value),
                        "bell-ringing" => accessor::set_shallow(&self.bell_ringing, value),
                        _ => unreachable!(),
                    }
                }
            }

            impl WidgetImpl for Surface {}
            impl BinImpl for Surface {}
        }

        glib::wrapper! {
            /// Stand-in for `class/surface.zig`'s `Surface`.
            // TODO(port-dedupe): replace with
            // crate::apprt::gtk::class::surface::Surface
            pub struct Surface(ObjectSubclass<surface_imp::Surface>)
                @extends libadwaita::Bin, gtk4::Widget,
                @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
        }

        impl Default for Surface {
            fn default() -> Self {
                Self::new(&Overrides::NONE)
            }
        }

        impl Surface {
            /// `Surface.new(overrides)`
            pub fn new(overrides: &Overrides) -> Surface {
                let this: Surface = glib::Object::new();
                if let Some(t) = &overrides.title {
                    this.imp().title_override.replace(Some(t.clone()));
                }
                this
            }

            /// `core()`
            pub fn core(&self) -> Option<Rc<CoreSurface>> {
                self.imp().core_surface.borrow().clone()
            }

            /// Stand-in hook: set the core surface (Zig: `initSurface`).
            pub fn set_core(&self, core: Option<Rc<CoreSurface>>) {
                self.imp().core_surface.replace(core);
            }

            /// `setParent`
            pub fn set_parent(&self, _parent: &CoreSurface, context: NewSurfaceContext) {
                if self.imp().core_surface.borrow().is_some() {
                    log::warn!(target: "gtk_ghostty_surface", "setParent called after surface is already realized");
                    return;
                }
                self.imp().context.set(Some(context));
            }

            /// The context stored by [`Surface::set_parent`].
            pub fn parent_context(&self) -> Option<NewSurfaceContext> {
                self.imp().context.get()
            }

            /// `getSplitMinSize`
            pub fn get_split_min_size(&self) -> Size {
                self.imp().split_min_size.get()
            }

            /// Stand-in hook for `setCellSize`/`syncSplitMinSize`.
            pub fn set_split_min_size(&self, size: Size) {
                self.imp().split_min_size.set(size);
            }

            /// `getEffectiveTitle`
            pub fn get_effective_title(&self) -> Option<String> {
                let imp = self.imp();
                imp.title_override.borrow().clone().or_else(|| imp.title.borrow().clone())
            }

            /// `getPwd`
            pub fn get_pwd(&self) -> Option<String> {
                self.imp().pwd.borrow().clone()
            }

            /// `getConfig`
            pub fn get_config(&self) -> Option<Config> {
                self.imp().config.borrow().clone()
            }

            /// `present`
            pub fn present(&self) {
                self.grab_focus();
            }
        }

        impl dst::View for Surface {
            fn eql(&self, other: &Self) -> bool {
                self == other
            }
        }

        /// `Surface.Tree` (`datastruct.SplitTree(Surface)`) as the boxed
        /// type `GhosttySplitTreeSurface`.
        // TODO(port-dedupe): replace with the surface module's tree type.
        #[derive(glib::Boxed)]
        #[boxed_type(name = "GhosttySplitTreeSurface", nullable)]
        pub struct SurfaceTree(pub dst::SplitTree<Surface>);

        impl Clone for SurfaceTree {
            fn clone(&self) -> Self {
                SurfaceTree(self.0.clone_tree())
            }
        }

        mod split_tree_imp {
            use super::*;

            #[derive(Default)]
            pub struct SplitTree {
                pub tree: RefCell<Option<SurfaceTree>>,
            }

            #[glib::object_subclass]
            impl ObjectSubclass for SplitTree {
                const NAME: &'static str = "GhosttySplitTree";
                type Type = super::SplitTree;
                type ParentType = gtk4::Box;
            }

            impl ObjectImpl for SplitTree {
                fn properties() -> &'static [glib::ParamSpec] {
                    static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                        vec![
                            glib::ParamSpecObject::builder::<Surface>("active-surface")
                                .read_only()
                                .build(),
                            glib::ParamSpecBoolean::builder("is-zoomed").read_only().build(),
                            glib::ParamSpecBoxed::builder::<SurfaceTree>("tree").build(),
                        ]
                    });
                    PROPS.as_ref()
                }

                fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
                    match pspec.name() {
                        "active-surface" => self.obj().get_active_surface().to_value(),
                        "is-zoomed" => false.to_value(),
                        "tree" => accessor::get(&self.tree),
                        _ => unreachable!(),
                    }
                }

                fn set_property(&self, _: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
                    match pspec.name() {
                        "tree" => self.obj().set_tree(value.get().unwrap()),
                        _ => unreachable!(),
                    }
                }
            }

            impl WidgetImpl for SplitTree {}
            impl BoxImpl for SplitTree {}
        }

        glib::wrapper! {
            /// Stand-in for `class/split_tree.zig`'s `SplitTree`.
            // TODO(port-dedupe): replace with
            // crate::apprt::gtk::class::split_tree::SplitTree
            pub struct SplitTree(ObjectSubclass<split_tree_imp::SplitTree>)
                @extends gtk4::Box, gtk4::Widget,
                @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget,
                    gtk4::Orientable;
        }

        impl SplitTree {
            /// `newSplit`
            pub fn new_split(
                &self,
                direction: dst::Direction,
                parent: Option<&Surface>,
                overrides: &Overrides,
            ) -> Result<(), dst::Error> {
                let surface = Surface::new(overrides);
                if let Some(p) = parent {
                    if let Some(core) = p.core() {
                        surface.set_parent(&core, NewSurfaceContext::Split);
                    }
                }
                let single = dst::SplitTree::init(&surface);
                let tree = match self.get_tree() {
                    Some(old) if !old.0.is_empty() => old.0.split(
                        dst::Handle(0),
                        direction,
                        dst::f16::from_f32(0.5),
                        &single,
                    )?,
                    _ => single,
                };
                self.set_tree(Some(SurfaceTree(tree)));
                Ok(())
            }

            /// `setTree`
            pub fn set_tree(&self, tree: Option<SurfaceTree>) {
                let _old = self.imp().tree.replace(tree);
                self.notify("tree");
                self.notify("active-surface");
            }

            /// `getTree`
            pub fn get_tree(&self) -> Option<SurfaceTree> {
                self.imp().tree.borrow().clone()
            }

            /// `getActiveSurface` (the stand-in returns the first surface).
            pub fn get_active_surface(&self) -> Option<Surface> {
                let tree = self.imp().tree.borrow();
                let tree = tree.as_ref()?;
                let first = tree.0.iterator().next().map(|e| e.view.clone());
                first
            }

            /// `getNeedsConfirmQuit`
            pub fn get_needs_confirm_quit(&self) -> bool {
                false
            }
        }

        /// Stand-in for `class/window.zig`'s `Window`.
        // TODO(port-dedupe): replace with crate::apprt::gtk::class::window::Window
        pub type Window = libadwaita::ApplicationWindow;
    }
}

#[cfg(all(test, feature = "gtk"))]
mod tests {
    use super::*;
    use glib::prelude::*;

    #[test]
    fn common_helpers() {
        let obj = glib::Object::new::<glib::Object>();
        let r = obj.ref_();
        assert_eq!(obj.ref_count(), 2);
        let s = obj.ref_sink();
        assert_eq!(obj.ref_count(), 3);
        s.unref();
        r.unref();
        assert_eq!(obj.ref_count(), 1);
        let _: &glib::Object = obj.as_();
        assert_eq!(obj.get_class().type_(), glib::Object::static_type());
    }

    #[test]
    fn pointer_values() {
        let x = 42u32;
        let v = pointer_value(&x);
        assert_eq!(v.type_(), glib::Type::POINTER);
        assert_eq!(*unsafe { pointer_from_value::<u32>(&v) }, 42);
    }

    #[test]
    fn template_args() {
        let values = [1i32.to_value(), "x".to_value(), glib::Object::new::<glib::Object>().to_value()];
        let args = TemplateArgs::new(&values);
        assert_eq!(args.len(), 3);
        assert_eq!(args.get::<i32>(0), 1);
        assert_eq!(args.get::<String>(1), "x");
        let _: glib::Object = args.template();
    }

    #[test]
    fn accessors() {
        use std::cell::{Cell, RefCell};
        let c = Cell::new(false);
        accessor::set_shallow(&c, &true.to_value());
        assert!(accessor::get_shallow(&c).get::<bool>().unwrap());
        let s: RefCell<Option<String>> = RefCell::new(None);
        accessor::set(&s, &Some("hi").to_value());
        assert_eq!(accessor::get(&s).get::<Option<String>>().unwrap().as_deref(), Some("hi"));
    }
}
