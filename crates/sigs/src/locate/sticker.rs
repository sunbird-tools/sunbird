//! The chat pane's sticker send, where `phoenix.messages.onStickerPick` sees
//! a sticker the picker or autocomplete chose, and the picker's tests that
//! keep a sticker needing Nitro from reaching it.

use std::collections::BTreeSet;

use phoenix_hook::arm64::Insn;

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
   locate::CodePatch,
};

/// The picker item's flag that the sticker needs Nitro, and the one that
/// leaves a press unanswered.
const NEEDS_NITRO: isize = 0x58;
const NO_SELECT: isize = 0x5A;

/// `ChatPaneView::chat_send_sticker_now`, `fn(&mut ChatPaneView, sticker,
/// bool, bool, ...)`, which every sticker choice goes through.
///
/// Its async block's span is entered by one function, called only by the
/// entity update that runs it, called only by the send itself.
pub fn sticker_send(ix: &Index<'_>) -> Found<usize> {
   let span = ix
      .literal(b"::chat_send_sticker_now::{{closure}}")
      .one("sticker send span")?;
   let update = span.callers().one("sticker send update")?;
   update.callers().one("sticker send").map(|func| func.start)
}

/// The two tests in the sticker picker's item press that keep a sticker
/// needing Nitro from the send.
///
/// One shows the Nitro ad, the next drops the press. Patched, the first
/// always falls through to the second, and the second never skips the select.
///
/// The press is called from a `sticker_picker.rs` closure. It reads both
/// flags as bytes of the item, and the first test's branch lands on the
/// second test:
///
/// - x86-64 has `cmp byte [item+58h], 0; je second` and `cmp byte [item+5Ah],
///   0; jne skip`. A compare with 0 clears OF, so `jno` always jumps and `jo`
///   never does, each one condition nibble away.
/// - arm64 has `ldrb w, [item, #0x58]; tbz w, #0, second` and `ldrb w, [item,
///   #0x5a]; tbnz w, #0, skip`, rewritten to `b second` and `nop`.
pub fn locked_gates(ix: &Index<'_>) -> Found<[CodePatch; 2]> {
   let mut seen = BTreeSet::new();
   let mut found = Vec::new();
   for closure in ix.source("sticker_picker.rs") {
      for callee in closure.calls() {
         if seen.insert(callee)
            && let Ok(press) = ix.func(callee)
            && let Some(gates) = gates(&press)
         {
            found.push(gates);
         }
      }
   }
   let [needs_nitro, no_select] = crate::one("sticker picker gates", &found)?;
   match ix.isa {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Isa::X86_64 { .. } => {
         Ok([
            x86_cond(ix, needs_nitro, 0x1)?,
            x86_cond(ix, no_select, 0x0)?,
         ])
      },
      Isa::Aarch64 => {
         let Op::Branch(second) = needs_nitro.op else {
            return Err(format!("{:#x} is no branch", needs_nitro.at));
         };
         Ok([
            CodePatch::at(
               ix,
               needs_nitro.at,
               &arm64_b(needs_nitro.at, second)?.to_le_bytes(),
            )?,
            CodePatch::at(ix, no_select.at, &ARM64_NOP.to_le_bytes())?,
         ])
      },
      Isa::Thumb2 => Err("Desktop Phoenix has no Thumb-2 build".into()),
   }
}

const ARM64_NOP: u32 = 0xD503_201F;

/// The branches after the press's two flag tests, when `press` has them.
fn gates(press: &Func<'_>) -> Option<[Step; 2]> {
   let steps = press.steps();
   let tested = |at: usize, offset: isize| -> Option<(Step, Option<Reg>)> {
      let branch = *steps.get(at + 1)?;
      let reg = match steps[at].op {
         Op::CmpMem {
            offset: off,
            size: 1,
            value: 0,
            ..
         } if off == offset => None,
         Op::Load {
            reg,
            offset: off,
            size: 1,
            ..
         } if off == offset => Some(reg),
         _ => return None,
      };
      Some((branch, reg))
   };
   (0..steps.len()).find_map(|at| {
      let (needs_nitro, first) = tested(at, NEEDS_NITRO)?;
      let Op::Branch(second) = needs_nitro.op else {
         return None;
      };
      let next = steps.iter().position(|step| step.at == second)?;
      let (no_select, again) = tested(next, NO_SELECT)?;
      (branches_on(needs_nitro, first, false) && branches_on(no_select, again, true))
         .then_some([needs_nitro, no_select])
   })
}

/// Whether `branch` jumps on the tested flag being zero, or on it being set
/// when `set`. x86-64 tests flags from the compare, arm64 bit 0 of `reg`.
fn branches_on(branch: Step, reg: Option<Reg>, set: bool) -> bool {
   match (branch.native, reg) {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      (Native::X86(mnemonic), None) => {
         use iced_x86::Mnemonic;
         mnemonic == if set { Mnemonic::Jne } else { Mnemonic::Je }
      },
      (
         Native::Arm64(Insn::Tbz {
            rt,
            bit: 0,
            nonzero,
            ..
         }),
         Some(reg),
      ) => rt == reg && nonzero == set,
      _ => false,
   }
}

/// The x86-64 conditional jump at `branch`, its condition nibble set to `cond`,
/// in a short `7x` or near `0F 8x` encoding.
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
fn x86_cond(ix: &Index<'_>, branch: Step, cond: u8) -> Found<CodePatch> {
   match ix.image.read(branch.at, 2) {
      Some(&[0x0F, near]) if near & 0xF0 == 0x80 => {
         CodePatch::at(ix, branch.at + 1, &[0x80 | cond])
      },
      Some(&[short, _]) if short & 0xF0 == 0x70 => CodePatch::at(ix, branch.at, &[0x70 | cond]),
      _ => Err(format!("{:#x} is no conditional jump", branch.at)),
   }
}

/// `b target` at `site`.
fn arm64_b(site: usize, target: usize) -> Found<u32> {
   const B: u32 = 0x1400_0000;
   const REACH: isize = 1 << 27;
   let delta = target.wrapping_sub(site).cast_signed();
   if !(-REACH..REACH).contains(&delta) || delta % 4 != 0 {
      return Err(format!(
         "{target:#x} is out of a branch's reach from {site:#x}"
      ));
   }
   let offset = u32::try_from((delta >> 2) & 0x03FF_FFFF).map_err(|err| err.to_string())?;
   Ok(B | offset)
}
