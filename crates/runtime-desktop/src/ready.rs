//! `READY`, which Phoenix's session parses itself and never passes through the
//! registry. Its session handler is hooked instead, and plugin hooks rewrite
//! the JSON before Phoenix reads it, as Android does from the host's `@ws`
//! events.

use std::sync::{
   Mutex,
   OnceLock,
   PoisonError,
};

use phoenix_hook::image::Image;
use phoenix_runtime::gateway;
use phoenix_sigs::locate::ready::{
   self,
   ReadySession,
};

/// The session's READY handler, `fn(out, payload) -> out`, owning the payload.
type SessionFn = unsafe extern "C" fn(*mut u8, *mut u8) -> *mut u8;
/// The session's parse of READY text under 2 MB, `fn(out, ptr, len) -> out`.
type TextFn = unsafe extern "C" fn(*mut u8, *const u8, usize) -> *mut u8;

static SESSION: OnceLock<(SessionFn, ReadySession)> = OnceLock::new();
static TEXT: OnceLock<TextFn> = OnceLock::new();
/// The last rewritten READY, which the payload borrows, kept until the next.
static REWRITTEN: Mutex<Option<String>> = Mutex::new(None);

/// Hooks both of the session's ways into READY from [`crate::prepare`], once
/// the registry is read and before Discord's threads run.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   let parse = || {
      gateway::original_handler("READY")
         .ok_or_else(|| "no READY handler".to_owned())
         .and_then(|handler| ready::ready_parse(ix, handler))
   };
   match crate::located::get("ready::ready_text", || ready::ready_text(ix, &parse()?)) {
      Ok(func) => {
         // SAFETY: runs before Discord's threads, and the detour has the
         // parse's ABI with its return slot first.
         if let Some(original) = unsafe {
            crate::hook_sret::<TextFn>(image, func, text_ready as *const () as usize, "ready")
         } {
            let _ = TEXT.set(original);
            log::info!("ready: hooked the text parse @ {func:#x}");
         }
      },
      Err(err) => log::warn!("ready: {err}, READYs under 2 MB pass unhooked"),
   }
   match crate::located::get("ready::ready_session", || {
      ready::ready_session(ix, &parse()?)
   }) {
      Ok(session) => {
         // SAFETY: as above, for the handler's ABI.
         if let Some(original) = unsafe {
            crate::hook_sret::<SessionFn>(
               image,
               session.func,
               session_ready as *const () as usize,
               "ready",
            )
         } {
            let _ = SESSION.set((original, session));
            log::info!("ready: hooked the session @ {:#x}", session.func);
         }
      },
      Err(err) => log::warn!("ready: {err}, READY hooks stay off"),
   }
}

/// Runs plugin hooks on the payload's READY and points the payload at their
/// JSON, parsed from the raw text.
///
/// # Safety
///
/// `payload` is the live payload Phoenix passes the handler, laid out as
/// `session` read from the handler's code.
unsafe fn rewrite(payload: *mut u8, session: &ReadySession) {
   let Some(event) = gateway::event("READY") else {
      return;
   };
   if !gateway::listened(event) {
      return;
   }
   // SAFETY: the handler reads the raw `&str` at these offsets of the payload.
   let (raw, raw_len, kind) = unsafe {
      (
         payload.add(session.raw).cast::<[u8; 8]>(),
         payload.add(session.raw_len).cast::<[u8; 8]>(),
         payload.add(session.kind),
      )
   };
   // SAFETY: as above.
   let (ptr, len) = unsafe {
      (
         usize::from_ne_bytes(raw.read()),
         usize::from_ne_bytes(raw_len.read()),
      )
   };
   if ptr == 0 {
      log::warn!("ready: a READY without its text passes unhooked");
      return;
   }
   // SAFETY: the payload borrows the text for the handler's call.
   let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
   let Ok(text) = std::str::from_utf8(bytes) else {
      return;
   };
   let Some(json) = gateway::intercept(event, text) else {
      return;
   };
   // SAFETY: the payload is Phoenix's own, read just above, and the new text
   // stays alive in `REWRITTEN`.
   unsafe {
      raw.write((json.as_ptr() as usize).to_ne_bytes());
      raw_len.write(json.len().to_ne_bytes());
      // The gateway's split of READY into top-level entries borrows the old
      // text, and the handler frees its buffer the same way for either kind.
      *kind = session.raw_kind;
   }
   log::info!("ready: READY rewritten ({} KB)", len / 1024);
   *REWRITTEN.lock().unwrap_or_else(PoisonError::into_inner) = Some(json);
}

/// The JSON plugin hooks made of READY's `text`, if any hook it.
fn intercept(text: &str) -> Option<String> {
   gateway::intercept(gateway::event("READY")?, text)
}

extern "C" fn text_ready(out: *mut u8, ptr: *const u8, len: usize) -> *mut u8 {
   // The jump is written just before `TEXT` is set, all before Discord runs.
   let original = *TEXT.wait();
   // SAFETY: Phoenix passes the text it is about to parse.
   let text = (!ptr.is_null()).then(|| unsafe { std::slice::from_raw_parts(ptr, len) });
   let Some(json) = text.and_then(|text| intercept(std::str::from_utf8(text).ok()?)) else {
      // SAFETY: the trampoline runs the original parse with Phoenix's
      // arguments.
      return unsafe { original(out, ptr, len) };
   };
   log::info!("ready: READY rewritten ({} KB)", len / 1024);
   // SAFETY: as above, on the rewritten text, which outlives the call.
   let parsed = unsafe { original(out, json.as_ptr(), json.len()) };
   *REWRITTEN.lock().unwrap_or_else(PoisonError::into_inner) = Some(json);
   parsed
}

extern "C" fn session_ready(out: *mut u8, payload: *mut u8) -> *mut u8 {
   // The jump is written just before `SESSION` is set, all before Discord
   // runs.
   let (original, session) = SESSION.wait();
   // SAFETY: Phoenix passes its live payload, laid out as `session`.
   unsafe { rewrite(payload, session) };
   // SAFETY: the trampoline runs the original handler with Phoenix's
   // arguments.
   unsafe { original(out, payload) }
}
