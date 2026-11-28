//! The timers `phoenix.ui.disableCallIdle` turns off.
//!
//! They are the 3-minute solo DM call disconnect in `ops/calls.rs` and the
//! guild AFK move and disconnect in `ops-voice/src/guild_afk_timeout.rs`.
//! Each is an async block that sleeps and then acts, so the runtime hooks its
//! `Future::poll` and answers `Poll::Ready(())`. Windows outlines the AFK
//! blocks' bodies into a function the poll calls, Linux inlines them.

use crate::{
   Found,
   http::find_vtable_method_slots,
   index::{
      Func,
      Index,
   },
};

/// The guild AFK timer's two `Future::poll`s.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct GuildAfk {
   pub move_poll:       usize,
   pub disconnect_poll: usize,
}

/// The solo DM call timer's `Future::poll`, which passes
/// `"call_idle_disconnect"` as the reason it leaves the call.
pub fn dm_idle(ix: &Index<'_>) -> Found<usize> {
   poll_of(ix, ix.literal(b"call_idle_disconnect").one("call idle disconnect")?)
}

pub fn guild_afk(ix: &Index<'_>) -> Found<GuildAfk> {
   let traced = ix.tracing("libdiscord_ops_voice::guild_afk_timeout");
   let traced_with = |marker: &[u8], what| ix.literal(marker).and(&traced).one(what);
   Ok(GuildAfk {
      move_poll:       poll_of(ix, traced_with(b"afk move", "guild AFK move")?)?,
      disconnect_poll: poll_of(ix, traced_with(b"afk disconnect", "guild AFK disconnect")?)?,
   })
}

/// `func` when a future's vtable holds it as `poll`, else its one caller.
fn poll_of(ix: &Index<'_>, func: Func<'_>) -> Found<usize> {
   let is_poll = |start| !find_vtable_method_slots(ix.image, start, 1..=0x1_0000).is_empty();
   if is_poll(func.start) {
      return Ok(func.start);
   }
   let caller = func.callers().one(&format!("caller of {func:?}"))?;
   if is_poll(caller.start) {
      Ok(caller.start)
   } else {
      Err(format!("neither {func:?} nor its caller {caller:?} is a Future::poll"))
   }
}
