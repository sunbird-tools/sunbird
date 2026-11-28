//! Collapsing the channel sidebar, for `phoenix.ui.collapseChannels`.
//!
//! The workspace sizes the sidebar from `SidebarResize`'s width, which the
//! resize handle clamps to 264..432 px, so the width itself can't reach the
//! server rail. While collapsed, the width getter answers the rail plus its
//! 1 px border inside the two functions that read it for layout, which leaves
//! the channel column 0 px wide and tells other views the sidebar ends at the
//! rail. The stored width stays, so restoring brings back the user's own width,
//! and the resize handle is left out as when an overlay covers the workspace.

use std::{
   cell::Cell,
   ffi::c_void,
   sync::{
      Mutex,
      OnceLock,
      PoisonError,
      atomic::{
         AtomicBool,
         Ordering,
      },
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::locate::channel;

/// `f32 width(&RootModel, &App)`, and `column`'s same signature.
type WidthFn = unsafe extern "C" fn(*const c_void, *const c_void) -> f32;
/// `publish(&RootModel, target, &App)`.
type PublishFn = unsafe extern "C" fn(*const c_void, *const c_void, *const c_void);
/// `f32 rail()`.
type RailFn = unsafe extern "C" fn() -> f32;
/// `bool covered(&RootModel, ..)`. Linux builds take a second argument, which
/// the detour passes along.
type CoveredFn = unsafe extern "C" fn(*const c_void, *const c_void) -> bool;

/// The server rail's right border, which the column's width leaves out.
const RAIL_BORDER: f32 = 1.0;

static COLLAPSED: AtomicBool = AtomicBool::new(false);
static WIDTH: OnceLock<WidthFn> = OnceLock::new();
static COLUMN: OnceLock<WidthFn> = OnceLock::new();
static PUBLISH: OnceLock<PublishFn> = OnceLock::new();
static RAIL: OnceLock<RailFn> = OnceLock::new();
static COVERED: OnceLock<CoveredFn> = OnceLock::new();
/// Whether every function was hooked, so collapsing can take effect.
static READY: AtomicBool = AtomicBool::new(false);
/// Plugins holding the sidebar collapsed.
static HOLDERS: Mutex<usize> = Mutex::new(0);

thread_local! {
   /// Inside `column` or `publish`, whose width reads answer the rail while
   /// collapsed. The settings save reads the width outside them.
   static LAYOUT: Cell<bool> = const { Cell::new(false) };
}

/// Hooks the width getter, its two layout callers and the handle's gate from
/// [`crate::prepare`], before Discord's threads run.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   let found =
      match crate::located::get("channel::channel_sidebar", || channel::channel_sidebar(ix)) {
         Ok(found) => found,
         Err(err) => {
            log::warn!("channel sidebar: {err}, it stays");
            return;
         },
      };
   // SAFETY: `rail` is a small function taking no arguments and returning an
   // `f32`, which the locator checked by the column width calling it alone.
   let _ = RAIL.set(unsafe { std::mem::transmute::<usize, RailFn>(found.rail) });
   let column = hook(
      image,
      &COLUMN,
      found.column,
      column_detour as *const () as usize,
   ) || hook_width_call(image, found.column_site);
   let hooked = hook(
      image,
      &WIDTH,
      found.width,
      width_detour as *const () as usize,
   ) && column
      && hook(
         image,
         &PUBLISH,
         found.publish,
         publish_detour as *const () as usize,
      )
      && hook(
         image,
         &COVERED,
         found.covered,
         covered_detour as *const () as usize,
      );
   READY.store(hooked, Ordering::Release);
}

/// Points `column`'s call of the width getter at [`column_width_detour`].
fn hook_width_call(image: &Image<'static>, site: usize) -> bool {
   // SAFETY: runs from `prepare` before Discord's threads; the detour takes the
   // getter's exact arguments and calls the getter through `WIDTH`.
   match unsafe { image.hook_call(site, &[], column_width_detour as *const () as usize) } {
      Ok(_) => {
         log::info!("channel sidebar: hooked the column's width call @ {site:#x}");
         true
      },
      Err(err) => {
         log::warn!("channel sidebar: column width call @ {site:#x}: {err}");
         false
      },
   }
}

/// Hooks `target` with `detour` and stores its original in `slot`.
fn hook<F: Copy>(image: &Image<'static>, slot: &OnceLock<F>, target: usize, detour: usize) -> bool {
   // SAFETY: runs from `prepare` before Discord's threads; each detour has its
   // target's signature as `F` and waits for `slot` before it calls the
   // original.
   let Some(original) = (unsafe { crate::hook::<F>(image, target, detour, "channel sidebar") })
   else {
      return false;
   };
   let _ = slot.set(original);
   log::info!("channel sidebar: hooked @ {target:#x}");
   true
}

/// Holds the sidebar collapsed until the matching `collapse(false)`. False when
/// this build's sidebar was not found.
pub fn collapse(hold: bool) -> bool {
   if !ready() {
      return false;
   }
   let mut holders = HOLDERS.lock().unwrap_or_else(PoisonError::into_inner);
   *holders = if hold {
      *holders + 1
   } else {
      holders.saturating_sub(1)
   };
   let collapsed = *holders > 0;
   let changed = COLLAPSED.swap(collapsed, Ordering::AcqRel) != collapsed;
   drop(holders);
   if changed {
      crate::accessories::invalidate_discord_windows();
   }
   true
}

pub fn ready() -> bool {
   READY.load(Ordering::Acquire)
}

unsafe extern "C" fn width_detour(root: *const c_void, cx: *const c_void) -> f32 {
   if COLLAPSED.load(Ordering::Acquire)
      && LAYOUT.get()
      && let Some(rail) = RAIL.get()
   {
      // SAFETY: the rail width reads only Discord's density setting.
      return unsafe { rail() } + RAIL_BORDER;
   }
   let width = *WIDTH.wait();
   // SAFETY: forwards the getter's own arguments.
   unsafe { width(root, cx) }
}

unsafe extern "C" fn column_detour(root: *const c_void, cx: *const c_void) -> f32 {
   let column = *COLUMN.wait();
   let outer = LAYOUT.replace(true);
   // SAFETY: forwards the column width's own arguments.
   let width = unsafe { column(root, cx) };
   LAYOUT.set(outer);
   width
}

unsafe extern "C" fn column_width_detour(root: *const c_void, cx: *const c_void) -> f32 {
   let outer = LAYOUT.replace(true);
   // SAFETY: forwards the getter's own arguments.
   let width = unsafe { width_detour(root, cx) };
   LAYOUT.set(outer);
   width
}

unsafe extern "C" fn publish_detour(root: *const c_void, target: *const c_void, cx: *const c_void) {
   let publish = *PUBLISH.wait();
   let outer = LAYOUT.replace(true);
   // SAFETY: forwards the publish's own arguments.
   unsafe { publish(root, target, cx) };
   LAYOUT.set(outer);
}

unsafe extern "C" fn covered_detour(root: *const c_void, extra: *const c_void) -> bool {
   if COLLAPSED.load(Ordering::Acquire) {
      return true;
   }
   let covered = *COVERED.wait();
   // SAFETY: forwards the predicate's own arguments.
   unsafe { covered(root, extra) }
}
