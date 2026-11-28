//! Desktop User Settings: page 24, the sidebar and page changes the runtime
//! adds Sunbird's entry with, and the client's optimistic message delete.

use std::collections::HashMap;

use crate::{
   Found,
   index::{
      Func,
      Index,
      Isa,
      Native,
      Op,
      Reg,
      Step,
   },
   one,
};

/// Page 24's sidebar name `Phoenix` and its id, both relabeled in place.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SettingsLabel {
   pub name:   usize,
   /// The id literal the code loads, `DESKTOP_PHOENIX` or `PHOENIX`.
   pub id:     usize,
   /// The `PHOENIX` that ends the id.
   pub suffix: usize,
   /// `fn(page) -> Text` through the return slot, every page's translated
   /// name.
   pub func:   usize,
}

/// The function that loads both an id ending in `PHOENIX` and a `Phoenix`
/// name.
///
/// COFF and Mach-O builds pack them as `PhoenixDESKTOP_PHOENIX`, ELF
/// builds place them apart, and Linux builds after 2233 drop `DESKTOP_`.
pub fn settings_label(ix: &Index<'_>) -> Found<SettingsLabel> {
   const NAME: &[u8] = b"Phoenix";
   const ID: &[u8] = b"PHOENIX";
   const PREFIX: &[u8] = b"DESKTOP_";

   let mut found = Vec::new();
   for suffix in ix.text(ID) {
      let prefixed = suffix
         .checked_sub(PREFIX.len())
         .filter(|&start| ix.image.read(start, PREFIX.len()) == Some(PREFIX));
      for id in prefixed.into_iter().chain([suffix]) {
         for func in ix.loaders(id..id + 1) {
            let names = func
               .loads()
               .into_iter()
               .filter(|&name| ix.image.read(name, NAME.len()) == Some(NAME));
            found.extend(names.map(|name| (name, id, suffix, func.start)));
         }
      }
   }
   found.sort_unstable();
   found.dedup();
   let (name, id, suffix, func) = one("settings label loader", &found)?;
   Ok(SettingsLabel {
      name,
      id,
      suffix,
      func,
   })
}

/// A `cmp page - 9, 15` that sends pages outside 9 to 24 to "visible".
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct VisibilityGuard {
   pub cmp: usize,
   pub len: u8,
}

impl VisibilityGuard {
   /// The x86-64 `cmp r32, imm8` immediate byte.
   #[must_use]
   pub const fn imm8(&self) -> usize {
      self.cmp + self.len as usize - 1
   }
}

/// Every copy of `is_page_visible`'s page-range guard, which the runtime
/// lowers so page 24 is always shown.
///
/// Pages 9 to 24 switch on flags through a jump table and the rest are
/// visible, so each copy opens with `page - 9`, a compare with 15, a branch,
/// and soon after the table's address.
pub fn settings_visibility(ix: &Index<'_>) -> Found<Vec<VisibilityGuard>> {
   const HEAD: usize = 12;
   const OPENS: usize = 4;
   const TABLE: usize = 8;

   let guard = |head: &[Step]| {
      (1..OPENS.min(head.len().saturating_sub(1))).find_map(|at| {
         let Op::Cmp { reg, value: 15 } = head[at].op else {
            return None;
         };
         let table = head[at + 2..head.len().min(at + 2 + TABLE)]
            .iter()
            .any(|step| matches!(step.op, Op::Addr { .. }));
         (matches!(head[at + 1].op, Op::Branch(_))
            && table
            && subtracts(ix, &head[at - 1]) == Some((reg, 9)))
         .then_some(VisibilityGuard {
            cmp: head[at].at,
            len: head[at].len,
         })
      })
   };
   // A compare with 15 among the first instructions rejects nearly every function cheaply.
   let found = ix
      .heads(OPENS, |head| {
         head
            .iter()
            .skip(1)
            .any(|step| matches!(step.op, Op::Cmp { value: 15, .. }))
      })
      .into_iter()
      .filter_map(|func| guard(&func.head(HEAD)))
      .collect::<Vec<_>>();
   match found.len() {
      1..=4 => Ok(found),
      count => Err(format!("settings visibility guard matched {count} times")),
   }
}

/// `(dst, imm)` for `dst = src - imm` in 32 bits: `lea r32, [r - imm]` or
/// `sub w, w, #imm`.
fn subtracts(ix: &Index<'_>, step: &Step) -> Option<(Reg, u64)> {
   match step.native {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Native::X86(iced_x86::Mnemonic::Lea) => {
         let code = ix.image.read(step.at, usize::from(step.len))?;
         let insn = iced_x86::Decoder::with_ip(64, code, step.at as u64, 0).decode();
         let dst = insn.op0_register();
         let disp = insn.memory_displacement64().cast_signed();
         if !dst.is_gpr32() || insn.memory_index() != iced_x86::Register::None || disp >= 0 {
            return None;
         }
         Some((
            Reg::try_from(dst.full_register().number()).ok()?,
            disp.unsigned_abs(),
         ))
      },
      Native::Arm64(phoenix_hook::arm64::Insn::Other(word))
         if word & 0xFFC0_0000 == 0x5100_0000 =>
      {
         Some(((word & 0x1F) as Reg, u64::from((word >> 10) & 0xFFF)))
      },
      _ => None,
   }
}

/// `host::section`, which renders a settings page, and the offset of the
/// active page byte in the state it renders.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Section {
   pub func:        usize,
   pub page_offset: usize,
}

/// The one function that loads `account-loading`, and its first byte read
/// off `self` or a copy of it at an offset a page byte sits at.
pub fn settings_section(ix: &Index<'_>) -> Found<Section> {
   const HEAD: usize = 96;

   let func = ix.literal(b"account-loading").one("account-loading load")?;
   // The hidden return pointer takes the first argument register on x86-64
   // and x8 on arm64, so `self` arrives second or first.
   let this = match ix.isa {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Isa::X86_64 { .. } => ix.isa.arg(1),
      Isa::Aarch64 | Isa::Thumb2 => ix.isa.arg(0),
   }
   .ok_or("no self register")?;
   let mut copies = vec![this];
   let page_offset = func
      .steps()
      .iter()
      .take(HEAD)
      .find_map(|step| {
         match step.op {
            Op::Move { dst, src } if copies.contains(&src) => copies.push(dst),
            Op::Load {
               base,
               offset,
               size: 1,
               ..
            } if copies.contains(&base) && (0x100..0x2000).contains(&offset) => {
               return usize::try_from(offset).ok();
            },
            _ => {},
         }
         None
      })
      .ok_or("settings section active page load not found")?;
   Ok(Section {
      func: func.start,
      page_offset,
   })
}

/// The User Settings sidebar's entry list, which Sunbird's row joins.
///
/// The sidebar render (the one loader of Log Out's `us-logout` id) moves each
/// entry into a `Vec` through one push. Log Out's row comes from its own
/// constructor right before its push, the last one.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SidebarRows {
   /// The render, with [`settings_section`]'s arguments.
   pub sidebar:       usize,
   /// `fn(page: u8) -> u8`, the page whose row is selected for the active one
   /// (a subpage's parent). The render only compares its result with its rows'
   /// pages.
   pub selected:      usize,
   /// The pointer the render calls `selected` through, on Linux, where
   /// `selected` is too short for an entry hook.
   pub selected_via:  Option<usize>,
   /// The render's call of `selected`, which macOS rewrites, calling it
   /// directly with no room for an entry hook.
   pub selected_call: usize,
   /// `fn(&mut Vec<Entry>, &mut Entry)`, moving the entry in.
   pub push:          usize,
   pub entry_size:    usize,
   /// `fn(out, id_ptr, id_len, &SharedString, icon: u16, selected: bool,
   /// danger: bool, &closure)`, boxing the closure with Log Out's click.
   pub row:           usize,
   /// Log Out's click vtable, stored at `click_offset` after the box.
   pub click_vtable:  usize,
   pub click_offset:  usize,
   /// Mana's gear icon, Sunbird's.
   pub gear:          u16,
}

#[expect(
   clippy::too_many_lines,
   reason = "locates the row, its push and its click in one pass"
)]
pub fn settings_sidebar_rows(ix: &Index<'_>, page_offset: usize) -> Found<SidebarRows> {
   const ID: &[u8] = b"us-logout";
   // Pages, headers, separators and Log Out all go through the push.
   const PUSHES: usize = 3;

   let sidebar = ix.literal(ID).one("us-logout load")?;
   let steps = sidebar.steps();
   let load = steps
      .iter()
      .position(|step| {
         matches!(step.op, Op::Addr { target, .. } if ix.image.read(target, ID.len()) == Some(ID))
      })
      .ok_or("the sidebar never takes us-logout's address")?;
   let mut calls = steps[load..].iter().filter_map(|step| {
      match step.op {
         Op::Call(target) => sidebar.through_outlined(target),
         _ => None,
      }
   });
   let (Some(row), Some(push)) = (calls.next(), calls.next()) else {
      return Err("Log Out's row is never built and pushed".into());
   };
   let pushes = sidebar
      .calls()
      .iter()
      .filter(|&&callee| callee == push)
      .count();
   if pushes < PUSHES {
      return Err(format!("sidebar entry push {push:#x} runs {pushes} times"));
   }
   let entry_size = ix
      .func(push)?
      .steps()
      .iter()
      .find_map(|step| {
         match step.op {
            Op::Imm { value, .. } => usize::try_from(value).ok(),
            _ => None,
         }
      })
      .ok_or("sidebar entry push copies no fixed size")?;
   // Flattened, and on arm64 ELF builds the store may trail the address by a
   // few steps (an outlined stub's moves, or loads scheduled between).
   let row_steps = ix.func(row)?.flat_steps();
   let reach = if ix.isa == crate::index::Isa::Aarch64 && ix.image.function_starts.is_none() {
      6
   } else {
      1
   };
   let (click_vtable, click_offset) = row_steps
      .iter()
      .enumerate()
      .filter_map(|(at, step)| {
         let Op::Addr { reg, target } = step.op else {
            return None;
         };
         let offset = row_steps[at + 1..].iter().take(reach).find_map(|next| {
            match next.op.stored_reg() {
               Some((stored, offset, 8)) if stored == reg => Some(Some(offset)),
               _ if next.writes & (1 << reg) != 0 => Some(None),
               _ => None,
            }
         })??;
         Some((target, usize::try_from(offset).ok()?))
      })
      .next_back()
      .ok_or("Log Out's row stores no click vtable")?;
   if click_offset < 8 || click_offset + 8 > entry_size {
      return Err(format!(
         "Log Out's click at {click_offset:#x} is outside its row"
      ));
   }
   let page = isize::try_from(page_offset).map_err(|err| err.to_string())?;
   // Windows stores a drop flag between the load and the call, Linux doesn't.
   // Linux may widen the load to the page's whole dword, straight into the
   // first argument.
   let first = ix.isa.arg(0);
   let selected = steps
      .iter()
      .enumerate()
      .filter(|(_, step)| {
         matches!(
            step.op,
            Op::Load { offset, size: 1, .. } if offset == page
         ) || matches!(
            step.op,
            Op::Load { reg, offset, size: 4, .. } if offset == page && Some(reg) == first
         )
      })
      .filter_map(|(at, _)| {
         steps.get(at + 1..at + 3)?.iter().find_map(|step| {
            match step.op {
               Op::Call(target) => Some((target, call_slot(ix, step), step.at)),
               _ => None,
            }
         })
      })
      .collect::<Vec<_>>();
   let (selected, selected_via, selected_call) = one("sidebar selected-page map", &selected)?;
   Ok(SidebarRows {
      sidebar: sidebar.start,
      selected,
      selected_via,
      selected_call,
      push,
      entry_size,
      row,
      click_vtable,
      click_offset,
      gear: mana_icon(ix, "Settings")?,
   })
}

/// What the settings pane's title names the active page with.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SettingsTitle {
   /// The page-name function, `fn(page) -> Text` through the return slot,
   /// [`SettingsLabel::func`].
   pub name:      usize,
   /// `fn(key, key_len, hash, &Default) -> Text` through the return slot,
   /// which the page-name function ends in. `Default` is the English text,
   /// `{ptr, len, 1, 0}`.
   pub translate: usize,
   /// The pane header's render, `fn(state, ..) -> Header` through the return
   /// slot on Windows and macOS, a closure Linux inlines it into.
   pub header:    usize,
   /// The header's call of the page-name function for the title.
   pub call:      usize,
}

/// The page-name function, the translate call each page's case shares (right
/// after the key's hash is loaded), and the header that names the active
/// page.
///
/// The header is the one caller besides the sidebar that reads the active
/// page byte as it opens. It names the page's parent, then the page, so its
/// last call of the page-name function is the title's.
pub fn settings_title(ix: &Index<'_>, page_offset: usize) -> Found<SettingsTitle> {
   const SETUP: usize = 8;
   const HEAD: usize = 40;

   let name = ix.func(settings_label(ix)?.func)?;
   let hash = ix.isa.slot_arg(2).ok_or("no hash register")?;
   let steps = name.steps();
   let translate = steps
      .iter()
      .enumerate()
      .filter_map(|(at, step)| {
         let Op::Call(target) = step.op else {
            return None;
         };
         steps[at.saturating_sub(SETUP)..at]
            .iter()
            .any(|step| matches!(step.op, Op::Imm { reg, .. } if reg == hash))
            .then_some(target)
      })
      .next_back()
      .ok_or("the page-name function translates nothing")?;

   let sidebar = ix.literal(b"us-logout").one("us-logout load")?.start;
   let page = isize::try_from(page_offset).map_err(|err| err.to_string())?;
   // ELF builds call it through a pointer slot.
   let through = ix
      .image
      .pointers_to(name.start)
      .into_iter()
      .flat_map(|slot| ix.loads_of(slot..slot + 1))
      .map(|(site, _)| site);
   let mut headers = ix
      .call_sites(name.start)
      .into_iter()
      .chain(through)
      .filter_map(|site| ix.function(site))
      .filter(|func| func.start != sidebar)
      .filter(|func| {
         func
            .head(HEAD)
            .iter()
            .any(|step| matches!(step.op, Op::Load { offset, size: 1, .. } if offset == page))
      })
      .map(|func| func.start)
      .collect::<Vec<_>>();
   headers.sort_unstable();
   headers.dedup();
   let header = ix.func(one("settings header", &headers)?)?;
   let call = header
      .steps()
      .iter()
      .filter(|step| step.op == Op::Call(name.start))
      .map(|step| step.at)
      .next_back()
      .ok_or("the settings header never names its page")?;
   Ok(SettingsTitle {
      name: name.start,
      translate,
      header: header.start,
      call,
   })
}

/// A Mana icon's number, by its name in a table of every icon's name, in
/// order. Windows has the enum's `Debug` tables (each name's length, then a
/// pointer to each); Linux has an array of records that start with the name's
/// pointer and length. Names may share bytes with other strings, so Log Out's
/// `DoorExit` finds the table by a pointer to it that sits in one.
fn mana_icon(ix: &Index<'_>, name: &str) -> Found<u16> {
   const ANCHOR: &[u8] = b"DoorExit";
   const LONGEST: usize = 64;
   const RECORD: usize = 0x100;

   let image = ix.image;
   let width = size_of::<usize>();
   let points = |slot: usize| {
      image
         .read_usize(slot)
         .is_some_and(|at| image.read(at, 1).is_some())
   };
   let text = |at: Option<usize>, len: Option<usize>| image.read(at?, len?);
   // Record tables are `(ptr, len)` pairs the index already holds; only
   // Windows' split tables need the whole-image scans.
   let records = ix
      .strs()
      .iter()
      .filter(|slot| slot.len == ANCHOR.len() && image.read(slot.ptr, slot.len) == Some(ANCHOR))
      .map(|slot| slot.slot);
   let anywhere = std::iter::once(ANCHOR)
      .flat_map(|anchor| ix.text(anchor))
      .flat_map(|at| image.pointers_to(at));
   for slot in records.chain(anywhere) {
      // Windows: a run of pointers right after a run of as many lengths.
      let mut start = slot;
      while start.checked_sub(width).is_some_and(points) {
         start -= width;
      }
      let mut end = slot;
      while points(end) {
         end += width;
      }
      let count = (end - start) / width;
      if let Some(lengths) = start.checked_sub(count * width) {
         let name_of = |index: usize| {
            text(
               image.read_usize(start + index * width),
               image.read_usize(lengths + index * width),
            )
         };
         if let Some(index) = named(count, (slot - start) / width, name, name_of) {
            return u16::try_from(index).map_err(|err| err.to_string());
         }
      }
      // Linux: `(ptr, len, ..)` records a fixed stride apart.
      let record = |at: usize| {
         points(at)
            && image
               .read_usize(at + width)
               .is_some_and(|len| (1..=LONGEST).contains(&len))
      };
      for stride in (2 * width..=RECORD).step_by(width) {
         if !record(slot + stride) || !slot.checked_sub(stride).is_some_and(record) {
            continue;
         }
         let mut start = slot;
         while start.checked_sub(stride).is_some_and(record) {
            start -= stride;
         }
         let mut end = slot;
         while record(end) {
            end += stride;
         }
         let name_of = |index: usize| {
            let at = start + index * stride;
            text(image.read_usize(at), image.read_usize(at + width))
         };
         if let Some(index) = named(
            (end - start) / stride,
            (slot - start) / stride,
            name,
            name_of,
         ) {
            return u16::try_from(index).map_err(|err| err.to_string());
         }
      }
   }
   Err(format!("no Mana icon named {name}"))
}

/// The pointer an x86 `call [rip + slot]` goes through.
fn call_slot(ix: &Index<'_>, step: &Step) -> Option<usize> {
   match step.native {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Native::X86(iced_x86::Mnemonic::Call) => {
         let code = ix.image.read(step.at, usize::from(step.len))?;
         let insn = iced_x86::Decoder::with_ip(64, code, step.at as u64, 0).decode();
         insn
            .is_ip_rel_memory_operand()
            .then(|| usize::try_from(insn.ip_rel_memory_address()).ok())
            .flatten()
      },
      _ => None,
   }
}

/// The index of `name` among `count` names, if the one at `found` is the
/// `DoorExit` that found them and there are enough to be every icon.
fn named<'a>(
   count: usize,
   found: usize,
   name: &str,
   name_of: impl Fn(usize) -> Option<&'a [u8]>,
) -> Option<usize> {
   const ICONS: usize = 100;
   if count < ICONS {
      return None;
   }
   if name_of(found)? != b"DoorExit" {
      return None;
   }
   (0..count).find(|&index| name_of(index) == Some(name.as_bytes()))
}

/// Settings' page-changed handler, `fn(state, cx)`.
///
/// It is one of the functions that can panic in
/// `nav/settings/user/host/load.rs`, reads the page byte early, and at least
/// three page setters call it right after storing it.
///
/// The setters a sidebar row runs store and call unconditionally, so clicking
/// the page already shown calls it too.
pub fn settings_page_changed(ix: &Index<'_>, page_offset: usize) -> Found<usize> {
   // The read is instruction 20 on Win64, after the frame and two field loads.
   const HEAD: usize = 40;
   // Setters run one or two calls between the store and the handler's call.
   const REACH: usize = 16;
   const SETTERS: usize = 3;

   let offset = isize::try_from(page_offset).map_err(|err| err.to_string())?;
   let reads = |steps: &[Step]| {
      steps
         .iter()
         .any(|step| matches!(step.op, Op::Load { offset: at, size: 1, .. } if at == offset))
   };
   let stores =
      |step: &Step| matches!(step.op, Op::Store { offset: at, size: 1, .. } if at == offset);
   let found = ix
      .source("settings/user/host/load.rs")
      .into_iter()
      .filter(|handler| reads(&handler.head(HEAD)))
      .filter(|handler| {
         let setters = handler.callers().into_iter().filter(|setter| {
            let steps = setter.steps();
            setter
               .calls_at()
               .into_iter()
               .filter(|&(_, callee)| callee == handler.start)
               .any(|(site, _)| {
                  let call = steps.partition_point(|step| step.at < site);
                  steps[call.saturating_sub(REACH)..call].iter().any(stores)
               })
         });
         setters.count() >= SETTERS
      })
      .map(|handler| handler.start)
      .collect::<Vec<_>>();
   one("settings page-changed handler", &found)
}

/// The client's own message deletes, found from the `MESSAGE_DELETE` handler.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OptimisticDelete {
   /// The one other caller of callees only the handler and it call.
   pub sibling: usize,
   /// `Store::delete_message`, which the message menu runs, and its copies
   /// for other message maps, sorted.
   pub deletes: Vec<usize>,
}

/// `Store::delete_message` and its sibling, from the `MESSAGE_DELETE` handler.
///
/// A sibling shares at least 4 callees with the handler, one of them called
/// by nothing else. The delete takes the sibling's lock, its first call, and
/// then removes through the handler's map delete, the shared callee with the
/// fewest callers beyond two, right after the call the sibling makes before
/// it. Other siblings exist, and only the store's has such a delete. A copy
/// makes the same calls but one, its map's removal (every build seen has one).
pub fn optimistic_delete<'ix>(ix: &'ix Index<'ix>, handler: Func<'ix>) -> Found<OptimisticDelete> {
   const SHARED: usize = 4;
   // The map deletes have under 10 call sites on every build seen, allocators
   // and drops hundreds of thousands, whose caller sets cost 110 ms on macOS.
   const MANY: usize = 64;

   let mut shared = handler.calls();
   shared.sort_unstable();
   shared.dedup();
   let parents = shared
      .iter()
      .filter(|&&callee| ix.call_sites(callee).len() <= MANY)
      .filter_map(|&callee| Some((callee, ix.func(callee).ok()?.callers())))
      .collect::<HashMap<_, _>>();
   let mut siblings = parents
      .values()
      .filter(|callers| {
         callers.len() == 2 && callers.iter().any(|func| func.start == handler.start)
      })
      .flat_map(|callers| {
         callers
            .iter()
            .map(|func| func.start)
            .filter(|&start| start != handler.start)
      })
      .collect::<Vec<_>>();
   siblings.sort_unstable();
   siblings.dedup();

   let delete_beside = |sibling: Func<'ix>| -> Option<Vec<usize>> {
      let calls = sibling.calls();
      let overlap = shared
         .iter()
         .filter(|callee| calls.contains(callee))
         .count();
      if overlap < SHARED {
         return None;
      }
      let lock = *calls.first()?;
      let (remove, callers) = calls
         .iter()
         .filter_map(|callee| Some((*callee, parents.get(callee)?)))
         .filter(|(_, callers)| callers.len() > 2)
         .min_by_key(|(_, callers)| callers.len())?;
      let before_remove = |calls: &[usize]| {
         let until = calls.iter().position(|&callee| callee == remove)?;
         calls[..until].last().copied()
      };
      let last = before_remove(&calls)?;
      let deletes = callers
         .iter()
         .filter(|func| func.start != handler.start && func.start != sibling.start)
         .filter(|func| {
            let calls = func.calls();
            calls.first() == Some(&lock) && before_remove(&calls) == Some(last)
         })
         .map(|func| func.start)
         .collect::<Vec<_>>();
      (!deletes.is_empty()).then_some(deletes)
   };
   let found = siblings
      .iter()
      .filter_map(|&start| {
         let sibling = ix.func(start).ok()?;
         Some((start, delete_beside(sibling)?))
      })
      .collect::<Vec<_>>();
   let (sibling, deletes) = match found.as_slice() {
      [(sibling, deletes)] => (*sibling, deletes),
      _ => {
         return Err(format!(
            "optimistic delete sibling matched {} times",
            found.len()
         ));
      },
   };
   let delete = ix.func(one("optimistic delete", deletes)?)?;
   let calls = delete.calls();
   let anchor = calls
      .iter()
      .min_by_key(|&&callee| ix.call_sites(callee).len())
      .ok_or("optimistic delete calls nothing")?;
   let mut deletes = ix
      .func(*anchor)?
      .callers()
      .iter()
      .filter(|func| {
         let copy = func.calls();
         copy.len() == calls.len()
            && copy
               .iter()
               .zip(&calls)
               .filter(|(copied, called)| copied != called)
               .count()
               <= 1
      })
      .map(|func| func.start)
      .collect::<Vec<_>>();
   deletes.sort_unstable();
   deletes.dedup();
   Ok(OptimisticDelete { sibling, deletes })
}

/// [`optimistic_delete`] from the gateway registry's `MESSAGE_DELETE` entry.
pub fn optimistic_delete_of<'ix>(ix: &'ix Index<'ix>) -> Found<OptimisticDelete> {
   let registry = ix.registry()?;
   let id = registry.id("MESSAGE_DELETE").ok_or("no MESSAGE_DELETE")?;
   optimistic_delete(ix, ix.func(registry.entry(id).handler)?)
}
