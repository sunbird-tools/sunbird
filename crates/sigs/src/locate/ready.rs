//! `READY`, which Phoenix's session parses itself, past the gateway registry.

use crate::{
   Found,
   index::{
      Func,
      Index,
      Op,
      Reg,
      Value,
      flow,
   },
};

/// How many instructions open a READY caller before what tells it apart.
const HEAD: usize = 48;

/// `serde_json::from_slice::<Ready>`, `fn(out, &mut SliceRead)`, the first
/// call of the `READY` handler `ready`. The session's handler and its text
/// parse call it too.
pub fn ready_parse<'ix>(ix: &'ix Index<'ix>, ready: usize) -> Found<Func<'ix>> {
   let first = *ix
      .func(ready)?
      .calls()
      .first()
      .ok_or_else(|| format!("READY handler at {ready:#x} makes no call"))?;
   let parse = ix.func(first)?;
   let callers = parse.callers().len();
   if callers < 3 {
      return Err(format!(
         "READY's first call at {first:#x} has {callers} callers, too few for its parse"
      ));
   }
   Ok(parse)
}

/// The session's READY parse for text under 2 MB, `fn(out, ptr, len) -> out`:
/// the caller of [`ready_parse`] that stores the text it takes into its reader
/// as it opens.
pub fn ready_text(ix: &Index<'_>, parse: &Func<'_>) -> Found<usize> {
   let ptr = ix.isa.slot_arg(0).ok_or("no text argument register")?;
   let len = ix.isa.slot_arg(1).ok_or("no length argument register")?;
   parse
      .callers()
      .filter(|func| stores_text(func, ptr, len))
      .one("READY text parse")
      .map(|func| func.start)
}

/// Whether `func` stores `ptr` and `len` as it got them to adjacent words.
fn stores_text(func: &Func<'_>, ptr: Reg, len: Reg) -> bool {
   let mut live = 1_u32 << ptr | 1 << len;
   let mut stored = Vec::<(Reg, Reg, isize)>::new();
   for step in func.head(HEAD) {
      let mut store = |value: Reg, base: Reg, offset: isize| {
         if live & 1 << value != 0 {
            stored.push((value, base, offset));
         }
      };
      match step.op {
         Op::Store {
            value: Value::Reg(value),
            base,
            offset,
            size: 8,
         } => store(value, base, offset),
         Op::StorePair {
            values: [first, second],
            base,
            offset,
            size: 8,
         } => {
            store(first, base, offset);
            store(second, base, offset + 8);
         },
         _ => {},
      }
      live &= !step.writes;
   }
   stored
      .iter()
      .any(|&(value, base, offset)| value == ptr && stored.contains(&(len, base, offset + 8)))
}

/// Phoenix's session READY handler, `fn(out, payload) -> out`, which owns the
/// gateway payload and parses READY itself, past the registry.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ReadySession {
   pub func:     usize,
   /// The payload's kind byte, and the kind whose READY is parsed from the raw
   /// text. Other kinds carry the gateway's own parse of it.
   pub kind:     usize,
   pub raw_kind: u8,
   /// The raw text's pointer and length.
   pub raw:      usize,
   pub raw_len:  usize,
}

/// The caller of [`ready_parse`] that opens by checking the payload's kind
/// byte, then loads the raw text's pointer and length from it.
pub fn ready_session(ix: &Index<'_>, parse: &Func<'_>) -> Found<ReadySession> {
   let payload = ix.isa.slot_arg(0).ok_or("no payload argument register")?;
   let found = parse
      .callers()
      .iter()
      .filter_map(|func| session_reads(ix, func, payload))
      .collect::<Vec<_>>();
   crate::one("READY session", &found)
}

/// The kind check and raw text loads `func` makes from `payload` as it opens.
fn session_reads(ix: &Index<'_>, func: &Func<'_>, payload: Reg) -> Option<ReadySession> {
   let mut byte = None::<(Reg, isize)>;
   let mut check = None::<(isize, u64)>;
   let mut words = Vec::<isize>::new();
   flow::follow(ix.isa, &func.head(HEAD), payload, |step, holds| {
      match (step.op, check) {
         (
            Op::CmpMem {
               base,
               offset,
               size: 1,
               value,
            },
            None,
         ) if holds(base) => check = Some((offset, value)),
         (
            Op::Load {
               reg,
               base,
               offset,
               size: 1,
            },
            None,
         ) if holds(base) => byte = Some((reg, offset)),
         (Op::Cmp { reg, value }, None) => {
            check = byte
               .filter(|&(loaded, _)| loaded == reg)
               .map(|(_, offset)| (offset, value));
         },
         (
            Op::Load {
               base,
               offset,
               size: 8,
               ..
            },
            Some(_),
         ) if holds(base) => words.push(offset),
         (
            Op::LoadPair {
               base,
               offset,
               size: 8,
               ..
            },
            Some(_),
         ) if holds(base) => words.extend([offset, offset + 8]),
         _ => {},
      }
   });
   let (kind, raw_kind) = check?;
   let raw = *words.iter().find(|&&word| words.contains(&(word + 8)))?;
   Some(ReadySession {
      func:     func.start,
      kind:     usize::try_from(kind).ok()?,
      raw_kind: u8::try_from(raw_kind).ok()?,
      raw:      usize::try_from(raw).ok()?,
      raw_len:  usize::try_from(raw + 8).ok()?,
   })
}
