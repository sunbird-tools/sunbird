//! Desktop `ManaTheme` global state and `SemanticColors::get_mut` locators.
//!
//! `App::global::<ManaTheme>` is the unique exact loader of the panic type name
//! `"mana_gpui::theme::ManaTheme"`. Its opening `TypeId` load identifies every
//! `ManaTheme` global accessor, and `App::set_global::<ManaTheme>` is the one
//! that allocates `size_of::<ManaTheme>()` bytes and stores the `dyn Any`
//! vtable `(drop_in_place, size, align)`.
//!
//! `SemanticColors::get` and `SemanticColors::get_mut` are the two functions
//! that load the exact token literals `"background-base-low"`,
//! `"background-base-lower"`, and `"background-base-lowest"`; `get` loads a
//! 4-byte `u32` color in its epilogue while `get_mut` returns `&mut u32`
//! directly.

use crate::{
   Found,
   index::{
      Func,
      Index,
      Isa,
      Op,
      Reg,
      Step,
      Value,
      flow::{
         Val,
         walk,
      },
   },
   one,
};

/// Byte offsets inside `ManaTheme` and its `Arc` payloads, decoded from
/// `drop_in_place::<ManaTheme>` and `PartialEq::eq::<ManaTheme>` rather than
/// hardcoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManaThemeLayout {
   /// `ThemeRef` enum (`tag` at +0, `payload` at +8).
   pub theme_ref:        usize,
   /// Active `SemanticColorsRef` enum (`tag` at +0, `payload` at +8).
   pub colors_ref:       usize,
   /// `Option<SemanticColorsRef>` pre-filter colors (`u64::MAX` = `None`).
   pub pre_filter:       usize,
   /// Byte offset of `SemanticColors` inside `ArcInner<Theme>` (tag 1).
   pub arc_theme_colors: usize,
   /// Byte offset of `SemanticColors` inside `ArcInner<SemanticColors>` (tag 2).
   pub arc_colors_data:  usize,
   /// `base_theme: u8` (`0` = Dark, `1` = Light, `2` = Darker, `3` = Midnight).
   pub base_theme:       usize,
   /// `Option<GradientTheme>` discriminant byte offset inside `ManaTheme`.
   pub gradient_tag:     usize,
   /// `Option<GradientTheme>::None` discriminant value.
   pub gradient_none:    u8,
}

/// Everything the desktop theming engine needs to hook `ManaTheme` and mutate
/// its `SemanticColors` table without hardcoding struct sizes or field offsets.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ManaTheme {
   /// `gpui::App::set_global::<ManaTheme>(&mut self, global: ManaTheme)`.
   pub set_global:    usize,
   /// Per-window `ManaTheme` pool sync helper (`fn(*const c_void, *mut c_void) -> bool`)
   /// that copies `App::global::<ManaTheme>` into every window's cached theme slot.
   pub sync_pool:     usize,
   /// Post-`set_global` sync helper (`fn(&mut gpui::App)`) that rebuilds
   /// Discord's GPUI component `Theme` global from the installed `ManaTheme`.
   pub sync_theme:    usize,
   /// `core::ptr::drop_in_place::<ManaTheme>` from the `dyn Any` vtable.
   pub drop_in_place: usize,
   /// `size_of::<ManaTheme>()` read from the `dyn Any` vtable.
   pub size:          usize,
   /// `SemanticColors::get_mut(&mut self, name: &str) -> Option<&mut u32>`.
   pub get_mut:       usize,
   /// `size_of::<SemanticColors>()` decoded from `get_mut`'s max token offset.
   pub colors_size:   usize,
   /// Field and `Arc` payload offsets decoded from `drop_in_place` and `eq`.
   pub layout:        ManaThemeLayout,
}

/// Locates `ManaTheme` global hooks, `SemanticColors::get_mut`, and the
/// `ManaTheme` / `SemanticColors` layout.
pub fn mana_theme(ix: &Index<'_>) -> Found<ManaTheme> {
   let global = locate_global(ix)?;
   let type_ids = candidate_type_ids(ix, global);
   if type_ids.is_empty() {
      return Err(format!("ManaTheme TypeId not found in {global:?}"));
   }
   let (set_global, drop_in_place, size) = locate_set_global(ix, &type_ids)?;
   let sync_theme = locate_sync_theme(ix, set_global)?;
   let sync_pool = locate_sync_pool(ix, set_global, sync_theme, global.start)?;
   let get_mut = locate_get_mut(ix)?;
   let colors_size = decode_colors_size(ix, get_mut)?;
   let (theme_ref, colors_ref, pre_filter) = decode_drop_layout(ix, drop_in_place, size)?;
   let (base_theme, arc_theme_colors, arc_colors_data, gradient_tag, gradient_none) =
      decode_eq_layout(ix, global, size)?;
   Ok(ManaTheme {
      set_global,
      sync_pool,
      sync_theme,
      drop_in_place,
      size,
      get_mut,
      colors_size,
      layout: ManaThemeLayout {
         theme_ref,
         colors_ref,
         pre_filter,
         arc_theme_colors,
         arc_colors_data,
         base_theme,
         gradient_tag,
         gradient_none,
      },
   })
}

/// The unique function that loads the exact `"mana_gpui::theme::ManaTheme"`
/// string address (used in `App::global`'s missing-global panic message).
fn locate_global<'ix>(ix: &'ix Index<'ix>) -> Found<Func<'ix>> {
   const TYPE_NAME: &[u8] = b"mana_gpui::theme::ManaTheme";

   let mut candidates = Vec::new();
   for hit in ix.text(TYPE_NAME) {
      candidates.extend(ix.loaders(hit..hit + 1));
   }
   candidates.sort_unstable_by_key(|func| func.start);
   candidates.dedup_by_key(|func| func.start);
   one("App::global::<ManaTheme>", &candidates)
}

/// Collects all distinct non-code data addresses loaded near the start of
/// `App::global::<ManaTheme>` as candidate `TypeId::of::<ManaTheme>()` slots.
fn candidate_type_ids(ix: &Index<'_>, global: Func<'_>) -> Vec<usize> {
   const HEAD: usize = 32;

   let steps = global.steps();
   let head = &steps[..steps.len().min(HEAD)];
   let mut ids = Vec::new();
   let mut pending_addr: Option<(u8, usize)> = None;
   for step in head {
      match step.op {
         Op::Global {
            target,
            store: false,
         } if !ix.image.is_code(target) => {
            ids.push(target);
         },
         Op::Addr { reg, target } if !ix.image.is_code(target) => {
            if let Some((prev_reg, prev_target)) = pending_addr {
               if prev_reg == reg {
                  // Resolved `adrp + add` pair on arm64: prefer the combined address.
                  ids.push(target);
                  pending_addr = None;
                  continue;
               }
               ids.push(prev_target);
            }
            pending_addr = Some((reg, target));
         },
         Op::Load { .. } | Op::CmpMem { .. } | Op::Branch(_) | Op::Call(_) => {
            if let Some((_, target)) = pending_addr.take() {
               ids.push(target);
            }
         },
         _ => {},
      }
   }
   if let Some((_, target)) = pending_addr {
      ids.push(target);
   }
   ids.sort_unstable();
   ids.dedup();
   ids
}

/// Checks whether `vtable` is a valid `dyn Any` vtable for `ManaTheme`:
/// `[drop_in_place (code), size (0x80..=0x200, 8-aligned), align (8)]`.
fn read_mana_vtable(ix: &Index<'_>, vtable: usize) -> Option<(usize, usize)> {
   if ix.image.is_code(vtable) {
      return None;
   }
   let width = ix.image.pointer_width;
   let drop_fn = ix.image.read_usize(vtable)?;
   let size = ix.image.read_usize(vtable + width)?;
   let align = ix.image.read_usize(vtable + 2 * width)?;
   if ix.image.is_code(drop_fn)
      && (0x80..=0x200).contains(&size)
      && size.is_multiple_of(8)
      && align == 8
   {
      Some((drop_fn, size))
   } else {
      None
   }
}

/// Verifies that `steps` captures `arg1` (the owned `ManaTheme` pointer) near
/// entry and executes an exact `__rust_alloc(size, 8)` call (`arg0 = size`,
/// `arg1 = 8`, followed by `Call`) before storing the `dyn Any` vtable.
fn allocates_and_captures_arg1(ix: &Index<'_>, steps: &[crate::index::Step], size: usize) -> bool {
   let (Some(arg0), Some(arg1)) = (ix.isa.arg(0), ix.isa.arg(1)) else {
      return false;
   };
   let head = &steps[..steps.len().min(20)];
   let captures_arg1 = head.iter().any(|step| {
      matches!(
         step.op,
         Op::Move { src, .. } if src == arg1
      ) || matches!(
         step.op,
         Op::Store {
            value: Value::Reg(reg),
            size: 8,
            ..
         } if reg == arg1
      )
   });
   if !captures_arg1 {
      return false;
   }

   // Match `arg0 = size`, `arg1 = 8` feeding the same `Call` (Rust's
   // `__rust_alloc(size, 8)`), with no intervening `Call`.
   let mut arg0_is_size = false;
   let mut arg1_is_align = false;
   for step in steps {
      match step.op {
         Op::Imm { reg, value } => {
            if reg == arg0 {
               arg0_is_size = value == size as u64;
            } else if reg == arg1 {
               arg1_is_align = value == 8;
            }
         },
         Op::Call(_) => {
            if arg0_is_size && arg1_is_align {
               return true;
            }
            arg0_is_size = false;
            arg1_is_align = false;
         },
         Op::Move { dst, .. } | Op::Load { reg: dst, .. } | Op::Offset { reg: dst, .. } => {
            if dst == arg0 {
               arg0_is_size = false;
            } else if dst == arg1 {
               arg1_is_align = false;
            }
         },
         _ => {},
      }
   }
   false
}

/// Finds `App::set_global::<ManaTheme>` among the functions loading any
/// candidate `TypeId`, verifying that it allocates `size_of::<ManaTheme>()`
/// bytes (`align == 8`), copies `size` bytes from arg 1 into the allocation,
/// notifies global observers (tag 3), and stores the `dyn Any` vtable.
fn locate_set_global(ix: &Index<'_>, type_ids: &[usize]) -> Found<(usize, usize, usize)> {
   let mut found = Vec::new();
   for &type_id in type_ids {
      for func in ix.loaders(type_id..type_id + 1) {
         let steps = func.steps();
         let notifies = steps.iter().any(|step| {
            matches!(
               step.op,
               Op::Imm { value: 3, .. }
                  | Op::Store {
                     value: Value::Imm(3),
                     size: 4,
                     ..
                  }
            )
         });
         if !notifies {
            continue;
         }
         let vtable = steps.iter().find_map(|step| {
            let Op::Addr { target, .. } = step.op else {
               return None;
            };
            let (drop_fn, size) = read_mana_vtable(ix, target)?;
            allocates_and_captures_arg1(ix, &steps, size).then_some((drop_fn, size))
         });
         if let Some((drop_fn, size)) = vtable {
            found.push((func.start, drop_fn, size));
         }
      }
   }
   found.sort_unstable();
   found.dedup();
   one("App::set_global::<ManaTheme>", &found)
}

/// Locates `SemanticColors::get_mut(&mut self, name: &str) -> Option<&mut u32>`.
///
/// Intersecting exact-boundary loaders of three non-prefix canonical Mana
/// tokens yields `[SemanticColors::get, SemanticColors::get_mut]`. `get` loads
/// a 4-byte `u32` color from `self` on its return path, whereas `get_mut`
/// computes `&mut u32` via pointer arithmetic without a 4-byte load.
fn locate_get_mut(ix: &Index<'_>) -> Found<usize> {
   // Three non-prefix canonical Mana tokens so substring hits cannot alias
   // another token literal in Rust's non-NUL-terminated rodata pool.
   const TOKENS: [&[u8]; 3] = [
      b"background-base-lowest",
      b"background-brand",
      b"text-default",
   ];

   let exact_loaders = |needle: &[u8]| -> Vec<usize> {
      let mut starts = Vec::new();
      for hit in ix.text(needle) {
         starts.extend(ix.loaders(hit..hit + 1).into_iter().map(|func| func.start));
      }
      starts.sort_unstable();
      starts.dedup();
      starts
   };

   let mut common = exact_loaders(TOKENS[0]);
   for token in &TOKENS[1..] {
      let next = exact_loaders(token);
      common.retain(|start| next.binary_search(start).is_ok());
   }

   // Count 4-byte `u32` loads across the full function (excluding string
   // comparison / length checks in the prefix is unnecessary because `get` and
   // `get_mut` share the exact same match tree, so `get` has strictly more
   // 4-byte color loads than `get_mut`, or `get_mut` has zero).
   let mut scored: Vec<(usize, usize)> = common
      .into_iter()
      .filter_map(|start| {
         let func = ix.func(start).ok()?;
         let u32_loads = func
            .steps()
            .iter()
            .filter(|step| matches!(step.op, Op::Load { size: 4, .. }))
            .count();
         Some((u32_loads, start))
      })
      .collect();
   scored.sort_unstable();

   match scored.as_slice() {
      [(0, start)] => Ok(*start),
      [(lo, start), (hi, _)] if lo < hi => Ok(*start),
      _ => {
         let addrs: Vec<usize> = scored.into_iter().map(|(_, s)| s).collect();
         Err(format!(
            "SemanticColors::get_mut could not be distinguished among {addrs:#x?}"
         ))
      },
   }
}

/// Decodes `size_of::<SemanticColors>()` from the largest 4-byte-aligned field
/// offset added to `self` inside `SemanticColors::get_mut`.
fn decode_colors_size(ix: &Index<'_>, get_mut: usize) -> Found<usize> {
   let arg0 = ix
      .isa
      .arg(0)
      .ok_or_else(|| "ISA has no arg0 register".to_owned())?;
   let func = ix.func(get_mut)?;
   let steps = func.steps();

   let mut self_regs = vec![arg0];
   for step in steps.iter().take(20) {
      if let Op::Move { dst, src } = step.op
         && src == arg0
         && !self_regs.contains(&dst)
      {
         self_regs.push(dst);
      }
   }

   let max_off = steps
      .iter()
      .filter_map(|step| {
         let Op::Offset { base, offset, .. } = step.op else {
            return None;
         };
         let off = usize::try_from(offset).ok()?;
         (self_regs.contains(&base) && (4..0x10_000).contains(&off) && off.is_multiple_of(4))
            .then_some(off)
      })
      .max()
      .filter(|&off| off >= 0x400)
      .ok_or_else(|| format!("SemanticColors max token offset not found in {get_mut:#x}"))?;

   Ok((max_off + 4).next_multiple_of(8))
}

/// Tracks registers and stack spill slots holding `arg0` through a small
/// function.
fn collect_self_regs(
   steps: &[crate::index::Step],
   arg0: u8,
) -> (Vec<u8>, Vec<(u8, isize)>) {
   let mut regs = vec![arg0];
   let mut spills = Vec::new();
   for step in steps {
      match step.op {
         Op::Move { dst, src } if regs.contains(&src) && !regs.contains(&dst) => {
            regs.push(dst);
         },
         Op::Store {
            value: Value::Reg(src),
            base,
            offset,
            size: 8,
         } if regs.contains(&src) => {
            let slot = (base, offset);
            if !spills.contains(&slot) {
               spills.push(slot);
            }
         },
         Op::Load {
            reg,
            base,
            offset,
            size: 8,
         } if spills.contains(&(base, offset)) && !regs.contains(&reg) => {
            regs.push(reg);
         },
         _ => {},
      }
   }
   (regs, spills)
}

/// Decodes `(theme_ref, colors_ref, pre_filter)` offsets from
/// `drop_in_place::<ManaTheme>` and its tail-called inner drop helper.
fn decode_drop_layout(
   ix: &Index<'_>,
   drop_in_place: usize,
   size: usize,
) -> Found<(usize, usize, usize)> {
   let arg0 = ix
      .isa
      .arg(0)
      .ok_or_else(|| "ISA has no arg0 register".to_owned())?;
   let func = ix.func(drop_in_place)?;
   let steps = func.steps();
   let (self_regs, _) = collect_self_regs(&steps, arg0);

   // `theme_ref`: first 8-byte tag check on `self` before the Arc atomic dec.
   let theme_ref = steps
      .iter()
      .take_while(|step| !matches!(step.op, Op::Atomic { .. }))
      .find_map(|step| {
         let (Op::Load {
            base,
            offset,
            size: 8,
            ..
         }
         | Op::CmpMem {
            base,
            offset,
            size: 8,
            ..
         }) = step.op
         else {
            return None;
         };
         let off = usize::try_from(offset).ok()?;
         (self_regs.contains(&base) && off < size && off.is_multiple_of(8)).then_some(off)
      })
      .ok_or_else(|| format!("ManaTheme::theme_ref offset not found in {drop_in_place:#x}"))?;

   // `colors_ref` + `inner_drop`: offset added to `self` before the tail jump.
   let mut last_off: Option<usize> = None;
   let mut colors_and_inner: Option<(usize, usize)> = None;
   for step in &steps {
      match step.op {
         Op::Offset { base, offset, .. } if self_regs.contains(&base) => {
            if let Ok(off) = usize::try_from(offset)
               && off > theme_ref
               && off < size
               && off.is_multiple_of(8)
            {
               last_off = Some(off);
            }
         },
         Op::Jump(target) if !(func.start..func.end).contains(&target) => {
            if let Some(off) = last_off {
               colors_and_inner = Some((off, target));
               break;
            }
         },
         _ => {},
      }
   }
   let (colors_ref, inner_addr) = colors_and_inner.ok_or_else(|| {
      format!("ManaTheme::colors_ref offset and inner drop not found in {drop_in_place:#x}")
   })?;

   // `pre_filter`: first positive 8-byte load from `inner_self` in `inner_drop`.
   let inner = ix.func(inner_addr)?;
   let inner_steps = inner.steps();
   let (inner_regs, _) = collect_self_regs(&inner_steps, arg0);
   let pre_filter_rel = inner_steps
      .iter()
      .find_map(|step| {
         let Op::Load {
            base,
            offset,
            size: 8,
            ..
         } = step.op
         else {
            return None;
         };
         let off = usize::try_from(offset).ok()?;
         (inner_regs.contains(&base)
            && off > 0
            && colors_ref + off < size
            && off.is_multiple_of(8))
         .then_some(off)
      })
      .ok_or_else(|| format!("ManaTheme::pre_filter offset not found in {inner_addr:#x}"))?;

   Ok((theme_ref, colors_ref, colors_ref + pre_filter_rel))
}

/// Locates the `fn(&mut gpui::App)` sync helper that every caller of
/// `App::set_global::<ManaTheme>` invokes immediately afterwards to rebuild
/// Discord's GPUI component `Theme` global (colors, fonts, shadows) from the
/// newly installed `ManaTheme`.
fn locate_sync_theme(ix: &Index<'_>, set_global: usize) -> Found<usize> {
   let func = ix.func(set_global)?;
   let shadow_loaders: Vec<usize> = ix
      .literal(b"shadow-low")
      .iter()
      .map(|f| f.start)
      .collect();
   let mut next_callees = Vec::new();
   for caller in func.callers() {
      let calls = caller.calls();
      for pair in calls.windows(2) {
         if pair[0] == set_global && shadow_loaders.binary_search(&pair[1]).is_ok() {
            next_callees.push(pair[1]);
         }
      }
   }
   next_callees.sort_unstable();
   next_callees.dedup();
   one("ManaTheme sync_theme", &next_callees)
}

/// Locates the `fn(*const c_void, *mut c_void) -> bool` pool sync helper
/// invoked right after `sync_theme` in `set_global`'s callers, which reads
/// `App::global::<ManaTheme>` and updates every window's cached `ManaTheme`.
fn locate_sync_pool(
   ix: &Index<'_>,
   set_global: usize,
   sync_theme: usize,
   global: usize,
) -> Found<usize> {
   let func = ix.func(set_global)?;
   let mut candidates = Vec::new();
   for caller in func.callers() {
      let calls = caller.calls();
      for (idx, pair) in calls.windows(2).enumerate() {
         if pair[0] == set_global && pair[1] == sync_theme {
            for &callee in calls[idx + 2..].iter().take(3) {
               if let Ok(cf) = ix.func(callee)
                  && cf.calls().contains(&global)
               {
                  candidates.push(callee);
               }
            }
         }
      }
   }
   candidates.sort_unstable();
   candidates.dedup();
   one("ManaTheme sync_pool", &candidates)
}

/// Decodes `(arc_colors_data, arc_theme_colors)` from the tag-2 / tag-1 branch
/// inside `PartialEq::eq::<ManaTheme>`.
fn find_arc_offsets(steps: &[crate::index::Step]) -> Option<(usize, usize)> {
   steps.windows(2).enumerate().find_map(|(idx, pair)| {
      let (Op::Cmp { value: 2, .. }, Op::Branch(target)) = (pair[0].op, pair[1].op) else {
         return None;
      };
      let after_branch = &steps[idx + 2..];
      let tag2_off = after_branch
         .iter()
         .take_while(|step| step.at < target)
         .find_map(|step| {
            let Op::Offset { offset, .. } = step.op else {
               return None;
            };
            let off = usize::try_from(offset).ok()?;
            (off > 0 && off < 0x200 && off.is_multiple_of(8)).then_some(off)
         })?;
      let tag1_off = after_branch
         .iter()
         .skip_while(|step| step.at < target)
         .take(4)
         .find_map(|step| {
            let Op::Offset { offset, .. } = step.op else {
               return None;
            };
            let off = usize::try_from(offset).ok()?;
            (off > 0 && off < 0x200 && off.is_multiple_of(8)).then_some(off)
         })?;
      (tag1_off != tag2_off).then_some((tag2_off, tag1_off))
   })
}

/// Decodes `(gradient_tag, gradient_none)` from the `Option<GradientTheme>`
/// discriminant comparison against `None` inside `PartialEq::eq::<ManaTheme>`.
fn find_gradient_tag(
   steps: &[crate::index::Step],
   arg0: u8,
   arg1: u8,
   size: usize,
) -> Option<(usize, u8)> {
   steps.iter().enumerate().find_map(|(idx, step)| {
      match step.op {
         Op::CmpMem {
            base,
            offset,
            size: 1,
            value,
         } if base == arg0 => {
            let off = usize::try_from(offset).ok()?;
            let none = u8::try_from(value).ok().filter(|&v| v > 0 && v < 16)?;
            if off >= size {
               return None;
            }
            let pair_matches = steps[idx + 1..].iter().take(4).any(|next| {
               matches!(
                  next.op,
                  Op::CmpMem {
                     base: b2,
                     offset: o2,
                     size: 1,
                     value: v2,
                  } if b2 == arg1 && o2 == offset && v2 == value
               )
            });
            pair_matches.then_some((off, none))
         },
         Op::Load {
            reg,
            base,
            offset,
            size: 1,
         } if base == arg0 => {
            let off = usize::try_from(offset).ok()?;
            if off >= size {
               return None;
            }
            let Op::Cmp { reg: cmp_reg, value } = steps.get(idx + 1)?.op else {
               return None;
            };
            let none = u8::try_from(value).ok().filter(|&v| v > 0 && v < 16)?;
            if cmp_reg != reg {
               return None;
            }
            let pair_matches = steps[idx + 2..].windows(2).take(4).any(|pair| {
               matches!(
                  (pair[0].op, pair[1].op),
                  (
                     Op::Load {
                        reg: r2,
                        base: b2,
                        offset: o2,
                        size: 1,
                     },
                     Op::Cmp {
                        reg: c2,
                        value: v2,
                     },
                  ) if b2 == arg1 && o2 == offset && c2 == r2 && v2 == value
               )
            });
            pair_matches.then_some((off, none))
         },
         _ => None,
      }
   })
}

/// Decodes `(base_theme, arc_theme_colors, arc_colors_data, gradient_tag, gradient_none)`
/// from `PartialEq::eq::<ManaTheme>`, called by GPUI's `ManaTheme` global observers.
fn decode_eq_layout(
   ix: &Index<'_>,
   global: Func<'_>,
   size: usize,
) -> Found<(usize, usize, usize, usize, u8)> {
   let arg0 = ix
      .isa
      .arg(0)
      .ok_or_else(|| "ISA has no arg0 register".to_owned())?;
   let arg1 = ix
      .isa
      .arg(1)
      .ok_or_else(|| "ISA has no arg1 register".to_owned())?;

   let mut callees = Vec::new();
   for caller in global.callers() {
      if caller.size() > 1024 {
         continue;
      }
      callees.extend(caller.calls());
   }
   callees.sort_unstable();
   callees.dedup();

   let mut decoded = Vec::new();
   for callee in callees {
      let Ok(func) = ix.func(callee) else {
         continue;
      };
      if func.size() < 64 || func.size() > 1024 {
         continue;
      }
      let steps = func.steps();
      if steps.iter().any(|step| matches!(step.op, Op::Call(_))) {
         continue;
      }
      let Some(base_theme) = steps.iter().take(4).find_map(|step| {
         let Op::Load {
            base,
            offset,
            size: 1,
            ..
         } = step.op
         else {
            return None;
         };
         let off = usize::try_from(offset).ok()?;
         (base == arg0 && off < size).then_some(off)
      }) else {
         continue;
      };
      let Some((arc_colors_data, arc_theme_colors)) = find_arc_offsets(&steps) else {
         continue;
      };
      let Some((gradient_tag, gradient_none)) = find_gradient_tag(&steps, arg0, arg1, size) else {
         continue;
      };
      decoded.push((
         base_theme,
         arc_theme_colors,
         arc_colors_data,
         gradient_tag,
         gradient_none,
      ));
   }

   decoded.sort_unstable();
   decoded.dedup();
   one("ManaTheme::eq layout", &decoded)
}

/// Renderer draw entrypoint and `Scene::quads` `Vec` offset for cross-platform
/// `--radius-scale` corner-radius scaling.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SceneDraw {
   /// `WgpuRenderer::draw` / `D3D` renderer `draw` (x86-64) or
   /// `MetalRenderer::draw` (macOS arm64), taking `(renderer, scene, ...)`.
   pub draw:    usize,
   /// Byte offset of `Vec<Quad>` (`cap`, `ptr`, `len`) inside `gpui::Scene`.
   pub quads:   usize,
   /// Byte offset of `Vec<Shadow>` inside `gpui::Scene`, where the binary
   /// names it in its scene-size panic and confirms the `Shadow` layout.
   pub shadows: Option<usize>,
}

/// Locates the GPUI renderer's `draw(&mut self, scene: &Scene)` and
/// `Scene::quads` across Windows, Linux, and macOS.
pub fn scene_draw(ix: &Index<'_>) -> Found<SceneDraw> {
   match ix.isa {
      Isa::X86_64 { .. } => scene_draw_x64(ix),
      Isa::Aarch64 => scene_draw_arm64(ix),
      Isa::Thumb2 => Err("scene draw locator requires x86-64 or arm64".to_owned()),
   }
}

fn scene_draw_x64(ix: &Index<'_>) -> Found<SceneDraw> {
   const ANCHOR: &[u8] = b"scene too large:";
   let text = one("scene template", &ix.text(ANCHOR))?;
   let formatter = ix
      .loaders(text.saturating_sub(1)..text + 1)
      .one("scene formatter")?;

   let tail = ix
      .image
      .read(text + ANCHOR.len(), 0x100)
      .ok_or("scene template is unmapped")?;
   let tail = &tail[..tail.iter().position(|&b| b == 0).unwrap_or(tail.len())];
   // Rust's `fmt::Arguments` template encodes each `{}` placeholder as `0xC0`
   // followed by a length-prefixed literal piece (`" quads, "`, etc.).
   let names = tail.split(|&b| b == 0xC0).skip(1).collect::<Vec<_>>();
   let piece_idx = |name: &[u8]| {
      names
         .iter()
         .position(|piece| piece.windows(name.len()).any(|w| w == name))
   };
   let quad_idx = piece_idx(b"quads").ok_or("scene template has no quads")?;
   let shadow_idx = piece_idx(b"shadows");

   // Only inspect loads before the template address load inside `formatter`.
   // When `formatter` returns a struct via hidden `sret`, `&Scene` shifts from
   // arg 1 to arg 2 (`rdx` -> `r8` on Win64, `rsi` -> `rdx` on SysV); `draw`
   // itself always receives `&Scene` in arg 1 (`rdx` on Win64, `rsi` on SysV).
   let steps = formatter.steps();
   let template_at = steps
      .iter()
      .find(|step| {
         matches!(
            step.op,
            Op::Addr { target, .. } if target == text || target + 1 == text
         )
      })
      .map_or(formatter.end, |step| step.at);

   let scene_regs: &[u8] = match ix.isa {
      Isa::X86_64 { win64: true } => &[2, 8],
      Isa::X86_64 { win64: false } => &[6, 2],
      _ => &[],
   };
   let mut matched = Vec::new();
   for &scene_reg in scene_regs {
      let lens = steps
         .iter()
         .take_while(|step| step.at < template_at)
         .filter_map(|step| {
            match step.op {
               Op::Load {
                  size: 8,
                  base,
                  offset,
                  ..
               } if base == scene_reg && offset >= 0x10 => usize::try_from(offset).ok(),
               _ => None,
            }
         })
         .collect::<Vec<_>>();
      if lens.len() == names.len()
         && let Some(quads) = lens[quad_idx].checked_sub(0x10)
      {
         matched.push((
            quads,
            shadow_idx.and_then(|idx| lens[idx].checked_sub(0x10)),
         ));
      }
   }
   matched.sort_unstable();
   matched.dedup();
   let (quads, shadows) = one("Scene::quads offset", &matched)?;

   let draw = formatter.callers().one("renderer draw")?;
   // `draw` must read `Scene::quads` `ptr` or `len` through a register or
   // stack slot still holding its `&Scene` argument, and it or one of its
   // direct upload callees (DirectX) must scale quad indices by
   // `size_of::<Quad>()`.
   let ptr_disp = isize::try_from(quads + 8).map_err(|_| "quads offset overflows")?;
   let scene_arg = ix.isa.arg(1).ok_or("renderer draw has no scene argument")?;
   let draw_steps = draw.steps();
   if !reads_arg_field(ix.isa, &draw_steps, scene_arg, &[ptr_disp, ptr_disp + 8]) {
      return Err(format!(
         "renderer draw never reads the scene's quads at +{ptr_disp:#x}"
      ));
   }
   let mut candidates = draw.calls();
   candidates.push(draw.start);
   let strided = candidates.into_iter().any(|addr| {
      let Ok(func) = ix.func(addr) else {
         return false;
      };
      ix.image
         .read(func.start, func.end - func.start)
         .is_some_and(|body| has_x64_quad_stride(body, func.start as u64))
   });
   if !strided {
      return Err("renderer draw never scales quad indices by 160 bytes".to_owned());
   }
   verify_quad_layout(ix)?;
   let shadows = shadows.filter(|&at| {
      isize::try_from(at + 8)
         .is_ok_and(|ptr| reads_arg_field(ix.isa, &draw_steps, scene_arg, &[ptr, ptr + 8]))
         && shadow_layout_confirmed(ix)
   });
   Ok(SceneDraw {
      draw: draw.start,
      quads,
      shadows,
   })
}

/// Whether `steps` load `[arg + disp]` for one of `disps` through a register
/// or stack slot still holding argument `arg` on the fall-through path.
fn reads_arg_field(isa: Isa, steps: &[Step], arg: Reg, disps: &[isize]) -> bool {
   let mut holders = 1u32 << arg;
   let mut slots = Vec::<isize>::new();
   let mut found = false;
   walk(isa, steps, |_, step, regs| {
      let held = |reg: Reg| holders & 1 << reg != 0;
      let slot = |base: Reg, offset: isize| {
         match regs[usize::from(base)] {
            Val::Stack(at) => Some(at + offset),
            _ => None,
         }
      };
      let mut next = holders & !step.writes;
      match step.op {
         Op::Load {
            reg,
            base,
            offset,
            size: 8,
         } => {
            found |= held(base) && disps.contains(&offset);
            if slot(base, offset).is_some_and(|at| slots.contains(&at)) {
               next |= 1 << reg;
            }
         },
         Op::Move { dst, src } if held(src) => next |= 1 << dst,
         Op::Store {
            value: Value::Reg(src),
            base,
            offset,
            size: 8,
         } => {
            if let Some(at) = slot(base, offset) {
               slots.retain(|&kept| kept != at);
               if held(src) {
                  slots.push(at);
               }
            }
         },
         _ => {},
      }
      if matches!(step.op, Op::Call(_)) {
         next &= !isa.caller_saved();
      }
      holders = next;
   });
   found
}

/// Confirms `bounds` at `+8` and `corner_radii` at `+128` in GPUI's `Quad`.
fn verify_quad_layout(ix: &Index<'_>) -> Found<()> {
   let confirmed = layout_confirmed(
      ix,
      b"struct Quad {",
      &[
         "order: u32",
         "border_style: u32",
         "bounds: Bounds",
         "content_mask: Bounds",
         "background: Background",
         "border_color: Hsla",
         "corner_radii: Corners",
         "border_widths: Edges",
      ],
      &[
         "DrawOrder order;",
         "enum BorderStyle border_style;",
         "struct Bounds_ScaledPixels bounds;",
         "struct ContentMask_ScaledPixels content_mask;",
         "struct Background background;",
         "struct Hsla border_color;",
         "struct Corners_ScaledPixels corner_radii;",
         "struct Edges_ScaledPixels border_widths;",
      ],
      &[
         (b"order", 0),
         (b"border_style", 4),
         (b"bounds", 8),
         (b"content_mask", 24),
         (b"background", 40),
         (b"border_color", 112),
         (b"corner_radii", 128),
         (b"border_widths", 144),
      ],
   );
   if confirmed {
      Ok(())
   } else {
      Err("no shader source or reflection confirms the Quad layout".to_owned())
   }
}

/// Whether GPUI's 112-byte `Shadow` keeps `corner_radii` at `+24`,
/// `shape_bounds` at `+72` and `shape_corner_radii` at `+88`.
fn shadow_layout_confirmed(ix: &Index<'_>) -> bool {
   layout_confirmed(
      ix,
      b"struct Shadow {",
      &[
         "order: u32",
         "blur_radius: f32",
         "bounds: Bounds",
         "corner_radii: Corners",
         "content_mask: Bounds",
         "color: Hsla",
         "shape_bounds: Bounds",
         "shape_corner_radii: Corners",
         "inset: u32",
         "pad: u32",
      ],
      &[
         "DrawOrder order;",
         "ScaledPixels blur_radius;",
         "struct Bounds_ScaledPixels bounds;",
         "struct Corners_ScaledPixels corner_radii;",
         "struct ContentMask_ScaledPixels content_mask;",
         "struct Hsla color;",
         "struct Bounds_ScaledPixels shape_bounds;",
         "struct Corners_ScaledPixels shape_corner_radii;",
         "PaddedBool32 inset;",
         "uint32_t pad;",
      ],
      &[
         (b"order", 0),
         (b"blur_radius", 4),
         (b"bounds", 8),
         (b"corner_radii", 24),
         (b"content_mask", 40),
         (b"color", 56),
         (b"shape_bounds", 72),
         (b"shape_corner_radii", 88),
         (b"inset", 104),
         (b"pad", 108),
      ],
   )
}

/// Whether some copy of a shader struct the binary carries lists `wgsl`
/// (Linux source), `metal` (macOS source) or `reflected` (Windows DXBC
/// `(name, offset)` members) field for field.
fn layout_confirmed(
   ix: &Index<'_>,
   header: &[u8],
   wgsl: &[&str],
   metal: &[&str],
   reflected: &[(&[u8], u32)],
) -> bool {
   let source = ix.text(header).into_iter().any(|addr| {
      let Some(bytes) = ix.image.read(addr, 0x400) else {
         return false;
      };
      let text = String::from_utf8_lossy(bytes);
      let Some(end) = text.find('}') else {
         return false;
      };
      let fields = text[..end]
         .lines()
         .skip(1)
         .map(|line| {
            line
               .split("//")
               .next()
               .unwrap_or_default()
               .trim()
               .trim_end_matches(',')
         })
         .filter(|line| !line.is_empty())
         .collect::<Vec<_>>();
      fields == wgsl || fields == metal
   });
   source || reflects_layout(ix, reflected)
}

/// Finds a struct's `$Element` member list in a DXBC `RDEF` chunk, whose
/// 12-byte records are `(name, type, offset)` with names relative to the
/// chunk.
fn reflects_layout(ix: &Index<'_>, fields: &[(&[u8], u32)]) -> bool {
   let word = |bytes: &[u8], at: usize| -> Option<u32> {
      Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
   };
   ix.text(b"RDEF").into_iter().any(|tag| {
      let Some(size) = ix
         .image
         .read(tag + 4, 4)
         .and_then(|bytes| word(bytes, 0))
         .and_then(|size| usize::try_from(size).ok())
         .filter(|&size| size <= 0x1_0000)
      else {
         return false;
      };
      let Some(chunk) = ix.image.read(tag + 8, size) else {
         return false;
      };
      let name = |at: u32| -> Option<&[u8]> {
         let rest = chunk.get(usize::try_from(at).ok()?..)?;
         Some(&rest[..rest.iter().position(|&b| b == 0)?])
      };
      (0..chunk.len()).step_by(4).any(|start| {
         fields.iter().enumerate().all(|(i, &(want, offset))| {
            let at = start + 12 * i;
            word(chunk, at).and_then(name) == Some(want) && word(chunk, at + 8) == Some(offset)
         })
      })
   })
}

fn has_x64_quad_stride(code: &[u8], ip: u64) -> bool {
   use iced_x86::{
      Decoder,
      DecoderOptions,
      Mnemonic,
      OpKind,
      Register,
   };

   let mut decoder = Decoder::with_ip(64, code, ip, DecoderOptions::NONE);
   let mut mul5_dst: Option<(Register, u8)> = None;
   let mut shl5_dst: Option<(Register, u8)> = None;
   while decoder.can_decode() {
      let ins = decoder.decode();
      match ins.mnemonic() {
         Mnemonic::Imul
            if ins.op_count() == 3
               && matches!(
                  ins.op2_kind(),
                  OpKind::Immediate8to64 | OpKind::Immediate32to64 | OpKind::Immediate32
               )
               && ins.immediate(2) == 160 =>
         {
            return true;
         },
         Mnemonic::Lea
            if ins.memory_index_scale() == 4
               && ins.memory_base() != Register::None
               && ins.memory_base() == ins.memory_index()
               && ins.memory_displacement64() == 0 =>
         {
            if shl5_dst.is_some_and(|(reg, _)| ins.memory_base() == reg) {
               return true;
            }
            mul5_dst = Some((ins.op0_register(), 3));
            shl5_dst = None;
            continue;
         },
         Mnemonic::Shl if ins.op1_kind() == OpKind::Immediate8 && ins.immediate8() == 5 => {
            let dst = ins.op0_register();
            if mul5_dst.is_some_and(|(reg, _)| dst == reg) {
               return true;
            }
            shl5_dst = Some((dst, 3));
            mul5_dst = None;
            continue;
         },
         _ => {},
      }
      if let Some((reg, rem)) = mul5_dst {
         mul5_dst = (rem > 1).then_some((reg, rem - 1));
      }
      if let Some((reg, rem)) = shl5_dst {
         shl5_dst = (rem > 1).then_some((reg, rem - 1));
      }
   }
   false
}

fn scene_draw_arm64(ix: &Index<'_>) -> Found<SceneDraw> {
   let exact_loader = |needle: &[u8], label: &str| -> Found<Func<'_>> {
      let mut funcs = Vec::new();
      for hit in ix.text(needle) {
         funcs.extend(ix.loaders(hit..hit + 1));
      }
      funcs.sort_unstable_by_key(|func| func.start);
      funcs.dedup_by_key(|func| func.start);
      one(label, &funcs)
   };

   let draw = exact_loader(b"draw() called on headless renderer", "MetalRenderer::draw")?;
   let draw_scene = exact_loader(b"allocating depth texture", "MetalRenderer::draw_scene")?;
   if !draw.calls().contains(&draw_scene.start) {
      return Err("MetalRenderer::draw does not call draw_scene".to_owned());
   }

   let mut candidates = draw_scene.calls();
   candidates.push(draw_scene.start);
   candidates.sort_unstable();
   candidates.dedup();

   let mut quad_offsets = Vec::new();
   for addr in candidates {
      let Ok(func) = ix.func(addr) else {
         continue;
      };
      if let Some(quads) = find_arm64_quads_offset(ix, func) {
         quad_offsets.push(quads);
      }
   }
   quad_offsets.sort_unstable();
   quad_offsets.dedup();
   let quads = one("Scene::quads offset", &quad_offsets)?;
   verify_quad_layout(ix)?;
   Ok(SceneDraw {
      draw: draw.start,
      quads,
      shadows: None,
   })
}

/// Decodes `Scene::quads` from `upload_scene_buffers` (or inlined `draw_scene`)
/// using a small dataflow window around `(ldr ptr [xB, #off], ldr len [xB, #off+8])`
/// followed by `add xL, xL, xL, lsl #2` (`* 5`) and `lsl _, xL, #5` (`* 32`,
/// total `len * 160 == len * size_of::<Quad>()`).
fn find_arm64_quads_offset(ix: &Index<'_>, func: Func<'_>) -> Option<usize> {
   let bytes = ix.image.read(func.start, func.end - func.start)?;
   let words = bytes
      .as_chunks::<4>()
      .0
      .iter()
      .map(|&chunk| u32::from_le_bytes(chunk))
      .collect::<Vec<_>>();
   for (i, &ldr_ptr) in words.iter().enumerate() {
      if (ldr_ptr & 0xFFC0_0000) != 0xF940_0000 {
         continue;
      }
      let base = (ldr_ptr >> 5) & 0x1F;
      let off_ptr = (((ldr_ptr >> 10) & 0xFFF) as usize) * 8;
      if off_ptr < 8 {
         continue;
      }
      let tail = &words[i + 1..(i + 9).min(words.len())];
      let Some((j, &ldr_len)) = tail.iter().enumerate().find(|&(_, &w)| {
         (w & 0xFFC0_0000) == 0xF940_0000
            && ((w >> 5) & 0x1F) == base
            && (((w >> 10) & 0xFFF) as usize) * 8 == off_ptr + 8
      }) else {
         continue;
      };
      let len_reg = ldr_len & 0x1F;
      let after_len = &tail[j + 1..];
      let want_mul5 = 0x8B00_0800 | (len_reg << 16) | (len_reg << 5) | len_reg;
      let Some((k, _)) = after_len.iter().enumerate().find(|&(_, &w)| w == want_mul5) else {
         continue;
      };
      let want_shl5_mask = 0xD37B_E800 | (len_reg << 5);
      if after_len[k + 1..]
         .iter()
         .any(|&w| (w & 0xFFFF_FFE0) == want_shl5_mask)
      {
         return Some(off_ptr - 8);
      }
   }
   None
}
