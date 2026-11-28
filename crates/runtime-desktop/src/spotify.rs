use std::sync::{
   Mutex,
   OnceLock,
   PoisonError,
};

use phoenix_sigs::locate::{
   CodePatch,
   spotify,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tweak {
   NoAutoPause,
   IdleActivity,
}

impl Tweak {
   const fn index(self) -> usize {
      match self {
         Self::NoAutoPause => 0,
         Self::IdleActivity => 1,
      }
   }
}

static PATCHES: [OnceLock<CodePatch>; 2] = [OnceLock::new(), OnceLock::new()];
static HOLDERS_PER_TWEAK: Mutex<[usize; 2]> = Mutex::new([0; 2]);

pub fn install(ix: &crate::located::LazyIndex<'_>) {
   let found = [
      (
         Tweak::NoAutoPause,
         crate::located::get("spotify::auto_pause", || spotify::auto_pause(ix)),
      ),
      (
         Tweak::IdleActivity,
         crate::located::get("spotify::idle_activity", || spotify::idle_activity(ix)),
      ),
   ];
   for (tweak, patch) in found {
      match patch {
         Ok(patch) => {
            let _ = PATCHES[tweak.index()].set(patch);
         },
         Err(err) => log::warn!("spotify {tweak:?}: {err}, left as Discord ships it"),
      }
   }
}

pub fn ready(tweak: Tweak) -> bool {
   PATCHES[tweak.index()].get().is_some()
}

/// Takes a hold on `tweak`, patching it in on the first one. False when this
/// build's site was not found or not writable.
pub fn acquire(tweak: Tweak) -> bool {
   let Some(patch) = PATCHES[tweak.index()].get() else {
      return false;
   };
   let mut holders = HOLDERS_PER_TWEAK
      .lock()
      .unwrap_or_else(PoisonError::into_inner);
   if holders[tweak.index()] == 0 && !write(tweak, patch, patch.patched) {
      return false;
   }
   holders[tweak.index()] += 1;
   true
}

/// Drops a hold [`acquire`] took, restoring Discord's code after the last.
pub fn release(tweak: Tweak) {
   let Some(patch) = PATCHES[tweak.index()].get() else {
      return;
   };
   let mut holders = HOLDERS_PER_TWEAK
      .lock()
      .unwrap_or_else(PoisonError::into_inner);
   let held = &mut holders[tweak.index()];
   *held = held.saturating_sub(1);
   if *held == 0 {
      write(tweak, patch, patch.shipped);
   }
   // Held through the write, so an acquire cannot patch in between.
   drop(holders);
}

fn write(tweak: Tweak, patch: &CodePatch, bytes: [u8; 4]) -> bool {
   let Some(state) = phoenix_runtime::state() else {
      return false;
   };
   // SAFETY: the locator patches one x86 opcode byte or one aligned arm64
   // instruction, so a thread at the site runs the old or the new
   // instruction whole.
   match unsafe { state.image.write_code(patch.site, &bytes[..patch.len]) } {
      Ok(()) => true,
      Err(err) => {
         log::error!("spotify {tweak:?}: {err}");
         false
      },
   }
}
