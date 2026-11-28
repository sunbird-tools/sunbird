//! libdiscord's named commands, which take `{"type": name, ...args}` as JSON
//! and answer in JSON, and the pending reply store the client is read from.

use std::collections::HashMap;

use crate::{
   Found,
   index::{
      Func,
      Index,
      Isa,
      Op,
      Reg,
   },
};

/// The command runner and what it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commands {
   /// `runner(client: &Client, json: *const u8, len) -> Pin<Box<dyn Future<
   /// Output = Vec<u8>>>>`, Rust ABI, the box and its vtable in two words.
   pub runner:      usize,
   /// `get_pending_reply`'s store function, `(out, client: &Client,
   /// channel_id)` with the return slot first, which the composer calls
   /// whenever it reads the bar's reply, handing over the client. `None`
   /// where it does not decode, which only loses telling accounts apart.
   pub pending_get: Option<usize>,
}

/// The runner's future polls in `client/src/command.rs`.
const RUNNER_SOURCE: &str = "client/src/command.rs";
/// `Box<dyn Future>` vtables: drop, size, align, then `poll`.
const POLL: usize = 0x18;
const ALIGN: usize = 0x10;

/// The runner loads the vtable of a future whose poll sits in
/// `client/src/command.rs`, and `main` loads the runner's own address.
///
/// The getter is the store function the `get_pending_reply` command calls
/// with the command's client, which every command entry stores just past the
/// arguments it copies:
///
/// ```text
/// entry: mov rax, rcx; mov ecx, args; rep movsb; mov [state + args], rax
///        ..; lea rdx, [future vtable]; ret
/// poll:  mov rsi, [state + args]; ..; mov rcx, rsi; call store function
/// ```
pub fn commands(ix: &Index<'_>) -> Found<Commands> {
   let runners = ix
      .source(RUNNER_SOURCE)
      .iter()
      .flat_map(|poll| future_vtables(ix, poll.start))
      .flat_map(|vtable| {
         ix.loaders(vtable..vtable + 1)
            .iter()
            .map(|func| func.start)
            .collect::<Vec<_>>()
      })
      .collect::<Vec<_>>();
   // `main` hands the runner to the client crate as a function pointer.
   let runners = dedup(runners)
      .into_iter()
      .filter(|&runner| !ix.loaders(runner..runner + 1).is_empty())
      .collect::<Vec<_>>();
   let runner = match runners.as_slice() {
      [runner] => *runner,
      runners => return Err(format!("command runners: {runners:x?}")),
   };
   let getters = entries(ix, b"get_pending_reply")
      .into_iter()
      .filter_map(|entry| store_call(ix, entry))
      .collect::<Vec<_>>();
   // Each registry copy has its own entry, and both reach the same store.
   let pending_get = match dedup(getters).as_slice() {
      [getter] => Some(*getter),
      _ => None,
   };
   Ok(Commands {
      runner,
      pending_get,
   })
}

fn dedup(mut found: Vec<usize>) -> Vec<usize> {
   found.sort_unstable();
   found.dedup();
   found
}

/// The vtables whose `poll` is `poll`.
fn future_vtables(ix: &Index<'_>, poll: usize) -> Vec<usize> {
   ix.image
      .pointers_to(poll)
      .into_iter()
      .filter_map(|slot| slot.checked_sub(POLL))
      .filter(|&vtable| {
         ix.image
            .read_usize(vtable + ALIGN)
            .is_some_and(|align| align.is_power_of_two() && align <= 64)
      })
      .collect()
}

/// The command table entries named `name`: `{name, len, fn}` rows.
fn entries(ix: &Index<'_>, name: &[u8]) -> Vec<usize> {
   ix.image
      .find_bytes(name)
      .into_iter()
      .flat_map(|at| ix.image.pointers_to(at))
      .filter(|&row| ix.image.read_usize(row + 8) == Some(name.len()))
      .filter_map(|row| ix.image.read_usize(row + 16))
      .filter(|&entry| ix.function(entry).is_some_and(|func| func.start == entry))
      .collect()
}

/// The store function `entry`'s future calls with the client.
fn store_call(ix: &Index<'_>, entry: usize) -> Option<usize> {
   let entry = ix.function(entry)?;
   let steps = entry.steps();
   // The args are copied in first and the client stored right after them.
   let length = copy_length(ix)?;
   let args = steps.iter().find_map(|step| {
      match step.op {
         Op::Imm { reg, value } if reg == length => isize::try_from(value).ok(),
         _ => None,
      }
   })?;
   let stores_client = steps
      .iter()
      .any(|step| matches!(step.op, Op::Store { offset, size: 8, .. } if offset == args));
   if !stores_client {
      return None;
   }
   let vtable = steps.iter().rev().find_map(|step| {
      match step.op {
         Op::Addr { target, .. } => Some(target),
         _ => None,
      }
   })?;
   let poll = ix.function(ix.image.read_usize(vtable + POLL)?)?;
   client_call(ix, &poll, args)
}

/// The rep-movsb count on x86-64, `memcpy`'s length on arm64.
const fn copy_length(ix: &Index<'_>) -> Option<Reg> {
   const RCX: Reg = 1;
   match ix.isa {
      Isa::X86_64 { .. } => Some(RCX),
      isa => isa.arg(2),
   }
}

/// The first call passing the word loaded from `+client` of some base as its
/// first argument, or as its second past a return slot.
///
/// Steps walk in address order, so a branch's target may sit past code the
/// branch skips; what a register may hold at each branch is merged back in at
/// its target.
fn client_call(ix: &Index<'_>, poll: &Func<'_>, client: isize) -> Option<usize> {
   let (first, second) = (ix.isa.arg(0)?, ix.isa.slot_arg(0)?);
   let mut holds = [false; 32];
   let mut at_target: HashMap<usize, [bool; 32]> = HashMap::new();
   for step in &poll.steps() {
      if let Some(taken) = at_target.get(&step.at) {
         for (held, taken) in holds.iter_mut().zip(taken) {
            *held |= taken;
         }
      }
      match step.op {
         Op::Call(callee) if holds[usize::from(first)] || holds[usize::from(second)] => {
            return Some(callee);
         },
         Op::Call(_) => {
            for (reg, held) in holds.iter_mut().enumerate() {
               if ix.isa.caller_saved() & (1 << reg) != 0 {
                  *held = false;
               }
            }
         },
         Op::Branch(target) => {
            let taken = at_target.entry(target).or_insert([false; 32]);
            for (taken, held) in taken.iter_mut().zip(holds) {
               *taken |= held;
            }
         },
         Op::Load {
            reg,
            offset,
            size: 8,
            ..
         } => holds[usize::from(reg)] = offset == client,
         Op::Move { dst, src } => holds[usize::from(dst)] = holds[usize::from(src)],
         _ => {
            for (reg, held) in holds.iter_mut().enumerate() {
               if step.writes & (1 << reg) != 0 {
                  *held = false;
               }
            }
         },
      }
   }
   None
}
/// `Client::new`'s future poll, the one function naming the tokio runtime
/// `Client::new` insists on.
///
/// It is `poll(self, cx) -> Poll<Result<Client, E>>` with Rust's ABI: the
/// variant (0 `Ok`, 1 `Err`, 2 pending) and the client's `Arc` or the
/// error in two registers.
pub fn client_new(ix: &Index<'_>) -> Found<usize> {
   ix.literal(b"Client::new must run on a tokio runtime")
      .one("Client::new")
      .map(|func| func.start)
}
