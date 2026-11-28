//! Phoenix's sticker send, which hands each sticker the picker or
//! autocomplete chose to `phoenix.messages.onStickerPick` handlers first. A
//! handler that takes it stops the send before Phoenix posts the sticker.
//! While one is registered, the picker's Nitro tests are patched out so a
//! sticker that needs Nitro reaches the send too, in place of the Nitro ad.

use std::sync::{
   OnceLock,
   atomic::{
      AtomicBool,
      Ordering,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::locate::{
   CodePatch,
   sticker,
};

/// `ChatPaneView::chat_send_sticker_now(&mut self, sticker: Sticker, bool,
/// bool, &mut Window, &mut App, ..)`, the sticker passed by pointer and
/// dropped by the send. Eight words cover its arguments on every desktop.
type SendFn = unsafe extern "C" fn(*mut u8, *mut u8, usize, usize, usize, usize, usize, usize);

/// `ChatPaneView`'s `Option<ChannelId>`, `Some` when its tag is 1, measured on
/// Windows gb71ec80, Linux 2233 and gbd95647 and macOS.
const CHANNEL_TAG: usize = 0;
const CHANNEL_SOME: u32 = 1;
const CHANNEL_ID: usize = 8;
/// The sticker's id, after its name.
const STICKER_ID: usize = 0x18;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SEND: OnceLock<SendFn> = OnceLock::new();
static GATES: OnceLock<[CodePatch; 2]> = OnceLock::new();

/// Hooks the send from [`crate::prepare`], before Discord's threads run.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   match crate::located::get("sticker::locked_gates", || sticker::locked_gates(ix)) {
      Ok(gates) => {
         let _ = GATES.set(gates);
      },
      Err(err) => log::warn!("sticker gates: {err}, stickers needing Nitro show the ad"),
   }
   let func = match crate::located::get("sticker::sticker_send", || sticker::sticker_send(ix)) {
      Ok(func) => func,
      Err(err) => {
         log::warn!("sticker send: {err}, stickers go straight to Phoenix");
         return;
      },
   };
   // SAFETY: runs before Discord's threads, and the detour takes and passes on
   // every argument word the send has.
   if let Some(original) =
      unsafe { crate::hook::<SendFn>(image, func, send as *const () as usize, "sticker send") }
   {
      let _ = SEND.set(original);
      log::info!("sticker send: hooked @ {func:#x}");
   }
}

/// Whether a plugin holds an `onStickerPick` handler, patching the picker's
/// Nitro tests out while one does.
pub fn set_active(active: bool) {
   if ACTIVE.swap(active, Ordering::AcqRel) == active {
      return;
   }
   // Without the send hooked, a sticker needing Nitro would reach Discord.
   let (Some(gates), Some(_), Some(state)) = (GATES.get(), SEND.get(), phoenix_runtime::state())
   else {
      return;
   };
   for gate in gates {
      let bytes = if active { gate.patched } else { gate.shipped };
      // SAFETY: the locator patches one x86 opcode byte or one aligned arm64
      // instruction, so a thread at the site runs the old or the new
      // instruction whole.
      if let Err(err) = unsafe { state.image.write_code(gate.site, &bytes[..gate.len]) } {
         log::error!("sticker gates: {err}");
      }
   }
}

/// A snowflake, which is past 32 bits for anything made after 2015.
fn snowflake(id: u64) -> Option<u64> {
   (id > u64::from(u32::MAX) && id < 1 << 63).then_some(id)
}

/// The sticker and channel in a send, or `None` when the layout no longer
/// holds them.
///
/// # Safety
///
/// `pane` and `sticker` are the live arguments Phoenix passed the send.
unsafe fn read(pane: *const u8, sticker: *const u8) -> Option<(u64, u64)> {
   if pane.is_null() || sticker.is_null() {
      return None;
   }
   // SAFETY: both point at the structs the send reads these words of.
   let (tag, channel, id) = unsafe {
      (
         pane.add(CHANNEL_TAG).cast::<u32>().read_unaligned(),
         pane.add(CHANNEL_ID).cast::<u64>().read_unaligned(),
         sticker.add(STICKER_ID).cast::<u64>().read_unaligned(),
      )
   };
   (tag == CHANNEL_SOME).then_some(())?;
   Some((snowflake(id)?, snowflake(channel)?))
}

extern "C" fn send(
   pane: *mut u8,
   sticker: *mut u8,
   a2: usize,
   a3: usize,
   a4: usize,
   a5: usize,
   a6: usize,
   a7: usize,
) {
   // The jump is written just before `SEND` is set, all before Discord runs.
   let original = *SEND.wait();
   let taken = ACTIVE.load(Ordering::Acquire)
      && std::panic::catch_unwind(|| {
         // SAFETY: Phoenix passes its pane and the chosen sticker.
         let Some((id, channel)) = (unsafe { read(pane, sticker) }) else {
            log::warn!("sticker send: no sticker and channel where expected");
            return false;
         };
         phoenix_runtime::engine()
            .is_some_and(|engine| crate::js::ui::pick_sticker(&engine, id, channel))
      })
      .unwrap_or(false);
   if taken {
      // The send owns the sticker and would drop it. Its name's few bytes leak
      // instead, since nothing here knows its drop.
      return;
   }
   // SAFETY: the trampoline runs the send with Phoenix's arguments.
   unsafe { original(pane, sticker, a2, a3, a4, a5, a6, a7) };
}
