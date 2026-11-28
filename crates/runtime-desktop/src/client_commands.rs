//! libdiscord's named commands ([`phoenix_sigs::locate::commands`]): a
//! `{"type": name, ...args}` JSON request in, a JSON reply out, through the
//! client's own command runner.
//!
//! Only commands that finish on their first poll run: the rest await
//! Discord's async runtime, which is not ours to drive, so they come back as
//! [`Error::Pending`] instead.

use std::{
   ffi::c_void,
   sync::{
      Mutex,
      OnceLock,
      PoisonError,
      atomic::{
         AtomicUsize,
         Ordering,
      },
   },
   task::{
      Context,
      Waker,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::{
   index::Index,
   locate::commands,
};

/// `Pin<Box<dyn Future<Output = Vec<u8>>>>`.
#[repr(C)]
struct RawFuture {
   ptr:    *mut u8,
   vtable: *const FutureVtable,
}

#[repr(C)]
struct FutureVtable {
   drop:  Option<unsafe fn(*mut u8)>,
   size:  usize,
   align: usize,
   /// Rust ABI, `Poll<Vec<u8>>` through the return slot.
   poll:  unsafe fn(*mut u8, *mut Context<'_>) -> RawPoll,
}

/// `Poll<Vec<u8>>`: the reply's `{cap, ptr, len}`, `cap` all ones while
/// pending.
#[repr(C)]
struct RawPoll {
   cap: usize,
   ptr: *mut u8,
   len: usize,
}

const PENDING: usize = usize::MAX;

/// Rust ABI, the box and its vtable returned in two registers.
type RunnerFn = unsafe fn(*const AtomicUsize, *const u8, usize) -> RawFuture;
type GetFn = unsafe extern "C" fn(*mut u8, *const usize, u64) -> *mut u8;
/// `Client::new`'s future poll, Rust ABI.
type ClientPollFn = unsafe fn(*mut u8, *mut c_void) -> ClientPoll;

/// `Poll<Result<Client, E>>` in two registers: the variant, then the
/// client's `Arc` or the error.
#[repr(C)]
#[derive(Clone, Copy)]
struct ClientPoll {
   variant: usize,
   value:   usize,
}

const CLIENT_OK: usize = 0;

static RUNNER: OnceLock<RunnerFn> = OnceLock::new();
static ORIG_GET: OnceLock<GetFn> = OnceLock::new();
static ORIG_CLIENT_POLL: OnceLock<ClientPollFn> = OnceLock::new();
/// The chat bar's client, the account on screen, once it has drawn.
static ON_SCREEN: AtomicUsize = AtomicUsize::new(0);
/// Every client seen, each holding one strong count. Every signed-in account
/// has its own.
static KEPT: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Desktop's main account and its client, matched by asking each.
static MAIN: Mutex<Option<(String, usize)>> = Mutex::new(None);

#[derive(Debug)]
pub enum Error {
   /// The runner is not located, or no client has been seen yet.
   Unavailable,
   /// The command awaits Discord's runtime.
   Pending,
   /// The reply is not UTF-8.
   Reply,
}

pub fn install(image: &Image<'static>, ix: &Index<'_>) {
   let found = match commands::commands(ix) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("commands: {err}, plugins fall back to the UI");
         return;
      },
   };
   // SAFETY: the located runner, whose Rust-ABI signature `RunnerFn` spells.
   let _ = RUNNER.set(unsafe { std::mem::transmute::<usize, RunnerFn>(found.runner) });
   log::info!("commands: runner @ {:#x}", found.runner);
   match found.pending_get {
      // SAFETY: runs from `prepare` before Discord's threads, and
      // `get_detour` has the getter's signature with the return slot first.
      Some(getter) => {
         if let Some(orig) = unsafe {
            crate::hook_sret::<GetFn>(
               image,
               getter,
               get_detour as *const () as usize,
               "commands: pending reply hook failed",
            )
         } {
            let _ = ORIG_GET.set(orig);
         }
      },
      None => {
         log::warn!("commands: no pending reply getter, the newest account's client runs them");
      },
   }
   match commands::client_new(ix) {
      // SAFETY: runs from `prepare` before Discord builds a client, and
      // `client_poll_detour` has the poll's Rust-ABI signature.
      Ok(poll) => {
         if let Some(orig) = unsafe {
            crate::hook::<ClientPollFn>(
               image,
               poll,
               client_poll_detour as *const () as usize,
               "commands: Client::new hook failed",
            )
         } {
            let _ = ORIG_CLIENT_POLL.set(orig);
         }
      },
      Err(err) => log::warn!("commands: {err}, the client comes from the chat bar"),
   }
}

/// Keeps each client `Client::new` builds, so commands run from startup.
unsafe fn client_poll_detour(future: *mut u8, cx: *mut c_void) -> ClientPoll {
   let orig = *ORIG_CLIENT_POLL
      .get()
      .expect("the detour only runs once ORIG_CLIENT_POLL is set");
   // SAFETY: forwards the poll's arguments.
   let polled = unsafe { orig(future, cx) };
   if polled.variant == CLIENT_OK && polled.value != 0 {
      keep(polled.value);
      log::info!("commands: a client is ready");
   }
   polled
}

/// Keeps `arc` alive with one strong count, the first time it is seen.
fn keep(arc: usize) {
   let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
   if !kept.contains(&arc) {
      // ponytail: each client keeps one strong count for the app's life, one
      // per account login, so it never adds up.
      // SAFETY: `arc` is a live client `Arc`, whose strong count is its first
      // word.
      unsafe { (*(arc as *const AtomicUsize)).fetch_add(1, Ordering::Relaxed) };
      kept.push(arc);
   }
}

/// The client of the account in front: the chat bar's once it has drawn,
/// else the one signed in as Desktop's main account, else the only or newest.
fn client() -> Option<usize> {
   let on_screen = ON_SCREEN.load(Ordering::Acquire);
   if on_screen != 0 {
      return Some(on_screen);
   }
   let kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner).clone();
   if let [only] = kept[..] {
      return Some(only);
   }
   main_client(&kept).or_else(|| kept.last().copied())
}

/// The client in `kept` signed in as Desktop's main account, cached until the
/// main account changes.
fn main_client(kept: &[usize]) -> Option<usize> {
   let main = phoenix_runtime::store::main_account(&phoenix_runtime::state()?.files_dir())?;
   let cached = MAIN
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .as_ref()
      .filter(|(id, _)| *id == main)
      .map(|&(_, arc)| arc);
   if cached.is_some() {
      return cached;
   }
   let arc = kept
      .iter()
      .copied()
      .find(|&arc| user_of(arc).as_deref() == Some(main.as_str()))?;
   *MAIN.lock().unwrap_or_else(PoisonError::into_inner) = Some((main, arc));
   Some(arc)
}

/// The user `arc` is signed in as, from its own `get_state`.
fn user_of(arc: usize) -> Option<String> {
   #[derive(serde::Deserialize)]
   struct Reply {
      ok: State,
   }
   #[derive(serde::Deserialize)]
   struct State {
      user: User,
   }
   #[derive(serde::Deserialize)]
   struct User {
      id: String,
   }
   let reply = run_on(arc, r#"{"type":"get_state"}"#).ok()?;
   serde_json::from_str::<Reply>(&reply).ok().map(|reply| reply.ok.user.id)
}

unsafe extern "C" fn get_detour(out: *mut u8, client: *const usize, channel: u64) -> *mut u8 {
   if !client.is_null() {
      // SAFETY: `client` is the live `&Client` the composer passes.
      let arc = unsafe { client.read() };
      if arc != 0 && arc != ON_SCREEN.load(Ordering::Relaxed) {
         keep(arc);
         ON_SCREEN.store(arc, Ordering::Release);
         log::info!("commands: the account on screen's client took over");
      }
   }
   let orig = *ORIG_GET
      .get()
      .expect("the detour only runs once ORIG_GET is set");
   // SAFETY: forwards the getter's arguments.
   unsafe { orig(out, client, channel) }
}

/// Runs `request` (`{"type": name, ...args}`) as the account in front and
/// returns the reply's JSON. Never logs either, since both can hold account
/// data.
pub fn run(request: &str) -> Result<String, Error> {
   run_on(client().ok_or(Error::Unavailable)?, request)
}

fn run_on(arc: usize, request: &str) -> Result<String, Error> {
   let runner = *RUNNER.get().ok_or(Error::Unavailable)?;
   // `&Client`, a pointer to the `Arc`.
   let client = AtomicUsize::new(arc);
   let mut cx = Context::from_waker(Waker::noop());
   // SAFETY: the runner copies the request out of the borrowed bytes and
   // takes `client` by reference. Its future is polled once and dropped and
   // freed below before returning, so nothing keeps the waker or either
   // reference.
   let polled = unsafe {
      let future = runner(&raw const client, request.as_ptr(), request.len());
      if future.ptr.is_null() || future.vtable.is_null() {
         return Err(Error::Unavailable);
      }
      let vtable = &*future.vtable;
      let polled = (vtable.poll)(future.ptr, &raw mut cx);
      if let Some(drop) = vtable.drop {
         drop(future.ptr);
      }
      free(future.ptr, vtable.size, vtable.align);
      polled
   };
   if polled.cap == PENDING {
      return Err(Error::Pending);
   }
   // SAFETY: a ready `Vec<u8>` of `len` initialized bytes, freed once copied.
   let reply = unsafe {
      let bytes = if polled.ptr.is_null() || polled.len == 0 {
         Vec::new()
      } else {
         std::slice::from_raw_parts(polled.ptr, polled.len).to_vec()
      };
      free(polled.ptr, polled.cap, 1);
      bytes
   };
   String::from_utf8(reply).map_err(|_| Error::Reply)
}

/// Frees `size` bytes Discord allocated at `ptr`, or leaks them where its
/// deallocator is not located.
///
/// # Safety
///
/// `ptr` is Discord's allocation of `size` bytes at `align`, or `size` is 0.
unsafe fn free(ptr: *mut u8, size: usize, align: usize) {
   if ptr.is_null() || size == 0 {
      return;
   }
   if let (_, Some(dealloc)) = crate::discord::host_allocator() {
      // SAFETY: the caller's allocation, from Discord's own allocator.
      unsafe { dealloc(ptr, size, align) };
   }
}

/// Puts a pending reply to `message` in its channel's chat bar, as the menu's
/// Reply does. Whether it did.
pub fn reply(message: u64) -> bool {
   let Some((channel, author, own)) = crate::js::ui::message_origin(message) else {
      return false;
   };
   let args = serde_json::json!({
      "channel_id": channel.to_string(),
      "reply": {
         "message_id": message.to_string(),
         "author_id": author.to_string(),
         "should_mention": !own,
         "show_mention_toggle": !own,
         "swiped": false,
      },
   });
   phoenix_runtime::discord::command("set_pending_reply", &args.to_string())
      .inspect_err(|err| log::warn!("commands: {err}"))
      .is_ok()
}
