//! Skips `ForwardDialog`'s single-destination `Workspace` channel switch while
//! `phoenix.ui.disableForwardFollow` has an active holder.

use std::sync::{
   Mutex, OnceLock, PoisonError,
   atomic::{AtomicBool, Ordering},
};

use phoenix_hook::image::Image;
use phoenix_sigs::index::Index;

/// `Workspace` channel navigation helper: `fn(workspace, id_lo, id_hi, cx)`.
type NavFn = unsafe extern "C" fn(usize, usize, usize, usize);

static DISABLED: AtomicBool = AtomicBool::new(false);
static HOLDERS: Mutex<usize> = Mutex::new(0);
static ORIG_NAV: OnceLock<NavFn> = OnceLock::new();
static ORIG_CLIPS: OnceLock<NavFn> = OnceLock::new();

fn hook_nav(
   image: &Image<'static>,
   func: usize,
   slot: &OnceLock<NavFn>,
   detour: usize,
) -> Result<(), String> {
   // SAFETY: runs from `prepare` before Discord's threads start; `detour`
   // matches the 4-word `extern "C"` ABI and preserves args when forwarding.
   let trampoline = unsafe { image.hook_entry(func, detour) }.map_err(|err| err.to_string())?;
   // SAFETY: `trampoline` replays the displaced prologue and continues in `func`.
   let _ = slot.set(unsafe { std::mem::transmute::<usize, NavFn>(trampoline) });
   Ok(())
}

/// Locates and hooks `ForwardDialog`'s `Workspace` channel switch during
/// [`crate::prepare`]. Leaves forwarding untouched if the locator misses.
pub fn install(image: &Image<'static>, ix: &Index<'_>) {
   let found = match phoenix_sigs::locate::forward::forward_follow(ix) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("forward: {err}, forward follow toggle unavailable");
         return;
      },
   };
   if let Err(err) = hook_nav(
      image,
      found.navigate,
      &ORIG_NAV,
      nav_detour as *const () as usize,
   ) {
      log::warn!("forward: navigate hook failed: {err}");
      return;
   }
   log::info!("forward: hooked navigate @ {:#x}", found.navigate);
   if let Some(clips) = found.clips {
      if let Err(err) = hook_nav(
         image,
         clips,
         &ORIG_CLIPS,
         clips_detour as *const () as usize,
      ) {
         log::warn!("forward: clips hook failed: {err}");
      } else {
         log::info!("forward: hooked clips @ {clips:#x}");
      }
   }
}

/// Whether the forward navigation hook is installed and ready.
pub fn ready() -> bool {
   ORIG_NAV.get().is_some()
}

/// Adds or removes a `disableForwardFollow` holder. While at least one holder
/// exists, post-forward single-destination channel switches are skipped.
pub fn disable(hold: bool) -> bool {
   if !ready() {
      return false;
   }
   let mut holders = HOLDERS.lock().unwrap_or_else(PoisonError::into_inner);
   if hold {
      *holders += 1;
   } else {
      *holders = holders.saturating_sub(1);
   }
   DISABLED.store(*holders > 0, Ordering::Release);
   true
}

unsafe extern "C" fn nav_detour(a: usize, b: usize, c: usize, d: usize) {
   if DISABLED.load(Ordering::Acquire) {
      return;
   }
   if let Some(orig) = ORIG_NAV.get() {
      // SAFETY: `orig` is the trampoline for `ForwardDialog`'s `Workspace` switch.
      unsafe { orig(a, b, c, d) };
   }
}

unsafe extern "C" fn clips_detour(a: usize, b: usize, c: usize, d: usize) {
   if DISABLED.load(Ordering::Acquire) {
      return;
   }
   if let Some(orig) = ORIG_CLIPS.get() {
      // SAFETY: `orig` is the trampoline for `clips-gallery`'s `Workspace` switch.
      unsafe { orig(a, b, c, d) };
   }
}
