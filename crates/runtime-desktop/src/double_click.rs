//! Double clicks on message rows, for `phoenix.ui.onEvent("rowDoubleTap")`.
//! A handler that calls `ev.press(action)` turns the click into the right
//! click that opens the row's menu, and the menu presses that item for it.

use std::{
   ffi::c_void,
   ptr,
   sync::{
      Mutex,
      PoisonError,
      atomic::{
         AtomicBool,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
   },
};

use crate::{
   gpui::{
      self,
      RawAnyElement,
      RcFnVtbl,
   },
   modal,
};

/// `gpui::MouseDownEvent`, measured on build 2233: `click_count` and the
/// `MouseButton` byte, where `Left` is 2 and `Right` 3 past the two
/// `Navigate` niches.
const CLICK_COUNT: usize = 0x8;
const BUTTON: usize = 0x10;
const LEFT: u8 = 2;
const RIGHT: u8 = 3;
const PRESS_WINDOW: Duration = Duration::from_secs(2);

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The message whose menu a double click pressed, and when: Discord builds a
/// menu again on each frame it draws, so it stays empty for a moment after.
static PRESSED: Mutex<Option<(u64, Instant)>> = Mutex::new(None);
const HIDE_FOR: Duration = Duration::from_secs(1);
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

struct Pending {
   message: u64,
   item:    String,
   at:      Instant,
   /// The window and app GPUI handed the click, which the menu item's
   /// handler gets.
   window:  usize,
   app:     usize,
}

/// A menu item to press, with the window and app to press it with.
pub struct Press {
   pub item:   String,
   pub window: *mut c_void,
   pub app:    *mut c_void,
}

static ROW_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     row_press as *const c_void,
   call_mut:      row_press as *const c_void,
   call:          row_press as *const c_void,
   extra:         ptr::null(),
};

const unsafe extern "C" fn noop_drop(_data: *mut u8) {}

/// Whether a plugin listens for double clicks.
pub fn set_active(active: bool) {
   ACTIVE.store(active, Ordering::Release);
}

/// Wraps the row `MessageRow::render` returned in `sret` in a div that
/// listens for presses on message `message`. The listener is zero-sized, so
/// the id rides in its data pointer, which GPUI never reads or frees.
pub fn wrap_row(sret: *mut RawAnyElement, message: u64) {
   if !ACTIVE.load(Ordering::Acquire) || message == 0 {
      return;
   }
   let Some(mut div) = gpui::v_stack(0.0) else {
      return;
   };
   // SAFETY: `sret` holds the rendered row, and gets it back or the wrapper.
   let mut row = unsafe { ptr::read(sret) };
   if !gpui::push_any(&mut div, &mut row) {
      // SAFETY: the row was not moved into the div.
      unsafe { ptr::write(sret, row) };
      return;
   }
   modal::listen_mouse_down(&mut div, message as *mut u8, &ROW_PRESS_VTBL);
   gpui::write_any(div, sret);
}

/// The menu item to press on `message`'s menu, when a double click asked for
/// one just now.
///
/// The builder's own arguments are no window, which crashed Discord when the
/// handler wrote to one, so the press carries the click's.
pub fn take_press(message: u64) -> Option<Press> {
   let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
   if pending
      .as_ref()
      .is_none_or(|held| held.message != message || held.at.elapsed() > PRESS_WINDOW)
   {
      return None;
   }
   pending.take().map(|held| {
      Press {
         item:   held.item,
         window: held.window as *mut c_void,
         app:    held.app as *mut c_void,
      }
   })
}

/// Notes that a double click pressed `message`'s menu.
pub fn pressed(message: u64) {
   *PRESSED.lock().unwrap_or_else(PoisonError::into_inner) = Some((message, Instant::now()));
}

/// Whether `message`'s menu was pressed just now, so it should draw nothing.
pub fn hidden(message: u64) -> bool {
   PRESSED
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .is_some_and(|(held, at)| held == message && at.elapsed() < HIDE_FOR)
}

/// Whether a press waits for a menu.
pub fn pending() -> bool {
   PENDING
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .is_some()
}

/// The chat pane's Reply and Edit, run straight from a double click with the
/// pane the row was drawn in, as the menu's items run them.
pub mod pane {
   use std::{
      collections::HashMap,
      ffi::c_void,
      sync::{
         LazyLock,
         Mutex,
         OnceLock,
         PoisonError,
      },
   };

   use phoenix_sigs::locate::chat::{
      self,
      PaneCommands,
   };

   /// `WeakEntity<ChatPaneView>`: its id, ref counts and type.
   type Handle = [u8; HANDLE];
   const HANDLE: usize = 32;
   const TYPE_AT: usize = 16;
   /// How much of the list's frame to search for the pane's handle.
   const FRAME: usize = 0x1000;
   const MAX_ROWS: usize = 4096;

   type CommandFn =
      unsafe extern "C" fn(*const Handle, *const *mut u8, *const u64, *const c_void) -> usize;

   static COMMANDS: OnceLock<PaneCommands> = OnceLock::new();
   /// The pane each drawn message's row belongs to.
   static PANES: LazyLock<Mutex<HashMap<u64, Handle>>> =
      LazyLock::new(|| Mutex::new(HashMap::new()));

   pub fn install(ix: &crate::located::LazyIndex<'_>) {
      match crate::located::get("chat::pane_commands", || chat::pane_commands(ix)) {
         Ok(found) => {
            let _ = COMMANDS.set(found);
         },
         Err(err) => log::warn!("double click: {err}, replies and edits open the menu"),
      }
   }

   /// Notes the pane drawing `message`'s row, from `frames`: arguments the row
   /// render is handed that point into its list's stack frame, which holds the
   /// pane's handle (0xe0 past the third argument on Windows, 0xb70 on Linux).
   /// Only a handle of the pane's own type counts, so a candidate that is no
   /// pointer at all finds nothing.
   pub fn note(message: u64, frames: &[usize]) {
      let Some(commands) = COMMANDS.get() else {
         return;
      };
      if message == 0 {
         return;
      }
      let mut panes = PANES.lock().unwrap_or_else(PoisonError::into_inner);
      if panes.contains_key(&message) {
         return;
      }
      let found = |frame: usize| -> Option<Handle> {
         // A checked read, since how far the frame reaches is not known.
         let bytes = phoenix_hook::x86::read_memory(frame, FRAME)?;
         let at = bytes
            .windows(16)
            .position(|window| window == commands.pane_type)?
            .checked_sub(TYPE_AT)?;
         Handle::try_from(bytes.get(at..at + HANDLE)?).ok()
      };
      let Some(handle) = frames
         .iter()
         .filter(|&&frame| frame != 0)
         .find_map(|&frame| found(frame))
      else {
         return;
      };
      if panes.len() >= MAX_ROWS {
         panes.clear();
      }
      panes.insert(message, handle);
   }
   /// Runs `item` (`reply` or `edit`) on `message` in its row's pane, with the
   /// click's window and app. Whether it ran.
   pub fn run(item: &str, message: u64, window: *const c_void, app: *mut u8) -> bool {
      let Some(commands) = COMMANDS.get() else {
         return false;
      };
      let handler = match item {
         "reply" => commands.reply,
         "edit" => commands.edit,
         _ => return false,
      };
      let Some(handle) = PANES
         .lock()
         .unwrap_or_else(PoisonError::into_inner)
         .get(&message)
         .copied()
      else {
         return false;
      };
      // SAFETY: the handler the menu item calls, `(pane, &app, &message,
      // window)`. It upgrades the pane handle and does nothing once the pane
      // is gone; `handle` is a copy of the row's, which needs no count of its
      // own while the call holds its upgrade.
      unsafe {
         let handler = std::mem::transmute::<usize, CommandFn>(handler);
         handler(
            &raw const handle,
            &raw const app,
            &raw const message,
            window,
         );
      }
      true
   }
}

/// Rust ABI like the `Fn::call` slot it fills, see `modal::scrim_press`. The
/// capture phase reaches this wrapper before the row's own bubble listener,
/// which opens the menu on a right press.
unsafe fn row_press(
   data: *const u8,
   event: *mut u8,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase != modal::PHASE_CAPTURE || !modal::hovered(hitbox, window) {
         return;
      }
      // SAFETY: GPUI passes the live `MouseDownEvent`.
      let (count, button) = unsafe {
         (
            event.add(CLICK_COUNT).cast::<u64>().read_unaligned(),
            event.add(BUTTON).read(),
         )
      };
      if count != 2 || button != LEFT {
         return;
      }
      let message = data as u64;
      let Some(item) =
         phoenix_runtime::engine().and_then(|engine| crate::js::ui::double_tap(&engine, message))
      else {
         return;
      };
      // Reply is a store command; edit mode only lives in the chat pane.
      if item == "reply" && crate::client_commands::reply(message) {
         return;
      }
      if pane::run(&item, message, window, app) {
         return;
      }
      *PENDING.lock().unwrap_or_else(PoisonError::into_inner) = Some(Pending {
         message,
         item,
         at: Instant::now(),
         window: window as usize,
         app: app as usize,
      });
      // SAFETY: the event GPUI hands every listener in turn, on this stack.
      unsafe { event.add(BUTTON).write(RIGHT) };
   });
}
