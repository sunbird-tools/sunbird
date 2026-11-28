//! The chat pane's message commands (reply, edit), which the message menu's
//! items and the row's hover buttons run.

use crate::{
   Found,
   index::{
      Func,
      Index,
      Op,
      Step,
   },
};

/// The menu items' handlers for one command each, and the pane's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneCommands {
   /// `fn(pane: &WeakEntity<ChatPaneView>, app: &&mut App, message_id: &u64,
   /// window: &mut Window)`, which upgrades the pane and runs the command
   /// in its update.
   pub reply:     usize,
   pub edit:      usize,
   /// `TypeId::of::<ChatPaneView>()`, which every pane handle carries.
   pub pane_type: [u8; 16],
}

/// `MessageCommand` numbers, as `chat/message_command.rs` matches them.
const REPLY: u64 = 0;
const EDIT: u64 = 1;

/// `chat/message_command.rs` runs a command on the pane. Its one caller is
/// the runner, and the runner's busiest caller the shim every menu item's
/// update callback calls with the command in `edx`:
///
/// ```text
/// callback: ..; pcmpeqb xmm0, [pane TypeId]; ..; mov dl, command; call shim
/// thunk:    jmp callback          (the callback's vtable `call_once`)
/// handler:  ..; lea rax, [vtable]; ..; call update_entity
/// ```
pub fn pane_commands(ix: &Index<'_>) -> Found<PaneCommands> {
   let runner = ix
      .source("message_command.rs")
      .iter()
      .filter_map(|func| func.callers().one("").ok())
      .filter(|runner| runner.callers().iter().count() >= 2)
      .collect::<Vec<_>>();
   let [runner] = runner.as_slice() else {
      return Err(format!("message command runners: {runner:?}"));
   };
   let shim = runner
      .callers()
      .iter()
      .max_by_key(|func| func.callers().iter().count())
      .copied()
      .ok_or("message command runner has no callers")?;
   let callbacks = shim.callers();
   let callback = |command| {
      let found = callbacks
         .iter()
         .filter(|func| passes(ix, func, shim.start, command))
         .collect::<Vec<_>>();
      match found.as_slice() {
         [func] => Ok(**func),
         funcs => Err(format!("message command {command} callbacks: {funcs:?}")),
      }
   };
   let (reply, edit) = (callback(REPLY)?, callback(EDIT)?);
   let pane_type = pane_type(ix, &edit)?;
   if pane_type != self::pane_type(ix, &reply)? {
      return Err("reply and edit check different pane types".to_owned());
   }
   let (reply, edit) = (handler(ix, &reply)?, handler(ix, &edit)?);
   // The numbers are the enum's order, so the menu's own items confirm them.
   for (id, handler) in [(&b"reply"[..], reply), (b"edit", edit)] {
      if !menu_item_runs(ix, id, handler) {
         return Err(format!(
            "the menu's {:?} item does not run {handler:#x}",
            String::from_utf8_lossy(id)
         ));
      }
   }
   Ok(PaneCommands {
      reply,
      edit,
      pane_type,
   })
}

/// Whether `MessageContextMenu` builds its item with id `id` around a click
/// handler that calls `handler`. The builder makes each item, then names it:
///
/// ```text
/// call item_ctor; ..; lea r8, ["reply"]; mov r9d, 5; call with_id
/// ```
///
/// and the constructor, a few small calls down, loads the click closure's
/// vtable, whose `call` slot calls the handler.
fn menu_item_runs(ix: &Index<'_>, id: &[u8], handler: usize) -> bool {
   const SLOTS: [usize; 3] = [0x18, 0x20, 0x28];
   const DEPTH: usize = 3;
   const SMALL: usize = 0x200;
   let named = |target: usize| ix.image.read(target, id.len()) == Some(id);
   let builders = ix
      .literal(b"MessageContextMenu")
      .filter(|func| func.size() > 0x1000);
   builders.iter().any(|builder| {
      let steps = builder.steps();
      steps.iter().enumerate().any(|(at, step)| {
         let Op::Addr { target, .. } = step.op else {
            return false;
         };
         // x86 loads the length after the address, arm64 may before.
         let length =
            |step: &Step| matches!(step.op, Op::Imm { value, .. } if value == id.len() as u64);
         let sized = steps[at + 1..].iter().take(6).any(length)
            || steps[..at].iter().rev().take(6).any(length);
         if !named(target) || !sized {
            return false;
         }
         let Some(ctor) = steps[..at].iter().rev().take(16).find_map(|step| {
            match step.op {
               Op::Call(callee) => Some(callee),
               _ => None,
            }
         }) else {
            return false;
         };
         let mut frontier = vec![ctor];
         for _ in 0..DEPTH {
            let funcs = frontier
               .iter()
               .filter_map(|&addr| ix.function(addr))
               .filter(|func| func.size() <= SMALL)
               .collect::<Vec<_>>();
            let calls_handler = funcs.iter().flat_map(Func::steps).any(|step| {
               let Op::Addr { target, .. } = step.op else {
                  return false;
               };
               SLOTS.iter().any(|slot| {
                  ix.image
                     .read_usize(target + slot)
                     .and_then(|call| ix.function(call))
                     .is_some_and(|call| call.calls().contains(&handler))
               })
            });
            if calls_handler {
               return true;
            }
            frontier = funcs.iter().flat_map(Func::calls).collect();
         }
         false
      })
   })
}

/// Whether `func` calls `shim` with `command` in the second argument.
fn passes(ix: &Index<'_>, func: &Func<'_>, shim: usize, command: u64) -> bool {
   let Some(arg) = ix.isa.arg(1) else {
      return false;
   };
   let steps = func.steps();
   steps.iter().enumerate().any(|(at, step)| {
      step.op == Op::Call(shim)
         && steps[..at].iter().rev().take(6).find_map(|step| {
            match step.op {
               Op::Imm { reg, value } if reg == arg => Some(value == command),
               _ if step.writes & (1 << arg) != 0 => Some(false),
               _ => None,
            }
         }) == Some(true)
   })
}

/// The 16 bytes the callback compares the pane's type with.
fn pane_type(ix: &Index<'_>, callback: &Func<'_>) -> Found<[u8; 16]> {
   let target = callback
      .steps()
      .iter()
      .find_map(|step| {
         match step.op {
            Op::Global {
               target,
               store: false,
            } => Some(target),
            _ => None,
         }
      })
      .ok_or("pane command callback reads no type")?;
   ix.image
      .read(target, 16)
      .and_then(|bytes| bytes.try_into().ok())
      .ok_or_else(|| "pane type unreadable".to_owned())
}

/// The handler loading the closure vtable whose `call_mut` is `callback`
/// (its `call_once` is a jump to it).
fn handler(ix: &Index<'_>, callback: &Func<'_>) -> Found<usize> {
   const CALL_MUT: usize = 0x20;
   let vtables = ix
      .image
      .pointers_to(callback.start)
      .into_iter()
      .filter_map(|slot| slot.checked_sub(CALL_MUT))
      .filter(|&vtable| {
         ix.image
            .read_usize(vtable + 0x10)
            .is_some_and(|align| align.is_power_of_two() && align <= 16)
      })
      .collect::<Vec<_>>();
   let vtable = match vtables.as_slice() {
      [vtable] => *vtable,
      vtables => return Err(format!("pane command vtables: {vtables:x?}")),
   };
   ix.loaders(vtable..vtable + 1)
      .one("pane command handler")
      .map(|func| func.start)
}
