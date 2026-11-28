//! macOS desktop backend. Clipboard, links, notifications and the file picker
//! go through the system's own command-line tools, and the tray is an
//! `NSStatusItem` made through the Objective-C runtime on Discord's main
//! thread, so the crate links nothing beyond libSystem.

use std::{
   ffi::{
      CStr,
      CString,
      c_char,
      c_void,
   },
   io::Write as _,
   path::PathBuf,
   process::{
      Command,
      Stdio,
   },
   ptr,
   sync::{
      Once,
      atomic::{
         AtomicUsize,
         Ordering,
      },
      mpsc,
   },
   thread,
};

use crate::{
   DesktopHost,
   Error,
   TrayAction,
};

/// Spawns `command` and waits for it on a background thread, so it never
/// lingers as a zombie.
fn spawn_reaped(command: &mut Command) -> Result<(), Error> {
   let mut child = command
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .map_err(|err| {
         Error::Platform {
            message: format!("{}: {err}", command.get_program().display()),
         }
      })?;
   let _ = thread::Builder::new()
      .name("sunbird-reap".to_owned())
      .spawn(move || child.wait());
   Ok(())
}

/// Runs an `AppleScript` that reads its text from `argv`, never from the
/// script itself, so a quote in `title` or `body` can't change what runs.
/// Returns what it printed, `None` when it failed or was cancelled.
fn osascript(line: &str, title: &str, body: &str) -> Option<String> {
   let output = Command::new("/usr/bin/osascript")
      .args([
         "-e",
         "on run argv",
         "-e",
         line,
         "-e",
         "end run",
         "--",
         title,
         body,
      ])
      .stdin(Stdio::null())
      .stderr(Stdio::null())
      .output()
      .ok()?;
   output
      .status
      .success()
      .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

const AGENT_LABEL: &str = "tools.sunbird.launcher";

fn launch_agent() -> Option<PathBuf> {
   std::env::var_os("HOME")
      .map(|home| PathBuf::from(home).join(format!("Library/LaunchAgents/{AGENT_LABEL}.plist")))
}

/// `&`, `<` and `>` escaped for a plist string.
fn xml_text(text: &str) -> String {
   text
      .replace('&', "&amp;")
      .replace('<', "&lt;")
      .replace('>', "&gt;")
}

const fn platform(message: String) -> Error {
   Error::Platform { message }
}

pub struct Host;

impl DesktopHost for Host {
   fn alert(title: &str, body: &str) {
      let _ = osascript(
         "display alert (item 1 of argv) message (item 2 of argv) as warning",
         title,
         body,
      );
   }

   fn confirm(title: &str, body: &str) -> bool {
      osascript(
         "return button returned of (display alert (item 1 of argv) message (item 2 of argv) as \
          warning buttons {\"No\", \"Yes\"} default button \"Yes\")",
         title,
         body,
      )
      .is_some_and(|button| button == "Yes")
   }

   /// `$SUNBIRD`, which `sunbird-macos.sh` sets in the copy's
   /// `LSEnvironment`, else where it installs `Sunbird.app`.
   fn launcher() -> Option<PathBuf> {
      let home = std::env::var_os("HOME").map(PathBuf::from);
      std::env::var_os("SUNBIRD")
         .map(PathBuf::from)
         .into_iter()
         .chain([PathBuf::from("/Applications/Sunbird.app")])
         .chain(home.map(|home| home.join("Applications/Sunbird.app")))
         .find(|app| app.join("Contents/MacOS/sunbird").is_file())
   }

   fn set_autostart(enabled: bool) -> Result<(), Error> {
      let agent = launch_agent().ok_or_else(|| platform("HOME is unset".to_owned()))?;
      if !enabled {
         return match std::fs::remove_file(&agent) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
               Err(platform(format!("{}: {err}", agent.display())))
            },
            _ => Ok(()),
         };
      }
      let app =
         Self::launcher().ok_or_else(|| platform("Sunbird.app is not installed".to_owned()))?;
      let plist = format!(
         "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST \
          1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist \
          version=\"1.0\">\n<dict>\n   <key>Label</key><string>{AGENT_LABEL}</string>\n   \
          <key>ProgramArguments</key>\n   <array>\n      <string>/usr/bin/open</string>\n      \
          <string>-a</string>\n      <string>{}</string>\n   </array>\n   \
          <key>RunAtLoad</key><true/>\n</dict>\n</plist>\n",
         xml_text(&app.to_string_lossy())
      );
      if let Some(dir) = agent.parent() {
         std::fs::create_dir_all(dir)
            .map_err(|err| platform(format!("{}: {err}", dir.display())))?;
      }
      std::fs::write(&agent, plist).map_err(|err| platform(format!("{}: {err}", agent.display())))
   }

   fn copy(text: &str) -> Result<(), Error> {
      if copy_pasteboard(text) {
         return Ok(());
      }
      let fail = |message: String| Error::Platform { message };
      let mut child = Command::new("/usr/bin/pbcopy")
         .stdin(Stdio::piped())
         .stdout(Stdio::null())
         .stderr(Stdio::null())
         .spawn()
         .map_err(|err| fail(format!("pbcopy: {err}")))?;
      let written = child
         .stdin
         .take()
         .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
      if child.wait().is_ok_and(|status| status.success()) && written {
         Ok(())
      } else {
         Err(fail("pbcopy failed".to_owned()))
      }
   }

   fn open(target: &str) -> Result<(), Error> {
      spawn_reaped(Command::new("/usr/bin/open").arg("--").arg(target))
   }

   fn pick_js_path() -> Option<PathBuf> {
      let (tx, rx) = mpsc::channel();
      on_main(move || {
         // SAFETY: on the main thread inside its autorelease pool.
         let picked = unsafe { pick_js_open_panel() };
         let _ = tx.send(picked);
      });
      rx.recv().ok().flatten()
   }

   fn notify(title: &str, body: &str) {
      // The text travels as arguments, never inside the script, so a plugin's
      // quote can't change what runs.
      let mut script = Command::new("/usr/bin/osascript");
      script.args([
         "-e",
         "on run argv",
         "-e",
         "display notification (item 2 of argv) with title (item 1 of argv)",
         "-e",
         "end run",
         "--",
         title,
         body,
      ]);
      if let Err(err) = spawn_reaped(&mut script) {
         log::debug!("notify: {err}");
      }
   }

   fn start_tray() {
      static STARTED: Once = Once::new();
      STARTED.call_once(|| {
         on_main(start_status_item);
         on_main(start_shortcuts);
      });
   }
}

/// Re-renders every visible window, from any thread. A dirty layer alone only
/// presents GPUI's last frame, while a key status change makes GPUI refresh
/// the whole window.
pub fn redraw_windows() {
   on_main(|| {
      // SAFETY: on the main thread, where AppKit may be used; every object is
      // checked for nil before it is messaged.
      unsafe {
         let app = send(class(c"NSApplication"), sel(c"sharedApplication"));
         let windows = send(app, sel(c"windows"));
         if windows.is_null() {
            return;
         }
         let count = send_usize(windows, sel(c"count"));
         for at in 0..count {
            let window = send_index(windows, sel(c"objectAtIndex:"), at);
            if window.is_null() || !send_get_bool(window, sel(c"isVisible")) {
               continue;
            }
            let delegate = send(window, sel(c"delegate"));
            let key_status = sel(c"windowDidBecomeKey:");
            if !delegate.is_null()
               && send_responds(delegate, sel(c"respondsToSelector:"), key_status)
            {
               send_id_unit(delegate, key_status, ptr::null_mut());
            }
         }
      }
   });
}

fn copy_pasteboard(text: &str) -> bool {
   // SAFETY: only reads the calling thread's identity.
   if unsafe { pthread_main_np() } != 0 {
      // SAFETY: on the main thread, inside its run loop's autorelease pool.
      return unsafe { write_pasteboard(text) };
   }
   let (tx, rx) = mpsc::channel();
   let text = text.to_owned();
   on_main(move || {
      // SAFETY: on the main thread inside its autorelease pool.
      let _ = tx.send(unsafe { write_pasteboard(&text) });
   });
   rx.recv().unwrap_or(false)
}

/// # Safety
///
/// Must run on the main thread inside an autorelease pool.
unsafe fn write_pasteboard(text: &str) -> bool {
   // SAFETY: forwarded from the caller.
   unsafe {
      let cls = class(c"NSPasteboard");
      if cls.is_null() {
         return false;
      }
      let pb = send(cls, sel(c"generalPasteboard"));
      if pb.is_null() {
         return false;
      }
      let _ = send_tag(pb, sel(c"clearContents"));
      let value = ns_string(text);
      if value.is_null() {
         return false;
      }
      send_id2_bool(
         pb,
         sel(c"setString:forType:"),
         value,
         ns_string("public.utf8-plain-text"),
      )
   }
}

/// # Safety
///
/// Must run on the main thread inside an autorelease pool.
unsafe fn pick_js_open_panel() -> Option<PathBuf> {
   const MODAL_OK: isize = 1;
   // SAFETY: on the main thread; every AppKit object is checked for nil.
   unsafe {
      let cls = class(c"NSOpenPanel");
      if cls.is_null() {
         return None;
      }
      let panel = send(cls, sel(c"openPanel"));
      if panel.is_null() {
         return None;
      }
      send_id(
         panel,
         sel(c"setTitle:"),
         ns_string("Install Sunbird Plugin (.js)"),
      );
      send_bool(panel, sel(c"setCanChooseFiles:"), true);
      send_bool(panel, sel(c"setCanChooseDirectories:"), false);
      send_bool(panel, sel(c"setAllowsMultipleSelection:"), false);
      let types = send_id(class(c"NSArray"), sel(c"arrayWithObject:"), ns_string("js"));
      if !types.is_null() {
         send_id(panel, sel(c"setAllowedFileTypes:"), types);
      }
      let app = send(class(c"NSApplication"), sel(c"sharedApplication"));
      if !app.is_null() {
         send_bool(app, sel(c"activateIgnoringOtherApps:"), true);
      }
      if send_tag(panel, sel(c"runModal")) != MODAL_OK {
         return None;
      }
      let url = send(panel, sel(c"URL"));
      if url.is_null() {
         return None;
      }
      let path = send(url, sel(c"path"));
      if path.is_null() {
         return None;
      }
      let utf8 = send_cstr(path, sel(c"UTF8String"));
      if utf8.is_null() {
         return None;
      }
      let text = CStr::from_ptr(utf8).to_str().ok()?;
      (!text.is_empty()).then(|| PathBuf::from(text))
   }
}

type Id = *mut c_void;
type Sel = *const c_void;

#[link(name = "objc")]
unsafe extern "C" {
   fn objc_getClass(name: *const c_char) -> Id;
   fn sel_registerName(name: *const c_char) -> Sel;
   fn objc_msgSend();
   fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
   fn objc_registerClassPair(class: Id);
   fn class_addMethod(class: Id, name: Sel, imp: *const c_void, types: *const c_char) -> bool;
}

unsafe extern "C" {
   fn pthread_main_np() -> i32;
   static _dispatch_main_q: c_void;
   fn dispatch_async_f(
      queue: *const c_void,
      context: *mut c_void,
      work: extern "C" fn(*mut c_void),
   );
}

fn class(name: &CStr) -> Id {
   // SAFETY: a lookup by NUL-terminated name, nil when missing.
   unsafe { objc_getClass(name.as_ptr()) }
}

fn sel(name: &CStr) -> Sel {
   // SAFETY: registers or finds a selector by NUL-terminated name.
   unsafe { sel_registerName(name.as_ptr()) }
}

/// `objc_msgSend` cast to one method's exact signature, which arm64 needs
/// since it passes variadic arguments differently.
macro_rules! msg_send {
   ($name:ident ($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty) => {
      /// # Safety
      ///
      /// `object` is nil or responds to `selector` with this signature.
      #[allow(clippy::too_many_arguments, reason = "one per argument of the method")]
      unsafe fn $name(object: Id, selector: Sel $(, $arg: $ty)*) -> $ret {
         // SAFETY: the caller matches the method's signature.
         unsafe {
            let send: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
               std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            send(object, selector $(, $arg)*)
         }
      }
   };
}

msg_send!(send() -> Id);
msg_send!(send_id(arg: Id) -> Id);
msg_send!(send_id_unit(arg: Id) -> ());
msg_send!(send_id2_bool(a: Id, b: Id) -> bool);
msg_send!(send_bool(arg: bool) -> ());
msg_send!(send_get_bool() -> bool);
msg_send!(send_responds(selector: Sel) -> bool);
msg_send!(send_usize() -> usize);
msg_send!(send_u16() -> u16);
msg_send!(send_index(at: usize) -> Id);
msg_send!(send_isize(arg: isize) -> ());
msg_send!(send_f64(arg: f64) -> Id);
msg_send!(send_str(arg: *const c_char) -> Id);
msg_send!(send_cstr() -> *const c_char);
msg_send!(send_monitor(mask: u64, handler: *const Block) -> Id);
msg_send!(send_symbol(name: Id, description: Id) -> Id);
msg_send!(send_item(title: Id, action: Sel, key: Id) -> Id);
msg_send!(send_tag() -> isize);

pub mod input;

/// An autoreleased `NSString`, stripping interior NUL bytes so `AppKit` calls
/// that reject `nil` never throw `NSInvalidArgumentException`.
///
/// # Safety
///
/// On the main thread, inside its autorelease pool.
unsafe fn ns_string(text: &str) -> Id {
   let cstr = CString::new(text)
      .unwrap_or_else(|_| CString::new(text.replace('\0', "")).unwrap_or_default());
   // SAFETY: forwarded from the caller; `cstr` outlives the call, which copies
   // it.
   unsafe {
      send_str(
         class(c"NSString"),
         sel(c"stringWithUTF8String:"),
         cstr.as_ptr(),
      )
   }
}

/// Runs `work` on the main thread, where `AppKit` lives, through its dispatch
/// queue, which GPUI's `NSApplication` run loop drains.
fn on_main(work: impl FnOnce() + Send + 'static) {
   type Work = Box<dyn FnOnce() + Send>;
   extern "C" fn run(context: *mut c_void) {
      // SAFETY: `context` is the `Box<Work>` `on_main` leaked, read back
      // once.
      let work = unsafe { Box::from_raw(context.cast::<Work>()) };
      work();
   }
   let work: Box<Work> = Box::new(Box::new(work));
   // SAFETY: the main queue lives as long as the process, and `run` takes
   // back the box exactly once.
   unsafe {
      dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(work).cast(), run);
   }
}

/// The status item's menu calls this with the clicked item, whose tag is its
/// index in [`TrayAction::MENU`]. Actions may block, so they run elsewhere.
extern "C" fn tray_clicked(_this: Id, _selector: Sel, sender: Id) {
   // SAFETY: AppKit passes the `NSMenuItem` that was clicked.
   let tag = unsafe { send_tag(sender, sel(c"tag")) };
   let Some(action) = usize::try_from(tag)
      .ok()
      .and_then(|at| TrayAction::MENU.get(at).copied().flatten())
   else {
      return;
   };
   spawn("sunbird-tray", move || action.run());
}

/// Runs `work` on a thread of its own, since `AppKit`'s may not block.
fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
   let _ = thread::Builder::new().name(name.to_owned()).spawn(work);
}

/// One instance of a new `NSObject` subclass `name`, kept for the life of the
/// process, whose methods are `(selector, imp, types)` tuples.
///
/// # Safety
///
/// On the main thread, and each `imp` matches its Objective-C `types` encoding.
unsafe fn target_object(name: &CStr, methods: &[(&CStr, *const c_void, &CStr)]) -> Id {
   let mut target = class(name);
   if target.is_null() {
      // SAFETY: registers a new subclass with the caller's methods.
      unsafe {
         target = objc_allocateClassPair(class(c"NSObject"), name.as_ptr(), 0);
         if target.is_null() {
            return ptr::null_mut();
         }
         for &(selector, imp, types) in methods {
            class_addMethod(target, sel(selector), imp, types.as_ptr());
         }
         objc_registerClassPair(target);
      }
   }
   // SAFETY: a fresh instance of an `NSObject` subclass, owned forever.
   unsafe { send(send(target, sel(c"alloc")), sel(c"init")) }
}

fn start_status_item() {
   /// `NSVariableStatusItemLength`.
   const VARIABLE_LENGTH: f64 = -1.0;

   // SAFETY: on the main thread; every object is checked for nil before
   // anything depends on it, and the item, menu and target are kept for the
   // life of the process.
   unsafe {
      let bar = send(class(c"NSStatusBar"), sel(c"systemStatusBar"));
      let target = target_object(c"SunbirdTrayTarget", &[(
         c"sunbirdTray:",
         tray_clicked as *const c_void,
         c"v@:@",
      )]);
      if bar.is_null() || target.is_null() {
         log::info!("tray: AppKit is not available");
         return;
      }
      let item = send(
         send_f64(bar, sel(c"statusItemWithLength:"), VARIABLE_LENGTH),
         sel(c"retain"),
      );
      let button = send(item, sel(c"button"));
      let image = send_symbol(
         class(c"NSImage"),
         sel(c"imageWithSystemSymbolName:accessibilityDescription:"),
         ns_string("bird"),
         ns_string("Sunbird"),
      );
      if image.is_null() {
         send_id(button, sel(c"setTitle:"), ns_string("Sunbird"));
      } else {
         send_id(button, sel(c"setImage:"), image);
      }
      send_id(button, sel(c"setToolTip:"), ns_string("Sunbird"));

      let menu = send_id(
         send(class(c"NSMenu"), sel(c"alloc")),
         sel(c"initWithTitle:"),
         ns_string("Sunbird"),
      );
      for (at, entry) in TrayAction::MENU.iter().enumerate() {
         let menu_item = entry.map_or_else(
            || send(class(c"NSMenuItem"), sel(c"separatorItem")),
            |action| {
               let menu_item = send_item(
                  send(class(c"NSMenuItem"), sel(c"alloc")),
                  sel(c"initWithTitle:action:keyEquivalent:"),
                  ns_string(action.label()),
                  sel(c"sunbirdTray:"),
                  // An uppercase key adds Shift to the default Command, so
                  // the menu shows the monitor's Shift-Command-I.
                  ns_string(if matches!(action, TrayAction::OpenDevtools) {
                     "I"
                  } else {
                     ""
                  }),
               );
               send_id(menu_item, sel(c"setTarget:"), target);
               send_isize(menu_item, sel(c"setTag:"), at.cast_signed());
               menu_item
            },
         );
         send_id(menu, sel(c"addItem:"), menu_item);
      }
      send_id(item, sel(c"setMenu:"), menu);
   }
}

struct Shortcut {
   modifiers: usize,
   code:      u16,
   label:     &'static str,
   run:       fn(),
}

/// Sunbird's shortcuts while Discord is focused: Command-Shift-I opens its
/// `DevTools`, which Discord leaves free. Matched by physical key code so
/// non-Latin layouts (Russian, Greek, etc.) trigger the same key.
const KVK_ANSI_I: u16 = 0x22;
const SHORTCUTS: [Shortcut; 1] = [Shortcut {
   modifiers: COMMAND_SHIFT,
   code:      KVK_ANSI_I,
   label:     "i",
   run:       crate::open_devtools,
}];

const SHIFT: usize = 1 << 17;
const CONTROL: usize = 1 << 18;
const OPTION: usize = 1 << 19;
const COMMAND: usize = 1 << 20;
/// `NSEventModifierFlagShift | NSEventModifierFlagCommand`, and the mask of
/// Shift, Control, Option and Command (ignoring Caps Lock and Fn).
const COMMAND_SHIFT: usize = SHIFT | COMMAND;
const MODIFIERS: usize = SHIFT | CONTROL | OPTION | COMMAND;

/// An Objective-C block, laid out as the runtime reads one.
#[repr(C)]
struct Block {
   isa:        *const c_void,
   flags:      i32,
   reserved:   i32,
   invoke:     extern "C" fn(*const Self, Id) -> Id,
   descriptor: *const BlockDescriptor,
}

#[repr(C)]
struct BlockDescriptor {
   reserved: usize,
   size:     usize,
}

unsafe extern "C" {
   static _NSConcreteGlobalBlock: c_void;
}

/// `NSEvent *(^)(NSEvent *)`: runs a shortcut and swallows its key, or hands
/// every other key on.
extern "C" fn key_down(_block: *const Block, event: Id) -> Id {
   // SAFETY: AppKit passes the `NSEvent` being dispatched.
   let (code, flags) = unsafe {
      (
         send_u16(event, sel(c"keyCode")),
         send_usize(event, sel(c"modifierFlags")),
      )
   };
   let pressed = flags & MODIFIERS;
   let Some(shortcut) = SHORTCUTS
      .iter()
      .find(|shortcut| shortcut.modifiers == pressed && code == shortcut.code)
   else {
      // SAFETY: as above.
      let repeat = unsafe { send_get_bool(event, sel(c"isARepeat")) };
      return if take_plugin_key(code, pressed, repeat) {
         ptr::null_mut()
      } else {
         event
      };
   };
   log::info!("shortcuts: key {}", shortcut.label);
   // Opening spawns a browser, which must not block the main thread.
   spawn("sunbird-shortcut", shortcut.run);
   ptr::null_mut()
}

/// Whether a plugin's `phoenix.ui.addShortcut` takes the key `code` pressed
/// with `pressed`. Command stands in for Control, and Control itself is left
/// to Discord. Repeats of a taken key are kept from Discord without running
/// the plugin again.
fn take_plugin_key(code: u16, pressed: usize, repeat: bool) -> bool {
   /// `kVK_ANSI_*` key codes to their letter or digit, `?` for other keys.
   const ANSI: &[u8] = b"ASDFHGZXCV?BQWERYT123465?97?80?OU?IP?LJ?K????NM";
   static TAKEN: AtomicUsize = AtomicUsize::new(usize::MAX);

   let combo = usize::from(code) | pressed;
   if repeat {
      return TAKEN.load(Ordering::Relaxed) == combo;
   }
   let Some(&key) = ANSI
      .get(usize::from(code))
      .filter(|key| key.is_ascii_alphanumeric())
   else {
      return false;
   };
   let shortcut = crate::Shortcut {
      key,
      ctrl: pressed & COMMAND != 0,
      shift: pressed & SHIFT != 0,
      alt: pressed & OPTION != 0,
   };
   let taken = pressed & CONTROL == 0
      && (shortcut.ctrl || shortcut.alt)
      && crate::take_shortcut(shortcut);
   if taken {
      TAKEN.store(combo, Ordering::Relaxed);
   }
   taken
}

fn start_shortcuts() {
   /// `NSEventMaskKeyDown`.
   const KEY_DOWN: u64 = 1 << 10;
   /// `BLOCK_IS_GLOBAL`: the runtime never copies or frees it.
   const IS_GLOBAL: i32 = 1 << 28;
   static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
      reserved: 0,
      size:     size_of::<Block>(),
   };

   // Leaked, since the monitor keeps it for the life of the process.
   let block = Box::leak(Box::new(Block {
      isa:        &raw const _NSConcreteGlobalBlock,
      flags:      IS_GLOBAL,
      reserved:   0,
      invoke:     key_down,
      descriptor: &raw const DESCRIPTOR,
   }));
   // SAFETY: on the main thread; the block has the handler's signature and
   // outlives the monitor, which AppKit retains.
   let monitor = unsafe {
      send_monitor(
         class(c"NSEvent"),
         sel(c"addLocalMonitorForEventsMatchingMask:handler:"),
         KEY_DOWN,
         block,
      )
   };
   if monitor.is_null() {
      log::info!("shortcuts: AppKit refused the key monitor");
   } else {
      log::info!("shortcuts: Command-Shift-I opens DevTools");
   }
}
