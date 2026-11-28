//! Register values through a function's steps, walked forward once.
//!
//! - A call clobbers the ISA's caller-saved registers, through a register or a
//!   slot too.
//! - A register an instruction writes without its [`Op`] saying how becomes
//!   [`Val::Unknown`].
//! - A branch target starts from what every earlier branch to it agrees on, met
//!   with the fall-through unless a jump or return ends it. Back edges and jump
//!   tables end the fall-through but are not followed.

use std::collections::HashMap;

use crate::index::{
   Isa,
   Native,
   Op,
   Reg,
   Step,
   Value,
};

/// What a register holds before a step, as far as the walk can tell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Val {
   #[default]
   Unknown,
   /// The stack pointer at entry plus this.
   Stack(isize),
   /// The word loaded from the stack slot at entry `sp` plus `slot`, plus
   /// `bias`.
   Spilled {
      slot: isize,
      bias: isize,
   },
   Addr(usize),
   Imm(u64),
}

impl Val {
   const fn offset(self, by: isize) -> Self {
      match self {
         Self::Stack(at) => Self::Stack(at + by),
         Self::Spilled { slot, bias } => {
            Self::Spilled {
               slot,
               bias: bias + by,
            }
         },
         Self::Addr(at) => Self::Addr(at.wrapping_add_signed(by)),
         Self::Unknown | Self::Imm(_) => Self::Unknown,
      }
   }

   const fn loaded(self, offset: isize) -> Self {
      match self {
         Self::Stack(at) => {
            Self::Spilled {
               slot: at + offset,
               bias: 0,
            }
         },
         _ => Self::Unknown,
      }
   }
}

/// Each register's value, indexed by [`Reg`].
pub type Regs = [Val; 32];

/// A direct call, or one [`Op`] leaves as [`Op::Other`] since its target
/// is only in a register or memory.
const fn calls(step: &Step) -> bool {
   match step.native {
      _ if matches!(step.op, Op::Call(_)) => true,
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Native::X86(iced_x86::Mnemonic::Call) => true,
      Native::Arm64(phoenix_hook::arm64::Insn::Blr { .. }) => true,
      // `blx rm`.
      Native::Thumb(word) => word >> 16 == 0 && word & 0xFF87 == 0x4780,
      _ => false,
   }
}

/// An indirect jump, a jump table or a tail call through a register, which
/// [`Op`] leaves as [`Op::Other`].
const fn jumps_away(step: &Step) -> bool {
   match step.native {
      #[cfg(any(
         target_arch = "x86_64",
         target_os = "macos",
         all(target_os = "linux", target_arch = "aarch64")
      ))]
      Native::X86(iced_x86::Mnemonic::Jmp) => matches!(step.op, Op::Other),
      Native::Arm64(phoenix_hook::arm64::Insn::Br { .. }) => true,
      // `bx rm`.
      Native::Thumb(word) => word >> 16 == 0 && word & 0xFF87 == 0x4700,
      _ => false,
   }
}

fn meet(into: &mut Regs, other: &Regs) {
   for (mine, theirs) in into.iter_mut().zip(other) {
      if mine != theirs {
         *mine = Val::Unknown;
      }
   }
}

/// Calls `visit(index, step, regs)` for each step with the registers before
/// it, `sp` starting at `Val::Stack(0)`.
pub fn walk(isa: Isa, steps: &[Step], mut visit: impl FnMut(usize, &Step, &Regs)) {
   let sp = usize::from(isa.sp());
   let mut regs = [Val::Unknown; 32];
   regs[sp] = Val::Stack(0);
   let mut incoming = HashMap::<usize, Regs>::new();
   let mut ended = false;
   for (index, step) in steps.iter().enumerate() {
      if let Some(joined) = incoming.remove(&step.at) {
         if ended {
            regs = joined;
         } else {
            meet(&mut regs, &joined);
         }
      }
      ended = false;
      visit(index, step, &regs);

      let before = regs;
      for (reg, val) in regs.iter_mut().enumerate() {
         if step.writes & 1 << reg != 0 {
            *val = Val::Unknown;
         }
      }
      let mut set = |reg: Reg, val: Val| regs[usize::from(reg)] = val;
      let of = |reg: Reg| before[usize::from(reg)];
      match step.op {
         Op::Addr { reg, target } => set(reg, Val::Addr(target)),
         Op::Imm { reg, value } => set(reg, Val::Imm(value)),
         Op::Move { dst, src } => set(dst, of(src)),
         Op::Offset { reg, base, offset } => set(reg, of(base).offset(offset)),
         Op::Load {
            reg, base, offset, ..
         } => set(reg, of(base).loaded(offset)),
         Op::LoadPair {
            regs: [first, second],
            base,
            offset,
            size,
         } => {
            set(first, of(base).loaded(offset));
            set(second, of(base).loaded(offset + isize::from(size)));
         },
         Op::Branch(target) => {
            incoming
               .entry(target)
               .and_modify(|joined| meet(joined, &regs))
               .or_insert(regs);
         },
         Op::Jump(target) => {
            incoming
               .entry(target)
               .and_modify(|joined| meet(joined, &regs))
               .or_insert(regs);
            ended = true;
         },
         Op::Ret => ended = true,
         _ => ended = jumps_away(step),
      }
      if calls(step) {
         for (reg, val) in regs.iter_mut().enumerate() {
            if isa.caller_saved() & 1 << reg != 0 {
               *val = Val::Unknown;
            }
         }
      }
      if let Some((reg, by)) = step.writeback {
         regs[usize::from(reg)] = before[usize::from(reg)].offset(by);
      }
   }
}

/// Calls `visit(step, holds)` for each step with what carries `entry`'s value.
///
/// `holds(reg)` tells whether `reg` carries the value `entry` held on entry: a
/// copy of it, or a reload of a slot it was spilled to through a base register
/// left alone since. A call clobbers the caller-saved copies.
pub fn follow(
   isa: Isa,
   steps: &[Step],
   entry: Reg,
   mut visit: impl FnMut(&Step, &dyn Fn(Reg) -> bool),
) {
   let mut copies = 1_u32 << entry;
   let mut spills = Vec::<(Reg, isize)>::new();
   for (index, step) in steps.iter().enumerate() {
      let holds = |reg: Reg| copies & 1 << reg != 0;
      visit(step, &holds);
      let (spill, alias) = match step.op {
         Op::Store {
            value: Value::Reg(value),
            base,
            offset,
            size: 8,
         } if holds(value) => (Some((base, offset)), None),
         Op::Move { dst, src } if holds(src) => (None, Some(dst)),
         Op::Load {
            reg,
            base,
            offset,
            size: 8,
         } if spills.contains(&(base, offset)) => (None, Some(reg)),
         _ => (None, None),
      };
      copies &= !step.writes;
      if calls(step) && !probes_stack(isa, steps, index) {
         copies &= !isa.caller_saved();
      }
      spills.retain(|&(base, _)| step.writes & 1 << base == 0);
      spills.extend(spill);
      if let Some(dst) = alias {
         copies |= 1 << dst;
      }
   }
}

/// Whether the call at `steps[index]` is Win64's `__chkstk`, which a frame over
/// a page opens with as `mov eax, size; call __chkstk` and which keeps every
/// register but `rax`.
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
fn probes_stack(isa: Isa, steps: &[Step], index: usize) -> bool {
   isa == (Isa::X86_64 { win64: true })
      && index
         .checked_sub(1)
         .and_then(|at| steps.get(at))
         .is_some_and(|before| matches!(before.op, Op::Imm { reg, .. } if reg == isa.ret()))
}

#[cfg(not(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
)))]
const fn probes_stack(_isa: Isa, _steps: &[Step], _index: usize) -> bool {
   false
}
