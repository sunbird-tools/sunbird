//! mana's dismiss stack (`mana_gpui::components::dismiss::Stacks`), which
//! Escape closes the top entry of before any keybinding runs.
//!
//! Layers register an entry every frame they render, by element id, and an
//! entry is gone once its element stops registering. A modal registered this
//! way closes on Escape before whatever it is drawn over, User Settings
//! included.

use crate::{
   Found,
   index::{
      Funcs,
      Index,
      Op,
      op::Value,
   },
};

/// What registering an entry takes, copied from the modal User Settings
/// registers for a failed connection check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DismissStack {
   /// `build(out: *mut Config, name: *const u8, len, kind: u8, closure:
   /// *const C)`, which moves `closure` into the entry's on-dismiss
   /// `Rc<dyn Fn(&mut Window, &mut App)>`.
   pub build:      usize,
   /// `register(config: *mut Config, window, cx: &mut App)`, which takes the
   /// config and must run while a view renders.
   pub register:   usize,
   /// The modal kind, which the stack dismisses before layers.
   pub kind:       u8,
   /// The on-dismiss vtable `build` stores in the config, `{drop, size,
   /// align, call_once, call_mut, call}`.
   pub on_dismiss: usize,
}

/// The element name User Settings gives its modal.
const MODAL: &[u8] = b"connection-verify-error";
/// The global every registration reads.
const STACKS: &[u8] = b"mana_gpui::components::dismiss::Stacks";
/// The closure sizes a vtable is believed for.
const CLOSURE_CAP: usize = 0x400;
/// `Fn::call` in a `Box<dyn Fn>` or `Rc<dyn Fn>` vtable.
const CALL: usize = 0x28;

/// `lea name; .. mov len; .. mov kind; call build; .. call register`, where
/// `register` hands the config to the function that reads [`STACKS`] (itself
/// on Windows, through a callee on Linux).
pub fn dismiss_stack(ix: &Index<'_>) -> Found<DismissStack> {
   let (name_arg, kind_arg) = (
      ix.isa.arg(1).ok_or("no name argument register")?,
      ix.isa.arg(3).ok_or("no kind argument register")?,
   );
   let registrars = ix.literal(STACKS);
   let mut found = Vec::new();
   for name in ix.text(MODAL) {
      for (site, _) in ix.loads_of(name..name + 1) {
         let Some(func) = ix.function(site) else {
            continue;
         };
         let steps = func.steps();
         let Some(at) = steps.iter().position(|step| {
            step.at == site && matches!(step.op, Op::Addr { reg, .. } if reg == name_arg)
         }) else {
            continue;
         };
         let mut kind = None;
         let mut calls = Vec::new();
         for step in &steps[at..] {
            match step.op {
               Op::Imm { reg, value } if reg == kind_arg && calls.is_empty() => {
                  kind = u8::try_from(value).ok();
               },
               Op::Call(callee) => {
                  calls.push(callee);
                  if calls.len() == 2 {
                     break;
                  }
               },
               _ => {},
            }
         }
         let (Some(kind), [build, register]) = (kind, calls.as_slice()) else {
            continue;
         };
         if !reaches(ix, *register, &registrars, 2) {
            continue;
         }
         if let Some(on_dismiss) = on_dismiss_vtable(ix, *build) {
            found.push(DismissStack {
               build: *build,
               register: *register,
               kind,
               on_dismiss,
            });
         }
      }
   }
   found.sort_unstable_by_key(|stack| stack.build);
   found.dedup();
   crate::one("User Settings' dismiss stack registration", &found)
}

/// Whether `func` calls one of `targets` within `depth` calls.
fn reaches(ix: &Index<'_>, func: usize, targets: &Funcs<'_>, depth: usize) -> bool {
   depth > 0
      && ix.function(func).is_some_and(|func| {
         func.calls().iter().any(|&callee| {
            targets.iter().any(|target| target.start == callee)
               || reaches(ix, callee, targets, depth - 1)
         })
      })
}

/// The one closure vtable `build` loads: a sized, aligned value with a `call`.
///
/// It is the one stored through the config pointer (the label's formatting
/// loads another), tracked from the first argument through moves.
fn on_dismiss_vtable(ix: &Index<'_>, build: usize) -> Option<usize> {
   let steps = ix.function(build)?.steps();
   let mut config = 1u32 << ix.isa.arg(0)?;
   let mut vtables = Vec::new();
   for (at, step) in steps.iter().enumerate() {
      match step.op {
         Op::Move { dst, src } if config & (1 << src) != 0 => config |= 1 << dst,
         Op::Addr { reg, target } => {
            let stored = steps[at + 1..].iter().take(2).any(|next| {
               matches!(next.op, Op::Store { value: Value::Reg(value), base, size: 8, .. }
                  if value == reg && config & (1 << base) != 0)
            });
            if stored && is_vtable(ix, target) {
               vtables.push(target);
            }
            config &= !(1 << reg);
         },
         _ => config &= !step.writes,
      }
   }
   match vtables.as_slice() {
      [only] => Some(*only),
      _ => None,
   }
}

fn is_vtable(ix: &Index<'_>, vtable: usize) -> bool {
   let size = ix.image.read_usize(vtable + 8);
   let align = ix.image.read_usize(vtable + 16);
   size.is_some_and(|size| size <= CLOSURE_CAP)
      && align.is_some_and(|align| align.is_power_of_two() && align <= 16)
      && ix
         .image
         .read_usize(vtable + CALL)
         .is_some_and(|call| ix.image.is_code(call))
}
