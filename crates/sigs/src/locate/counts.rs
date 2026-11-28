//! What draws and computes Phoenix's pending friend and message request counts.

use crate::{
   Found,
   index::{
      Func,
      Index,
      Isa,
      Op,
      Reg,
      flow,
   },
   locate::entry_hookable,
   one,
};

/// The id the DM list gives its pending friend requests badge.
pub const FRIENDS_BADGE: &[u8] = b"dm-friends-badge";
/// The id the DM list gives its message requests badge.
pub const MESSAGE_REQUESTS_BADGE: &[u8] = b"dm-message-requests-badge";
/// The Friends page's Pending tab label.
const PENDING_TAB: &[u8] = b"FRIENDS_SECTION_PENDING";
/// How far past the load of its id the call drawing a badge is.
const BADGE_REACH: usize = 0x40;

/// What draws the DM list's pending counts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingCounts {
   /// The DM list's row badge, `fn(out, id: &str, count: u32, _, _) -> out`,
   /// which draws [`FRIENDS_BADGE`] and [`MESSAGE_REQUESTS_BADGE`] only. Linux
   /// builds give each id its own copy of it.
   pub badges:          Vec<usize>,
   /// `u32 f(&RelationshipStore)`, the incoming friend and game friend
   /// requests the DM list's Friends row and the inbox's Friends tab show.
   pub friend_requests: usize,
}

/// The DM list draws both badges through one builder, each call just after the
/// load of its id. The friends count comes from the call whose result the count
/// argument was copied from.
pub fn pending_counts(ix: &Index<'_>) -> Found<PendingCounts> {
   let (list, friends_call, badge) = badge_call(ix, FRIENDS_BADGE)?;
   let (_, _, requests_badge) = badge_call(ix, MESSAGE_REQUESTS_BADGE)?;
   let badges = if badge == requests_badge {
      vec![badge]
   } else if same_shape(ix, badge, requests_badge) {
      vec![badge, requests_badge]
   } else {
      return Err(format!(
         "the DM badges are drawn by {badge:#x} and {requests_badge:#x}"
      ));
   };
   let friend_requests = count_source(ix, &list, friends_call)?;
   if !counts_incoming(&ix.func(friend_requests)?) {
      return Err(format!(
         "friend requests count at {friend_requests:#x} compares no type with 3"
      ));
   }
   Ok(PendingCounts {
      badges,
      friend_requests,
   })
}

/// `u32 f(&RelationshipStore)`, the number on the Home button.
///
/// It adds other counts to the friend requests `friend_requests` counts,
/// inlined but for its one call, which counts the game friend requests. The
/// Home button is the other caller of that which counts incoming requests and
/// returns a plain count.
pub fn home_count(ix: &Index<'_>, friend_requests: usize) -> Found<usize> {
   game_requests(ix, friend_requests)?
      .callers()
      .filter(|func| {
         func.start != friend_requests && !returns_badge(ix, func) && counts_incoming(func)
      })
      .one("Home count")
      .map(|func| func.start)
}

/// `(state, count) f(&Store, _)`, a badge built from the same counts as
/// [`home_count`], returned in the first two return registers.
///
/// State 2 is a count, state 1 an unread dot. The badge is the caller of the
/// game friend requests count which returns a count beside its state.
pub fn home_badge(ix: &Index<'_>, friend_requests: usize) -> Found<usize> {
   game_requests(ix, friend_requests)?
      .callers()
      .filter(|func| func.start != friend_requests && returns_badge(ix, func))
      .one("Home badge")
      .map(|func| func.start)
}

/// How many tallies a build keeps. macOS keeps a second one with the first's
/// loop, which x86 builds inline into the Friends page.
pub const TALLIES: usize = 2;

/// Each `fn(out: &mut [u64; 8], &RelationshipStore) -> out` that tallies the
/// relationships by type.
///
/// The Friends page badges its Pending tab with the incoming requests in
/// `out[1]`, and shows the tab while `out[1..=4]` adds up to more than 0. A
/// tally is what the page, which loads the tab's label, reaches in at most two
/// calls that compares a relationship type with 3 and 4 and fills its return
/// slot. Tallies the entry hook can't take are left out.
pub fn relationship_counts(ix: &Index<'_>) -> Found<Vec<usize>> {
   let near = ix
      .literal(PENDING_TAB)
      .iter()
      .flat_map(Func::calls)
      .collect::<Vec<_>>();
   let mut tallies = near
      .iter()
      .filter_map(|&callee| ix.func(callee).ok())
      .flat_map(|func| func.calls())
      .chain(near.iter().copied())
      .filter(|&callee| {
         ix.func(callee)
            .is_ok_and(|func| sorts_relationships(&func) && fills_slot(ix, &func))
      })
      .collect::<Vec<_>>();
   tallies.sort_unstable();
   tallies.dedup();
   if tallies.len() > TALLIES {
      return Err(format!(
         "relationship counts matched {} times",
         tallies.len()
      ));
   }
   let (hookable, refused) = tallies
      .into_iter()
      .partition::<Vec<_>, _>(|&tally| entry_hookable(ix, tally).is_ok());
   if hookable.is_empty() {
      return Err(format!(
         "no tally can be hooked, {refused:x?} set x16 and x17 as they open"
      ));
   }
   Ok(hookable)
}

/// Whether `func` compares one register with 3 and with 4, the last two
/// relationship types.
fn sorts_relationships(func: &Func<'_>) -> bool {
   let steps = func.steps();
   let compared = |value| {
      steps.iter().fold(0_u32, |regs, step| {
         match step.op {
            Op::Cmp { reg, value: with } if with == value => regs | 1 << reg,
            _ => regs,
         }
      })
   };
   compared(3) & compared(4) != 0
}

/// Whether `func` writes all 8 words of the `[u64; 8]` it returns through a
/// slot.
fn fills_slot(ix: &Index<'_>, func: &Func<'_>) -> bool {
   let Some(slot) = ix.isa.slot() else {
      return false;
   };
   let mut words = 0_u8;
   let mut fill = |offset: isize| {
      if offset % 8 == 0
         && let Ok(word @ 0..8) = usize::try_from(offset / 8)
      {
         words |= 1 << word;
      }
   };
   flow::follow(ix.isa, &func.steps(), slot, |step, holds| {
      match step.op {
         Op::Store {
            base,
            offset,
            size: 8,
            ..
         } if holds(base) => fill(offset),
         Op::StorePair {
            base,
            offset,
            size: 8,
            ..
         } if holds(base) => {
            fill(offset);
            fill(offset + 8);
         },
         _ => {},
      }
   });
   words == u8::MAX
}

/// The function loading the UI id `id`, and the site and callee of the call
/// that draws its badge, the first after the load.
fn badge_call<'ix>(ix: &'ix Index<'ix>, id: &[u8]) -> Found<(Func<'ix>, usize, usize)> {
   let name = String::from_utf8_lossy(id);
   // The ids are packed back to back without a NUL, so `literal` misses them.
   let loads = ix
      .text(id)
      .into_iter()
      .flat_map(|at| ix.loads_of(at..at + 1))
      .map(|(site, _)| site)
      .collect::<Vec<_>>();
   let load = one(&format!("{name} load"), &loads)?;
   let list = ix
      .function(load)
      .ok_or_else(|| format!("the {name} load at {load:#x} is in no function"))?;
   let (site, badge) = list
      .calls_at()
      .into_iter()
      .find(|&(site, _)| site > load)
      .filter(|&(site, _)| site - load <= BADGE_REACH)
      .ok_or_else(|| format!("no call follows the {name} load at {load:#x}"))?;
   Ok((list, site, badge))
}

/// Follows the count argument of the call at `site` in `list` back through
/// register copies to the call that returned it.
fn count_source(ix: &Index<'_>, list: &Func<'_>, site: usize) -> Found<usize> {
   let mut reg = ix.isa.slot_arg(2).ok_or("no count argument register")?;
   let steps = list.steps();
   let before = steps.partition_point(|step| step.at < site);
   for step in steps[..before].iter().rev() {
      match step.op {
         Op::Move { dst, src } if dst == reg => reg = src,
         Op::Call(target) if reg == ix.isa.ret() => {
            if let Some(callee) = list.through_outlined(target) {
               return Ok(callee);
            }
         },
         Op::Call(_) if ix.isa.caller_saved() & 1 << reg != 0 => {
            return Err(format!(
               "a call at {:#x} clobbers the friends count",
               step.at
            ));
         },
         _ if step.writes & 1 << reg != 0 => {
            return Err(format!(
               "the friends count at {:#x} is no call's result",
               step.at
            ));
         },
         _ => {},
      }
   }
   Err(format!("the badge call at {site:#x} has no count source"))
}

/// The one call `friend_requests` makes, which counts the game friend requests.
fn game_requests<'ix>(ix: &'ix Index<'ix>, friend_requests: usize) -> Found<Func<'ix>> {
   let calls = ix.func(friend_requests)?.calls();
   let [game_requests] = calls[..] else {
      return Err(format!(
         "friend requests count at {friend_requests:#x} makes {} calls, not 1",
         calls.len()
      ));
   };
   ix.func(game_requests)
}

/// Whether `func` compares a relationship type with 3, an incoming request.
fn counts_incoming(func: &Func<'_>) -> bool {
   func.steps().iter().any(|step| {
      matches!(
         step.op,
         Op::Cmp { value: 3, .. }
            | Op::CmpMem {
               value: 3,
               size: 1,
               ..
            }
      )
   })
}

/// Whether `func` can answer state 2 and sets the second return register after
/// its last call.
fn returns_badge(ix: &Index<'_>, func: &Func<'_>) -> bool {
   let second: Reg = match ix.isa {
      Isa::X86_64 { .. } => 2,
      Isa::Aarch64 => 1,
      Isa::Thumb2 => return false,
   };
   let steps = func.steps();
   let tail = steps
      .iter()
      .rposition(|step| matches!(step.op, Op::Call(_)))
      .map_or(&steps[..], |last| &steps[last + 1..]);
   let state = Op::Imm {
      reg:   ix.isa.ret(),
      value: 2,
   };
   steps.iter().any(|step| step.op == state)
      && tail.iter().any(|step| step.writes & 1 << second != 0)
}

/// Whether `first` and `second` run the same instructions, whatever they
/// branch to or call. Each copy of a generic calls its own copies of others.
fn same_shape(ix: &Index<'_>, first: usize, second: usize) -> bool {
   let shape = |func: usize| {
      ix.func(func).map(|func| {
         func
            .steps()
            .iter()
            .map(|step| {
               let op = match step.op {
                  Op::Call(_) => Op::Call(0),
                  Op::Jump(_) => Op::Jump(0),
                  Op::Branch(_) => Op::Branch(0),
                  Op::Addr { reg, .. } => Op::Addr { reg, target: 0 },
                  Op::Global { store, .. } => Op::Global { target: 0, store },
                  op => op,
               };
               (step.len, op)
            })
            .collect::<Vec<_>>()
      })
   };
   matches!((shape(first), shape(second)), (Ok(first), Ok(second)) if first == second)
}
