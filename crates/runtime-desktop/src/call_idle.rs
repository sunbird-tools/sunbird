//! The call idle timers `phoenix.ui.disableCallIdle` turns off. Each timer's
//! `Future::poll` goes through a shim that answers `Poll::Ready(())` while the
//! timer is off, so its future ends without acting, and otherwise jumps to the
//! poll with every register as Discord passed it.

use std::{
   str::FromStr,
   sync::atomic::{
      AtomicBool,
      AtomicUsize,
      Ordering,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::locate::call_idle;

static DM_OFF: AtomicBool = AtomicBool::new(false);
static AFK_OFF: AtomicBool = AtomicBool::new(false);
static DM_POLL: AtomicUsize = AtomicUsize::new(0);
static AFK_MOVE_POLL: AtomicUsize = AtomicUsize::new(0);
static AFK_DISCONNECT_POLL: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Timer {
   DmCall,
   GuildAfk,
}

impl FromStr for Timer {
   type Err = ();

   fn from_str(name: &str) -> Result<Self, ()> {
      match name {
         "dmCall" => Ok(Self::DmCall),
         "guildAfk" => Ok(Self::GuildAfk),
         _ => Err(()),
      }
   }
}

/// Hooks the solo DM call timer from [`crate::prepare`], before Discord's
/// threads run.
pub fn install_dm(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   match crate::located::get("call_idle::dm_idle", || call_idle::dm_idle(ix)) {
      // SAFETY: runs before Discord's threads, and the shim keeps the poll's
      // ABI.
      Ok(poll) => unsafe { hook(image, poll, dm_shim as *const () as usize, &DM_POLL) },
      Err(err) => log::warn!("DM call idle: {err}, the timer stays"),
   }
}

/// Hooks the guild AFK move and disconnect timers from [`crate::prepare`].
pub fn install_guild_afk(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   match crate::located::get("call_idle::guild_afk", || call_idle::guild_afk(ix)) {
      Ok(found) => {
         // SAFETY: as in `install_dm`.
         unsafe {
            hook(image, found.move_poll, afk_move_shim as *const () as usize, &AFK_MOVE_POLL);
            hook(
               image,
               found.disconnect_poll,
               afk_disconnect_shim as *const () as usize,
               &AFK_DISCONNECT_POLL,
            );
         }
      },
      Err(err) => log::warn!("guild AFK: {err}, the timers stay"),
   }
}

/// Turns off each timer in `off` and turns the others back on.
pub fn turn_off(off: impl IntoIterator<Item = Timer>) {
   let (mut dm, mut afk) = (false, false);
   for timer in off {
      match timer {
         Timer::DmCall => dm = true,
         Timer::GuildAfk => afk = true,
      }
   }
   DM_OFF.store(dm, Ordering::Release);
   AFK_OFF.store(afk, Ordering::Release);
}

/// # Safety
///
/// As for [`crate::hook`].
unsafe fn hook(image: &Image<'static>, poll: usize, shim: usize, original: &AtomicUsize) {
   // SAFETY: forwarded from the caller.
   if let Some(trampoline) = unsafe { crate::hook::<usize>(image, poll, shim, "call idle") } {
      original.store(trampoline, Ordering::Release);
      log::info!("call idle: hooked @ {poll:#x}");
   }
}

#[cfg(target_arch = "x86_64")]
macro_rules! shim {
   ($name:ident, $off:ident, $original:ident) => {
      #[unsafe(naked)]
      extern "C" fn $name() {
         core::arch::naked_asm!(
            "cmp byte ptr [rip + {off}], 0",
            "je 2f",
            "xor eax, eax",
            "ret",
            "2:",
            "jmp qword ptr [rip + {original}]",
            off = sym $off,
            original = sym $original,
         );
      }
   };
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
macro_rules! shim {
   ($name:ident, $off:ident, $original:ident) => {
      #[unsafe(naked)]
      extern "C" fn $name() {
         core::arch::naked_asm!(
            "adrp x16, {off}@PAGE",
            "ldrb w16, [x16, {off}@PAGEOFF]",
            "cbz w16, 2f",
            "mov w0, #0",
            "ret",
            "2:",
            "adrp x16, {original}@PAGE",
            "ldr x16, [x16, {original}@PAGEOFF]",
            "br x16",
            off = sym $off,
            original = sym $original,
         );
      }
   };
}

// ELF spells the page and low-12 relocations differently from Mach-O.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
macro_rules! shim {
   ($name:ident, $off:ident, $original:ident) => {
      #[unsafe(naked)]
      extern "C" fn $name() {
         core::arch::naked_asm!(
            "adrp x16, {off}",
            "ldrb w16, [x16, :lo12:{off}]",
            "cbz w16, 2f",
            "mov w0, #0",
            "ret",
            "2:",
            "adrp x16, {original}",
            "ldr x16, [x16, :lo12:{original}]",
            "br x16",
            off = sym $off,
            original = sym $original,
         );
      }
   };
}

shim!(dm_shim, DM_OFF, DM_POLL);
shim!(afk_move_shim, AFK_OFF, AFK_MOVE_POLL);
shim!(afk_disconnect_shim, AFK_OFF, AFK_DISCONNECT_POLL);
