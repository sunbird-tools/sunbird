//! Mana's `Clickable`, the hoverable wrapper Discord's toolbar buttons are,
//! with the tooltip it draws from its label.

use crate::{
   Found,
   index::{
      Func,
      Index,
      Isa,
      Op,
      Reg,
   },
   locate::channel,
};

/// `(destination, length)` registers of a block copy: `rep movsb` on x86-64,
/// `memcpy(x0, x1, x2)` on arm64.
fn copy_regs(isa: Isa) -> Option<(Reg, Reg)> {
   const RDI: Reg = 7;
   const RCX: Reg = 1;
   match isa {
      Isa::X86_64 { .. } => Some((RDI, RCX)),
      _ => Some((isa.arg(0)?, isa.arg(2)?)),
   }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Clickable {
   /// `new(out, id, id_len, label: &mut SharedString) -> out`, whose label
   /// the tooltip shows.
   pub new:        usize,
   /// `into_any(out: &mut AnyElement, clickable) -> out`.
   pub into_any:   usize,
   pub size:       usize,
   /// Where the `Div` the content goes in sits, and the bytes `new` copies
   /// there.
   pub inner:      usize,
   pub inner_size: usize,
   /// The tooltip placement byte: none while `0xFF`, 0 above, 1 below.
   pub tooltip:    usize,
}

const TYPE_NAME: &[u8] = b"mana_gpui::components::clickable::Clickable";

/// The channel toolbar's button builder makes a `Clickable` and sets its
/// tooltip placement with struct update syntax right after:
///
/// ```text
/// call Clickable::new; ..; mov ecx, tooltip; rep movsb; mov [rdx + tooltip], cl
/// ```
///
/// where the copy before the field is as long as the field's offset.
/// `Clickable::new` copies the inner `Div` it built into place the same way
/// (`lea rdi, [out + inner]; mov ecx, size; rep movsb`), and `into_any` is the
/// busiest function naming the type.
pub fn clickable(ix: &Index<'_>) -> Found<Clickable> {
   let toolbar = channel::channel_toolbar(ix)?;
   let builder = ix.func(toolbar.button)?;
   let mut works = vec![builder];
   works.extend(
      builder
         .calls()
         .into_iter()
         .filter_map(|callee| ix.function(callee)),
   );
   let found = works
      .iter()
      .filter_map(|func| placement(ix.isa, func))
      .collect::<Vec<_>>();
   let (new, tooltip) = match found.as_slice() {
      [found] => *found,
      found => return Err(format!("toolbar button tooltip placements: {found:x?}")),
   };
   if tooltip >= toolbar.button_size {
      return Err(format!(
         "the tooltip placement +{tooltip:#x} sits past the {:#x}-byte Clickable",
         toolbar.button_size
      ));
   }
   let (inner, inner_size) = inner_copy(ix.isa, &ix.func(new)?)
      .ok_or_else(|| format!("Clickable::new {new:#x} copies no inner Div"))?;
   let into_any = ix
      .literal(TYPE_NAME)
      .iter()
      .max_by_key(|func| func.callers().iter().count())
      .map(|func| func.start)
      .ok_or("Clickable is never named in code")?;
   Ok(Clickable {
      new,
      into_any,
      size: toolbar.button_size,
      inner,
      inner_size,
      tooltip,
   })
}

/// `(new, field)`: a copy as long as a byte field's offset, the field stored
/// right after, and the call building the value before them.
fn placement(isa: Isa, func: &Func<'_>) -> Option<(usize, usize)> {
   let (_, length) = copy_regs(isa)?;
   let steps = func.steps();
   steps.iter().enumerate().find_map(|(at, step)| {
      let Op::Imm { reg, value } = step.op else {
         return None;
      };
      if reg != length {
         return None;
      }
      let field = isize::try_from(value).ok().filter(|&field| field > 0x100)?;
      let stored = steps[at + 1..]
         .iter()
         .take(8)
         .any(|step| matches!(step.op, Op::Store { offset, size: 1, .. } if offset == field));
      if !stored {
         return None;
      }
      let new = steps[..at].iter().rev().take(12).find_map(|step| {
         match step.op {
            Op::Call(callee) => Some(callee),
            _ => None,
         }
      })?;
      Some((new, usize::try_from(field).ok()?))
   })
}

/// `(offset, bytes)` of the block `new` copies into its return slot.
fn inner_copy(isa: Isa, new: &Func<'_>) -> Option<(usize, usize)> {
   let (destination, length) = copy_regs(isa)?;
   let steps = new.steps();
   steps.iter().enumerate().find_map(|(at, step)| {
      let Op::Offset { reg, offset, .. } = step.op else {
         return None;
      };
      if reg != destination {
         return None;
      }
      let size = steps[at + 1..].iter().take(4).find_map(|step| {
         match step.op {
            Op::Imm { reg, value } if reg == length => usize::try_from(value).ok(),
            _ => None,
         }
      })?;
      Some((usize::try_from(offset).ok()?, size))
   })
}
