//! Phoenix's pending friend and message request counts, which
//! `phoenix.ui.hideCount` hides while the requests themselves stay listed.
//!
//! The friend requests count is answered with 0 where Phoenix computes it, for
//! the DM list's Friends row and the inbox's Friends tab, and taken out of the
//! Home button's count and the badge built like it. The Friends page's Pending
//! tab reads it from a tally of every relationship type, where it moves into a
//! count that is only ever added to it, so the tab itself stays. The message
//! requests count also decides whether the Message Requests row shows, so only
//! the DM list's badge for it draws 0.

use std::{
   str::FromStr,
   sync::{
      OnceLock,
      atomic::{
         AtomicBool,
         Ordering,
      },
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::{
   Found,
   locate::counts::{
      self,
      FRIENDS_BADGE,
      MESSAGE_REQUESTS_BADGE,
      TALLIES,
   },
};

/// The DM list's row badge, `fn(out, id: &str, count: u32, _, _) -> out`.
type BadgeFn = unsafe extern "C" fn(*mut u8, *const u8, usize, u32, usize, usize) -> *mut u8;
/// `u32 f(&RelationshipStore)`.
type CountFn = unsafe extern "C" fn(*const u8) -> u32;
/// `fn(out: &mut [u64; 8], &RelationshipStore) -> out`.
type TallyFn = unsafe extern "C" fn(*mut u64, *const u8) -> *mut u64;

/// The tally's incoming friend requests, which badge the Pending tab.
const TALLY_INCOMING: usize = 1;
/// The tally's count after them, which its readers only add to the others.
const TALLY_SUMMED: usize = 2;

/// The Home badge's state when it shows a count.
const STATE_COUNT: u32 = 2;
/// The Home badge's state when it shows nothing.
const STATE_NONE: u32 = 0;

static HIDE_FRIEND_REQUESTS: AtomicBool = AtomicBool::new(false);
static HIDE_MESSAGE_REQUESTS: AtomicBool = AtomicBool::new(false);
/// Linux builds draw each DM badge through its own copy of the badge.
const BADGES: usize = 2;
static BADGE: [OnceLock<BadgeFn>; BADGES] = [const { OnceLock::new() }; BADGES];
const BADGE_DETOURS: [BadgeFn; BADGES] = [badge::<0>, badge::<1>];
static FRIEND_REQUESTS: OnceLock<CountFn> = OnceLock::new();
static HOME_COUNT: OnceLock<CountFn> = OnceLock::new();
static TALLY: [OnceLock<TallyFn>; TALLIES] = [const { OnceLock::new() }; TALLIES];
const TALLY_DETOURS: [TallyFn; TALLIES] = [tally::<0>, tally::<1>];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Count {
   FriendRequests,
   MessageRequests,
}

impl FromStr for Count {
   type Err = ();

   fn from_str(name: &str) -> Result<Self, ()> {
      match name {
         "friendRequests" => Ok(Self::FriendRequests),
         "messageRequests" => Ok(Self::MessageRequests),
         _ => Err(()),
      }
   }
}

/// How a hooked function hands back what it returns.
#[derive(Clone, Copy)]
enum Returns {
   /// In registers.
   Registers,
   /// Through a slot the caller passes, `x8` on arm64.
   Slot,
}

/// Hooks what draws and computes the counts from [`crate::prepare`], before
/// Discord's threads run.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   let found = match crate::located::get("counts::pending_counts", || counts::pending_counts(ix)) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("pending counts: {err}, the counts stay");
         return;
      },
   };
   // They read the friend requests count's code, so they look before it is
   // hooked.
   let home = crate::located::get("counts::home_count", || {
      counts::home_count(ix, found.friend_requests)
   });
   let home_state = crate::located::get("counts::home_badge", || {
      counts::home_badge(ix, found.friend_requests)
   });
   let tallies = crate::located::get("counts::relationship_counts", || {
      counts::relationship_counts(ix)
   });

   for (index, &func) in found.badges.iter().take(BADGES).enumerate() {
      // SAFETY: runs before Discord's threads, and the detour has the badge's
      // ABI.
      if let Some(original) = unsafe {
         attach::<BadgeFn>(
            image,
            Ok(func),
            BADGE_DETOURS[index] as usize,
            Returns::Slot,
         )
      } {
         let _ = BADGE[index].set(original);
      }
   }
   // SAFETY: as above, for the count's ABI.
   if let Some(original) = unsafe {
      attach::<CountFn>(
         image,
         Ok(found.friend_requests),
         friend_requests as *const () as usize,
         Returns::Registers,
      )
   } {
      let _ = FRIEND_REQUESTS.set(original);
   }
   // SAFETY: as above, for the count's ABI.
   if let Some(original) = unsafe {
      attach::<CountFn>(
         image,
         home,
         home_count as *const () as usize,
         Returns::Registers,
      )
   } {
      let _ = HOME_COUNT.set(original);
   }
   // SAFETY: as above, for the Home badge's ABI.
   if let Some(original) = unsafe {
      attach::<home_badge::Original>(
         image,
         home_state,
         home_badge::detour_addr(),
         Returns::Registers,
      )
   } {
      home_badge::set(original);
   }
   let tallies = match tallies {
      Ok(tallies) => tallies,
      Err(err) => {
         log::warn!("pending counts: {err}");
         return;
      },
   };
   for (index, &func) in tallies.iter().enumerate() {
      // SAFETY: as above, for the tally's ABI.
      if let Some(original) = unsafe {
         attach::<TallyFn>(
            image,
            Ok(func),
            TALLY_DETOURS[index] as usize,
            Returns::Slot,
         )
      } {
         let _ = TALLY[index].set(original);
      }
   }
}

/// Sends `func` to `detour` and returns what runs the original as `F`.
///
/// # Safety
///
/// As for [`crate::hook`] and [`crate::hook_sret`], by `returns`.
unsafe fn attach<F: Copy>(
   image: &Image<'static>,
   func: Found<usize>,
   detour: usize,
   returns: Returns,
) -> Option<F> {
   let func = func
      .map_err(|err| log::warn!("pending counts: {err}"))
      .ok()?;
   let original = match returns {
      // SAFETY: forwarded from the caller.
      Returns::Registers => unsafe { crate::hook::<F>(image, func, detour, "pending counts") },
      // SAFETY: forwarded from the caller.
      Returns::Slot => unsafe { crate::hook_sret::<F>(image, func, detour, "pending counts") },
   }?;
   log::info!("pending counts: hooked @ {func:#x}");
   Some(original)
}

/// Hides each count in `hidden` and shows the others again.
pub fn hide(hidden: impl IntoIterator<Item = Count>) {
   let (mut friends, mut requests) = (false, false);
   for count in hidden {
      match count {
         Count::FriendRequests => friends = true,
         Count::MessageRequests => requests = true,
      }
   }
   let friends_changed = HIDE_FRIEND_REQUESTS.swap(friends, Ordering::AcqRel) != friends;
   let requests_changed = HIDE_MESSAGE_REQUESTS.swap(requests, Ordering::AcqRel) != requests;
   if friends_changed || requests_changed {
      crate::accessories::invalidate_discord_windows();
   }
}

extern "C" fn badge<const N: usize>(
   out: *mut u8,
   id: *const u8,
   id_len: usize,
   count: u32,
   cx: usize,
   window: usize,
) -> *mut u8 {
   // The jump is written just before `BADGE` is set, all before Discord runs.
   let original = *BADGE[N].wait();
   // SAFETY: Phoenix passes the id as a live `&str`.
   let id_bytes = unsafe { std::slice::from_raw_parts(id, id_len) };
   let hidden = id_bytes == FRIENDS_BADGE && HIDE_FRIEND_REQUESTS.load(Ordering::Acquire)
      || id_bytes == MESSAGE_REQUESTS_BADGE && HIDE_MESSAGE_REQUESTS.load(Ordering::Acquire);
   let count = if hidden { 0 } else { count };
   // SAFETY: the trampoline runs the original with Phoenix's arguments.
   unsafe { original(out, id, id_len, count, cx, window) }
}

extern "C" fn friend_requests(store: *const u8) -> u32 {
   if HIDE_FRIEND_REQUESTS.load(Ordering::Acquire) {
      return 0;
   }
   let original = *FRIEND_REQUESTS.wait();
   // SAFETY: the trampoline runs the original with Phoenix's argument.
   unsafe { original(store) }
}

extern "C" fn home_count(store: *const u8) -> u32 {
   // SAFETY: the trampoline runs the original with Phoenix's argument.
   let count = unsafe { (*HOME_COUNT.wait())(store) };
   without_friend_requests(store, count)
}

extern "C" fn tally<const N: usize>(out: *mut u64, store: *const u8) -> *mut u64 {
   // SAFETY: the trampoline runs the original with Phoenix's arguments.
   let returned = unsafe { (*TALLY[N].wait())(out, store) };
   if HIDE_FRIEND_REQUESTS.load(Ordering::Acquire) {
      // SAFETY: the original filled the counts `out` points to.
      unsafe {
         *out.add(TALLY_SUMMED) += *out.add(TALLY_INCOMING);
         *out.add(TALLY_INCOMING) = 0;
      }
   }
   returned
}

/// `count` less the friend requests while they are hidden.
fn without_friend_requests(store: *const u8, count: u32) -> u32 {
   match FRIEND_REQUESTS.get() {
      Some(requests) if HIDE_FRIEND_REQUESTS.load(Ordering::Acquire) => {
         // SAFETY: Phoenix passed the store the count came from.
         count.saturating_sub(unsafe { requests(store) })
      },
      _ => count,
   }
}

/// Takes the friend requests out of a Home badge's `(state, count)`.
fn badge_without_friend_requests(store: *const u8, state: u32, count: u32) -> (u32, u32) {
   if state != STATE_COUNT {
      return (state, count);
   }
   let count = without_friend_requests(store, count);
   // Phoenix looks for unread DMs only when it counts nothing, so a dot they
   // would show is lost when only friend requests were counted.
   (if count == 0 { STATE_NONE } else { STATE_COUNT }, count)
}

/// The Home badge, `(state, count) f(&Store, _)`, which returns its pair in
/// the first two return registers.
#[cfg(not(windows))]
mod home_badge {
   use std::sync::OnceLock;

   /// What the badge returns in its two return registers, each holding a
   /// `u32`. System V and arm64 return a 16-byte pair of integers the same way.
   #[repr(C)]
   pub struct Pair {
      state: u64,
      count: u64,
   }

   pub type Original = unsafe extern "C" fn(*const u8, usize) -> Pair;

   static ORIGINAL: OnceLock<Original> = OnceLock::new();

   pub fn detour_addr() -> usize {
      detour as *const () as usize
   }

   pub fn set(original: Original) {
      let _ = ORIGINAL.set(original);
   }

   #[expect(
      clippy::cast_possible_truncation,
      reason = "the badge returns each u32 in a 64-bit register"
   )]
   extern "C" fn detour(store: *const u8, arg: usize) -> Pair {
      // SAFETY: the trampoline runs the original with Phoenix's arguments.
      let pair = unsafe { (*ORIGINAL.wait())(store, arg) };
      let (state, count) =
         super::badge_without_friend_requests(store, pair.state as u32, pair.count as u32);
      Pair {
         state: u64::from(state),
         count: u64::from(count),
      }
   }
}

/// The Home badge on Win64, which returns `(state, count)` in `eax` and `edx`.
/// Win64 returns a 16-byte pair through a slot, so a shim takes `edx`.
#[cfg(windows)]
mod home_badge {
   use std::sync::atomic::{
      AtomicUsize,
      Ordering,
   };

   pub type Original = usize;

   /// The badge's trampoline, which [`shim`] calls.
   static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

   pub fn detour_addr() -> usize {
      shim as *const () as usize
   }

   pub fn set(original: Original) {
      ORIGINAL.store(original, Ordering::Release);
   }

   /// Answers the badge as `state | count << 32` for [`shim`].
   extern "C" fn adjust(store: *const u8, state: u32, count: u32) -> u64 {
      let (state, count) = super::badge_without_friend_requests(store, state, count);
      u64::from(state) | u64::from(count) << 32
   }

   /// Runs the badge and hands the `(state, count)` it returns in `eax` and
   /// `edx`, which no stable Rust ABI returns, through [`adjust`].
   #[unsafe(naked)]
   extern "C" fn shim() {
      core::arch::naked_asm!(
         "push rbx",
         "sub rsp, 32",
         "mov rbx, rcx",
         "call qword ptr [rip + {original}]",
         "mov rcx, rbx",
         "mov r8d, edx",
         "mov edx, eax",
         "call {adjust}",
         "mov rdx, rax",
         "shr rdx, 32",
         "add rsp, 32",
         "pop rbx",
         "ret",
         original = sym ORIGINAL,
         adjust = sym adjust,
      );
   }
}
