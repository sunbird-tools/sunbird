//! What draws a server's tooltip in the server list.

use crate::{
   Found,
   index::{
      Index,
      Op,
      flow,
   },
   one,
};

/// The tooltip's line for unread mentions.
pub const MENTIONS: &[u8] = b"SERVER_TOOLTIP_NOTIFICATION_CONTEXT_MENTION";
/// Its line for new activity.
pub const ACTIVITY: &[u8] = b"SERVER_TOOLTIP_NOTIFICATION_CONTEXT_ACTIVITY";
/// The fewest calls the tooltip's render makes. The only other caller of the
/// line, which refreshes the line the tooltip keeps, makes a handful.
const RENDER_CALLS: usize = 20;

/// What draws a server's tooltip.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tooltip {
   /// `fn(out: &mut Option<AnyElement>, &self, window, cx) -> out`, what the
   /// tooltip shows under its line: who is in the server's voice channels
   /// and streams, or nothing.
   pub render: usize,
   /// Where `self` keeps the server's id.
   pub guild:  isize,
}

/// `fn(out, cx, guild_id: u64) -> out`, the line under a server's name in its
/// tooltip.
///
/// The line tells of unread mentions or new activity, or where the user's
/// request to join the server stands. The function loads both first kinds'
/// keys.
pub fn context(ix: &Index<'_>) -> Found<usize> {
   let mentions = loader(ix, MENTIONS)?;
   let activity = loader(ix, ACTIVITY)?;
   if mentions != activity {
      return Err(format!(
         "the tooltip lines load from {mentions:#x} and {activity:#x}"
      ));
   }
   Ok(mentions)
}

/// The tooltip's render: the only caller of [`context`] making at least
/// [`RENDER_CALLS`] calls, which hands it the server's id from a field of its
/// `self`.
pub fn tooltip(ix: &Index<'_>) -> Found<Tooltip> {
   let line = context(ix)?;
   let renders = ix
      .func(line)?
      .callers()
      .into_iter()
      .filter(|func| func.calls().len() >= RENDER_CALLS)
      .collect::<Vec<_>>();
   let render = one("the tooltip render", &renders)?;
   // Only the tooltip calls it, so the hook never reads the id from a `self`
   // of another shape.
   render.callers().one("the tooltip render's caller")?;
   let sites = render
      .calls_at()
      .into_iter()
      .filter(|&(_, target)| target == line)
      .map(|(site, _)| site)
      .collect::<Vec<_>>();
   let site = one("the tooltip render's call to its line", &sites)?;
   let this = ix.isa.slot_arg(0).ok_or("no register for self")?;
   let guild_arg = ix.isa.slot_arg(1).ok_or("no register for the server id")?;
   // The last load into the id's register before the call, from `self`. A
   // call or another write in between drops it.
   let mut guild = None;
   flow::follow(ix.isa, &render.steps(), this, |step, holds| {
      if step.at >= site {
         return;
      }
      match step.op {
         Op::Load {
            reg,
            base,
            offset,
            size: 8,
         } if reg == guild_arg => guild = holds(base).then_some(offset),
         _ if flow::calls(step) || step.writes & 1 << guild_arg != 0 => guild = None,
         _ => {},
      }
   });
   let guild = guild.ok_or_else(|| {
      format!(
         "{:#x} hands the tooltip line no server id from self",
         render.start
      )
   })?;
   crate::locate::entry_hookable(ix, render.start)?;
   Ok(Tooltip {
      render: render.start,
      guild,
   })
}

/// The function loading `key`. Keys sit back to back without a NUL, so this
/// counts loads of the key's first byte, as `counts` does for UI ids.
fn loader(ix: &Index<'_>, key: &[u8]) -> Found<usize> {
   let name = String::from_utf8_lossy(key);
   let loads = ix
      .text(key)
      .into_iter()
      .flat_map(|at| ix.loads_of(at..at + 1))
      .map(|(site, _)| site)
      .collect::<Vec<_>>();
   let load = one(&format!("{name} load"), &loads)?;
   ix.function(load)
      .map(|func| func.start)
      .ok_or_else(|| format!("the {name} load at {load:#x} is in no function"))
}
