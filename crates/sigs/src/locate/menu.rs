//! `MenuItem::Submenu`, which plugin message actions copy for their submenus.

use crate::{
   Found,
   index::{
      Index,
      Op,
      Reg,
      Step,
      Value,
   },
};

/// `MenuItem::Submenu`, from the constructor Discord's own submenus ("Create
/// Reminder", "Apps") go through.
#[derive(
   Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct SubmenuLayout {
   pub tag:        u64,
   /// The `Rc<dyn Fn() -> Vec<MenuItem>>` that builds the items on opening,
   /// its vtable in the next word.
   pub builder:    usize,
   /// A word the constructor sets to `kind_value` past the builder.
   pub kind:       usize,
   pub kind_value: u64,
   /// `MenuItemProps`, `props_size` bytes.
   pub props:      usize,
   pub props_size: usize,
}

/// Instructions a constructor runs before its last store: its frame, the
/// label's conversion, the props copy and the variant's words.
const HEAD: usize = 40;

/// Every copy of `MenuItem::submenu(label, builder)` copies the props it built
/// to `out + props`, then stores the tag at `out`, the builder's `Rc` and
/// vtable, and the kind word. On x86-64:
///
/// ```text
/// lea rdi, [rbx + props]; ..; mov ecx, props_size; rep movsb
/// mov qword ptr [rbx], tag
/// mov [rbx + builder], rax
/// lea rax, [rip + vtable]; mov [rbx + builder + 8], rax
/// mov qword ptr [rbx + kind], kind_value
/// ```
///
/// arm64 copies through an outlined stub that loads the length before its
/// `b memcpy`. Every copy must agree.
pub fn submenu(ix: &Index<'_>) -> Found<SubmenuLayout> {
   let sp = ix.isa.sp();
   let candidates = if ix.isa == crate::index::Isa::Aarch64 {
      // Pre-filter on raw words to avoid decoding 40 instructions of all ~300k functions:
      // `shape()` requires an `adrp` (for the builder vtable), a non-SP `add #imm` (for
      // `props` field address), and an 8-byte store off a non-SP base (for `tag` at +0).
      ix.heads_arm64_words(HEAD, |words| {
         let (mut has_adrp, mut has_add, mut has_store0) = (false, false, false);
         for &w in words {
            if w & 0x9F00_0000 == 0x9000_0000 {
               has_adrp = true;
            } else if w & 0x7F80_0000 == 0x1100_0000 && ((w >> 5) & 31) != 31 {
               has_add = true;
            } else if ((w & 0xFFFF_FC00 == 0xF900_0000)
               || (w & 0xFFE0_0000 == 0xF800_0000)
               || (w & 0xFC40_0000 == 0xA800_0000))
               && ((w >> 5) & 31) != 31
            {
               has_store0 = true;
            }
            if has_adrp && has_add && has_store0 {
               return true;
            }
         }
         false
      })
   } else {
      ix.heads(HEAD, |head| shape(head, sp, |_| Some(0)).is_some())
   };
   let mut found = candidates
      .iter()
      .filter_map(|func| {
         shape(&func.head(HEAD), sp, |target| {
            func
               .outlined(target, 4)?
               .iter()
               .find_map(|step| imm(step.op))
         })
      })
      .collect::<Vec<_>>();
   found.sort_unstable();
   found.dedup();
   match found.as_slice() {
      [layout] => Ok(*layout),
      [] => Err("no MenuItem::submenu constructor".to_owned()),
      layouts => {
         Err(format!(
            "MenuItem::submenu constructors disagree: {layouts:x?}"
         ))
      },
   }
}

/// What a store wrote, with constants loaded into a register first (arm64
/// stores them from one) followed through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
   Imm(u64),
   Addr,
   Other,
}

struct Stored {
   /// Its place in the head.
   step:   usize,
   base:   Reg,
   offset: isize,
   word:   Word,
}

/// `(step, base, offset)`: a field's address.
type Field = (usize, Reg, isize);

const fn imm(op: Op) -> Option<u64> {
   match op {
      Op::Imm { value, .. } => Some(value),
      _ => None,
   }
}

/// The head's 8-byte stores and field addresses.
fn trace(head: &[Step]) -> (Vec<Stored>, Vec<Field>) {
   let mut regs = [Word::Other; 32];
   let mut fields: Vec<Field> = Vec::new();
   let mut stores = Vec::new();
   for (index, step) in head.iter().enumerate() {
      let word = |value: Value| {
         match value {
            Value::Imm(imm) => Word::Imm(imm),
            Value::Reg(reg) => regs.get(usize::from(reg)).copied().unwrap_or(Word::Other),
         }
      };
      match step.op {
         Op::Store {
            value,
            base,
            offset,
            size: 8,
         } => {
            stores.push(Stored {
               step: index,
               base,
               offset,
               word: word(value),
            });
         },
         Op::StorePair {
            values,
            base,
            offset,
            size: 8,
         } => {
            for (at, reg) in [offset, offset + 8].into_iter().zip(values) {
               stores.push(Stored {
                  step: index,
                  base,
                  offset: at,
                  word: word(Value::Reg(reg)),
               });
            }
         },
         _ => {},
      }
      for (reg, slot) in regs.iter_mut().enumerate() {
         if step.writes & (1 << reg) != 0 {
            *slot = Word::Other;
         }
      }
      match step.op {
         Op::Imm { reg, value } => {
            if let Some(slot) = regs.get_mut(usize::from(reg)) {
               *slot = Word::Imm(value);
            }
         },
         Op::Addr { reg, .. } => {
            if let Some(slot) = regs.get_mut(usize::from(reg)) {
               *slot = Word::Addr;
            }
         },
         Op::Offset { base, offset, .. } => fields.push((index, base, offset)),
         _ => {},
      }
   }
   (stores, fields)
}

/// `copied` reads the length an outlined copy stub at its address loads.
fn shape(head: &[Step], sp: Reg, copied: impl Fn(usize) -> Option<u64>) -> Option<SubmenuLayout> {
   let (stores, fields) = trace(head);
   let small = |word| matches!(word, Word::Imm(imm) if imm < 0x10);
   let at = |base, offset| {
      stores
         .iter()
         .find(|store| store.base == base && store.offset == offset)
         .map(|store| store.word)
   };
   let tag = stores
      .iter()
      .find(|store| store.offset == 0 && store.base != sp && small(store.word))?;
   let base = tag.base;
   let builder = stores.iter().find(|store| {
      store.base == base
         && store.offset > 0
         && store.word == Word::Other
         && at(base, store.offset + 8) == Some(Word::Addr)
   })?;
   let kind = stores
      .iter()
      .find(|store| store.base == base && store.offset > builder.offset + 8 && small(store.word))?;
   // The props are copied in before the variant's words go on.
   let &(field, _, props) = fields.iter().find(|&&(step, field_base, offset)| {
      step < tag.step && field_base == base && offset > kind.offset
   })?;
   let size = head.get(field + 1..tag.step)?.iter().find_map(|step| {
      match step.op {
         Op::Call(target) | Op::Jump(target) => Some(copied(target)),
         op => imm(op).map(Some),
      }
   })?;
   let word = |word| {
      match word {
         Word::Imm(imm) => Some(imm),
         Word::Addr | Word::Other => None,
      }
   };
   Some(SubmenuLayout {
      tag:        word(tag.word)?,
      builder:    usize::try_from(builder.offset).ok()?,
      kind:       usize::try_from(kind.offset).ok()?,
      kind_value: word(kind.word)?,
      props:      usize::try_from(props).ok()?,
      props_size: usize::try_from(size?).ok()?,
   })
}
