//! Native GPUI / Mana widget bindings for Desktop Phoenix User Settings.
//!
//! Resolves `Discord.exe`'s own compiled GPUI (`Div`, `StyleRefinement`,
//! `AnyElement`, `SharedString`) and Mana (`Switch`, `Button`, `TextBlock`,
//! `ActionRow`) builders from unique string anchors during [`crate::prepare`] —
//! no hardcoded addresses and no webview overlay. All controls render natively
//! inside Discord's GPUI render tree with category tabs, rounded bordered
//! cards, collapsible plugin settings dropdowns, and native select dropdowns.

use std::{
   collections::HashSet,
   ffi::c_void,
   mem::MaybeUninit,
   ptr,
   sync::{
      Arc,
      LazyLock,
      Mutex,
      OnceLock,
      PoisonError,
   },
};

use phoenix_hook::image::Image;
use phoenix_runtime::{
   Chosen,
   Control,
   update::{
      self,
      Catalog,
      Listed,
   },
};
use phoenix_sigs::{
   desktop::{
      self,
      GpuiLayout,
      StyleOffsets,
      layout,
   },
   locate,
};

use crate::modal::{
   ModalButton,
   ModalSection,
   PageModalSpec,
   SettingField,
};

/// `gpui::SharedString` (24 bytes): tag `0x18` is `Static(&'static str)` and
/// `0x19` is `Arc<str>`, the `ArcInner` at `ptr` with the bytes at `+0x10`.
/// A smaller tag is the length of up to 23 bytes stored inline after it.
/// Not `Copy`: a heap one must reach Discord exactly once, which drops it.
#[repr(C)]
pub struct SharedString {
   pub tag: u8,
   _pad:    [u8; 7],
   pub ptr: *const u8,
   pub len: usize,
}

impl SharedString {
   pub const fn from_static(s: &'static str) -> Self {
      Self {
         tag:  0x18,
         _pad: [0; 7],
         ptr:  s.as_ptr(),
         len:  s.len(),
      }
   }

   /// An `Arc<str>` on Discord's heap, which Discord frees with the element
   /// it moves into. Interned only before the builders resolve.
   pub fn from_str(s: &str) -> Self {
      let Some(sym) = SYMBOLS.get() else {
         return Self::from_static(intern(s));
      };
      // SAFETY: Discord's own allocator, with the layout `Arc<str>`'s drop
      // frees: two counts, then the bytes, padded to 8.
      let inner = unsafe { (sym.alloc_or_oom)(8, (16 + s.len()).next_multiple_of(8)) };
      if inner.is_null() {
         return Self::from_static(intern(s));
      }
      // SAFETY: `inner` holds `16 + s.len()` bytes, aligned to 8.
      unsafe {
         inner.cast::<[usize; 2]>().write_unaligned([1, 1]);
         ptr::copy_nonoverlapping(s.as_ptr(), inner.add(16), s.len());
      }
      Self {
         tag:  0x19,
         _pad: [0; 7],
         ptr:  inner.cast_const(),
         len:  s.len(),
      }
   }
}

/// Process-lifetime strings for element IDs and badge URLs, which Discord
/// borrows as `&'static str`. Only bounded sets of strings belong here.
static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
   LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn intern_str(s: &str) -> &'static str {
   intern(s)
}

fn intern(s: &str) -> &'static str {
   let mut set = INTERNED.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(&existing) = set.get(s) {
      return existing;
   }
   let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
   set.insert(leaked);
   leaked
}

/// A zeroed buffer at least as large as the Discord struct built in it, whose
/// size is decoded at startup and checked against `N` by `sigs`.
#[repr(C, align(16))]
pub struct Scratch<const N: usize>([u8; N]);

impl<const N: usize> Scratch<N> {
   pub const fn zeroed() -> Self {
      Self([0; N])
   }

   pub const fn as_mut_ptr(&mut self) -> *mut u8 {
      self.0.as_mut_ptr()
   }

   /// Writes `value` at `at`, or nothing if it would not fit.
   pub fn put<T>(&mut self, at: usize, value: T) {
      if at + size_of::<T>() <= N {
         // SAFETY: in bounds of `self.0`, checked above.
         unsafe {
            self
               .0
               .as_mut_ptr()
               .add(at)
               .cast::<T>()
               .write_unaligned(value);
         }
      }
   }
}

pub type RawDiv = Scratch<{ layout::DIV_CAP }>;
type RawSwitch = Scratch<{ layout::SCRATCH_CAP }>;
pub type RawButton = Scratch<{ layout::SCRATCH_CAP }>;

#[repr(C, align(8))]
pub struct RawAnyElement([u8; layout::ANY_ELEMENT]);

#[repr(C)]
pub struct RcInner {
   strong:    usize,
   weak:      usize,
   action_id: u64,
}

/// An `Rc<u64>` whose counts never reach zero, so Discord never frees it:
/// for when Discord's allocator isn't located (macOS, whose Rust heap may be
/// its own malloc zone). Leaks 24 bytes.
pub fn pinned_rc_u64(value: u64) -> *mut RcInner {
   Box::into_raw(Box::new(RcInner {
      strong:    usize::MAX / 4,
      weak:      usize::MAX / 4,
      action_id: value,
   }))
}

#[repr(C)]
pub struct RcFnVtbl {
   pub drop_in_place: *const c_void,
   pub size:          usize,
   pub align:         usize,
   pub call_once:     *const c_void,
   pub call_mut:      *const c_void,
   pub call:          *const c_void,
   pub extra:         *const c_void,
}

// SAFETY: vtables contain only function pointers and plain integers.
unsafe impl Sync for RcFnVtbl {}
// SAFETY: as above, nothing in it is tied to a thread.
unsafe impl Send for RcFnVtbl {}

static SWITCH_FN_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     switch_callback as *const c_void,
   call_mut:      switch_callback as *const c_void,
   call:          switch_callback as *const c_void,
   extra:         ptr::null(),
};

static BUTTON_FN_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     button_callback as *const c_void,
   call_mut:      button_callback as *const c_void,
   call:          button_callback as *const c_void,
   extra:         ptr::null(),
};

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum BtnVariant {
   Primary   = 0,
   Secondary = 1,
   Critical  = 3,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum BtnSize {
   Small  = 1,
   Medium = 2,
}

/// Encodes `Some(Length::Definite(DefiniteLength::Fraction(1.0)))` (`w_full` /
/// `h_full`) in GPUI's `StyleRefinement`.
pub const W_FULL: u64 = 0x3F80_0000_0000_0002;

/// Encodes `Some(AbsoluteLength::Pixels(px))` in GPUI's `StyleRefinement`.
pub fn gpui_px(px: f32) -> u64 {
   u64::from(px.to_bits()) << 32
}

/// Encodes `Some(AbsoluteLength::Rems(rems))`, which follows the window's rem
/// size and so Discord's zoom, as its own chat bar icons do.
pub fn gpui_rems(rems: f32) -> u64 {
   (u64::from(rems.to_bits()) << 32) | 1
}

/// Encodes `Some(Length::Definite(DefiniteLength::Fraction(f)))`, a share of
/// the parent like [`W_FULL`].
pub fn fraction(f: f32) -> u64 {
   (u64::from(f.to_bits()) << 32) | 2
}

/// `CursorStyle::PointingHand`.
const CURSOR_POINTING_HAND: u8 = 5;

/// A press on a plain element made clickable by [`Builder::clickable`]. It is
/// zero-sized, so the action's id plus one rides in its data pointer, which
/// GPUI never reads or frees.
static PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     element_press as *const c_void,
   call_mut:      element_press as *const c_void,
   call:          element_press as *const c_void,
   extra:         ptr::null(),
};

const unsafe extern "C" fn noop_drop(_data: *mut u8) {}

/// Rust ABI like the `Fn::call` slot it fills, see `modal::scrim_press`.
unsafe fn element_press(
   data: *const u8,
   _event: *mut u8,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   _app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase == crate::modal::PHASE_BUBBLE
         && crate::modal::hovered(hitbox, window)
         && let Some(action) = lookup_action((data as u64).wrapping_sub(1))
      {
         run(action);
         crate::modal::force_redraw();
      }
   });
}
type DivNewFn = unsafe extern "C" fn(*mut RawDiv, *const c_void) -> *mut RawDiv;
type DivMutFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv) -> *mut RawDiv;
type DivStyleMutFn = unsafe extern "C" fn(*mut RawDiv) -> *mut u8;
type DivChildDivFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv, *mut RawDiv) -> *mut RawDiv;
type DivChildBtnFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv, *mut RawButton) -> *mut RawDiv;
type DivIntoAnyFn = unsafe extern "C" fn(*mut RawAnyElement, *mut RawDiv) -> *mut RawAnyElement;
type DivPushAnyFn = unsafe extern "C" fn(*mut c_void, *mut RawAnyElement);
type StrIntoAnyFn =
   unsafe extern "C" fn(*mut RawAnyElement, *mut SharedString) -> *mut RawAnyElement;
type TextBlockFn =
   unsafe extern "C" fn(*mut RawDiv, *const SharedString, *const SharedString) -> *mut RawDiv;
type SwitchNewFn = unsafe extern "C" fn(*mut RawSwitch, *const u8, usize, u8) -> *mut RawSwitch;
type SwitchIntoDivFn = unsafe extern "C" fn(*mut RawDiv, *mut RawSwitch) -> *mut RawDiv;
type ButtonNewFn =
   unsafe extern "C" fn(*mut RawButton, *const u8, usize, *const SharedString) -> *mut RawButton;
type ActionRowFn = unsafe extern "C" fn(
   *mut RawDiv,
   *const SharedString,
   *const SharedString,
   *mut RawButton,
) -> *mut RawDiv;
/// Discord's `alloc_or_oom(align: usize, size: usize) -> *mut u8`.
type AllocOrOomFn = unsafe extern "C" fn(usize, usize) -> *mut u8;

struct GpuiSymbols {
   caller_loc:       *const c_void,
   div_new:          DivNewFn,
   div_flex:         DivMutFn,
   div_flex_col:     DivMutFn,
   div_style_mut:    DivStyleMutFn,
   div_child_div:    DivChildDivFn,
   div_child_button: DivChildBtnFn,
   div_into_any:     DivIntoAnyFn,
   /// Raw Discord address of `Div::into_any_element` (before `sret_caller`
   /// wrapping on arm64), for sibling locators like `crate::modal`.
   div_into_any_raw: usize,
   div_push_any:     Option<DivPushAnyFn>,
   str_into_any:     Option<StrIntoAnyFn>,
   text_block:       TextBlockFn,
   switch_new:       SwitchNewFn,
   switch_into_div:  SwitchIntoDivFn,
   button_new:       ButtonNewFn,
   action_row:       ActionRowFn,
   alloc_or_oom:     AllocOrOomFn,
   layout:           GpuiLayout,
}

// SAFETY: function pointers and static `.rdata` `Location` pointer in
// `Discord.exe`.
unsafe impl Send for GpuiSymbols {}
unsafe impl Sync for GpuiSymbols {}

static SYMBOLS: OnceLock<GpuiSymbols> = OnceLock::new();

// ---------------------------------------------------------------------------
// Interactive UI state (tabs, filters, collapsible dropdowns, open selects)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
   Plugins,
   Catalog,
   General,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginFilter {
   All,
   Enabled,
   Disabled,
}

#[derive(Clone)]
struct UiState {
   tab:                 Tab,
   filter:              PluginFilter,
   filter_open:         bool,
   catalog_filter:      Option<CatalogGroup>,
   catalog_filter_open: bool,
}

/// The catalog's plugins by desktop support.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CatalogGroup {
   Desktop,
   Partial,
   Other,
}

impl Default for UiState {
   fn default() -> Self {
      Self {
         tab:                 Tab::Plugins,
         filter:              PluginFilter::All,
         filter_open:         false,
         catalog_filter:      None,
         catalog_filter_open: false,
      }
   }
}

static UI_STATE: LazyLock<Mutex<UiState>> = LazyLock::new(|| Mutex::new(UiState::default()));

#[derive(Clone)]
enum Action {
   SetTab(Tab),
   SetFilter(PluginFilter),
   ToggleFilterMenu,
   SetCatalogFilter(Option<CatalogGroup>),
   ToggleCatalogFilterMenu,
   OpenPlugin(String),
   ToggleStaff,
   ToggleAutoStart,
   ToggleAutoUpdate,
   ReloadAll,
   OpenDevTools,
   CheckUpdates,
   ApplyUpdates,
   RefreshCatalog,
   TogglePlugin(String),
   EditPluginCode(String),
   OpenInstallUrl,
   InstallFromFile,
   UninstallPlugin(String),
   InstallCatalog(String),
}

static ACTIONS: LazyLock<Mutex<Vec<Action>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn register_action(action: Action) -> u64 {
   let mut list = ACTIONS.lock().unwrap_or_else(PoisonError::into_inner);
   let id = list.len() as u64;
   list.push(action);
   id
}

fn lookup_action(id: u64) -> Option<Action> {
   let list = ACTIONS.lock().unwrap_or_else(PoisonError::into_inner);
   usize::try_from(id)
      .ok()
      .and_then(|idx| list.get(idx).cloned())
}

/// Rust ABI like the `Fn::call` slot it fills: on Linux `extern "C"` would
/// assume the caller widened `checked`, which Rust callers do not.
unsafe fn switch_callback(
   self_ptr: *const u64,
   checked: u8,
   _window: *mut c_void,
   _app: *mut c_void,
) {
   if self_ptr.is_null() {
      return;
   }
   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `self_ptr` points to `RcInner.action_id` inside our `Rc<dyn
      // Fn>`.
      let id = unsafe { *self_ptr };
      let on = checked != 0;
      let Some(action) = lookup_action(id) else {
         return;
      };
      match action {
         Action::ToggleStaff => phoenix_runtime::set_staff_enabled(on),
         Action::ToggleAutoStart | Action::ToggleAutoUpdate => {
            let config = crate::config::update(|config| {
               if matches!(action, Action::ToggleAutoStart) {
                  config.auto_start = on;
               } else {
                  config.auto_update = on;
               }
            });
            // The login entry is the registry or a file, so it is written off
            // Discord's UI thread.
            std::thread::spawn(move || crate::config::save_or_report(config));
         },
         Action::TogglePlugin(name) => phoenix_runtime::set_enabled(&name, on),
         _ => {},
      }
   });
}

unsafe extern "C" fn button_callback(
   self_ptr: *const u64,
   _event: *const c_void,
   _window: *mut c_void,
   _app: *mut c_void,
) {
   if self_ptr.is_null() {
      return;
   }
   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `self_ptr` points to `RcInner.action_id` inside our `Rc<dyn
      // Fn>`.
      let id = unsafe { *self_ptr };
      if let Some(action) = lookup_action(id) {
         run(action);
      }
   });
}

/// Runs a pressed button's or element's action.
fn run(action: Action) {
   let _ = std::panic::catch_unwind(move || {
      match action {
         Action::SetTab(tab) => {
            let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            ui.tab = tab;
         },
         Action::SetFilter(filter) => {
            let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            ui.filter = filter;
            ui.filter_open = false;
         },
         Action::ToggleFilterMenu => {
            let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            ui.filter_open = !ui.filter_open;
         },
         Action::SetCatalogFilter(group) => {
            let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            ui.catalog_filter = group;
            ui.catalog_filter_open = false;
         },
         Action::ToggleCatalogFilterMenu => {
            let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            ui.catalog_filter_open = !ui.catalog_filter_open;
         },
         Action::OpenPlugin(name) => open_plugin_page(&name),
         Action::ReloadAll => phoenix_runtime::reload_all(),
         Action::OpenDevTools => phoenix_ui_desktop::open_devtools(),
         Action::CheckUpdates => update::check_now(),
         Action::ApplyUpdates => update::apply(),
         Action::RefreshCatalog => update::fetch_catalog(),
         Action::EditPluginCode(name) => {
            let Some(source) = phoenix_runtime::source(&name) else {
               phoenix_ui_desktop::toast(&format!("Could not read source for {name}"));
               return;
            };
            crate::modal::open_code_editor(name, source);
         },
         Action::OpenInstallUrl => {
            crate::modal::open_install_url();
         },
         Action::InstallFromFile => {
            std::thread::spawn(|| {
               let Some((file_name, bytes)) = phoenix_ui_desktop::pick_js_file() else {
                  return;
               };
               match phoenix_runtime::install_file(&file_name, bytes) {
                  Ok(installed) => {
                     log::info!("sunbird settings: {installed} from {file_name}");
                     phoenix_ui_desktop::toast(&installed.to_string());
                     crate::modal::force_redraw();
                  },
                  Err(err) => {
                     log::warn!("sunbird settings: install file {file_name}: {err}");
                     phoenix_ui_desktop::toast(&format!("Install failed: {err}"));
                  },
               }
            });
         },
         Action::UninstallPlugin(name) => {
            if let Err(err) = phoenix_runtime::uninstall(&name) {
               log::warn!("sunbird settings: uninstall {name}: {err}");
            }
         },
         Action::InstallCatalog(name) => {
            std::thread::spawn(move || {
               match update::install_listed(&name) {
                  Ok(installed) => {
                     log::info!("sunbird settings: {installed}");
                     phoenix_ui_desktop::toast(&installed.to_string());
                  },
                  Err(err) => {
                     log::warn!("sunbird settings: install {name}: {err}");
                     phoenix_ui_desktop::toast(&format!("Install failed: {err}"));
                  },
               }
            });
         },
         _ => {},
      }
   });
}

/// Whether [`locate`] resolved the builders.
pub fn located() -> bool {
   SYMBOLS.get().is_some()
}

/// Resolves Discord's native GPUI / Mana widget builders from unique string
/// anchors. Safe to call from `DllMain` (`prepare()`).
pub fn locate(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) -> bool {
   if SYMBOLS.get().is_some() {
      return true;
   }

   let found = if crate::X86_CODE {
      crate::located::get("locate::gpui::gpui", || locate::gpui::gpui(ix)).and_then(|gpui| {
         let layout = crate::located::get("desktop::gpui_layouts", || {
            desktop::gpui_layouts(image, &gpui)
         })?;
         let raw = gpui.div_into_any;
         Ok((gpui, layout, raw))
      })
   } else {
      arm64_builders(image, ix)
   };
   let (
      desktop::Gpui {
         caller_loc,
         div_new,
         div_flex,
         div_flex_col,
         div_style_mut,
         div_child_div,
         div_child_button,
         div_into_any,
         div_push_any,
         str_into_any,
         text_block,
         switch_new,
         switch_into_div,
         button_new,
         action_row,
         alloc_or_oom,
         ..
      },
      found_layout,
      div_into_any_raw,
   ) = match found {
      Ok(found) => found,
      Err(err) => {
         log::warn!("desktop gpui: {err}, leaving the settings page and accessories off");
         return false;
      },
   };

   // SAFETY: the builders were found by their call order and cross-checked,
   // their layouts match `layout`, and they have these Rust-ABI signatures.
   let symbols = unsafe {
      GpuiSymbols {
         caller_loc:       caller_loc as *const c_void,
         div_new:          std::mem::transmute::<usize, DivNewFn>(div_new),
         div_flex:         std::mem::transmute::<usize, DivMutFn>(div_flex),
         div_flex_col:     std::mem::transmute::<usize, DivMutFn>(div_flex_col),
         div_style_mut:    std::mem::transmute::<usize, DivStyleMutFn>(div_style_mut),
         div_child_div:    std::mem::transmute::<usize, DivChildDivFn>(div_child_div),
         div_child_button: std::mem::transmute::<usize, DivChildBtnFn>(div_child_button),
         div_into_any:     std::mem::transmute::<usize, DivIntoAnyFn>(div_into_any),
         div_into_any_raw,
         div_push_any:     div_push_any
            .map(|addr| std::mem::transmute::<usize, DivPushAnyFn>(addr)),
         str_into_any:     str_into_any
            .map(|addr| std::mem::transmute::<usize, StrIntoAnyFn>(addr)),
         text_block:       std::mem::transmute::<usize, TextBlockFn>(text_block),
         switch_new:       std::mem::transmute::<usize, SwitchNewFn>(switch_new),
         switch_into_div:  std::mem::transmute::<usize, SwitchIntoDivFn>(switch_into_div),
         button_new:       std::mem::transmute::<usize, ButtonNewFn>(button_new),
         action_row:       std::mem::transmute::<usize, ActionRowFn>(action_row),
         alloc_or_oom:     std::mem::transmute::<usize, AllocOrOomFn>(alloc_or_oom),
         layout:           found_layout,
      }
   };
   let _ = SYMBOLS.set(symbols);
   log::info!(
      "desktop gpui: resolved native Mana builders (div={div_new:#x}, style={div_style_mut:#x}, \
       switch={switch_new:#x}, button={button_new:#x}, row={action_row:#x}, \
       push_any={div_push_any:#x?}, str_any={str_into_any:#x?})"
   );
   log::info!("desktop gpui: {found_layout:x?}");
   true
}

/// The arm64 builders ([`phoenix_sigs::locate::gpui::gpui`]) with every one
/// that returns through `x8` swapped for an `image::sret_caller` thunk, so they
/// are called the x86 way, result slot first, like the types here say.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
fn arm64_builders(
   image: &Image<'_>,
   ix: &crate::located::LazyIndex<'_>,
) -> Result<(desktop::Gpui, GpuiLayout, usize), String> {
   let gpui = crate::located::get("locate::gpui::gpui", || locate::gpui::gpui(ix))?;
   let layout = phoenix_sigs::macos::gpui_layouts(image, &gpui)?;
   let raw = gpui.div_into_any;
   let thunk = |addr: usize| {
      phoenix_hook::image::sret_caller(addr).ok_or_else(|| "no memory for a thunk".to_owned())
   };
   let thunked = desktop::Gpui {
      div_new: thunk(gpui.div_new)?,
      div_flex: thunk(gpui.div_flex)?,
      div_flex_col: thunk(gpui.div_flex_col)?,
      div_child_div: thunk(gpui.div_child_div)?,
      div_child_button: thunk(gpui.div_child_button)?,
      div_into_any: thunk(gpui.div_into_any)?,
      str_into_any: gpui.str_into_any.map(thunk).transpose()?,
      text_block: thunk(gpui.text_block)?,
      switch_new: thunk(gpui.switch_new)?,
      switch_into_div: thunk(gpui.switch_into_div)?,
      button_new: thunk(gpui.button_new)?,
      action_row: thunk(gpui.action_row)?,
      ..gpui
   };
   Ok((thunked, layout, raw))
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
fn arm64_builders(
   _image: &Image<'_>,
   _ix: &crate::located::LazyIndex<'_>,
) -> Result<(desktop::Gpui, GpuiLayout, usize), String> {
   Err("no arm64 builders off macOS".to_owned())
}

/// `div.f(..)` for a `Div` builder taking and returning it by value.
fn chain(f: DivMutFn, mut div: RawDiv) -> RawDiv {
   let mut out = RawDiv::zeroed();
   // SAFETY: `f` moves `div` into `out`, both buffers fit a `Div`.
   unsafe { f(&raw mut out, &raw mut div) };
   out
}

/// `size` bytes aligned to `align` from Discord's allocator, once the builders
/// are located.
pub fn host_alloc(size: usize, align: usize) -> Option<*mut u8> {
   // SAFETY: Discord's own `alloc_or_oom`, which aborts rather than return null.
   SYMBOLS.get().map(|sym| unsafe { (sym.alloc_or_oom)(align, size) })
}

/// The `GpuiLayout` decoded from this `Discord.exe`.
pub fn layout() -> Option<&'static GpuiLayout> {
   SYMBOLS.get().map(|sym| &sym.layout)
}

/// Discord's `alloc_or_oom`, once the builders are located.
pub fn alloc_or_oom_addr() -> Option<usize> {
   SYMBOLS.get().map(|sym| sym.alloc_or_oom as usize)
}

/// A `Div`'s `StyleRefinement`, written through the decoded field offsets.
pub struct Style {
   ptr:    *mut u8,
   pub at: &'static StyleOffsets,
}

impl Style {
   fn put<T>(&self, field: usize, value: T) {
      if field + size_of::<T>() <= self.at.bytes {
         // SAFETY: `ptr` is the live `StyleRefinement` of `at.bytes` bytes that
         // `div_style_mut` returned, and the write is inside it.
         unsafe { self.ptr.add(field).cast::<T>().write_unaligned(value) };
      }
   }

   /// A one-byte enum field.
   pub fn set(&self, field: usize, value: u8) {
      self.put(field, value);
   }

   /// An `Option<Length>` or similar 8-byte field, [`gpui_px`] or [`W_FULL`].
   pub fn length(&self, field: usize, value: u64) {
      self.put(field, value);
   }

   pub fn lengths(&self, fields: &[usize], value: u64) {
      for &field in fields {
         self.length(field, value);
      }
   }

   /// `Some(Hsla)`: a `u32` tag, then four `f32`s.
   pub fn hsla(&self, field: usize, hsla: [f32; 4]) {
      if field + 20 <= self.at.bytes {
         self.put(field, 1u32);
         self.put(field + 4, hsla);
      }
   }

   /// `Some(f32)`: a `u32` tag, then the value.
   pub fn float(&self, field: usize, value: f32) {
      if field + 8 <= self.at.bytes {
         self.put(field, 1u32);
         self.put(field + 4, value);
      }
   }

   pub fn shared_string(&self, field: usize, value: SharedString) {
      self.put(field, value);
   }

   /// `Some(Background::Solid(hsla))`: tag `0`, the color at `+8`, and zeroed
   /// gradient stops up to `+0x48`.
   pub fn solid_background(&self, hsla: [f32; 4]) {
      let field = self.at.background;
      if field + 0x48 <= self.at.bytes {
         self.put(field, 0u64);
         self.put(field + 8, hsla);
         self.put(field + 0x18, [0u64; 6]);
      }
   }
}

// ---------------------------------------------------------------------------
// Native GPUI / Mana Layout & Styling Helpers
// ---------------------------------------------------------------------------

struct Builder {
   sym: &'static GpuiSymbols,
}

impl Builder {
   #[expect(
      clippy::cast_ptr_alignment,
      reason = "alloc_or_oom returns memory aligned to align_of::<RcInner>()"
   )]
   fn alloc_rc_action(&self, action: Action) -> *mut RcInner {
      let id = register_action(action);
      // SAFETY: calls Discord's own `alloc_or_oom(align=8, size=24)` so
      // Discord's `Rc::drop` frees it with the matching allocator.
      unsafe {
         let raw = (self.sym.alloc_or_oom)(align_of::<RcInner>(), size_of::<RcInner>());
         let inner = raw.cast::<RcInner>();
         inner.write(RcInner {
            strong:    1,
            weak:      1,
            action_id: id,
         });
         inner
      }
   }

   fn raw_div(&self) -> RawDiv {
      let mut d = RawDiv::zeroed();
      // SAFETY: constructs an empty GPUI `Div` into a buffer that fits it.
      unsafe { (self.sym.div_new)(&raw mut d, self.sym.caller_loc) };
      d
   }

   fn style_div(&self, div: &mut RawDiv, f: impl FnOnce(&Style)) {
      // SAFETY: `div_style_mut` returns `Div`'s boxed `StyleRefinement`.
      let ptr = unsafe { (self.sym.div_style_mut)(&raw mut *div) };
      if !ptr.is_null() {
         f(&Style {
            ptr,
            at: &self.sym.layout.style,
         });
      }
   }

   fn v_stack(&self, gap_px: f32) -> RawDiv {
      let d = chain(self.sym.div_flex, self.raw_div());
      let mut d = chain(self.sym.div_flex_col, d);
      self.style_div(&mut d, |s| s.lengths(&s.at.gap, gpui_px(gap_px)));
      d
   }

   fn h_row(&self, gap_px: f32, space_between: bool) -> RawDiv {
      const ALIGN_CENTER: u8 = 4;
      const JUSTIFY_SPACE_BETWEEN: u8 = 6;
      let mut d = chain(self.sym.div_flex, self.raw_div());
      self.style_div(&mut d, |s| {
         s.lengths(&s.at.gap, gpui_px(gap_px));
         s.set(s.at.align_items, ALIGN_CENTER);
         if space_between {
            s.set(s.at.justify_content, JUSTIFY_SPACE_BETWEEN);
         }
      });
      d
   }

   /// Rounded bordered card container matching Discord's native Settings cards.
   fn card(&self, gap_px: f32, pad_px: f32, radius_px: f32, border_alpha: f32) -> RawDiv {
      let border = crate::mana_theme::color_to_hsla(
         crate::mana_theme::sunbird_palette().border,
         Some(border_alpha),
      );
      let mut d = self.v_stack(gap_px);
      self.style_div(&mut d, |s| {
         s.lengths(&s.at.border_widths, gpui_px(1.0));
         s.lengths(&s.at.corner_radii, gpui_px(radius_px));
         s.lengths(&s.at.padding, gpui_px(pad_px));
         s.hsla(s.at.border_color, border);
      });
      d
   }

   /// Compact tinted tag matching Mana's 6px button geometry for non-confirmed
   /// desktop plugins, when `str_into_any` and `div_push_any` are available.
   fn compat_badge(&self, compat: DesktopCompat) -> Option<RawDiv> {
      let label = compat.tag()?;
      let (bg, text) = compat.colors()?;
      let mut badge = self.text_line(label, text, 12.0)?;
      self.style_div(&mut badge, |s| {
         s.solid_background(bg);
         s.lengths(&s.at.corner_radii, gpui_px(6.0));
         s.length(s.at.padding[0], gpui_px(4.0));
         s.length(s.at.padding[1], gpui_px(10.0));
         s.length(s.at.padding[2], gpui_px(4.0));
         s.length(s.at.padding[3], gpui_px(10.0));
      });
      Some(badge)
   }

   /// Subtle 1px horizontal divider line between sections or rows.
   fn divider(&self) -> RawDiv {
      let border = crate::mana_theme::color_to_hsla(
         crate::mana_theme::sunbird_palette().border,
         Some(0.24),
      );
      let mut d = self.raw_div();
      self.style_div(&mut d, |s| {
         s.length(s.at.border_widths[2], gpui_px(1.0));
         s.length(s.at.size[1], gpui_px(1.0));
         s.hsla(s.at.border_color, border);
      });
      d
   }

   fn push_div(&self, mut parent: RawDiv, mut child: RawDiv) -> RawDiv {
      let mut out = RawDiv::zeroed();
      // SAFETY: moves both `RawDiv`s into `div_child_div`.
      unsafe { (self.sym.div_child_div)(&raw mut out, &raw mut parent, &raw mut child) };
      out
   }

   fn build_button(
      &self,
      id: &str,
      label: &str,
      variant: BtnVariant,
      size: BtnSize,
      action: Action,
   ) -> RawButton {
      let rc_ptr = self.alloc_rc_action(action);
      new_button(
         self.sym,
         id,
         label,
         variant,
         size,
         &raw const BUTTON_FN_VTBL,
         rc_ptr,
      )
   }

   fn push_button(
      &self,
      mut parent: RawDiv,
      id: &str,
      label: &str,
      variant: BtnVariant,
      size: BtnSize,
      action: Action,
   ) -> RawDiv {
      let mut btn = self.build_button(id, label, variant, size, action);
      let mut out = RawDiv::zeroed();
      // SAFETY: appends native Mana `Button` directly into `parent`.
      unsafe { (self.sym.div_child_button)(&raw mut out, &raw mut parent, &raw mut btn) };
      out
   }

   fn text_block(&self, title: &str, subtitle: &str) -> RawDiv {
      let title = SharedString::from_str(title);
      let subtitle = SharedString::from_str(subtitle);
      let mut out = RawDiv::zeroed();
      // SAFETY: Mana's `TextBlock::new` takes both strings.
      unsafe { (self.sym.text_block)(&raw mut out, &raw const title, &raw const subtitle) };
      out
   }

   fn switch_row(
      &self,
      id: &str,
      label: &str,
      description: Option<&str>,
      checked: bool,
      action: Action,
   ) -> RawDiv {
      let rc_ptr = self.alloc_rc_action(action);
      new_switch(
         self.sym,
         id,
         label,
         description,
         checked,
         &raw const SWITCH_FN_VTBL,
         rc_ptr,
      )
   }

   #[expect(clippy::too_many_arguments, reason = "one per GPUI row part")]
   fn action_row(
      &self,
      id: &str,
      title: &str,
      subtitle: &str,
      button_label: &str,
      variant: BtnVariant,
      size: BtnSize,
      action: Action,
   ) -> RawDiv {
      let title_ss = SharedString::from_str(title);
      let sub_ss = SharedString::from_str(subtitle);
      let mut btn = self.build_button(id, button_label, variant, size, action);
      let mut out = RawDiv::zeroed();
      // SAFETY: builds Discord's native `ActionRow(title, subtitle, button)`.
      unsafe {
         (self.sym.action_row)(
            &raw mut out,
            &raw const title_ss,
            &raw const sub_ss,
            &raw mut btn,
         );
      }
      out
   }

   fn write_any(&self, mut div: RawDiv, sret: *mut RawAnyElement) {
      // SAFETY: writes the 32-byte `gpui::AnyElement` directly to `sret`.
      unsafe {
         (self.sym.div_into_any)(sret, &raw mut div);
      }
   }

   fn push_any(&self, parent: &mut RawDiv, child: &mut RawAnyElement) -> bool {
      let Some(push) = self.sym.div_push_any else {
         return false;
      };
      // SAFETY: `children` is the `Div`'s `SmallVec<[AnyElement; 2]>`, decoded
      // from `div_child_div` and inside the `Div`.
      unsafe {
         let children = parent
            .as_mut_ptr()
            .add(self.sym.layout.div.children)
            .cast::<c_void>();
         push(children, child);
      }
      true
   }

   /// `div`, running `action` when pressed.
   fn clickable(&self, mut div: RawDiv, action: Action) -> RawDiv {
      let id = register_action(action);
      self.style_div(&mut div, |s| s.set(s.at.mouse_cursor, CURSOR_POINTING_HAND));
      crate::modal::listen_mouse_down(&mut div, (id + 1) as *mut u8, &PRESS_VTBL);
      div
   }

   /// Draws `div` like Discord's text inputs: the darkest surface, a subtle
   /// border, and rounded corners.
   fn input_surface(&self, div: &mut RawDiv) {
      let pal = crate::mana_theme::sunbird_palette();
      self.style_div(div, |s| {
         s.lengths(&s.at.border_widths, gpui_px(1.0));
         s.lengths(&s.at.corner_radii, gpui_px(8.0));
         s.lengths(&[s.at.padding[0], s.at.padding[2]], gpui_px(10.0));
         s.lengths(&[s.at.padding[1], s.at.padding[3]], gpui_px(12.0));
         s.hsla(
            s.at.border_color,
            crate::mana_theme::color_to_hsla(pal.border, Some(0.6)),
         );
         s.solid_background(crate::mana_theme::color_to_hsla(pal.bg_code, None));
      });
   }

   /// An 18px icon drawn from `svg`, or `fallback` as text when icons cannot
   /// be drawn, running `action` when pressed.
   fn icon_button(&self, svg: &str, fallback: &str, action: Action) -> Option<RawDiv> {
      let pal = crate::mana_theme::sunbird_palette();
      let color = crate::mana_theme::color_to_hsla(pal.text_muted, None);
      let mut button = chain(self.sym.div_flex, self.raw_div());
      self.style_div(&mut button, |s| {
         s.lengths(&s.at.padding, gpui_px(4.0));
         s.lengths(&s.at.corner_radii, gpui_px(4.0));
      });
      if let (Some(svgs), Some(path)) = (SVG.get(), crate::icons::register(svg)) {
         let mut icon = svg_icon(self, svgs, path, gpui_px(18.0), color);
         self.push_any(&mut button, &mut icon);
      } else {
         button = self.push_div(button, self.text_line(fallback, color, 16.0)?);
      }
      Some(self.clickable(button, action))
   }

   fn text_line(&self, text: &str, color_hsla: [f32; 4], font_px: f32) -> Option<RawDiv> {
      let str_into_any = self.sym.str_into_any?;
      let mut line_div = self.raw_div();
      self.style_div(&mut line_div, |s| {
         s.length(s.at.font_size, gpui_px(font_px));
         s.length(s.at.line_height, gpui_px(font_px + 4.0));
         s.hsla(s.at.text_color, color_hsla);
      });
      let mut ss = SharedString::from_str(text);
      // SAFETY: moves the `SharedString` into a 32-byte `AnyElement` and
      // pushes it.
      let mut any = unsafe {
         let mut out = MaybeUninit::<RawAnyElement>::uninit();
         str_into_any(out.as_mut_ptr(), &raw mut ss);
         out.assume_init()
      };
      if !self.push_any(&mut line_div, &mut any) {
         return None;
      }
      Some(line_div)
   }
}

/// Returns the resolved address of `Div::into_any_element` for sibling desktop
/// locators (`crate::modal`).
pub fn div_into_any_addr() -> Option<usize> {
   SYMBOLS.get().map(|s| s.div_into_any_raw)
}

pub fn raw_div() -> Option<RawDiv> {
   let sym = SYMBOLS.get()?;
   Some(Builder { sym }.raw_div())
}

pub fn style_div(div: &mut RawDiv, f: impl FnOnce(&Style)) {
   if let Some(sym) = SYMBOLS.get() {
      Builder { sym }.style_div(div, f);
   }
}

pub fn v_stack(gap_px: f32) -> Option<RawDiv> {
   let sym = SYMBOLS.get()?;
   Some(Builder { sym }.v_stack(gap_px))
}

pub fn h_row(gap_px: f32, space_between: bool) -> Option<RawDiv> {
   let sym = SYMBOLS.get()?;
   Some(Builder { sym }.h_row(gap_px, space_between))
}

pub fn push_div(parent: RawDiv, child: RawDiv) -> RawDiv {
   let Some(sym) = SYMBOLS.get() else {
      return parent;
   };
   Builder { sym }.push_div(parent, child)
}

pub fn push_any(parent: &mut RawDiv, child: &mut RawAnyElement) -> bool {
   let Some(sym) = SYMBOLS.get() else {
      return false;
   };
   Builder { sym }.push_any(parent, child)
}

pub fn str_to_any(text: &str) -> Option<RawAnyElement> {
   let str_into_any = SYMBOLS.get()?.str_into_any?;
   let mut ss = SharedString::from_str(text);
   // SAFETY: moves the `SharedString` into a 32-byte `AnyElement`.
   unsafe {
      let mut out = MaybeUninit::<RawAnyElement>::uninit();
      str_into_any(out.as_mut_ptr(), &raw mut ss);
      Some(out.assume_init())
   }
}

pub fn build_raw_button(
   id: &str,
   label: &str,
   variant: BtnVariant,
   size: BtnSize,
   vtbl: *const RcFnVtbl,
   action_id: u64,
) -> Option<RawButton> {
   let sym = SYMBOLS.get()?;
   let rc_ptr = alloc_rc_u64(action_id)?;
   Some(new_button(sym, id, label, variant, size, vtbl, rc_ptr))
}

/// A Mana `Switch` row whose `on_change` calls `vtbl` with `action_id`, for
/// switches outside the settings page (`crate::modal`).
pub fn build_raw_switch(
   id: &str,
   label: &str,
   description: Option<&str>,
   checked: bool,
   vtbl: *const RcFnVtbl,
   action_id: u64,
) -> Option<RawDiv> {
   let sym = SYMBOLS.get()?;
   let rc_ptr = alloc_rc_u64(action_id)?;
   Some(new_switch(
      sym,
      id,
      label,
      description,
      checked,
      vtbl,
      rc_ptr,
   ))
}

/// A native Mana `Switch` in its `Div`, with our `Rc<dyn Fn(bool)>`
/// `on_change`.
fn new_switch(
   sym: &GpuiSymbols,
   id: &str,
   label: &str,
   description: Option<&str>,
   checked: bool,
   vtbl: *const RcFnVtbl,
   rc_ptr: *mut RcInner,
) -> RawDiv {
   let at = &sym.layout.switch;
   let id_static = intern(id);
   let mut sw = RawSwitch::zeroed();
   // SAFETY: creates a native Mana `Switch` in a buffer that fits it.
   unsafe {
      (sym.switch_new)(
         &raw mut sw,
         id_static.as_ptr(),
         id_static.len(),
         u8::from(checked),
      );
   }
   // The defaults these overwrite are static `SharedString`s and `None`,
   // which own nothing.
   sw.put(at.label, SharedString::from_str(label));
   if let Some(desc) = description
      && !desc.is_empty()
   {
      sw.put(at.description, SharedString::from_str(desc));
   }
   sw.put(at.on_change, rc_ptr);
   sw.put(at.on_change + 8, vtbl);

   let mut out = RawDiv::zeroed();
   // SAFETY: moves the `Switch` into its `Div`.
   unsafe { (sym.switch_into_div)(&raw mut out, &raw mut sw) };
   out
}

/// A native Mana `Button` with our `variant`, `size` and `Rc<dyn Fn>` click
/// handler.
fn new_button(
   sym: &GpuiSymbols,
   id: &str,
   label: &str,
   variant: BtnVariant,
   size: BtnSize,
   vtbl: *const RcFnVtbl,
   rc_ptr: *mut RcInner,
) -> RawButton {
   let at = &sym.layout.button;
   let id_static = intern(id);
   let label_ss = SharedString::from_str(label);
   let mut btn = RawButton::zeroed();
   // SAFETY: constructs a native Mana `Button` in a buffer that fits it.
   unsafe {
      (sym.button_new)(
         &raw mut btn,
         id_static.as_ptr(),
         id_static.len(),
         &raw const label_ss,
      );
   }
   btn.put(at.variant, variant as u8);
   btn.put(at.scale, size as u8);
   btn.put(at.on_click, rc_ptr);
   btn.put(at.on_click + 8, vtbl);
   btn
}

pub fn push_raw_button(mut parent: RawDiv, mut btn: RawButton) -> RawDiv {
   let Some(sym) = SYMBOLS.get() else {
      return parent;
   };
   let mut out = RawDiv::zeroed();
   // SAFETY: appends native Mana `Button` directly into `parent`.
   unsafe { (sym.div_child_button)(&raw mut out, &raw mut parent, &raw mut btn) };
   out
}

/// `div` laid out as a flex row.
pub fn flex(div: RawDiv) -> Option<RawDiv> {
   Some(chain(SYMBOLS.get()?.div_flex, div))
}

/// An SVG registered at `path`, `size` square (a [`gpui_px`] or
/// [`gpui_rems`] length) in `color`, or `None` where SVGs cannot be drawn.
pub fn svg_any(path: &'static str, size: u64, color: [f32; 4]) -> Option<RawAnyElement> {
   let builder = Builder {
      sym: SYMBOLS.get()?,
   };
   Some(svg_icon(&builder, SVG.get()?, path, size, color))
}

/// Mana's `TextBlock`: `title` in its heading weight over `subtitle`.
pub fn text_block(title: &str, subtitle: &str) -> Option<RawDiv> {
   Some(
      Builder {
         sym: SYMBOLS.get()?,
      }
      .text_block(title, subtitle),
   )
}

pub fn write_any(div: RawDiv, sret: *mut RawAnyElement) {
   if let Some(sym) = SYMBOLS.get() {
      Builder { sym }.write_any(div, sret);
   }
}

/// Converts 32-bit ARGB (`0xAARRGGBB`) into GPUI's `[h, s, l, a]`
/// (`0.0..=1.0`).
#[expect(clippy::many_single_char_names, reason = "the HSL formula's own names")]
pub fn argb_to_hsla(argb: u32) -> [f32; 4] {
   let [alpha, r, g, b] = argb.to_be_bytes().map(|byte| f32::from(byte) / 255.0);
   let a = if alpha == 0.0 && argb != 0 {
      1.0
   } else {
      alpha
   };

   let max = r.max(g).max(b);
   let min = r.min(g).min(b);
   let delta = max - min;
   let l = f32::midpoint(max, min);

   let (h, s) = if delta == 0.0 {
      (0.0, 0.0)
   } else {
      let s = delta / (1.0 - 2.0f32.mul_add(l, -1.0).abs());
      let mut h = if (max - r).abs() < f32::EPSILON {
         ((g - b) / delta) % 6.0
      } else if (max - g).abs() < f32::EPSILON {
         (b - r) / delta + 2.0
      } else {
         (r - g) / delta + 4.0
      };
      h /= 6.0;
      if h < 0.0 {
         h += 1.0;
      }
      (h, s)
   };
   [h, s, l, a]
}

/// Allocates an `RcInner { strong: 1, weak: 1, action_id: value }` on
/// `Discord.exe`'s own heap so Discord's `Rc::drop` frees it cleanly.
pub fn alloc_rc_u64(value: u64) -> Option<*mut RcInner> {
   let sym = SYMBOLS.get()?;
   // SAFETY: uses Discord's resolved `alloc_or_oom(align, size)` for a 24-byte
   // `RcInner` layout matching `std::rc::RcBox<u64>`.
   unsafe {
      #[expect(
         clippy::cast_ptr_alignment,
         reason = "allocated with `RcInner`'s alignment"
      )]
      let ptr = (sym.alloc_or_oom)(align_of::<RcInner>(), size_of::<RcInner>()).cast::<RcInner>();
      if ptr.is_null() {
         return None;
      }
      ptr.write(RcInner {
         strong:    1,
         weak:      1,
         action_id: value,
      });
      Some(ptr)
   }
}

/// Whether `div_push_any` resolved, which every native modal needs to take
/// Discord's own `Modal` element as a child.
#[must_use]
pub fn supports_push_any() -> bool {
   SYMBOLS.get().is_some_and(|s| s.div_push_any.is_some())
}

/// Whether `div_push_any` and `str_into_any` resolved for native message
/// accessories.
#[must_use]
pub fn supports_accessories() -> bool {
   SYMBOLS
      .get()
      .is_some_and(|s| s.div_push_any.is_some() && s.str_into_any.is_some())
}

/// Wraps a rendered message row `RawAnyElement` in place with native GPUI
/// accessory lines underneath and an optional tinted left accent bar.
pub fn wrap_with_accessories(
   sret: *mut RawAnyElement,
   lines: &[(String, Option<u32>)],
   tint: Option<u32>,
) -> bool {
   let muted = crate::mana_theme::color_to_hsla(
      crate::mana_theme::sunbird_palette().text_muted,
      Some(0.85),
   );

   let Some(sym) = SYMBOLS.get() else {
      return false;
   };
   if sym.div_push_any.is_none() || sym.str_into_any.is_none() {
      return false;
   }
   let b = Builder { sym };

   // SAFETY: takes ownership of the original rendered `AnyElement` out of
   // `sret`; we write the replacement `AnyElement` back into `sret` before
   // returning.
   let mut orig = unsafe { ptr::read(sret) };

   let mut outer = b.v_stack(2.0);
   if let Some(argb) = tint {
      let hsla = argb_to_hsla(argb);
      // The text color is inherited by child text runs.
      b.style_div(&mut outer, |s| {
         s.hsla(s.at.text_color, hsla);
         s.length(s.at.border_widths[3], gpui_px(3.0));
         s.hsla(s.at.border_color, hsla);
         // A faint wash of the tint behind the whole row.
         let [h, sat, l, _] = hsla;
         s.solid_background([h, sat, l, 0.12]);
      });
   }
   if !b.push_any(&mut outer, &mut orig) {
      // SAFETY: restore original element if push failed.
      unsafe { ptr::write(sret, orig) };
      return false;
   }

   if !lines.is_empty() {
      let mut acc_box = b.v_stack(2.0);
      // Indent accessories under the message text column.
      b.style_div(&mut acc_box, |s| {
         let [_, right, bottom, left] = s.at.padding;
         s.length(right, gpui_px(16.0));
         s.length(bottom, gpui_px(4.0));
         s.length(left, gpui_px(72.0));
      });
      for (text, color) in lines {
         let hsla = color.map_or(muted, argb_to_hsla);
         for subline in text.lines() {
            if subline.is_empty() {
               continue;
            }
            if let Some(line_div) = b.text_line(subline, hsla, 12.0) {
               acc_box = b.push_div(acc_box, line_div);
            }
         }
      }
      outer = b.push_div(outer, acc_box);
   }

   b.write_any(outer, sret);
   true
}

/// Muted accessory text, for lines without a color of their own.
fn accessory_muted() -> [f32; 4] {
   crate::mana_theme::color_to_hsla(crate::mana_theme::sunbird_palette().text_muted, Some(0.85))
}

/// One piece of an inline accessory: SVG icons when it has them and the build
/// can draw them, else its text.
#[derive(Clone, Debug)]
pub struct Piece {
   pub text:  String,
   pub color: Option<u32>,
   /// Asset paths from [`crate::icons::register`], each with its own color.
   pub icons: Vec<(&'static str, Option<u32>)>,
}

/// GPUI's `Svg` builders and layout, resolved by [`locate_svg`].
struct SvgSymbols {
   new:       SvgNewFn,
   path:      SvgPathFn,
   into_any:  SvgIntoAnyFn,
   style_box: usize,
}

type RawSvg = Scratch<{ layout::SVG_CAP }>;
type SvgNewFn = unsafe extern "C" fn(*mut RawSvg, *const c_void) -> *mut RawSvg;
type SvgPathFn = unsafe extern "C" fn(*mut RawSvg, *mut RawSvg, *const u8, usize) -> *mut RawSvg;
type SvgIntoAnyFn = unsafe extern "C" fn(*mut RawAnyElement, *mut RawSvg) -> *mut RawAnyElement;

static SVG: OnceLock<SvgSymbols> = OnceLock::new();

/// Resolves the `Svg` builders, after [`locate`].
pub fn locate_svg(image: &Image<'static>) {
   let located = if crate::X86_CODE {
      crate::located::get("desktop::svg_builders", || desktop::svg_builders(image))
   } else {
      arm64_svg_builders(image)
   };
   let found = match located {
      Ok(found) => found,
      Err(err) => {
         log::warn!("desktop gpui: {err}, icons fall back to text");
         return;
      },
   };
   if found.size > layout::SVG_CAP {
      log::warn!(
         "desktop gpui: Svg is {:#x} bytes, over {:#x}",
         found.size,
         layout::SVG_CAP
      );
      return;
   }
   // SAFETY: decoded from the code drawing a bundled icon, which calls them
   // with these Rust-ABI signatures.
   let symbols = unsafe {
      SvgSymbols {
         new:       std::mem::transmute::<usize, SvgNewFn>(found.new),
         path:      std::mem::transmute::<usize, SvgPathFn>(found.path),
         into_any:  std::mem::transmute::<usize, SvgIntoAnyFn>(found.into_any),
         style_box: found.style_box,
      }
   };
   let _ = SVG.set(symbols);
   log::info!("desktop gpui: resolved Svg builders {found:x?}");
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
fn arm64_svg_builders(image: &Image<'_>) -> Result<desktop::SvgBuilders, String> {
   let svg = phoenix_sigs::macos::svg_builders(image)?;
   let thunk = |addr: usize| {
      phoenix_hook::image::sret_caller(addr)
         .ok_or_else(|| "no memory for an Svg thunk".to_owned())
   };
   Ok(desktop::SvgBuilders {
      new: thunk(svg.new)?,
      path: thunk(svg.path)?,
      into_any: thunk(svg.into_any)?,
      ..svg
   })
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
fn arm64_svg_builders(_image: &Image<'_>) -> Result<desktop::SvgBuilders, String> {
   Err("no arm64 Svg builders off macOS".to_owned())
}

/// Whether icons can be drawn.
pub fn svg_ready() -> bool {
   SVG.get().is_some() && SYMBOLS.get().is_some()
}

/// `len` bytes from Discord's own allocator, so Discord can free them.
pub fn discord_alloc(len: usize) -> Option<*mut u8> {
   let sym = SYMBOLS.get()?;
   // SAFETY: Discord's `alloc_or_oom(align, size)`, which aborts rather than
   // returning null.
   let ptr = unsafe { (sym.alloc_or_oom)(1, len.max(1)) };
   (!ptr.is_null()).then_some(ptr)
}

/// An `svg().path(path)` of `px` pixels filled with `color`, as an
/// `AnyElement`.
fn svg_icon(
   b: &Builder,
   svg: &SvgSymbols,
   path: &'static str,
   size: u64,
   color: [f32; 4],
) -> RawAnyElement {
   let mut fresh = RawSvg::zeroed();
   let mut icon = RawSvg::zeroed();
   // SAFETY: `svg()` builds into a buffer that fits an `Svg`, and `path` moves
   // it into the second with a `'static` path.
   unsafe {
      (svg.new)(&raw mut fresh, b.sym.caller_loc);
      (svg.path)(&raw mut icon, &raw mut fresh, path.as_ptr(), path.len());
   }
   // SAFETY: the `Svg`'s boxed `StyleRefinement`, at the decoded offset.
   let style = unsafe {
      icon
         .as_mut_ptr()
         .add(svg.style_box)
         .cast::<*mut u8>()
         .read_unaligned()
   };
   if !style.is_null() {
      let s = Style {
         ptr: style,
         at:  &b.sym.layout.style,
      };
      s.lengths(&s.at.size, size);
      s.float(s.at.flex_shrink, 0.0);
      s.hsla(s.at.text_color, color);
   }
   let mut any = MaybeUninit::<RawAnyElement>::uninit();
   // SAFETY: moves the `Svg` into a 32-byte `AnyElement`.
   unsafe {
      (svg.into_any)(any.as_mut_ptr(), &raw mut icon);
      any.assume_init()
   }
}

/// `pieces` side by side in a row: each piece's icons when it has any and
/// they can be drawn, else its text.
fn inline_row(b: &Builder, pieces: &[Piece], font_px: f32) -> RawDiv {
   let mut row = b.h_row(4.0, false);
   for piece in pieces {
      let hsla = piece.color.map_or_else(accessory_muted, argb_to_hsla);
      if let Some(svg) = SVG.get().filter(|_| !piece.icons.is_empty()) {
         let mut icons = b.h_row(2.0, false);
         for &(path, color) in &piece.icons {
            let mut icon = svg_icon(
               b,
               svg,
               path,
               gpui_px(font_px + 3.0),
               color.map_or(hsla, argb_to_hsla),
            );
            b.push_any(&mut icons, &mut icon);
         }
         row = b.push_div(row, icons);
      } else if !piece.text.is_empty()
         && let Some(line) = b.text_line(&piece.text, hsla, font_px)
      {
         row = b.push_div(row, line);
      }
   }
   row
}

/// Adds `pieces` as one more child of a `Div` Discord built by value, like the
/// member name's row right after the name, `lead` pixels after the child
/// before them.
pub fn append_pieces(div: *mut c_void, pieces: &[Piece], lead: f32) -> bool {
   let Some(sym) = SYMBOLS.get() else {
      return false;
   };
   let Some(push) = sym.div_push_any else {
      return false;
   };
   if div.is_null() || sym.str_into_any.is_none() {
      return false;
   }
   let b = Builder { sym };
   let mut row = inline_row(&b, pieces, 13.0);
   if lead > 0.0 {
      b.style_div(&mut row, |s| s.length(s.at.padding[3], gpui_px(lead)));
   }
   let mut any = MaybeUninit::<RawAnyElement>::uninit();
   b.write_any(row, any.as_mut_ptr());
   // SAFETY: `div` is a live `Div` that `Div::child` just returned, whose
   // children sit at the decoded offset; `push` moves the element into them.
   unsafe {
      let children = div
         .cast::<u8>()
         .add(sym.layout.div.children)
         .cast::<c_void>();
      push(children, any.as_mut_ptr());
   }
   true
}

// ---------------------------------------------------------------------------
// Sunbird Categorized Native GPUI Settings Page
// ---------------------------------------------------------------------------

const DESKTOP_FULL: &[&str] = &[
   "silent-typing",
   "message-latency",
   "message-logger",
   "no-profile-themes",
   "no-reply-mention",
   "spoiler-mode",
   "text-replace",
   "view-raw",
   "global-badges",
   "dearrow",
   "translate",
   "clear-urls",
   "fix-embeds",
   "platform-indicators",
   "disable-call-idle",
   "font-changer",
   "collapse-channels",
   "desktop-themes",
   "fakenitro",
];

const DESKTOP_PARTIAL: &[&str] = &["hide-composer-buttons", "reviewdb"];

const MOBILE_ONLY: &[&str] = &["taptap", "example"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum DesktopCompat {
   Full,
   Partial,
   MobileOnly,
   Unverified,
}

impl DesktopCompat {
   fn of(name: &str) -> Self {
      if DESKTOP_FULL.contains(&name) {
         Self::Full
      } else if DESKTOP_PARTIAL.contains(&name) {
         Self::Partial
      } else if MOBILE_ONLY.contains(&name) {
         Self::MobileOnly
      } else {
         Self::Unverified
      }
   }

   const fn tag(self) -> Option<&'static str> {
      match self {
         Self::Full => None,
         Self::Partial => Some("Partial Desktop"),
         Self::MobileOnly => Some("Mobile Only"),
         Self::Unverified => Some("Unverified"),
      }
   }

   /// `(background, text)` HSLA colors for non-full compat badges.
   const fn colors(self) -> Option<([f32; 4], [f32; 4])> {
      match self {
         Self::Full => None,
         // Warm amber
         Self::Partial => Some(([0.10, 0.85, 0.52, 0.14], [0.11, 0.92, 0.72, 1.0])),
         // Soft violet/purple — distinct from neutral buttons and red Remove
         Self::MobileOnly => Some(([0.76, 0.72, 0.62, 0.18], [0.76, 0.86, 0.82, 1.0])),
         // Soft sky blue
         Self::Unverified => Some(([0.58, 0.78, 0.60, 0.14], [0.58, 0.88, 0.80, 1.0])),
      }
   }
}

/// Draws the Sunbird page into `sret`. `window` and `app` are the settings
/// section's, which the search box's `TextInput` is built with.
pub fn render_sunbird_page(
   sret: *mut RawAnyElement,
   window: *mut c_void,
   app: *mut c_void,
) -> bool {
   let Some(sym) = SYMBOLS.get() else {
      return false;
   };
   let b = Builder { sym };

   ACTIONS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clear();

   let ui = UI_STATE
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clone();

   let plugins = phoenix_runtime::plugins();
   let enabled_count = plugins.iter().filter(|p| p.state.is_on()).count();

   let mut root = b.v_stack(16.0);

   // 1. Header + Category Tab Bar
   let summary = format!(
      "v{} · {enabled_count}/{} plugins active · {} for DevTools",
      update::VERSION,
      plugins.len(),
      phoenix_ui_desktop::DEVTOOLS_SHORTCUT,
   );
   root = b.push_div(root, b.text_block("Sunbird", &summary));

   let variant_for = |t: Tab| {
      if ui.tab == t {
         BtnVariant::Primary
      } else {
         BtnVariant::Secondary
      }
   };
   let mut tab_bar = b.h_row(8.0, false);
   tab_bar = b.push_button(
      tab_bar,
      "sunbird-tab-plugins",
      &format!("Plugins ({})", plugins.len()),
      variant_for(Tab::Plugins),
      BtnSize::Small,
      Action::SetTab(Tab::Plugins),
   );
   tab_bar = b.push_button(
      tab_bar,
      "sunbird-tab-catalog",
      "Plugin Catalog",
      variant_for(Tab::Catalog),
      BtnSize::Small,
      Action::SetTab(Tab::Catalog),
   );
   tab_bar = b.push_button(
      tab_bar,
      "sunbird-tab-general",
      "General & Updates",
      variant_for(Tab::General),
      BtnSize::Small,
      Action::SetTab(Tab::General),
   );
   root = b.push_div(root, tab_bar);
   root = b.push_div(root, b.divider());

   // 2. Active Category Content
   match ui.tab {
      Tab::Plugins => {
         let search = crate::modal::search_field(app, window);
         root = render_plugins_tab(&b, root, &plugins, &ui, search);
      },
      Tab::Catalog => {
         let search = crate::modal::search_field(app, window);
         let installed: HashSet<String> = plugins.iter().map(|p| p.name.clone()).collect();
         root = render_catalog_tab(&b, root, &installed, &ui, search);
      },
      Tab::General => {
         root = render_general_tab(&b, root);
      },
   }

   b.write_any(root, sret);
   true
}

/// Settings' gear, for a card whose plugin has settings.
const GEAR_SVG: &str =
   "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\"><path fill-rule=\"evenodd\" \
    d=\"M19.90 9.81 L22.64 10.17 L22.64 13.83 L19.90 14.19 L19.13 16.04 L20.82 18.23 L18.23 20.82 \
    L16.04 19.13 L14.19 19.90 L13.83 22.64 L10.17 22.64 L9.81 19.90 L7.96 19.13 L5.77 20.82 L3.18 \
    18.23 L4.87 16.04 L4.10 14.19 L1.36 13.83 L1.36 10.17 L4.10 9.81 L4.87 7.96 L3.18 5.77 L5.77 \
    3.18 L7.96 4.87 L9.81 4.10 L10.17 1.36 L13.83 1.36 L14.19 4.10 L16.04 4.87 L18.23 3.18 L20.82 \
    5.77 L19.13 7.96Z M12 8.6 A3.4 3.4 0 1 0 12 15.4 A3.4 3.4 0 1 0 12 8.6Z\"/></svg>";

/// An `i` in a ring, for a card whose plugin has only details.
const INFO_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\"><path \
                        fill-rule=\"evenodd\" d=\"M12 2 A10 10 0 1 0 12 22 A10 10 0 1 0 12 2Z M12 \
                        3.8 A8.2 8.2 0 1 1 12 20.2 A8.2 8.2 0 1 1 12 3.8Z M10.9 10.5 H13.1 V17 \
                        H10.9Z M12 6.4 A1.35 1.35 0 1 0 12 9.1 A1.35 1.35 0 1 0 12 6.4Z\"/></svg>";

impl PluginFilter {
   const fn label(self) -> &'static str {
      match self {
         Self::All => "Show All",
         Self::Enabled => "Show Enabled",
         Self::Disabled => "Show Disabled",
      }
   }

   const fn keeps(self, plugin: &phoenix_runtime::PluginInfo) -> bool {
      match self {
         Self::All => true,
         Self::Enabled => plugin.state.is_on(),
         Self::Disabled => !plugin.state.is_on(),
      }
   }
}

impl CatalogGroup {
   const fn label(group: Option<Self>) -> &'static str {
      match group {
         None => "Show All",
         Some(Self::Desktop) => "Desktop Ready",
         Some(Self::Partial) => "Partial Desktop Support",
         Some(Self::Other) => "Mobile Only & Unverified",
      }
   }

   fn of(name: &str) -> Self {
      match DesktopCompat::of(name) {
         DesktopCompat::Full => Self::Desktop,
         DesktopCompat::Partial => Self::Partial,
         DesktopCompat::MobileOnly | DesktopCompat::Unverified => Self::Other,
      }
   }
}

/// Whether `name` or `description` holds `query`, ignoring case.
fn matches_query(name: &str, description: Option<&str>, query: &str) -> bool {
   let query = query.trim().to_lowercase();
   name.to_lowercase().contains(&query)
      || description.is_some_and(|desc| desc.to_lowercase().contains(&query))
}

/// "Filters", the search box and the dropdown with `actions` beside it, and
/// the dropdown's menu while it is open. Returns what the box holds.
fn render_filters(
   b: &Builder,
   mut root: RawDiv,
   search: Option<crate::modal::PageField>,
   dropdown: Dropdown,
   actions: RawDiv,
) -> (RawDiv, String) {
   let pal = crate::mana_theme::sunbird_palette();
   let text = crate::mana_theme::color_to_hsla(pal.text, None);
   if let Some(label) = b.text_line("Filters", text, 16.0) {
      root = b.push_div(root, label);
   }

   let mut query = String::new();
   if let Some(mut field) = search {
      let mut wrap = b.raw_div();
      b.style_div(&mut wrap, |s| s.length(s.at.size[0], W_FULL));
      if b.push_any(&mut wrap, &mut field.element) {
         root = b.push_div(root, wrap);
         query = field.text;
      }
   }

   let mut controls = b.h_row(12.0, true);
   let muted = crate::mana_theme::color_to_hsla(pal.text_muted, None);
   let mut picker = b.h_row(8.0, true);
   b.input_surface(&mut picker);
   b.style_div(&mut picker, |s| s.length(s.at.size[0], fraction(0.5)));
   if let Some(label) = b.text_line(dropdown.label, text, 15.0) {
      picker = b.push_div(picker, label);
   }
   let chevron = if dropdown.open {
      "\u{25b4}"
   } else {
      "\u{25be}"
   };
   if let Some(chevron) = b.text_line(chevron, muted, 14.0) {
      picker = b.push_div(picker, chevron);
   }
   controls = b.push_div(controls, b.clickable(picker, dropdown.toggle));
   controls = b.push_div(controls, actions);
   root = b.push_div(root, controls);

   if dropdown.open {
      let mut menu = b.v_stack(2.0);
      b.input_surface(&mut menu);
      b.style_div(&mut menu, |s| {
         s.lengths(&s.at.padding, gpui_px(4.0));
         s.length(s.at.size[0], fraction(0.5));
      });
      let chosen_bg = crate::mana_theme::color_to_hsla(pal.accent, Some(0.18));
      for (label, chosen, action) in dropdown.options {
         if let Some(mut option) = b.text_line(label, if chosen { text } else { muted }, 14.0) {
            b.style_div(&mut option, |s| {
               s.lengths(&[s.at.padding[0], s.at.padding[2]], gpui_px(8.0));
               s.lengths(&[s.at.padding[1], s.at.padding[3]], gpui_px(10.0));
               s.lengths(&s.at.corner_radii, gpui_px(6.0));
               if chosen {
                  s.solid_background(chosen_bg);
               }
            });
            menu = b.push_div(menu, b.clickable(option, action));
         }
      }
      root = b.push_div(root, menu);
   }
   (root, query)
}

/// A dropdown under the search box: its label, whether its menu shows, and
/// each option with whether it is the chosen one.
struct Dropdown {
   label:   &'static str,
   open:    bool,
   toggle:  Action,
   options: Vec<(&'static str, bool, Action)>,
}

/// `cards` two to a row under a "`title` (count)" label, or `empty` when there
/// are none.
fn render_grid(
   b: &Builder,
   mut root: RawDiv,
   title: &str,
   cards: Vec<RawDiv>,
   empty: &str,
) -> RawDiv {
   let pal = crate::mana_theme::sunbird_palette();
   let text = crate::mana_theme::color_to_hsla(pal.text, None);
   if let Some(label) = b.text_line(&format!("{title} ({})", cards.len()), text, 16.0) {
      root = b.push_div(root, label);
   }
   if cards.is_empty() {
      let mut box_div = b.card(8.0, 16.0, 8.0, 0.18);
      box_div = b.push_div(box_div, b.text_block("Nothing here", empty));
      return b.push_div(root, box_div);
   }
   let mut grid = b.v_stack(12.0);
   let mut cards = cards.into_iter();
   while let Some(first) = cards.next() {
      let mut row = chain(b.sym.div_flex, b.raw_div());
      b.style_div(&mut row, |s| s.lengths(&s.at.gap, gpui_px(12.0)));
      row = b.push_div(row, first);
      if let Some(second) = cards.next() {
         row = b.push_div(row, second);
      } else {
         // An empty half keeps a lone last card as wide as the others.
         let mut spacer = b.raw_div();
         b.style_div(&mut spacer, |s| s.length(s.at.size[0], fraction(0.5)));
         row = b.push_div(row, spacer);
      }
      grid = b.push_div(grid, row);
   }
   b.push_div(root, grid)
}

/// A half-width card: `title` and `desc` on the left, `controls` on the
/// right.
fn grid_card(b: &Builder, title: &str, desc: &str, controls: RawDiv) -> RawDiv {
   const ALIGN_START: u8 = 2;
   const JUSTIFY_SPACE_BETWEEN: u8 = 6;
   let pal = crate::mana_theme::sunbird_palette();
   let mut card = chain(b.sym.div_flex, b.raw_div());
   b.style_div(&mut card, |s| {
      s.lengths(&s.at.gap, gpui_px(12.0));
      s.set(s.at.align_items, ALIGN_START);
      s.set(s.at.justify_content, JUSTIFY_SPACE_BETWEEN);
      s.lengths(&s.at.border_widths, gpui_px(1.0));
      s.lengths(&s.at.corner_radii, gpui_px(8.0));
      s.lengths(&s.at.padding, gpui_px(12.0));
      s.hsla(
         s.at.border_color,
         crate::mana_theme::color_to_hsla(pal.border, Some(0.24)),
      );
      // A faint tint of the text color lifts the card off whatever the theme
      // paints behind it, where a solid surface gray clashed with dark themes.
      s.solid_background(crate::mana_theme::color_to_hsla(pal.text, Some(0.03)));
      s.length(s.at.size[0], fraction(0.5));
      // Flex items keep their content's width unless allowed below it, which
      // pushed the pair past the page and the text past the controls.
      s.length(s.at.min_size[0], gpui_px(0.0));
   });
   let mut text = b.raw_div();
   b.style_div(&mut text, |s| {
      s.length(s.at.min_size[0], gpui_px(0.0));
      s.float(s.at.flex_shrink, 1.0);
   });
   text = b.push_div(text, b.text_block(title, desc));
   card = b.push_div(card, text);
   let mut controls = controls;
   b.style_div(&mut controls, |s| s.float(s.at.flex_shrink, 0.0));
   b.push_div(card, controls)
}

/// The filters, then the plugins as a grid of cards. A card's gear or info
/// icon opens its modal.
fn render_plugins_tab(
   b: &Builder,
   root: RawDiv,
   plugins: &[phoenix_runtime::PluginInfo],
   ui: &UiState,
   search: Option<crate::modal::PageField>,
) -> RawDiv {
   let mut actions = b.h_row(6.0, false);
   actions = b.push_button(
      actions,
      "sunbird-toolbar-install-url",
      "+ URL",
      BtnVariant::Primary,
      BtnSize::Small,
      Action::OpenInstallUrl,
   );
   actions = b.push_button(
      actions,
      "sunbird-toolbar-install-file",
      "+ File...",
      BtnVariant::Secondary,
      BtnSize::Small,
      Action::InstallFromFile,
   );
   actions = b.push_button(
      actions,
      "sunbird-toolbar-reload-all",
      "Reload All",
      BtnVariant::Secondary,
      BtnSize::Small,
      Action::ReloadAll,
   );
   let filters = [
      PluginFilter::All,
      PluginFilter::Enabled,
      PluginFilter::Disabled,
   ];
   let dropdown = Dropdown {
      label:   ui.filter.label(),
      open:    ui.filter_open,
      toggle:  Action::ToggleFilterMenu,
      options: filters
         .into_iter()
         .map(|filter| {
            (
               filter.label(),
               filter == ui.filter,
               Action::SetFilter(filter),
            )
         })
         .collect(),
   };
   let (root, query) = render_filters(b, root, search, dropdown, actions);

   let cards = plugins
      .iter()
      .filter(|plugin| {
         ui.filter.keeps(plugin)
            && matches_query(&plugin.name, plugin.header.description.as_deref(), &query)
      })
      .map(|plugin| plugin_card(b, plugin))
      .collect();
   let empty = if plugins.is_empty() {
      "Open the Plugin Catalog tab to install plugins."
   } else {
      "No plugins match the current filters."
   };
   render_grid(b, root, "Plugins", cards, empty)
}

/// A plugin's card: its name and description, the gear or info icon, and its
/// switch.
fn plugin_card(b: &Builder, plugin: &phoenix_runtime::PluginInfo) -> RawDiv {
   let mut desc = plugin.header.description.clone().unwrap_or_default();
   // The modal shows the whole error; a stack trace would stretch the card.
   if let Some(err) = plugin.state.error().and_then(|err| err.lines().next()) {
      if !desc.is_empty() {
         desc.push_str(" · ");
      }
      desc.push_str("Error: ");
      desc.push_str(err);
   }
   let mut controls = b.h_row(8.0, false);
   let (svg, fallback) = if plugin.settings.is_empty() {
      (INFO_SVG, "i")
   } else {
      (GEAR_SVG, "\u{2699}")
   };
   if let Some(icon) = b.icon_button(svg, fallback, Action::OpenPlugin(plugin.name.clone())) {
      controls = b.push_div(controls, icon);
   }
   controls = b.push_div(
      controls,
      b.switch_row(
         &format!("sunbird-plugin-{}", plugin.name),
         "",
         None,
         plugin.state.is_on(),
         Action::TogglePlugin(plugin.name.clone()),
      ),
   );
   grid_card(b, &plugin.name, &desc, controls)
}

/// The filters, then the site's plugins as a grid of cards, each with its
/// install button.
fn render_catalog_tab(
   b: &Builder,
   mut root: RawDiv,
   installed: &HashSet<String>,
   ui: &UiState,
   search: Option<crate::modal::PageField>,
) -> RawDiv {
   let actions = b.push_button(
      b.h_row(6.0, false),
      "sunbird-catalog-refresh",
      "Refresh",
      BtnVariant::Secondary,
      BtnSize::Small,
      Action::RefreshCatalog,
   );
   let groups = [
      None,
      Some(CatalogGroup::Desktop),
      Some(CatalogGroup::Partial),
      Some(CatalogGroup::Other),
   ];
   let dropdown = Dropdown {
      label:   CatalogGroup::label(ui.catalog_filter),
      open:    ui.catalog_filter_open,
      toggle:  Action::ToggleCatalogFilterMenu,
      options: groups
         .into_iter()
         .map(|group| {
            (
               CatalogGroup::label(group),
               group == ui.catalog_filter,
               Action::SetCatalogFilter(group),
            )
         })
         .collect(),
   };
   let (filtered_root, query) = render_filters(b, root, search, dropdown, actions);
   root = filtered_root;

   let catalog = update::catalog();
   if matches!(catalog, Catalog::Unfetched) {
      update::fetch_catalog();
   }
   let items = match catalog {
      Catalog::Unfetched | Catalog::Loading => {
         let mut box_div = b.card(8.0, 14.0, 8.0, 0.18);
         box_div = b.push_div(
            box_div,
            b.text_block(
               "Fetching catalog...",
               "Loading official plugins from assets.sunbird.tools.",
            ),
         );
         return b.push_div(root, box_div);
      },
      Catalog::Failed(err) => {
         let mut box_div = b.card(8.0, 14.0, 8.0, 0.22);
         box_div = b.push_div(
            box_div,
            b.action_row(
               "sunbird-catalog-retry",
               "Could not reach catalog",
               &err,
               "Retry",
               BtnVariant::Primary,
               BtnSize::Small,
               Action::RefreshCatalog,
            ),
         );
         return b.push_div(root, box_div);
      },
      Catalog::Loaded(items) => items,
   };

   let cards = items
      .iter()
      .filter(|item| {
         ui.catalog_filter
            .is_none_or(|group| CatalogGroup::of(&item.name) == group)
            && matches_query(&item.name, Some(&item.description), &query)
      })
      .map(|item| catalog_card(b, item, installed.contains(&item.name)))
      .collect();
   render_grid(
      b,
      root,
      "Plugins",
      cards,
      "No plugins match the current filters.",
   )
}

/// A site plugin's card: its name, description and note, and its install
/// button over its desktop support.
fn catalog_card(b: &Builder, item: &Listed, installed: bool) -> RawDiv {
   const ALIGN_END: u8 = 3;
   let mut desc = item.description.clone();
   if let Some(note) = item.warning.as_deref().filter(|note| !note.is_empty()) {
      desc.push_str(" · Note: ");
      desc.push_str(note);
   }
   // The tag goes under the button, in the space the description leaves.
   let mut column = b.v_stack(8.0);
   b.style_div(&mut column, |s| s.set(s.at.align_items, ALIGN_END));
   let mut controls = b.push_button(
      column,
      &format!("sunbird-cat-{}", item.name),
      if installed { "Reinstall" } else { "Install" },
      if installed {
         BtnVariant::Secondary
      } else {
         BtnVariant::Primary
      },
      BtnSize::Small,
      Action::InstallCatalog(item.name.clone()),
   );
   if let Some(badge) = b.compat_badge(DesktopCompat::of(&item.name)) {
      controls = b.push_div(controls, badge);
   }
   grid_card(b, &item.name, &desc, controls)
}

/// The page the last gear or info icon opened, rebuilt after each change on
/// it.
static PLUGIN_PAGE: Mutex<Option<u64>> = Mutex::new(None);

fn open_plugin_page(name: &str) {
   crate::modal::refill_page_fields();
   if let Some(spec) = plugin_page(name) {
      *PLUGIN_PAGE.lock().unwrap_or_else(PoisonError::into_inner) =
         Some(crate::modal::open_page(spec));
   }
}

/// Shows `name`'s page again with what changed, and the cards under it.
fn refresh_plugin_page(name: &str) {
   let id = *PLUGIN_PAGE.lock().unwrap_or_else(PoisonError::into_inner);
   if let (Some(id), Some(spec)) = (id, plugin_page(name)) {
      crate::modal::replace_page(id, spec);
   }
   crate::modal::force_redraw();
}

fn page_button(
   label: &str,
   primary: bool,
   on_press: impl Fn() + Send + Sync + 'static,
) -> ModalButton {
   ModalButton {
      label: label.to_owned(),
      danger: false,
      primary,
      on_press: Arc::new(on_press),
   }
}

/// A plugin's modal: what it is and reaches, its actions, and its settings
/// while it is on.
fn plugin_page(name: &str) -> Option<PageModalSpec> {
   let plugin = phoenix_runtime::plugins()
      .into_iter()
      .find(|p| p.name == name)?;
   let mut sections = Vec::new();
   if let Some(desc) = &plugin.header.description {
      sections.push(ModalSection::Text {
         title: None,
         text:  desc.clone(),
      });
   }

   let mut about = Vec::new();
   match (&plugin.header.version, &plugin.header.author) {
      (Some(version), Some(author)) => about.push(format!("v{version} by {author}")),
      (Some(version), None) => about.push(format!("v{version}")),
      (None, Some(author)) => about.push(format!("By {author}")),
      (None, None) => {},
   }
   if plugin.permissions.is_empty() {
      about.push("No permissions".to_owned());
   } else {
      let labels = plugin
         .permissions
         .iter()
         .map(|permission| permission.label());
      about.push(format!("Reaches {}", labels.collect::<Vec<_>>().join(", ")));
   }
   match DesktopCompat::of(&plugin.name) {
      DesktopCompat::Full => {},
      DesktopCompat::Partial => about.push("Partial desktop support".to_owned()),
      DesktopCompat::MobileOnly => {
         about.push("Targets mobile UI; may not work on desktop".to_owned());
      },
      DesktopCompat::Unverified => about.push("Unverified on desktop".to_owned()),
   }
   if let Some(err) = plugin.state.error() {
      about.push(format!("Error: {err}"));
   }
   sections.push(ModalSection::Text {
      title: Some("About".to_owned()),
      text:  about.join("\n"),
   });

   let mut actions = Vec::new();
   let owned = plugin.name.clone();
   actions.push(page_button("Edit Code", false, move || {
      run(Action::EditPluginCode(owned.clone()));
   }));
   if plugin.state.is_on() && plugin.installed {
      let owned = plugin.name.clone();
      actions.push(page_button("Reload", false, move || {
         phoenix_runtime::reload(&owned);
         refresh_plugin_page(&owned);
      }));
   }
   if !plugin.state.is_on() && plugin.installed {
      let owned = plugin.name.clone();
      actions.push(ModalButton {
         label:    "Remove".to_owned(),
         danger:   true,
         primary:  false,
         on_press: Arc::new(move || {
            run(Action::UninstallPlugin(owned.clone()));
            let page = *PLUGIN_PAGE.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(id) = page {
               crate::modal::close_page(id);
            }
         }),
      });
   }
   sections.push(ModalSection::Buttons {
      title:   None,
      buttons: actions,
   });

   if plugin.settings.is_empty() {
      return Some(PageModalSpec {
         title: plugin.name,
         sections,
      });
   }
   if !plugin.state.is_on() {
      sections.push(ModalSection::Text {
         title: Some("Settings".to_owned()),
         text:  "Turn the plugin on to change them.".to_owned(),
      });
      return Some(PageModalSpec {
         title: plugin.name,
         sections,
      });
   }
   for setting in &plugin.settings {
      sections.push(setting_section(&plugin.name, setting));
   }
   Some(PageModalSpec {
      title: plugin.name,
      sections,
   })
}

/// One setting on a plugin's modal. Each change shows the page again.
fn setting_section(plugin: &str, setting: &phoenix_runtime::Setting) -> ModalSection {
   let title = |shown: Option<&str>| {
      let mut line = setting.title.clone();
      for part in [setting.description.as_deref(), shown]
         .into_iter()
         .flatten()
      {
         if !part.is_empty() {
            line.push_str(" · ");
            line.push_str(part);
         }
      }
      Some(line)
   };
   let (plugin, key) = (plugin.to_owned(), setting.key.clone());
   match &setting.control {
      Control::Switch(checked) => {
         ModalSection::Switch {
            id:          format!("sunbird-set-{plugin}-{key}"),
            label:       setting.title.clone(),
            description: setting.description.clone(),
            checked:     *checked,
            on_toggle:   Arc::new(move |on| {
               phoenix_runtime::set_setting(&plugin, &key, Chosen::Switch(on));
               refresh_plugin_page(&plugin);
            }),
         }
      },
      Control::Select { options, chosen } => {
         let buttons = options
            .iter()
            .map(|option| {
               let (plugin, key, value) = (plugin.clone(), key.clone(), option.value.clone());
               page_button(&option.label, option.value == *chosen, move || {
                  phoenix_runtime::set_setting(&plugin, &key, Chosen::Text(value.clone()));
                  refresh_plugin_page(&plugin);
               })
            })
            .collect();
         ModalSection::Buttons {
            title: title(None),
            buttons,
         }
      },
      Control::Button => {
         ModalSection::Buttons {
            title:   setting.description.clone(),
            buttons: vec![page_button(&setting.title, false, move || {
               phoenix_runtime::press(&plugin, &key);
            })],
         }
      },
      Control::Text(value) => {
         ModalSection::Field {
            id:          format!("sunbird-set-{plugin}-{key}"),
            label:       setting.title.clone(),
            description: setting.description.clone(),
            initial:     value.clone(),
            multiline:   setting.multiline,
            on_change:   Arc::new(move |text| {
               phoenix_runtime::set_setting(&plugin, &key, Chosen::Text(text));
            }),
         }
      },
      Control::Number(value) => {
         ModalSection::Field {
            id:          format!("sunbird-set-{plugin}-{key}"),
            label:       setting.title.clone(),
            description: setting.description.clone(),
            initial:     value.to_string(),
            multiline:   false,
            // Text that is not a number yet waits for the next change.
            on_change:   Arc::new(move |text| {
               if let Ok(number) = text.trim().parse::<f64>()
                  && number.is_finite()
               {
                  phoenix_runtime::set_setting(&plugin, &key, Chosen::Number(number));
               }
            }),
         }
      },
      // A secret is never drawn, so it is typed into the editor, which starts
      // empty.
      Control::Secret(value) => {
         let shown = if value.is_empty() { "not set" } else { "set" };
         let reopen = plugin.clone();
         let field = SettingField {
            plugin,
            key,
            title: setting.title.clone(),
            // Closing the editor brings the page back, still showing the old
            // value.
            then: Some(Arc::new(move || refresh_plugin_page(&reopen))),
         };
         ModalSection::Buttons {
            title:   title(Some(shown)),
            buttons: vec![page_button("Edit", false, move || {
               crate::modal::open_setting_editor(field.clone());
            })],
         }
      },
   }
}

fn render_general_tab(b: &Builder, mut root: RawDiv) -> RawDiv {
   // Card 1: Discord & Developer controls
   let mut dev_card = b.card(12.0, 16.0, 8.0, 0.20);
   dev_card = b.push_div(
      dev_card,
      b.switch_row(
         "sunbird-staff-unlock",
         "Staff Unlock",
         Some("Unlock Discord staff-only settings, experiments, and internal developer menus."),
         phoenix_runtime::staff_enabled(),
         Action::ToggleStaff,
      ),
   );
   dev_card = b.push_div(dev_card, b.divider());
   dev_card = b.push_div(
      dev_card,
      b.action_row(
         "sunbird-open-devtools",
         "Runtime Console & DevTools",
         &format!(
            "Evaluate JavaScript in QuickJS and view live runtime logs ({}).",
            phoenix_ui_desktop::DEVTOOLS_SHORTCUT
         ),
         "Open DevTools",
         BtnVariant::Secondary,
         BtnSize::Medium,
         Action::OpenDevTools,
      ),
   );
   dev_card = b.push_div(dev_card, b.divider());
   dev_card = b.push_div(
      dev_card,
      b.action_row(
         "sunbird-reload-all",
         "Reload All Plugins",
         "Re-evaluate every currently enabled plugin without restarting Discord.",
         "Reload All",
         BtnVariant::Secondary,
         BtnSize::Medium,
         Action::ReloadAll,
      ),
   );
   root = b.push_div(root, dev_card);

   // Card 2: how the launcher starts Sunbird (`launcher.conf`)
   let config = crate::config::load();
   let mut start_card = b.card(12.0, 16.0, 8.0, 0.20);
   start_card = b.push_div(
      start_card,
      b.switch_row(
         "sunbird-auto-start",
         "Start Sunbird on login",
         Some("Open Discord with Sunbird when you sign in to your computer."),
         config.auto_start,
         Action::ToggleAutoStart,
      ),
   );
   start_card = b.push_div(start_card, b.divider());
   start_card = b.push_div(
      start_card,
      b.switch_row(
         "sunbird-auto-update",
         "Auto-update Sunbird on launch",
         Some("Download the newest Sunbird loader each time the launcher starts Discord."),
         config.auto_update,
         Action::ToggleAutoUpdate,
      ),
   );
   let failed = crate::health::snapshot().feature_failures();
   if !failed.is_empty() {
      let names = failed
         .iter()
         .map(|patch| patch.name)
         .collect::<Vec<_>>()
         .join(", ");
      start_card = b.push_div(start_card, b.divider());
      start_card = b.push_div(
         start_card,
         b.text_block(
            "Patch Health",
            &format!(
               "Some features did not hook on this Discord build and are off: {names}. The rest \
                of Sunbird works."
            ),
         ),
      );
   }
   root = b.push_div(root, start_card);

   // Card 3: Updates
   let (upd_desc, upd_btn, upd_var, upd_act) = updates_card_state();

   let mut upd_card = b.card(10.0, 16.0, 8.0, 0.20);
   upd_card = b.push_div(
      upd_card,
      b.action_row(
         "sunbird-updates",
         "Sunbird Updates",
         &upd_desc,
         upd_btn,
         upd_var,
         BtnSize::Medium,
         upd_act,
      ),
   );
   b.push_div(root, upd_card)
}

fn updates_card_state() -> (String, &'static str, BtnVariant, Action) {
   let status = update::status();
   if update::busy() {
      return (
         status.unwrap_or_else(|| "Checking for updates...".to_owned()),
         "Working...",
         BtnVariant::Secondary,
         Action::CheckUpdates,
      );
   }
   if let Some(found) = update::found() {
      if !found.plugins.is_empty() {
         let desc = match &found.loader {
            Some(version) => {
               format!(
                  "Plugin updates ready: {} · Sunbird {version} available (restart to update)",
                  found.plugins.join(", ")
               )
            },
            None => format!("Plugin updates ready: {}", found.plugins.join(", ")),
         };
         return (
            desc,
            "Apply Updates",
            BtnVariant::Primary,
            Action::ApplyUpdates,
         );
      }
      if let Some(version) = found.loader {
         return (
            format!("Sunbird {version} is available — restart Sunbird to update."),
            "Check for Updates",
            BtnVariant::Secondary,
            Action::CheckUpdates,
         );
      }
   }
   (
      status.unwrap_or_else(|| format!("Sunbird {} is up to date.", update::VERSION)),
      "Check for Updates",
      BtnVariant::Secondary,
      Action::CheckUpdates,
   )
}
