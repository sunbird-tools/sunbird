//! The composer's Nitro gift button, which `phoenix.ui.hide("composer.gift")`
//! leaves out, and `phoenix.ui.addComposerButton`'s buttons, drawn ahead of
//! the chat bar's own through a detour on the bar's render.

use std::sync::{
   Mutex,
   OnceLock,
   PoisonError,
};

use phoenix_hook::image::Image;
use phoenix_sigs::desktop;

/// `jne rel32`, and `nop; jmp rel32` to the same place on x86-64.
const SHOWN: [u8; 2] = [0x0F, 0x85];
const HIDDEN: [u8; 2] = [0x90, 0xE9];

struct Patch {
   site:   usize,
   shown:  [u8; 4],
   hidden: [u8; 4],
   len:    usize,
}

static GIFT: OnceLock<Patch> = OnceLock::new();
/// Plugins holding the gift button hidden.
static HIDERS: Mutex<usize> = Mutex::new(0);

/// Finds the gift button's branch from [`crate::prepare`], while the locator
/// indexes are built.
pub fn install(image: &Image<'static>) {
   if crate::X86_CODE {
      match crate::located::get("desktop::composer_gift", || desktop::composer_gift(image)) {
         Ok(site) if image.read(site, 2) == Some(&SHOWN[..]) => {
            let _ = GIFT.set(Patch {
               site,
               shown: [SHOWN[0], SHOWN[1], 0, 0],
               hidden: [HIDDEN[0], HIDDEN[1], 0, 0],
               len: 2,
            });
         },
         Ok(site) => log::warn!("composer gift: {site:#x} is no jne"),
         Err(err) => log::warn!("composer gift: {err}, the gift button stays"),
      }
   } else {
      match phoenix_sigs::macos::composer_gift(image) {
         Ok(gift) => {
            let _ = GIFT.set(Patch {
               site:   gift.site,
               shown:  gift.shown.to_le_bytes(),
               hidden: gift.hidden.to_le_bytes(),
               len:    4,
            });
         },
         Err(err) => log::warn!("composer gift: {err}, the gift button stays"),
      }
   }
}

/// Hides the gift button until the matching `hide_gift(false)`. False when
/// this build's button was not found.
pub fn hide_gift(hide: bool) -> bool {
   let (Some(patch), Some(state)) = (GIFT.get(), phoenix_runtime::state()) else {
      return false;
   };
   let mut hiders = HIDERS.lock().unwrap_or_else(PoisonError::into_inner);
   let next = if hide {
      *hiders + 1
   } else {
      hiders.saturating_sub(1)
   };
   if (*hiders > 0) != (next > 0) {
      let bytes = if hide {
         &patch.hidden[..patch.len]
      } else {
         &patch.shown[..patch.len]
      };
      // SAFETY: all patch bytes lie in one aligned word, which the locator
      // checked, so a thread at the branch reads the old or the new opcode
      // whole.
      if let Err(err) = unsafe { state.image.write_code(patch.site, bytes) } {
         log::error!("composer gift: {err}");
         return false;
      }
   }
   *hiders = next;
   true
}

pub mod buttons {
   use std::{
      cell::Cell,
      ffi::c_void,
      panic::{
         self,
         AssertUnwindSafe,
      },
      ptr,
      sync::{
         Mutex,
         OnceLock,
         PoisonError,
         atomic::Ordering,
         mpsc,
      },
   };

   use phoenix_hook::image::Image;
   use phoenix_sigs::locate::{
      clickable,
      composer,
   };

   use crate::{
      accessories,
      gpui,
      gpui::{
         BtnSize,
         BtnVariant,
         RawAnyElement,
         RawDiv,
         RcFnVtbl,
         W_FULL,
         build_raw_button,
         flex,
         gpui_px,
         gpui_rems,
         h_row,
         push_any,
         push_div,
         push_raw_button,
         raw_div,
         str_to_any,
         style_div,
         svg_any,
         write_any,
      },
      js,
      modal,
   };

   /// A plugin button as the render draws it: its icon, else its label, in
   /// its `tint` while it has one.
   pub struct Button {
      pub id:    u64,
      pub label: String,
      /// The plugin's ARGB tint.
      pub tint:  Option<u32>,
      /// The SVG the plugin passed as its icon, registered for drawing.
      pub icon:  Option<&'static str>,
   }

   static BUTTONS: Mutex<Vec<Button>> = Mutex::new(Vec::new());

   type RenderFn = unsafe extern "C" fn(usize, usize, usize, usize, usize) -> RawAnyElement;

   static ORIG: OnceLock<RenderFn> = OnceLock::new();

   /// Adds or replaces the button with `button.id` and redraws.
   pub fn set(button: Button) {
      let mut buttons = BUTTONS.lock().unwrap_or_else(PoisonError::into_inner);
      match buttons.iter_mut().find(|known| known.id == button.id) {
         Some(known) => *known = button,
         None => buttons.push(button),
      }
      drop(buttons);
      accessories::invalidate_discord_windows();
   }

   pub fn remove(ids: &[u64]) {
      let mut buttons = BUTTONS.lock().unwrap_or_else(PoisonError::into_inner);
      let before = buttons.len();
      buttons.retain(|button| !ids.contains(&button.id));
      if buttons.len() != before {
         drop(buttons);
         accessories::invalidate_discord_windows();
      }
   }

   /// Hooks the chat bar's button row from [`crate::prepare`].
   pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
      let func = match crate::located::get("composer::composer_buttons", || {
         composer::composer_buttons(ix)
      }) {
         Ok(func) => func,
         Err(err) => {
            log::warn!("composer buttons: {err}, plugin buttons stay off");
            return;
         },
      };
      // SAFETY: runs from `prepare` before Discord's threads, and `detour` has
      // the render's signature on every desktop, five arguments and an
      // `AnyElement` returned through the return slot.
      let Some(orig) = (unsafe {
         crate::hook::<RenderFn>(
            image,
            func,
            detour as *const () as usize,
            "composer buttons: hook failed",
         )
      }) else {
         return;
      };
      let _ = ORIG.set(orig);
      log::info!("composer buttons: hooked the button row @ {func:#x}");
      install_ids(image);
      install_tooltips(ix);
   }

   type DivIdFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv, *const u8, usize) -> *mut RawDiv;

   static ORIG_ID: OnceLock<DivIdFn> = OnceLock::new();

   thread_local! {
      /// Whether this thread is inside the chat bar's render.
      static IN_BAR: Cell<bool> = const { Cell::new(false) };
   }

   /// The emoji button's wrapper, which the plugin buttons join.
   const ANCHOR: &[u8] = b"emoji-tooltip";
   /// The chat bar's icons sit 2.5rem apart, so they follow Discord's zoom:
   /// 1.25rem boxes with 1.25rem between ours, and less before the emoji
   /// button, whose element is wider than its icon.
   const ICON_REMS: f32 = 1.25;
   const GAP_REMS: f32 = 1.25;
   const EMOJI_GAP_REMS: f32 = 0.875;
   /// The bar puts less before the emoji button's wrapper than between its
   /// other buttons, measured 0.375rem short.
   const LEAD_REMS: f32 = 0.375;
   /// `CursorStyle::PointingHand`.
   const CURSOR_POINTING_HAND: u8 = 5;

   /// Hooks `Div::id`, where the chat bar names the emoji button's wrapper.
   /// arm64 returns the div through `x8`, which `hook_sret` adapts.
   fn install_ids(image: &Image<'static>) {
      #[cfg(windows)]
      if crate::surfaces::div_id_hooked() {
         return;
      }
      let addr = crate::modal::DIV_ID.load(Ordering::Acquire);
      if addr == 0 {
         return;
      }
      // SAFETY: runs from `prepare` before Discord's threads, and `id_detour`
      // has `Div::id`'s signature with the return slot first.
      if let Some(orig) = unsafe {
         crate::hook_sret::<DivIdFn>(
            image,
            addr,
            id_detour as *const () as usize,
            "composer buttons: Div::id hook failed",
         )
      } {
         let _ = ORIG_ID.set(orig);
      }
   }

   unsafe extern "C" fn id_detour(
      out: *mut RawDiv,
      div: *mut RawDiv,
      id: *const u8,
      len: usize,
   ) -> *mut RawDiv {
      let orig = *ORIG_ID
         .get()
         .expect("the detour only runs once ORIG_ID is set");
      // SAFETY: forwards `Div::id`'s own arguments.
      let out = unsafe { orig(out, div, id, len) };
      // SAFETY: out is the native Div and id is the original Div::id string.
      unsafe { append_at_anchor(out, id, len) };
      out
   }

   pub unsafe fn append_at_anchor(out: *mut RawDiv, id: *const u8, len: usize) {
      if IN_BAR.get()
         && !out.is_null()
         && !id.is_null()
         && len == ANCHOR.len()
         // SAFETY: the id, `len` bytes.
         && unsafe { std::slice::from_raw_parts(id, len) } == ANCHOR
      {
         let _ = panic::catch_unwind(AssertUnwindSafe(|| join(out)));
      }
   }

   /// Lays the emoji button's wrapper out as a row and puts the plugin buttons
   /// in it, ahead of the emoji button Discord adds next.
   fn join(wrapper: *mut RawDiv) {
      const ALIGN_CENTER: u8 = 4;
      let buttons = BUTTONS.lock().unwrap_or_else(PoisonError::into_inner);
      if buttons.is_empty() {
         return;
      }
      // SAFETY: `wrapper` holds the div `Div::id` just returned, and gets back
      // the div it becomes.
      let div = unsafe { ptr::read(wrapper) };
      let Some(mut div) = flex(div) else {
         return;
      };
      style_div(&mut div, |s| {
         s.lengths(&s.at.gap, gpui_rems(EMOJI_GAP_REMS));
         s.set(s.at.align_items, ALIGN_CENTER);
      });
      let Some(mut ours) = h_row(0.0, false) else {
         return;
      };
      style_div(&mut ours, |s| {
         s.lengths(&s.at.gap, gpui_rems(GAP_REMS));
         s.length(s.at.padding[3], gpui_rems(LEAD_REMS));
      });
      for button in buttons.iter() {
         if let Some(cell) = icon_button(button) {
            if let Some(found) = CLICKABLE.get() {
               push_any(&mut ours, &mut tooltipped(cell, button, found));
            } else {
               ours = push_div(ours, cell);
            }
         }
      }
      drop(buttons);
      div = push_div(div, ours);
      // SAFETY: as above.
      unsafe { ptr::write(wrapper, div) };
   }

   /// Mana's `Clickable`, which the buttons sit in for their tooltips.
   static CLICKABLE: OnceLock<clickable::Clickable> = OnceLock::new();
   /// Bytes a `Clickable` is built in.
   const CLICKABLE_CAP: usize = 0x400;
   /// `TooltipPlacement::Above`.
   const TOOLTIP_ABOVE: u8 = 0;

   fn install_tooltips(ix: &crate::located::LazyIndex<'_>) {
      let found = crate::located::get("clickable::clickable", || clickable::clickable(ix))
         .and_then(callable)
         .and_then(|found| {
            let div = gpui::layout().map(|layout| layout.div.size);
            if found.size > CLICKABLE_CAP
               || found.inner + found.inner_size > found.size
               || div != Some(found.inner_size)
            {
               return Err(format!(
                  "Clickable is {:#x} bytes with a {:#x}-byte Div, not {div:x?}",
                  found.size, found.inner_size
               ));
            }
            Ok(found)
         });
      match found {
         Ok(found) => {
            let _ = CLICKABLE.set(found);
         },
         Err(err) => log::warn!("composer buttons: {err}, buttons have no tooltips"),
      }
   }

   /// `found` with `new` and `into_any` callable with the return slot first,
   /// as arm64 takes it in `x8`.
   fn callable(found: clickable::Clickable) -> Result<clickable::Clickable, String> {
      if crate::X86_CODE {
         return Ok(found);
      }
      let thunk = |func: usize| {
         phoenix_hook::image::sret_caller(func)
            .ok_or_else(|| format!("no memory for a Clickable thunk to {func:#x}"))
      };
      Ok(clickable::Clickable {
         new: thunk(found.new)?,
         into_any: thunk(found.into_any)?,
         ..found
      })
   }

   /// `cell` inside a Mana `Clickable` labeled `button.label`, which shows the
   /// label as its tooltip above the button.
   fn tooltipped(cell: RawDiv, button: &Button, found: &clickable::Clickable) -> RawAnyElement {
      type NewFn =
         unsafe extern "C" fn(*mut u8, *const u8, usize, *mut gpui::SharedString) -> *mut u8;
      type IntoAnyFn = unsafe extern "C" fn(*mut RawAnyElement, *mut u8) -> *mut RawAnyElement;
      let mut inner = std::mem::MaybeUninit::<RawAnyElement>::uninit();
      write_any(cell, inner.as_mut_ptr());
      let id = gpui::intern_str(&format!("sunbird-composer-{}", button.id));
      let mut label = gpui::SharedString::from_str(&button.label);
      let mut built = gpui::Scratch::<CLICKABLE_CAP>::zeroed();
      let mut out = std::mem::MaybeUninit::<RawAnyElement>::uninit();
      // SAFETY: `found` was located and checked at install: `new` builds a
      // `found.size`-byte `Clickable`, which `built` holds, taking the label;
      // its `Div`, as large as ours, sits in it at `found.inner`, the
      // placement byte at `found.tooltip`; and `into_any` consumes it.
      unsafe {
         let mut inner = inner.assume_init();
         let new = std::mem::transmute::<usize, NewFn>(found.new);
         let into_any = std::mem::transmute::<usize, IntoAnyFn>(found.into_any);
         new(built.as_mut_ptr(), id.as_ptr(), id.len(), &raw mut label);
         // The inner `Div` need not sit aligned, so it moves out and back.
         let at = built.as_mut_ptr().add(found.inner);
         let mut div = RawDiv::zeroed();
         ptr::copy_nonoverlapping(at, div.as_mut_ptr(), found.inner_size);
         push_any(&mut div, &mut inner);
         ptr::copy_nonoverlapping(div.as_mut_ptr(), at, found.inner_size);
         built.put(found.tooltip, TOOLTIP_ABOVE);
         into_any(out.as_mut_ptr(), built.as_mut_ptr());
         out.assume_init()
      }
   }
   /// A button drawn like the chat bar's own: its icon in Discord's
   /// interactive icon color, or the plugin's tint while it has one.
   fn icon_button(button: &Button) -> Option<RawDiv> {
      let color = button.tint.map_or_else(
         || crate::mana_theme::color_to_hsla(crate::mana_theme::sunbird_palette().icon, None),
         gpui::argb_to_hsla,
      );
      let mut cell = h_row(0.0, false)?;
      style_div(&mut cell, |s| {
         s.set(s.at.mouse_cursor, CURSOR_POINTING_HAND);
      });
      if let Some(mut icon) = button
         .icon
         .and_then(|path| svg_any(path, gpui_rems(ICON_REMS), color))
      {
         push_any(&mut cell, &mut icon);
      } else {
         let mut label = raw_div()?;
         style_div(&mut label, |s| {
            s.length(s.at.font_size, gpui_px(13.0));
            s.length(s.at.line_height, gpui_px(18.0));
            s.hsla(s.at.text_color, color);
         });
         let mut text = str_to_any(&button.label)?;
         push_any(&mut label, &mut text);
         cell = push_div(cell, label);
      }
      // The id is never 0, so the data pointer is never null.
      modal::listen_mouse_down(&mut cell, button.id as *mut u8, &MOUSE_VTBL);
      Some(cell)
   }

   /// A press on a plugin button. It is zero-sized, so the button's id rides in
   /// its data pointer, which GPUI never reads or frees.
   static MOUSE_VTBL: RcFnVtbl = RcFnVtbl {
      drop_in_place: noop_drop as *const c_void,
      size:          0,
      align:         1,
      call_once:     mouse_press as *const c_void,
      call_mut:      mouse_press as *const c_void,
      call:          mouse_press as *const c_void,
      extra:         ptr::null(),
   };

   const unsafe extern "C" fn noop_drop(_data: *mut u8) {}

   /// Rust ABI like the `Fn::call` slot it fills, see `modal::scrim_press`.
   unsafe fn mouse_press(
      data: *const u8,
      _event: *mut u8,
      phase: u8,
      hitbox: *const c_void,
      window: *const c_void,
      _app: *mut u8,
   ) {
      let _ = panic::catch_unwind(|| {
         if phase == modal::PHASE_BUBBLE && modal::hovered(hitbox, window) {
            let _ = worker().send(data as u64);
         }
      });
   }

   unsafe extern "C" fn detour(
      a0: usize,
      a1: usize,
      a2: usize,
      a3: usize,
      a4: usize,
   ) -> RawAnyElement {
      let orig = *ORIG.get().expect("the detour only runs once ORIG is set");
      // The emoji button's wrapper is named while the bar renders.
      IN_BAR.set(true);
      // SAFETY: forwards every argument to the render.
      let mut out = unsafe { orig(a0, a1, a2, a3, a4) };
      IN_BAR.set(false);
      let _ = panic::catch_unwind(AssertUnwindSafe(|| decorate(&raw mut out)));
      out
   }

   /// Puts the plugin buttons in a row ahead of the element `sret` holds, on
   /// desktops without the `Div::id` hook.
   fn decorate(sret: *mut RawAnyElement) {
      // Where the buttons join the emoji button, the bar is left as is.
      if ORIG_ID.get().is_some() {
         return;
      }
      let buttons = BUTTONS.lock().unwrap_or_else(PoisonError::into_inner);
      if buttons.is_empty() {
         return;
      }
      let Some(mut row) = h_row(4.0, false) else {
         return;
      };
      for button in buttons.iter() {
         let variant = if button.tint.is_some() {
            BtnVariant::Primary
         } else {
            BtnVariant::Secondary
         };
         let id = format!("sunbird-composer-{}", button.id);
         if let Some(built) = build_raw_button(
            &id,
            &button.label,
            variant,
            BtnSize::Small,
            &raw const PRESS_VTBL,
            button.id,
         ) {
            row = push_raw_button(row, built);
         }
      }
      drop(buttons);
      let Some(mut bar) = raw_div() else {
         return;
      };
      // The bar fills its parent, so it shrinks to the width the buttons leave.
      style_div(&mut row, |style| style.length(style.at.size[0], W_FULL));
      style_div(&mut bar, |style| {
         style.length(style.at.size[0], W_FULL);
         style.length(style.at.min_size[0], gpui_px(0.0));
         style.float(style.at.flex_shrink, 1.0);
      });
      // SAFETY: moves the rendered element out of `sret`, which gets it back or
      // the row holding it.
      let mut orig = unsafe { ptr::read(sret) };
      if !push_any(&mut bar, &mut orig) {
         // SAFETY: puts back the element read above, which nothing took.
         unsafe { ptr::write(sret, orig) };
         return;
      }
      write_any(push_div(row, bar), sret);
   }

   static PRESS_VTBL: RcFnVtbl = RcFnVtbl {
      drop_in_place: ptr::null(),
      size:          size_of::<u64>(),
      align:         align_of::<u64>(),
      call_once:     press as *const c_void,
      call_mut:      press as *const c_void,
      call:          press as *const c_void,
      extra:         ptr::null(),
   };

   unsafe extern "C" fn press(
      id: *const u64,
      _event: *const c_void,
      _window: *mut c_void,
      _app: *mut c_void,
   ) {
      if id.is_null() {
         return;
      }
      // SAFETY: GPUI passes a pointer to the `u64` payload of the `Rc` that
      // `build_raw_button` allocated.
      let id = unsafe { *id };
      let _ = worker().send(id);
   }

   fn worker() -> &'static mpsc::Sender<u64> {
      static WORKER: OnceLock<mpsc::Sender<u64>> = OnceLock::new();
      WORKER.get_or_init(|| {
         let (tx, rx) = mpsc::channel::<u64>();
         let _ = std::thread::Builder::new()
            .name("sunbird-composer".to_owned())
            .spawn(move || {
               while let Ok(id) = rx.recv() {
                  if let Some(engine) = phoenix_runtime::engine() {
                     js::ui::press_composer_button(&engine, id);
                  }
               }
            });
         tx
      })
   }
}
