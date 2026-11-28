//! The channel sidebar's width and the channel header toolbar's member list
//! button, which `phoenix.ui.collapseChannels` and `addHeaderButton` patch.

use iced_x86::Mnemonic;

use crate::{
   Found,
   desktop::layout,
   index::{
      Func,
      Index,
      Native,
      Op,
      Reg,
      Step,
      Value,
      flow::{
         self,
         Val,
      },
   },
   one,
};

/// The type name `Entity::read` panics with for the sidebar's width state,
/// under `discord_desktop` or `discord_desktop_workspace`.
const SIDEBAR_RESIZE: &[u8] = b"sidebar_resize::SidebarResize";
/// The workspace's id for the channel sidebar's resize handle.
const SIDEBAR_HANDLE_ID: &[u8] = b"channel-sidebar-resize";
/// The channel header toolbar's member list button id.
const TOOLBAR_MEMBERS_ID: &[u8] = b"tb-members";
/// The most bytes the width getter and the two small functions calling it
/// take. The getter's third caller, which saves the width, takes 160 or more.
const SMALL: usize = 0x60;
/// How far past the call answering whether an overlay covers the workspace
/// `Workspace::render` loads the resize handle's id.
const HANDLE_REACH: usize = 0x400;

/// What collapses the channel sidebar.
///
/// The workspace sizes the sidebar from `SidebarResize`'s width, the server
/// rail plus 1 px plus the channel column (clamped to 264..432 px by the
/// handle): `column` returns `width - rail - 1`, and `publish` hands the whole
/// width to other views. The width getter's third caller saves the width to
/// settings, so the getter answers the rail only inside the other two.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChannelSidebar {
   /// `f32 width(&RootModel, &App)`.
   pub width:       usize,
   /// `f32 column(&RootModel, &App)`.
   pub column:      usize,
   /// `call width` in `column`, for x86 builds whose `column` opens with a
   /// `call [rip + slot]` sunhook can't move.
   pub column_site: usize,
   /// `publish(&RootModel, target, &App)`.
   pub publish:     usize,
   /// `f32 rail()`, the server rail's width.
   pub rail:        usize,
   /// `bool covered(&RootModel, ..)`, whose `true` leaves out the resize
   /// handle in `Workspace::render`, its only caller.
   pub covered:     usize,
}

/// Finds [`ChannelSidebar`] from `SidebarResize`'s type name.
///
/// The width getter is the small function whose only call is that
/// `Entity::read`, and `publish` is the small caller handing a stack slot to
/// its other call.
pub fn channel_sidebar(ix: &Index<'_>) -> Found<ChannelSidebar> {
   let named = ix.literal(SIDEBAR_RESIZE);
   if named.is_empty() {
      return Err("SidebarResize is never named in code".to_owned());
   }
   // arm64 loads the type name in a three-instruction stub `Entity::read`
   // calls.
   let reads = named
      .iter()
      .filter(|func| func.size() <= SMALL / 4)
      .flat_map(Func::callers)
      .chain(named.iter().copied())
      .collect::<Vec<_>>();
   let mut widths = reads
      .iter()
      .flat_map(Func::callers)
      .filter(|func| {
         func.size() <= SMALL
            && matches!(flat_calls(func)[..], [(_, read)] if reads.iter().any(|it| it.start == read))
            && straight(func)
      })
      .collect::<Vec<_>>();
   widths.sort_unstable_by_key(|func| func.start);
   widths.dedup_by_key(|func| func.start);
   let width = one("sidebar width getter", &widths)?;

   let small = width
      .callers()
      .filter(|func| func.size() <= SMALL)
      .into_iter()
      .collect::<Vec<_>>();
   let target = ix.isa.arg(2).ok_or("no third argument register")?;
   let (publishes, columns) = small
      .iter()
      .partition::<Vec<_>, _>(|func| passes_stack(ix, func, width.start, target));
   let publish = one("sidebar width publish", &publishes)?;
   let column = one("sidebar column width", &columns)?;
   let rail = flat_calls(&column)
      .into_iter()
      .map(|(_, callee)| callee)
      .skip_while(|&callee| callee != width.start)
      .nth(1)
      .ok_or_else(|| format!("sidebar column width {column:?} calls nothing after the width"))?;
   let column_site = column
      .steps()
      .iter()
      .find_map(|step| {
         match step.op {
            Op::Call(callee) if column.through_outlined(callee) == Some(width.start) => {
               Some(step.at)
            },
            _ => None,
         }
      })
      .ok_or_else(|| format!("sidebar column width {column:?} never calls the width"))?;
   if ix.func(rail)?.size() > SMALL {
      return Err(format!("sidebar rail width {rail:#x} is no small function"));
   }
   Ok(ChannelSidebar {
      width: width.start,
      column: column.start,
      column_site,
      publish: publish.start,
      rail,
      covered: covered(ix, publish.start)?,
   })
}

/// Whether `func` runs to its one `ret` without branching, as the width
/// getter does. A sibling reading the same state branches on its variant.
fn straight(func: &Func<'_>) -> bool {
   let steps = func.flat_steps();
   steps.last().is_some_and(|step| step.op == Op::Ret)
      && !steps
         .iter()
         .any(|step| matches!(step.op, Op::Branch(_) | Op::Jump(_)))
}

/// Whether `func`'s first call past `width` gets a stack slot in `arg`.
fn passes_stack(ix: &Index<'_>, func: &Func<'_>, width: usize, arg: Reg) -> bool {
   let steps = func.flat_steps();
   let mut past_width = false;
   let mut passes = None;
   flow::walk(ix.isa, &steps, |_, step, regs| {
      let Op::Call(callee) = step.op else {
         return;
      };
      if callee == width {
         past_width = true;
      } else if past_width && passes.is_none() {
         passes = Some(matches!(regs[usize::from(arg)], Val::Stack(_)));
      }
   });
   passes == Some(true)
}

/// The call after `publish`'s in `Workspace::render`, the function loading the
/// resize handle's id soon after.
fn covered(ix: &Index<'_>, publish: usize) -> Found<usize> {
   let (load, id) = exact_load(ix, SIDEBAR_HANDLE_ID)?;
   let render = ix
      .function(load)
      .ok_or_else(|| format!("the sidebar handle id load at {load:#x} is in no function"))?;
   let steps = render.steps();
   let calls = steps
      .iter()
      .filter_map(|step| {
         match step.op {
            Op::Call(callee) => Some((step.at, render.through_outlined(callee)?)),
            _ => None,
         }
      })
      .collect::<Vec<_>>();
   let after = calls
      .iter()
      .position(|&(_, callee)| callee == publish)
      .ok_or_else(|| format!("{render:?} loads the sidebar handle id but never publishes"))?;
   let &(site, covered) = calls
      .get(after + 1)
      .ok_or("nothing is called after the sidebar width publish")?;
   if !(site..site + HANDLE_REACH).contains(&load) {
      return Err(format!(
         "the sidebar handle id {id:#x} loads at {load:#x}, not just past {site:#x}"
      ));
   }
   let callers = ix.func(covered)?.callers();
   if callers.len() != 1 {
      return Err(format!(
         "the sidebar handle gate {covered:#x} has {} callers",
         callers.len()
      ));
   }
   Ok(covered)
}

/// The channel header toolbar's member list button, which plugin header
/// buttons follow.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChannelToolbar {
   /// `button(out, id, id_len, icon: u16, tooltip: &mut SharedString,
   /// selected, _, cx) -> out`, the toolbar icon button builder.
   pub button:       usize,
   /// `append(out, &mut toolbar, &mut button) -> out`, which pushes the button
   /// into the toolbar in place and then moves the toolbar to `out`.
   pub append:       usize,
   /// The member list button's id, the `id` the builder gets for it.
   pub members_id:   usize,
   /// The button's bytes.
   pub button_size:  usize,
   /// Bytes `append` moves from the toolbar to its return slot.
   pub toolbar_size: usize,
   /// The button's `Option<Rc<dyn Fn>>` click handler: data, then vtable.
   pub on_click:     usize,
}

/// Finds [`ChannelToolbar`] from the member list button's id.
///
/// The function loading the id builds the button with the first call that
/// takes the id, sets its click handler, and appends it to the toolbar it
/// returns with the last call before its first return past the build.
pub fn channel_toolbar(ix: &Index<'_>) -> Found<ChannelToolbar> {
   let (load, members_id) = exact_load(ix, TOOLBAR_MEMBERS_ID)?;
   let toolbar = ix
      .function(load)
      .ok_or_else(|| format!("the members button id load at {load:#x} is in no function"))?;
   let id_arg = ix.isa.slot_arg(0).ok_or("no id argument register")?;
   let steps = toolbar.flat_steps();

   let mut builds = Vec::new();
   let mut loaded = false;
   flow::walk(ix.isa, &steps, |index, step, regs| {
      loaded |= step.at == load;
      if let Op::Call(callee) = step.op
         && loaded
         && builds.is_empty()
         && regs[usize::from(id_arg)] == Val::Addr(members_id)
      {
         builds.push((index, callee));
      }
   });
   let (built, button) = one("members toolbar button builder", &builds)?;

   let returned = built
      + steps[built..]
         .iter()
         .position(|step| step.op == Op::Ret)
         .ok_or("the members toolbar never returns past its button")?;
   let (appended, append) = steps[..returned]
      .iter()
      .enumerate()
      .rev()
      .find_map(|(index, step)| {
         match step.op {
            Op::Call(callee) => Some((index, callee)),
            _ => None,
         }
      })
      .ok_or("the members toolbar calls nothing before it returns")?;
   if append == button || appended <= built {
      return Err(format!(
         "the members toolbar appends with {append:#x} before building with {button:#x}"
      ));
   }

   let between = steps[built + 1..appended]
      .iter()
      .filter_map(|step| {
         match step.op {
            Op::Call(callee) => Some(callee),
            _ => None,
         }
      })
      .collect::<Vec<_>>();
   // The runtime adds plugin buttons after the next `append` the member list
   // button's build reaches, which has to be the toolbar's.
   if between.iter().any(|&callee| {
      callee == append
         || ix
            .func(callee)
            .is_ok_and(|func| func.calls().contains(&append))
   }) {
      return Err(format!(
         "the members toolbar appends with {append:#x} before its last call"
      ));
   }
   let handlers = between
      .iter()
      .filter_map(|&callee| handler_store(ix, callee))
      .collect::<Vec<_>>();
   let (on_click, button_size) = one("members toolbar click handler", &handlers)?;
   let toolbar_arg = ix.isa.slot_arg(0).ok_or("no toolbar argument register")?;
   let toolbar_size = one(
      "members toolbar append copy",
      &copies_from(ix, &ix.func(append)?.flat_steps(), toolbar_arg),
   )?;
   if on_click + 16 > button_size {
      return Err(format!(
         "the toolbar button click handler +{on_click:#x} sits past its {button_size:#x} bytes"
      ));
   }
   if button_size.max(toolbar_size) > layout::SCRATCH_CAP {
      return Err(format!(
         "the toolbar ({toolbar_size:#x} bytes) or its button ({button_size:#x}) outgrows the \
          scratch buffers"
      ));
   }
   Ok(ChannelToolbar {
      button,
      append,
      members_id,
      button_size,
      toolbar_size,
      on_click,
   })
}

/// Where a click handler setter stores the handler in the button it takes,
/// and the button's bytes it then copies out: the `Rc` data word and the
/// vtable address after it, both stored off the button.
fn handler_store(ix: &Index<'_>, setter: usize) -> Option<(usize, usize)> {
   let button = ix.isa.slot_arg(0)?;
   let steps = ix.func(setter).ok()?.flat_steps();
   let held = holders(ix, &steps, button);
   let mut words = Vec::new();
   let mut vtables = Vec::new();
   flow::walk(ix.isa, &steps, |index, step, regs| {
      let holds = |reg: Reg| held[index] & 1 << reg != 0;
      let vtable = |reg: Reg| matches!(regs[usize::from(reg)], Val::Addr(_));
      match step.op {
         Op::Store {
            value: Value::Reg(value),
            base,
            offset,
            size: 8,
         } if holds(base) => {
            words.push(offset);
            if vtable(value) {
               vtables.push(offset - 8);
            }
         },
         Op::StorePair {
            values: [_, value],
            base,
            offset,
            size: 8,
         } if holds(base) && vtable(value) => {
            words.push(offset);
            vtables.push(offset);
         },
         _ => {},
      }
   });
   let mut on_clicks = vtables
      .into_iter()
      .filter(|offset| words.contains(offset))
      .filter_map(|offset| usize::try_from(offset).ok())
      .collect::<Vec<_>>();
   on_clicks.sort_unstable();
   on_clicks.dedup();
   let [on_click] = on_clicks[..] else {
      return None;
   };
   let [size] = copies_from(ix, &steps, button)[..] else {
      return None;
   };
   Some((on_click, size))
}

/// The sizes `steps` copy from what `src` held on entry, through a call taking
/// `(dst, src, len)` or x86's `rep movs`, each once.
fn copies_from(ix: &Index<'_>, steps: &[Step], src: Reg) -> Vec<usize> {
   let held = holders(ix, steps, src);
   let (Some(from), Some(len)) = (ix.isa.arg(1), ix.isa.arg(2)) else {
      return Vec::new();
   };
   let mut sizes = Vec::new();
   flow::walk(ix.isa, steps, |index, step, regs| {
      let holds = |reg: Reg| held[index] & 1 << reg != 0;
      let imm = |reg: Reg| {
         match regs[usize::from(reg)] {
            Val::Imm(value) => usize::try_from(value).ok(),
            _ => None,
         }
      };
      let size = match (step.op, step.native) {
         (Op::Call(_) | Op::Jump(_), _) if holds(from) => imm(len),
         (_, Native::X86(Mnemonic::Movsb)) if holds(6) => imm(1),
         (_, Native::X86(Mnemonic::Movsq)) if holds(6) => imm(1).map(|words| words * 8),
         _ => None,
      };
      sizes.extend(size);
   });
   sizes.sort_unstable();
   sizes.dedup();
   sizes
}

/// Which registers hold what `entry` held on entry, a bit each, before every
/// step.
fn holders(ix: &Index<'_>, steps: &[Step], entry: Reg) -> Vec<u32> {
   let mut held = Vec::with_capacity(steps.len());
   flow::follow(ix.isa, steps, entry, |_, holds| {
      held.push(
         (0..32)
            .filter(|&reg| holds(reg))
            .fold(0, |mask, reg| mask | 1 << reg),
      );
   });
   held
}

/// The calls in `func`'s flattened steps, with their indices.
fn flat_calls(func: &Func<'_>) -> Vec<(usize, usize)> {
   func
      .flat_steps()
      .iter()
      .enumerate()
      .filter_map(|(index, step)| {
         match step.op {
            Op::Call(callee) => Some((index, callee)),
            _ => None,
         }
      })
      .collect()
}

/// The one load of `id`'s exact address, and that address. Ids are packed back
/// to back without a NUL, so a load of an earlier literal doesn't count.
fn exact_load(ix: &Index<'_>, id: &[u8]) -> Found<(usize, usize)> {
   let loads = ix
      .text(id)
      .into_iter()
      .flat_map(|at| ix.loads_of(at..at + 1))
      .collect::<Vec<_>>();
   one(&format!("{} load", String::from_utf8_lossy(id)), &loads)
}
