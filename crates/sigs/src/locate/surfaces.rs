use iced_x86::{
   Decoder,
   DecoderOptions,
   FlowControl,
   InstructionInfoFactory,
   Mnemonic,
   OpAccess,
   Register,
};
use serde::{
   Deserialize,
   Serialize,
};

use crate::{
   Found,
   index::{
      Func,
      Index,
      Native,
      Op,
      Value,
      flow,
      op::Step,
   },
   one,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Surface {
   Composer,
   ServerList,
   Account,
   Profile,
   MediaViewer,
   MessageToolbar,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Menu {
   Server,
   Channel,
   User,
   Message,
}

impl Menu {
   pub const ALL: [Self; 4] = [Self::Server, Self::Channel, Self::User, Self::Message];

   #[must_use]
   pub const fn name(self) -> &'static str {
      match self {
         Self::Server => "server",
         Self::Channel => "channel",
         Self::User => "user",
         Self::Message => "message",
      }
   }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Menus {
   pub finish:    usize,
   pub drop_item: Option<usize>,
   pub calls:     Vec<MenuCall>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MenuCall {
   pub menu:   Menu,
   pub site:   usize,
   pub prefix: Vec<u8>,
}

pub fn menus(ix: &Index<'_>, item_size: usize) -> Found<Menus> {
   let server = ix
      .source("nav\\menus\\guild.rs")
      .and(&ix.literal(b"HIDE_MUTED_CHANNELS"))
      .and(&ix.literal(b"LEAVE_SERVER"))
      .one("server context menu")?;
   let channel = ix
      .literal(b"SPOILER_CHANNEL_CLEAR_AGREE")
      .and(&ix.literal(b"app_channel_context_menu"))
      .one("channel context menu")?;
   let user_trace = ix.text(b"guild channel user menu");
   let user = ix
      .literal(b"guild channel user menu")
      .filter(|func| func.loads().iter().any(|addr| user_trace.contains(addr)))
      .and(&ix.literal(b"COPY_ID_CHANNEL"))
      .one("user context menu")?;
   let channel_calls = channel.calls();
   let user_calls = user.calls();
   let mut finish = server
      .calls()
      .into_iter()
      .filter(|at| channel_calls.contains(at) && user_calls.contains(at))
      .filter(|&at| {
         ix.func(at)
            .is_ok_and(|func| accepts_menu_vec(ix, &func, item_size))
      })
      .collect::<Vec<_>>();
   finish.sort_unstable();
   finish.dedup();
   let finish = one("shared context menu item collector", &finish)?;
   let mut calls = Vec::new();
   for (menu, function) in [
      (Menu::Server, &server),
      (Menu::Channel, &channel),
      (Menu::User, &user),
   ] {
      let sites = function
         .calls_at()
         .into_iter()
         .filter_map(|(site, target)| (target == finish).then_some(site))
         .collect::<Vec<_>>();
      let site = one(menu.name(), &sites)?;
      calls.push(MenuCall {
         menu,
         site,
         prefix: target_prefix(ix, function, site),
      });
   }
   Ok(Menus {
      finish,
      drop_item: menu_item_drop(ix, finish, item_size),
      calls,
   })
}

fn menu_item_drop(ix: &Index<'_>, collector: usize, item_size: usize) -> Option<usize> {
   let steps = ix.func(collector).ok()?.steps();
   let candidates = steps
      .windows(5)
      .filter_map(|steps| {
         let [
            Op::Imm { reg: 1, value },
            Op::Move { dst: 7, src: 0 },
            _,
            Op::Move { dst: 1, src: 0 },
            Op::Call(drop),
         ] = [
            steps[0].op,
            steps[1].op,
            steps[2].op,
            steps[3].op,
            steps[4].op,
         ]
         else {
            return None;
         };
         if usize::try_from(value) != Ok(item_size)
            || !matches!(steps[2].native, Native::X86(Mnemonic::Movsb))
         {
            return None;
         }
         let head = crate::index::op::decode_head(ix.image, ix.isa, drop, drop + 12, 3);
         matches!(head.as_slice(), [
         Step { op: Op::CmpMem { base: 1, offset: 0, size: 4, value }, .. },
         Step { op: Op::Branch(_), .. },
         Step { op: Op::Ret, .. },
      ] if (*value & u64::from(u32::MAX)) == u64::from(u32::MAX))
         .then_some(drop)
      })
      .collect::<Vec<_>>();
   one("native menu item destructor", &candidates).ok()
}

pub fn menu_id_field(ix: &Index<'_>, with_id: usize, props_size: usize) -> Option<usize> {
   let mut offsets = ix
      .func(with_id)
      .ok()?
      .calls()
      .into_iter()
      .filter_map(|call| ix.func(call).ok())
      .filter_map(|func| ix.image.read(func.start, func.size()))
      .flat_map(|code| code.windows(36))
      .filter_map(|code| {
         let [
            0x48,
            0x8D,
            0x88,
            a,
            b,
            c,
            d,
            0x48,
            0x89,
            0x4D,
            saved,
            0xE8,
            _,
            _,
            _,
            _,
            0x90,
            0x48,
            0x8B,
            0x45,
            _,
            0x48,
            0x8B,
            0x4D,
            reload,
            0x48,
            0x89,
            0x41,
            0x10,
            0x0F,
            0x28,
            0x45,
            _,
            0x0F,
            0x11,
            0x01,
         ] = code
         else {
            return None;
         };
         let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
         (saved == reload && offset.checked_add(24)? <= props_size).then_some(offset)
      })
      .collect::<Vec<_>>();
   offsets.sort_unstable();
   offsets.dedup();
   one("native menu id field", &offsets).ok()
}

fn target_prefix(ix: &Index<'_>, function: &Func<'_>, site: usize) -> Vec<u8> {
   let steps = function.steps();
   let Some(call) = steps.iter().position(|step| step.at == site) else {
      return vec![0x45, 0x31, 0xC0];
   };
   let prologue_len = steps[..call]
      .iter()
      .take(40)
      .take_while(|step| !matches!(step.op, Op::Jump(_) | Op::Branch(_) | Op::Ret))
      .count();
   let mut captures = Vec::new();
   let mut index = 0;
   flow::follow(ix.isa, &steps[..prologue_len], 2, |step, holds| {
      if holds(2) {
         captures.push((index, *step));
      }
      index += 1;
   });
   for &(index, step) in &captures {
      if let Op::Move { dst, src: 2 } = step.op
         && matches!(dst, 3 | 6 | 7 | 12..=15)
         && ix
            .image
            .read(step.at, 1)
            .is_some_and(|first| (0x48..=0x4F).contains(&first[0]))
         && steps[index + 1..call]
            .iter()
            .all(|step| step.writes & (1 << dst) == 0)
      {
         return vec![0x4C | (dst >> 3), 0x8B, 0xC0 | (dst & 7)];
      }
   }
   for (index, step) in captures {
      if let Op::Store {
         value: crate::index::Value::Reg(2),
         base: 5,
         offset,
         size: 8,
      } = step.op
         && preserved_spill(ix, &steps[index + 1..call], offset)
         && let Ok(offset) = i32::try_from(offset)
      {
         let mut prefix = vec![0x4C, 0x8B, 0x85];
         prefix.extend_from_slice(&offset.to_le_bytes());
         return prefix;
      }
   }
   vec![0x45, 0x31, 0xC0]
}

fn preserved_spill(ix: &Index<'_>, steps: &[Step], offset: isize) -> bool {
   let offset = offset as i64;
   let mut info = InstructionInfoFactory::new();
   steps.iter().all(|step| {
      if step.writes & (1 << 5) != 0 {
         return false;
      }
      let Some(code) = ix.image.read(step.at, usize::from(step.len)) else {
         return false;
      };
      let insn = Decoder::new(64, code, DecoderOptions::NONE).decode();
      info.info(&insn).used_memory().iter().all(|memory| {
         if memory.base() != Register::RBP || !writes(memory.access()) {
            return true;
         }
         let start = memory.displacement().cast_signed();
         let size = i64::try_from(memory.memory_size().size()).unwrap_or_default();
         memory.index() == Register::None
            && size != 0
            && (start + size <= offset || offset + 8 <= start)
      })
   })
}

const fn writes(access: OpAccess) -> bool {
   matches!(
      access,
      OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite
   )
}

fn accepts_menu_vec(ix: &Index<'_>, func: &Func<'_>, item_size: usize) -> bool {
   let Ok(size) = u32::try_from(item_size) else {
      return false;
   };
   let size = size.to_le_bytes();
   let steps = func.steps();
   collector_arguments(ix, func, &steps)
      && steps.iter().take(32).any(|step| {
         matches!(step.op, Op::Load {
            base: 2,
            offset: 16,
            size: 8,
            ..
         })
      })
      && steps.iter().any(|step| {
         ix.image.read(step.at, 7).is_some_and(|bytes| {
            matches!(bytes, [0x48..=0x4F, 0x69, 0xC0..=0xFF, ..]) && bytes[3..] == size
         })
      })
}

fn collector_arguments(ix: &Index<'_>, func: &Func<'_>, steps: &[Step]) -> bool {
   let Some(code) = ix.image.read(func.start, func.size()) else {
      return false;
   };
   let mut info = InstructionInfoFactory::new();
   let mut stack_depth = 0isize;
   let mut frame_depth = None;
   let mut initialized_r8 = false;
   for (insn, step) in Decoder::with_ip(64, code, func.start as u64, DecoderOptions::NONE)
      .into_iter()
      .zip(steps)
   {
      if insn.is_invalid() {
         return false;
      }
      if let Some((4, delta)) = step.writeback {
         stack_depth -= delta;
      }
      if let Op::Offset {
         reg: 5,
         base: 4,
         offset,
      } = step.op
      {
         frame_depth = Some(stack_depth - offset);
      }
      let registers = info.info(&insn).used_registers();
      for used in registers {
         match used.register().full_register() {
            Register::R9 => return false,
            Register::R8 if !initialized_r8 => {
               if used.access() != OpAccess::Write || used.register().size() < 4 {
                  return false;
               }
               initialized_r8 = true;
            },
            _ => {},
         }
      }
      if !initialized_r8 && insn.flow_control() != FlowControl::Next {
         return false;
      }
      let offset = insn.memory_displacement64().cast_signed();
      let depth = match insn.memory_base() {
         Register::RSP => Some(stack_depth),
         Register::RBP => {
            let Some(depth) = frame_depth else {
               return false;
            };
            Some(depth)
         },
         _ => None,
      };
      if depth.is_some_and(|depth| offset >= depth as i64) {
         return false;
      }
   }
   initialized_r8
}

impl Surface {
   pub const DIVS: [Self; 4] = [
      Self::ServerList,
      Self::Account,
      Self::Profile,
      Self::MediaViewer,
   ];

   pub const ALL: [Self; 6] = [
      Self::Composer,
      Self::ServerList,
      Self::Account,
      Self::Profile,
      Self::MediaViewer,
      Self::MessageToolbar,
   ];

   #[must_use]
   pub const fn name(self) -> &'static str {
      match self {
         Self::Composer => "composer",
         Self::ServerList => "serverList",
         Self::Account => "account",
         Self::Profile => "profile",
         Self::MediaViewer => "mediaViewer",
         Self::MessageToolbar => "messageToolbar",
      }
   }

   #[must_use]
   pub const fn native_id(self) -> Option<&'static str> {
      match self {
         Self::ServerList => Some("rail-nav"),
         Self::Account => Some("account-row"),
         Self::Profile => Some("profile-body"),
         Self::MediaViewer => Some("media-viewer-pill"),
         Self::Composer | Self::MessageToolbar => None,
      }
   }
}

pub fn div_ids(ix: &Index<'_>, div_id: usize, surface: Surface) -> Found<Vec<usize>> {
   let id = surface.native_id().ok_or("surface has no native Div id")?;
   let mut ids = div_id_calls(ix, div_id, id)?
      .into_iter()
      .map(|(address, _)| address)
      .collect::<Vec<_>>();
   ids.sort_unstable();
   ids.dedup();
   Ok(ids)
}

fn div_id_calls(ix: &Index<'_>, div_id: usize, id: &str) -> Found<Vec<(usize, usize)>> {
   let mut load_length = [0x41, 0xB9, 0, 0, 0, 0];
   let len = u32::try_from(id.len()).map_err(|err| err.to_string())?;
   load_length[2..].copy_from_slice(&len.to_le_bytes());
   let mut found = Vec::new();
   for addr in ix.text(id.as_bytes()) {
      for (site, _) in ix.loads_of(addr..addr + 1) {
         let Some(func) = ix.function(site) else {
            continue;
         };
         let steps = func.steps();
         let Some(at) = steps.iter().position(|step| {
            step.at == site && matches!(step.op, Op::Addr { reg: 8, target } if target == addr)
         }) else {
            continue;
         };
         let mut correct_length = false;
         for step in steps.iter().skip(at + 1).take(8) {
            match step.op {
               Op::Call(callee) => {
                  if callee == div_id && correct_length {
                     found.push((addr, step.at));
                  }
                  break;
               },
               Op::Branch(_) | Op::Jump(_) | Op::Ret => break,
               _ if step.writes & (1 << 8) != 0 => break,
               _ if step.writes & (1 << 9) != 0 => {
                  let immediate = ix.image.read(step.at, load_length.len()) == Some(&load_length);
                  let push_pop = u8::try_from(len).is_ok_and(|length| {
                     length < 0x80
                        && step.at.checked_sub(2).is_some_and(|push| {
                           ix.image.read(push, 4) == Some(&[0x6A, length, 0x41, 0x59])
                        })
                  });
                  correct_length = immediate || push_pop;
               },
               _ => {},
            }
         }
      }
   }
   found.sort_unstable();
   found.dedup();
   if found.is_empty() {
      return Err(format!("{id} has no validated Div::id call"));
   }
   Ok(found)
}

pub fn composer_buttons(ix: &Index<'_>, div_id: usize, div_size: usize) -> Found<(usize, usize)> {
   let areas = div_id_calls(ix, div_id, "composer-area")?;
   let (_, area) = one("composer area", &areas)?;
   let composer = ix
      .function(area)
      .ok_or("composer render function missing")?;
   let fields = div_id_calls(ix, div_id, "composer-field")?
      .into_iter()
      .filter_map(|(_, site)| (composer.start..area).contains(&site).then_some(site))
      .collect::<Vec<_>>();
   let field = one("composer field", &fields)?;
   let emoji = b"emoji-tooltip";
   if !composer
      .loads()
      .iter()
      .any(|addr| ix.image.read(*addr, emoji.len()) == Some(emoji))
   {
      return Err("composer render function does not build the emoji tooltip".into());
   }
   let calls = composer.calls_at();
   let candidates = calls
      .iter()
      .copied()
      .filter(|&(site, target)| {
         (field..area).contains(&site)
            && optional_composer_child(ix, target, div_size, &calls, area..composer.end)
      })
      .collect::<Vec<_>>();
   one("composer emoji insertion", &candidates)
}

fn optional_composer_child(
   ix: &Index<'_>,
   address: usize,
   div_size: usize,
   calls: &[(usize, usize)],
   after_area: std::ops::Range<usize>,
) -> bool {
   let Ok(wrapper) = ix.func(address) else {
      return false;
   };
   let steps = wrapper.head(24);
   let Some(call) = steps.iter().position(|step| matches!(step.op, Op::Call(_))) else {
      return false;
   };
   let Op::Call(branch) = steps[call].op else {
      return false;
   };
   let head = &steps[..call];
   let copies_div = head.windows(4).any(|steps| {
      matches!(steps[0].op, Op::Imm { reg: 1, value } if usize::try_from(value) == Ok(div_size))
         && matches!(steps[1].op, Op::Move { dst: 6, src: 9 })
         && matches!(steps[2].native, Native::X86(Mnemonic::Movsb))
         && ix.image.read(steps[2].at, 2) == Some(&[0xF3, 0xA4])
         && matches!(steps[3].op, Op::Offset {
            reg: 8,
            base: 4,
            ..
         })
   });
   let captures_flag = head.iter().any(|step| {
      matches!(step.op, Op::Store {
         value: Value::Reg(8),
         size: 1,
         ..
      })
   });
   if !copies_div || !captures_flag || head.iter().any(|step| step.writes & (1 << 2) != 0) {
      return false;
   }
   let Ok(branch) = ix.func(branch) else {
      return false;
   };
   let steps = branch.steps();
   steps.iter().any(|step| {
      matches!(step.op, Op::CmpMem {
         size: 1,
         value: 0,
         ..
      })
   }) && steps.iter().any(|step| {
      let Op::Call(child) = step.op else {
         return false;
      };
      calls
         .iter()
         .any(|&(site, target)| target == child && after_area.contains(&site))
   })
}

pub fn button_id_field(ix: &Index<'_>, constructor: usize, size: usize) -> Found<usize> {
   let steps = ix.func(constructor)?.head(32);
   let offsets = steps
      .windows(3)
      .filter_map(|window| {
         use crate::index::Value::{
            Imm,
            Reg,
         };
         let [
            Op::Store {
               value: Imm(24),
               base,
               offset: tag,
               size: 1,
            },
            Op::Store {
               value: Reg(2),
               base: ptr_base,
               offset: ptr,
               size: 8,
            },
            Op::Store {
               value: Reg(8),
               base: len_base,
               offset: len,
               size: 8,
            },
         ] = [window[0].op, window[1].op, window[2].op]
         else {
            return None;
         };
         let tag = usize::try_from(tag).ok()?;
         let from_output = base == 1
            || base == 0
               && matches!(
                  steps.first().map(|step| step.op),
                  Some(Op::Move { dst: 0, src: 1 })
               );
         (from_output
            && base == ptr_base
            && base == len_base
            && tag.is_multiple_of(8)
            && usize::try_from(ptr).ok() == tag.checked_add(8)
            && usize::try_from(len).ok() == tag.checked_add(16)
            && tag.checked_add(24).is_some_and(|end| end <= size))
         .then_some(tag)
      })
      .collect::<Vec<_>>();
   one("native button's borrowed SharedString id", &offsets)
}
