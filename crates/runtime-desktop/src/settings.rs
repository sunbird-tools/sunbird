//! Desktop Phoenix's User Settings: Discord's own **Phoenix** page shown, and a
//! native **Sunbird** entry under the sidebar that opens Sunbird's GPUI page.
//!
//! All patches are build-independent anchor lookups run from [`crate::prepare`]
//! before Discord's threads:
//! 1. `is_page_visible`'s range guard (`pages 9..=24`) is lowered to `9..=23`,
//!    so page 24 (`Other -> Phoenix`) falls through to `return true` and shows
//!    with Discord's own label and content.
//! 2. The sidebar's entry push gets a native **Sunbird** row, built by Log
//!    Out's own constructor and pushed right before Log Out (so under Phoenix),
//!    and `host::section` (the one loader of `account-loading`) draws
//!    [`gpui::render_sunbird_page`] instead of the active page while Sunbird is
//!    open.
//! 3. Discord's page-changed handler, which the page setters a sidebar row runs
//!    call after storing the page (even when it stays the same), closes
//!    Sunbird, so any sidebar row leaves it.
//!
//! The active page byte is never written: no page id is free (every value in
//! `0..30` is a real page, and over a thousand sites read the byte). While
//! Sunbird is open, the sidebar's selected-page map answers no page during the
//! sidebar's render instead, so the last page's row deselects.
//!
//! A build where the sidebar's rows or the handler don't decode falls back to
//! taking page 24 over, relabeled `Sunbird` in place.

use std::{
   ffi::c_void,
   ptr,
   sync::{
      OnceLock,
      atomic::{
         AtomicBool,
         AtomicU16,
         AtomicUsize,
         Ordering,
      },
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::{
   Found,
   index::Isa,
   locate::settings as locate,
};

use crate::gpui::{
   self,
   RawAnyElement,
   RcFnVtbl,
   SharedString,
};

/// The last flag-gated settings page, `Other -> Phoenix` (`24`, from `sub #9;
/// cmp #0xf`), which the fallback takes over.
const PHOENIX_PAGE: u8 = 24;

/// The largest sidebar entry and click closure Sunbird's row is built in.
const MAX_ENTRY: usize = 0x200;
const MAX_CLOSURE: usize = 0x40;

/// `host::section` and the sidebar render share this shape.
type RenderFn = unsafe extern "C" fn(
   sret: *mut RawAnyElement,
   host_state: *mut c_void,
   host_data: *mut c_void,
   host_vtable: *const c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement;

/// `fn(&mut HostState, &mut Context)`. Its result, if any, passes through.
type PageChangedFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize;

/// The sidebar's selected-page map, `fn(page) -> page`.
type SelectedFn = unsafe extern "C" fn(u8) -> u8;

/// The page-name function, `fn(page) -> Text`, see
/// [`locate::SettingsTitle::name`].
type NameFn = unsafe extern "C" fn(sret: *mut c_void, page: u8) -> *mut c_void;

/// Discord's translate, see [`locate::SettingsTitle::translate`].
type TranslateFn = unsafe extern "C" fn(
   sret: *mut c_void,
   key: *const u8,
   key_len: usize,
   hash: u64,
   default: *const Untranslated,
) -> *mut c_void;

/// The English text translate falls back to when no translation has the
/// key's hash, laid out as the page-name function builds it.
#[repr(C)]
struct Untranslated {
   text: *const u8,
   len:  usize,
   one:  usize,
   zero: usize,
}

/// The settings header's render on macOS, see
/// [`locate::SettingsTitle::header`].
type HeaderFn = unsafe extern "C" fn(
   sret: *mut c_void,
   state: *mut c_void,
   arg1: usize,
   arg2: usize,
) -> *mut c_void;

/// What names the active page in the pane's title while Sunbird is open.
struct Title {
   /// The page-name function.
   name:      NameFn,
   translate: TranslateFn,
}

/// Past every page byte.
const UNTITLED: u16 = 0x100;

/// Past every page id, which the sidebar only compares with its rows' pages.
const NO_PAGE: u8 = u8::MAX;

/// The sidebar's `bl` to the selected-page map where the map is too short to
/// hook and is called directly (macOS), swapped for `mov w0, #NO_PAGE` while
/// Sunbird is open.
struct SelectedCall {
   at:        usize,
   call:      u32,
   rewritten: AtomicBool,
}

impl SelectedCall {
   const UNSELECTED: u32 = 0x5280_0000 | ((NO_PAGE as u32) << 5);

   /// Runs on the GPUI thread, the only one that runs the sidebar render.
   fn show(&self, open: bool) {
      if self.rewritten.swap(open, Ordering::AcqRel) == open {
         return;
      }
      let Some(state) = phoenix_runtime::state() else {
         return;
      };
      let word = if open { Self::UNSELECTED } else { self.call };
      // SAFETY: one aligned instruction in the sidebar render, written on the
      // thread that runs it while that thread is outside the render.
      if let Err(err) = unsafe { state.image.write_code(self.at, &word.to_le_bytes()) } {
         log::warn!("desktop settings: selected-page call rewrite failed: {err}");
      }
   }
}

/// The sidebar's `fn(&mut Vec<Entry>, &mut Entry)`.
type PushFn = unsafe extern "C" fn(*mut c_void, *mut u8);

/// Log Out's row constructor, see [`locate::SidebarRows::row`].
type RowFn = unsafe extern "C" fn(
   out: *mut u8,
   id: *const u8,
   id_len: usize,
   label: *const SharedString,
   icon: u16,
   selected: bool,
   danger: bool,
   closure: *const u8,
) -> *mut u8;

/// What Sunbird's row is built from.
struct Rows {
   row:          RowFn,
   click_vtable: usize,
   click_offset: usize,
   icon:         u16,
   /// Sunbird's click with the layout of Log Out's closure, so Discord frees
   /// the box Log Out's constructor makes.
   click:        RcFnVtbl,
}

/// How Sunbird reaches its page on this build.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
   /// Its own sidebar entry, with page 24 left to Discord.
   Entry,
   /// Page 24 relabeled and drawn by Sunbird.
   TakeOver,
}

static MODE: OnceLock<Mode> = OnceLock::new();
/// Whether the Sunbird entry was clicked since the last page change.
static OPEN: AtomicBool = AtomicBool::new(false);
static ORIG_SECTION: OnceLock<RenderFn> = OnceLock::new();
static ORIG_PUSH: OnceLock<PushFn> = OnceLock::new();
static ORIG_SIDEBAR: OnceLock<RenderFn> = OnceLock::new();
static ORIG_SELECTED: OnceLock<SelectedFn> = OnceLock::new();
static TITLE: OnceLock<Title> = OnceLock::new();
static ORIG_HEADER: OnceLock<HeaderFn> = OnceLock::new();
/// The page the header is naming with Sunbird open, [`UNTITLED`] otherwise.
static TITLE_PAGE: AtomicU16 = AtomicU16::new(UNTITLED);
/// Set while the sidebar renders with Sunbird open.
static UNSELECT: AtomicBool = AtomicBool::new(false);
static SELECTED_CALL: OnceLock<SelectedCall> = OnceLock::new();
static ROWS: OnceLock<Rows> = OnceLock::new();
static ORIG_PAGE_CHANGED: OnceLock<PageChangedFn> = OnceLock::new();
static PAGE_OFFSET: AtomicUsize = AtomicUsize::new(0);

/// Whether [`install`] hooked the section render that draws Sunbird's page.
pub fn hooked() -> bool {
   ORIG_SECTION.get().is_some()
}

/// Shows page 24, hooks `host::section`, and adds the Sunbird entry, or takes
/// page 24 over where the entry can't be added.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   patch_page_visibility(image, ix);
   if !gpui::locate(image, ix) {
      return;
   }
   let Some(page_offset) = hook_section(
      image,
      crate::located::get("locate::settings_section", || locate::settings_section(ix)),
   ) else {
      return;
   };
   if install_entry(image, ix, page_offset) {
      let _ = MODE.set(Mode::Entry);
      log::info!("desktop settings: Sunbird has its own entry, page {PHOENIX_PAGE} stays Phoenix");
   } else {
      relabel(image, ix);
   }
}

/// The value, or logs why this build has none.
fn found<T>(result: Found<T>) -> Option<T> {
   result
      .map_err(|err| log::warn!("desktop settings: {err}"))
      .ok()
}

/// Falls back to page 24 as Sunbird: its `Phoenix` name and the `PHOENIX` that
/// ends its id replaced in place.
fn relabel(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   const NAME: &[u8] = b"Sunbird";
   const SUFFIX: &[u8] = b"SUNBIRD";
   const _: () = assert!(NAME.len() == b"Phoenix".len());
   const _: () = assert!(SUFFIX.len() == b"PHOENIX".len());

   let Some(label) = found(crate::located::get("locate::settings_label", || {
      locate::settings_label(ix)
   })) else {
      return;
   };
   // SAFETY: exact-length string patches from `prepare`, before Discord's
   // threads run. The id ends in the suffix, so it becomes `DESKTOP_SUNBIRD` or
   // `SUNBIRD`.
   let written = unsafe {
      image
         .write_code(label.name, NAME)
         .and_then(|()| image.write_code(label.suffix, SUFFIX))
   };
   match written {
      Ok(()) => {
         let _ = MODE.set(Mode::TakeOver);
         log::info!("desktop settings: page {PHOENIX_PAGE} is Sunbird");
      },
      Err(err) => log::warn!("desktop settings: relabel failed: {err}"),
   }
}

fn patch_page_visibility(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   let Some(guards) = found(crate::located::get("locate::settings_visibility", || {
      locate::settings_visibility(ix)
   })) else {
      return;
   };
   // Tightening `cmp r8d, 0xf` -> `0x0e` excludes page 24 (`24 - 9 = 15`) from
   // the gated switch so `is_page_visible(24)` returns `true` unconditionally.
   for guard in guards {
      let written = if let Isa::X86_64 { .. } = Isa::HOST {
         // SAFETY: single-byte immediate patch in `.text` executed from
         // `DllMain`.
         unsafe { image.write_code(guard.imm8(), &[0x0E]) }
      } else {
         let Some(cmp) = image.read_u32(guard.cmp) else {
            log::warn!(
               "desktop settings: visibility guard {:#x} unmapped",
               guard.cmp
            );
            continue;
         };
         let lowered = phoenix_sigs::macos::lowered_guard(cmp).to_le_bytes();
         // SAFETY: one-instruction patch from `prepare`, before Discord's
         // threads run.
         unsafe { image.write_code(guard.cmp, &lowered) }
      };
      match written {
         Ok(()) => log::info!("desktop settings: page 24 visible @ {:#x}", guard.cmp),
         Err(err) => log::warn!("desktop settings: visibility patch failed: {err}"),
      }
   }
}

/// Hooks `host::section` and returns the active page's offset in its state.
fn hook_section(image: &Image<'static>, section: Found<locate::Section>) -> Option<usize> {
   let section = found(section)?;
   // SAFETY: runs from `prepare` before Discord's threads; `section_detour`
   // matches `host::section`'s 6-argument sret ABI.
   let orig = unsafe {
      crate::hook_sret::<RenderFn>(
         image,
         section.func,
         section_detour as *const () as usize,
         "desktop settings: section hook failed",
      )
   }?;
   let _ = ORIG_SECTION.set(orig);
   PAGE_OFFSET.store(section.page_offset, Ordering::Release);
   log::info!(
      "desktop settings: hooked section render @ {:#x} (page offset {:#x})",
      section.func,
      section.page_offset
   );
   Some(section.page_offset)
}

/// Hooks the page-changed handler, then the sidebar's entry push. The
/// handler's hook only closes Sunbird, so it is harmless if the push's then
/// fails.
fn install_entry(
   image: &Image<'static>,
   ix: &crate::located::LazyIndex<'_>,
   page_offset: usize,
) -> bool {
   let (Some(rows), Some(changed)) = (
      found(crate::located::get("locate::settings_sidebar_rows", || {
         locate::settings_sidebar_rows(ix, page_offset)
      })),
      found(crate::located::get("locate::settings_page_changed", || {
         locate::settings_page_changed(ix, page_offset)
      })),
   ) else {
      return false;
   };
   if rows.entry_size > MAX_ENTRY {
      log::warn!(
         "desktop settings: sidebar entries are {:#x} bytes",
         rows.entry_size
      );
      return false;
   }
   let (Some(size), Some(align)) = (
      image.read_usize(rows.click_vtable + 8),
      image.read_usize(rows.click_vtable + 16),
   ) else {
      log::warn!("desktop settings: Log Out's click unmapped");
      return false;
   };
   if size > MAX_CLOSURE {
      log::warn!("desktop settings: Log Out's click captures {size:#x} bytes");
      return false;
   }
   let (Some(_), Some(_), Some(row)) = (
      found(ix.hookable(rows.push)),
      found(ix.hookable(changed)),
      found(crate::modal::sret_wrap(rows.row).map_err(str::to_owned)),
   ) else {
      return false;
   };
   let _ = ROWS.set(Rows {
      // SAFETY: the constructor the locator decoded, called with its out
      // pointer first as `RowFn` has it.
      row:          unsafe { std::mem::transmute::<usize, RowFn>(row) },
      click_vtable: rows.click_vtable,
      click_offset: rows.click_offset,
      icon:         rows.gear,
      click:        RcFnVtbl {
         drop_in_place: ptr::null(),
         size,
         align,
         call_once: entry_clicked as *const c_void,
         call_mut: entry_clicked as *const c_void,
         call: entry_clicked as *const c_void,
         extra: ptr::null(),
      },
   });
   // SAFETY: runs from `prepare` before Discord's threads; the detour has the
   // handler's `fn(state, cx)` ABI.
   let Some(orig) = (unsafe {
      crate::hook::<PageChangedFn>(
         image,
         changed,
         page_changed_detour as *const () as usize,
         "desktop settings: page-changed hook failed",
      )
   }) else {
      return false;
   };
   let _ = ORIG_PAGE_CHANGED.set(orig);
   // SAFETY: as above; the detour has the push's `fn(list, entry)` ABI.
   let Some(orig) = (unsafe {
      crate::hook::<PushFn>(
         image,
         rows.push,
         push_detour as *const () as usize,
         "desktop settings: sidebar push hook failed",
      )
   }) else {
      return false;
   };
   let _ = ORIG_PUSH.set(orig);
   log::info!(
      "desktop settings: hooked sidebar push @ {:#x}, page changes @ {changed:#x}",
      rows.push
   );
   if !hook_unselect(image, ix, &rows) {
      log::warn!("desktop settings: the active page stays selected beside Sunbird");
   } else if !hook_title(image, ix, page_offset) {
      log::warn!("desktop settings: the active page's title stays beside Sunbird");
   }
   true
}

/// Has the pane's title name Sunbird while it is open: x86 sends the header's
/// one call of the page-name function through [`title_call_detour`], arm64,
/// with no call-site hooks, marks the header's render for [`name_detour`].
fn hook_title(
   image: &Image<'static>,
   ix: &crate::located::LazyIndex<'_>,
   page_offset: usize,
) -> bool {
   let Some(title) = found(crate::located::get("locate::settings_title", || {
      locate::settings_title(ix, page_offset)
   })) else {
      return false;
   };
   let Some(translate) = found(crate::modal::sret_wrap(title.translate).map_err(str::to_owned))
   else {
      return false;
   };
   // SAFETY: the translate the locator decoded, called with its return slot
   // first as `TranslateFn` has it.
   let translate = unsafe { std::mem::transmute::<usize, TranslateFn>(translate) };
   let name = if let Isa::X86_64 { .. } = Isa::HOST {
      // SAFETY: runs from `prepare` before Discord's threads; the detour has
      // the page-name function's ABI.
      unsafe { image.hook_call(title.call, &[], title_call_detour as *const () as usize) }
         .map_err(|err| log::warn!("desktop settings: title call hook failed: {err}"))
         .ok()
         // SAFETY: the call's target is the page-name function.
         .map(|name| unsafe { std::mem::transmute::<usize, NameFn>(name) })
   } else {
      hook_header(image, ix, &title)
   };
   name.is_some_and(|name| TITLE.set(Title { name, translate }).is_ok())
}

/// Hooks the page-name function and the header's render, which marks the page
/// it names while Sunbird is open.
fn hook_header(
   image: &Image<'static>,
   ix: &crate::located::LazyIndex<'_>,
   title: &locate::SettingsTitle,
) -> Option<NameFn> {
   found(ix.hookable(title.name))?;
   found(ix.hookable(title.header))?;
   // SAFETY: runs from `prepare` before Discord's threads; the detour has the
   // page-name function's sret ABI.
   let name = unsafe {
      crate::hook_sret::<NameFn>(
         image,
         title.name,
         name_detour as *const () as usize,
         "desktop settings: page-name hook failed",
      )
   }?;
   // SAFETY: as above; the detour has the header render's sret ABI.
   let header = unsafe {
      crate::hook_sret::<HeaderFn>(
         image,
         title.header,
         header_detour as *const () as usize,
         "desktop settings: header hook failed",
      )
   }?;
   let _ = ORIG_HEADER.set(header);
   Some(name)
}

/// Sunbird's name, as Discord's own page names are built.
fn sunbird_title(sret: *mut c_void, title: &Title) -> *mut c_void {
   const KEY: &str = "SUNBIRD";
   const NAME: &str = "Sunbird";
   /// No translation has it, so translate falls back to [`NAME`].
   const HASH: u64 = 0x5355_4E42_4952_4400;

   let default = Untranslated {
      text: NAME.as_ptr(),
      len:  NAME.len(),
      one:  1,
      zero: 0,
   };
   // SAFETY: `sret` is the caller's slot for a page's name, which translate
   // fills with the same type; the key and default are static.
   unsafe { (title.translate)(sret, KEY.as_ptr(), KEY.len(), HASH, &raw const default) }
}

/// The header's call of the page-name function for its title.
unsafe extern "C" fn title_call_detour(sret: *mut c_void, page: u8) -> *mut c_void {
   let Some(title) = TITLE.get() else {
      return sret;
   };
   if OPEN.load(Ordering::Acquire) {
      return sunbird_title(sret, title);
   }
   // SAFETY: forwards the arguments to the page-name function.
   unsafe { (title.name)(sret, page) }
}

/// Sunbird for the page the header names, its parent left alone.
unsafe extern "C" fn name_detour(sret: *mut c_void, page: u8) -> *mut c_void {
   let Some(title) = TITLE.get() else {
      return sret;
   };
   if u16::from(page) == TITLE_PAGE.load(Ordering::Acquire) {
      return sunbird_title(sret, title);
   }
   // SAFETY: forwards the arguments to the original function.
   unsafe { (title.name)(sret, page) }
}

unsafe extern "C" fn header_detour(
   sret: *mut c_void,
   state: *mut c_void,
   arg1: usize,
   arg2: usize,
) -> *mut c_void {
   let Some(orig) = ORIG_HEADER.get().copied() else {
      return sret;
   };
   if OPEN.load(Ordering::Acquire) && !state.is_null() {
      // SAFETY: `state` is Discord's live settings state, which holds the
      // active page byte at the offset `host::section` reads it at.
      let page = unsafe { *state.cast::<u8>().add(PAGE_OFFSET.load(Ordering::Acquire)) };
      TITLE_PAGE.store(u16::from(page), Ordering::Release);
   }
   // SAFETY: forwards all arguments verbatim to the original render.
   let out = unsafe { orig(sret, state, arg1, arg2) };
   TITLE_PAGE.store(UNTITLED, Ordering::Release);
   out
}

/// Hooks the selected-page map, then the sidebar render that has it select no
/// page row while Sunbird is open. The map's hook alone changes nothing.
fn hook_unselect(
   image: &Image<'static>,
   ix: &crate::located::LazyIndex<'_>,
   rows: &locate::SidebarRows,
) -> bool {
   let Some(_) = found(ix.hookable(rows.sidebar)) else {
      return false;
   };
   if !hook_selected(image, ix, rows) {
      return false;
   }
   // SAFETY: runs from `prepare` before Discord's threads; the sidebar render
   // has `host::section`'s ABI.
   let Some(orig) = (unsafe {
      crate::hook_sret::<RenderFn>(
         image,
         rows.sidebar,
         sidebar_detour as *const () as usize,
         "desktop settings: sidebar hook failed",
      )
   }) else {
      return false;
   };
   let _ = ORIG_SIDEBAR.set(orig);
   true
}

/// Hooks the selected-page map at its entry, or where it is too short, swaps
/// the pointer the sidebar calls it through (Linux) or readies the sidebar's
/// call of it for [`SelectedCall::show`] (macOS).
fn hook_selected(
   image: &Image<'static>,
   ix: &crate::located::LazyIndex<'_>,
   rows: &locate::SidebarRows,
) -> bool {
   const BL: (u32, u32) = (0xFC00_0000, 0x9400_0000);

   let detour = selected_detour as *const () as usize;
   let orig = match (ix.hookable(rows.selected), rows.selected_via, Isa::HOST) {
      (Ok(_), ..) => {
         // SAFETY: runs from `prepare` before Discord's threads; the detour has
         // the map's `fn(u8) -> u8` ABI.
         unsafe {
            crate::hook::<SelectedFn>(
               image,
               rows.selected,
               detour,
               "desktop settings: selected-page hook failed",
            )
         }
      },
      (Err(_), Some(slot), _) => {
         // SAFETY: an aligned pointer slot in the image, swapped from `prepare`
         // before Discord's threads; the old value is the map, the detour has
         // its ABI.
         unsafe { image.swap_words(&[(slot, detour)]) }
            .map_err(|err| log::warn!("desktop settings: selected-page swap failed: {err}"))
            .ok()
            .and_then(|old| old.first().copied())
            // SAFETY: the slot held the map's address.
            .map(|old| unsafe { std::mem::transmute::<usize, SelectedFn>(old) })
      },
      (Err(_), None, Isa::Aarch64) => {
         let Some(call) = image
            .read_u32(rows.selected_call)
            .filter(|&word| word & BL.0 == BL.1)
         else {
            log::warn!(
               "desktop settings: {:#x} is no call of the selected-page map",
               rows.selected_call
            );
            return false;
         };
         let _ = SELECTED_CALL.set(SelectedCall {
            at: rows.selected_call,
            call,
            rewritten: AtomicBool::new(false),
         });
         return true;
      },
      (Err(err), ..) => {
         log::warn!("desktop settings: {err}");
         None
      },
   };
   orig.is_some_and(|orig| ORIG_SELECTED.set(orig).is_ok())
}

/// The sidebar render, marked for [`selected_detour`] while Sunbird is open.
unsafe extern "C" fn sidebar_detour(
   sret: *mut RawAnyElement,
   host_state: *mut c_void,
   host_data: *mut c_void,
   host_vtable: *const c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement {
   let Some(orig) = ORIG_SIDEBAR.get().copied() else {
      return sret;
   };
   let open = OPEN.load(Ordering::Acquire);
   if let Some(call) = SELECTED_CALL.get() {
      call.show(open);
   }
   UNSELECT.store(open, Ordering::Release);
   // SAFETY: forwards all arguments verbatim to the original render.
   let out = unsafe { orig(sret, host_state, host_data, host_vtable, window, cx) };
   UNSELECT.store(false, Ordering::Release);
   out
}

/// No page while the sidebar renders with Sunbird open, so only Sunbird's
/// row is selected. Its other caller indexes with the page, so it always gets
/// the real one.
unsafe extern "C" fn selected_detour(page: u8) -> u8 {
   if UNSELECT.load(Ordering::Acquire) {
      return NO_PAGE;
   }
   let Some(orig) = ORIG_SELECTED.get().copied() else {
      return page;
   };
   // SAFETY: forwards the page to the original map.
   unsafe { orig(page) }
}

unsafe extern "C" fn page_changed_detour(state: *mut c_void, cx: *mut c_void) -> usize {
   OPEN.store(false, Ordering::Release);
   let Some(orig) = ORIG_PAGE_CHANGED.get().copied() else {
      return 0;
   };
   // SAFETY: forwards both arguments verbatim to the original handler.
   unsafe { orig(state, cx) }
}

/// Pushes Sunbird's row right before Log Out's, so it sits under the last
/// page, Phoenix.
unsafe extern "C" fn push_detour(list: *mut c_void, entry: *mut u8) {
   let Some(orig) = ORIG_PUSH.get().copied() else {
      return;
   };
   if let Some(rows) = ROWS.get()
      // SAFETY: every entry is the push's size, which holds the click slot.
      && unsafe { entry.add(rows.click_offset).cast::<usize>().read_unaligned() }
         == rows.click_vtable
   {
      let _ = std::panic::catch_unwind(|| push_sunbird(orig, list, rows));
   }
   // SAFETY: forwards both arguments verbatim to the original push.
   unsafe { orig(list, entry) };
}

/// Builds Sunbird's row with Log Out's constructor, then swaps in its click.
fn push_sunbird(push: PushFn, list: *mut c_void, rows: &Rows) {
   const ID: &str = "sunbird-settings";
   let mut entry = [0u64; MAX_ENTRY / 8];
   let closure = [0u64; MAX_CLOSURE / 8];
   let label = SharedString::from_static("Sunbird");
   let open = OPEN.load(Ordering::Acquire);
   // SAFETY: the constructor fills the entry (at most `MAX_ENTRY` bytes) and
   // moves in the static label and the closure's `size` zero bytes, boxed with
   // Log Out's layout. Sunbird's vtable keeps that layout and drops nothing,
   // so Discord frees the box as usual, and the push moves the entry in.
   unsafe {
      let entry = entry.as_mut_ptr().cast::<u8>();
      (rows.row)(
         entry,
         ID.as_ptr(),
         ID.len(),
         &raw const label,
         rows.icon,
         open,
         false,
         closure.as_ptr().cast(),
      );
      entry
         .add(rows.click_offset)
         .cast::<*const RcFnVtbl>()
         .write_unaligned(&raw const rows.click);
      push(list, entry);
   }
}

/// The Sunbird row's click, in the shape of [`gpui`]'s button callbacks.
unsafe extern "C" fn entry_clicked(
   _self: *const u64,
   _event: *const c_void,
   _window: *mut c_void,
   _app: *mut c_void,
) {
   OPEN.store(true, Ordering::Release);
   crate::modal::force_redraw();
}

unsafe extern "C" fn section_detour(
   sret: *mut RawAnyElement,
   host_state: *mut c_void,
   host_data: *mut c_void,
   host_vtable: *const c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement {
   let app = if cx.is_null() {
      ptr::null_mut()
   } else {
      // SAFETY: `[cx]` is `*mut gpui::App` inside `Context<T>`.
      unsafe { *(cx.cast::<*mut c_void>()) }
   };
   if !app.is_null() {
      // SAFETY: `app` is a live `&mut gpui::App` on the UI thread.
      unsafe { crate::mana_theme::flush_pending(app) };
   }
   if !host_state.is_null() && !sret.is_null() {
      let offset = PAGE_OFFSET.load(Ordering::Acquire);
      // SAFETY: `host_state` is Discord's live `&mut HostState` for this render
      // call.
      let page = unsafe { *host_state.cast::<u8>().add(offset) };
      let show = match MODE.get().copied() {
         Some(Mode::Entry) => OPEN.load(Ordering::Acquire),
         Some(Mode::TakeOver) => page == PHOENIX_PAGE,
         None => false,
      };
      if show {
         let rendered = std::panic::catch_unwind(|| gpui::render_sunbird_page(sret, window, app))
            .unwrap_or(false);
         if rendered {
            return sret;
         }
      }
   }
   let Some(orig) = ORIG_SECTION.get().copied() else {
      return sret;
   };
   // SAFETY: forwards all arguments verbatim to the original `host::section`.
   unsafe { orig(sret, host_state, host_data, host_vtable, window, cx) }
}
