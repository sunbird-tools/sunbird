//! Spotify behaviors `phoenix.spotify` turns off, each a one-instruction
//! rewrite of `libdiscord/crates/ops-spotify` and the presence builder.

use phoenix_hook::pattern::Pattern;

use crate::{
   Found,
   index::{
      Func,
      Index,
      Isa,
      Op,
      Reg,
   },
   locate::CodePatch,
};

const NO_THUMB: &str = "Desktop Phoenix has no Thumb-2 build";

/// The entry of the function that pauses Spotify after 30 s of talking in
/// voice, which logs `spotify: playback auto paused`, made to return at once.
///
/// Every caller is its timer's poll, which passes the address of its own
/// `Arc` and reaches that slot again after the call to drop it, so the callee
/// only borrows and returning early leaves nothing behind. The check requires
/// that shape at every call site.
pub fn auto_pause(ix: &Index<'_>) -> Found<CodePatch> {
   const ARM64_RET: [u8; 4] = 0xD65F_03C0_u32.to_le_bytes();
   // Room for a `nop` after a call that may unwind, and for an inlined drop's
   // first load.
   const REACH: usize = 3;

   let func = ix
      .literal(b"spotify: playback auto paused")
      .one("spotify auto pause")?;
   let this = ix.isa.arg(0).ok_or("no first argument register")?;
   let sites = ix.call_sites(func.start);
   if sites.is_empty() {
      return Err(format!("spotify auto pause at {func:?} has no callers"));
   }
   for site in sites {
      let caller = ix
         .function(site)
         .ok_or_else(|| format!("spotify auto pause call {site:#x} is outside a function"))?;
      let steps = caller.steps();
      let at = steps
         .iter()
         .position(|step| step.at == site)
         .ok_or_else(|| format!("spotify auto pause call {site:#x} does not decode"))?;
      let Some(Op::Offset { reg, base, offset }) = at.checked_sub(1).map(|lend| steps[lend].op)
      else {
         return Err(format!("spotify auto pause call {site:#x} passes no local's address"));
      };
      let kept = steps[at + 1..]
         .iter()
         .take(REACH)
         .any(|step| takes_slot(step.op, base, offset) || outlined_takes(ix, step.op, base, offset));
      if reg != this || !kept {
         return Err(format!("spotify auto pause call {site:#x} gives its argument away"));
      }
   }
   match ix.isa {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Isa::X86_64 { .. } => CodePatch::at(ix, func.start, &[0xC3]),
      Isa::Aarch64 => CodePatch::at(ix, func.start, &ARM64_RET),
      Isa::Thumb2 => Err(NO_THUMB.into()),
   }
}

const fn takes_slot(op: Op, base: Reg, offset: isize) -> bool {
   matches!(
      op,
      Op::Offset { base: b, offset: o, .. } | Op::Load { base: b, offset: o, .. }
         if b == base && o == offset
   )
}

/// Whether `op` calls an arm64 outlined stub (a few frameless instructions
/// ending in `b` or `ret`) that takes the slot, as both callers' drops do.
fn outlined_takes(ix: &Index<'_>, op: Op, base: Reg, offset: isize) -> bool {
   const MOST: usize = 6;
   let Op::Call(target) = op else {
      return false;
   };
   ix.isa == Isa::Aarch64
      && ix.func(target).is_ok_and(|stub| {
         stub
            .steps()
            .iter()
            .take(MOST)
            .take_while(|step| !matches!(step.op, Op::Jump(_) | Op::Ret | Op::Call(_)))
            .any(|step| takes_slot(step.op, base, offset))
      })
}

/// The idle test that hides the Spotify activity while you are idle.
///
/// The presence builder returns its activity through a hidden slot and takes
/// the user's state first (`idle: bool` at +0, `idle_since: u64` at +8) and
/// whether Spotify is shown fifth. It keeps Spotify activities only when
/// `idle_since == 0 && shown`:
///
/// - x86-64 computes `test since, since; sete r8; and r8, shown`, with `shown`
///   in `r9b` on System V or the second stack argument on Win64. Turning the
///   `test` (`85`) into `cmp` (`39`) on the same register sets ZF.
/// - arm64 computes `cmp since, #0; csel wD, wzr, w4, ne; strb wD, [sp, #n]`.
///   `mov wD, w4` in place of the `csel` sets no flags either.
///
/// Either way the flag becomes `shown` alone and presence keeps its `since`.
pub fn idle_activity(ix: &Index<'_>) -> Found<CodePatch> {
   let state = arg_after_return_slot(ix.isa, 0).ok_or("no state argument register")?;
   let shown = arg_after_return_slot(ix.isa, 4);
   let candidates = match ix.isa {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Isa::X86_64 { win64 } => {
         ix.image
            .scan(Pattern::new("?? 85 ?? 0F 94 ??"))
            .into_iter()
            .filter(|&site| x86::tests_since_and_shown(ix, site, win64, shown))
            .collect::<Vec<_>>()
      },
      Isa::Aarch64 => {
         let shown = shown.ok_or("no fifth argument register")?;
         ix.image
            .scan(Pattern::new("?? ?? 00 F1 ?? 13 ?? 1A ?? ?? ?? 39"))
            .into_iter()
            .filter(|&site| arm64::tests_since_and_shown(ix, site, shown))
            .collect::<Vec<_>>()
      },
      Isa::Thumb2 => return Err(NO_THUMB.into()),
   };
   let sites = candidates
      .into_iter()
      .filter(|&site| ix.function(site).is_some_and(|func| reads_idle_state(&func, site, state)))
      .collect::<Vec<_>>();
   let site = crate::one("spotify idle flag", &sites)?;
   match ix.isa {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Isa::X86_64 { .. } => CodePatch::at(ix, site + 1, &[0x39]),
      Isa::Thumb2 => Err(NO_THUMB.into()),
      Isa::Aarch64 => {
         let csel = ix
            .image
            .read_u32(site + 4)
            .ok_or_else(|| format!("{:#x} is unmapped", site + 4))?;
         CodePatch::at(ix, site + 4, &arm64::mov_from_shown(csel).to_le_bytes())
      },
   }
}

/// Integer argument `index` of a function returning through a hidden slot,
/// which x86-64 passes first and arm64 in `x8`.
const fn arg_after_return_slot(isa: Isa, index: usize) -> Option<Reg> {
   match isa {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Isa::X86_64 { .. } => isa.arg(index + 1),
      Isa::Aarch64 | Isa::Thumb2 => isa.arg(index),
   }
}

/// Whether `func` reads the state's `idle` tag (a byte on x86-64, a word on
/// arm64) and loads its `idle_since` before `site`.
fn reads_idle_state(func: &Func<'_>, site: usize, state: Reg) -> bool {
   const IDLE: isize = 0;
   const SINCE: isize = 8;
   let steps = func.steps();
   let before = steps.iter().take_while(|step| step.at < site);
   let reads_idle = before.clone().any(|step| {
      matches!(
         step.op,
         Op::CmpMem { base, offset: IDLE, size: 1 | 4, value: 0 }
            | Op::Load { base, offset: IDLE, size: 1 | 4, .. } if base == state
      )
   });
   let loads_since = before.clone().any(|step| {
      matches!(step.op, Op::Load { base, offset: SINCE, size: 8, .. } if base == state)
   });
   reads_idle && loads_since
}

mod arm64 {
   use crate::index::{
      Index,
      Reg,
   };

   const CSEL_W: u32 = 0x1A80_0000;
   const NE: u32 = 1;
   const ZR: u32 = 31;

   const fn field(word: u32, at: u32) -> u32 {
      (word >> at) & 0x1F
   }

   /// `cmp xS, #0; csel wD, wzr, w(shown), ne; strb wD, [sp, #n]`.
   pub(super) fn tests_since_and_shown(ix: &Index<'_>, site: usize, shown: Reg) -> bool {
      if !site.is_multiple_of(4) {
         return false;
      }
      let [Some(cmp), Some(csel), Some(strb)] = [0, 4, 8].map(|off| ix.image.read_u32(site + off))
      else {
         return false;
      };
      let flag = field(csel, 0);
      let csel_shown = CSEL_W | u32::from(shown) << 16 | NE << 12 | ZR << 5;
      cmp & 0xFFFF_FC1F == 0xF100_001F
         && field(cmp, 5) != ZR
         && csel & !0x1F == csel_shown
         && strb & 0xFFC0_03E0 == 0x3900_03E0
         && field(strb, 0) == flag
   }

   /// `mov wD, w(shown)` (`orr wD, wzr, w(shown)`) for `csel wD, wzr, w(shown),
   /// ne`.
   pub(super) const fn mov_from_shown(csel: u32) -> u32 {
      const ORR_W_ZR: u32 = 0x2A00_03E0;
      ORR_W_ZR | (csel & (0x1F << 16)) | (csel & 0x1F)
   }
}

#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
mod x86 {
   use iced_x86::{
      Decoder,
      DecoderOptions,
      Instruction,
      Mnemonic,
      OpKind,
      Register,
   };

   use crate::index::{
      Index,
      Reg,
   };

   // Above the return address, the 32-byte shadow space and the fifth argument.
   const WIN64_SIXTH: u64 = 0x30;

   fn decode<const N: usize>(ix: &Index<'_>, at: usize) -> Option<[Instruction; N]> {
      const LONGEST: usize = 15;
      let code = ix.image.read(at, N * LONGEST)?;
      let mut decoder = Decoder::with_ip(64, code, at as u64, DecoderOptions::NONE);
      let insns = std::array::from_fn(|_| decoder.decode());
      insns.iter().all(|insn| !insn.is_invalid()).then_some(insns)
   }

   /// `test r64, r64` on one register, `sete r8`, `and r8, shown`, where
   /// `shown` is the register the ABI names or, on Win64, the stack slot of
   /// the sixth argument.
   pub(super) fn tests_since_and_shown(
      ix: &Index<'_>,
      site: usize,
      win64: bool,
      shown: Option<Reg>,
   ) -> bool {
      let Some([test, set, and]) = decode::<3>(ix, site) else {
         return false;
      };
      let since = test.op0_register();
      let flag = set.op0_register();
      let shape = test.mnemonic() == Mnemonic::Test
         && since.is_gpr64()
         && test.op1_kind() == OpKind::Register
         && test.op1_register() == since
         && set.mnemonic() == Mnemonic::Sete
         && flag.is_gpr8()
         && and.mnemonic() == Mnemonic::And
         && and.op0_kind() == OpKind::Register
         && and.op0_register() == flag;
      if !shape {
         return false;
      }
      if win64 {
         and.op1_kind() == OpKind::Memory
            && and.memory_base() == Register::RSP
            && and.memory_index() == Register::None
            && ix.function(site).is_some_and(|func| {
               frame_below_return(ix, func.start)
                  .is_some_and(|frame| and.memory_displacement64() == frame + WIN64_SIXTH)
            })
      } else {
         and.op1_kind() == OpKind::Register
            && and.op1_register().is_gpr8()
            && u8::try_from(and.op1_register().full_register().number()).ok() == shown
      }
   }

   fn frame_below_return(ix: &Index<'_>, start: usize) -> Option<u64> {
      let mut pushed = 0;
      for insn in decode::<12>(ix, start)? {
         match (insn.mnemonic(), insn.op0_kind(), insn.op1_kind()) {
            (Mnemonic::Push, OpKind::Register, _) => pushed += 8,
            (Mnemonic::Sub, OpKind::Register, OpKind::Immediate8to64 | OpKind::Immediate32to64)
               if insn.op0_register() == Register::RSP =>
            {
               return Some(pushed + insn.immediate(1));
            },
            _ => return None,
         }
      }
      None
   }
}
