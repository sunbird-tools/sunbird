//! Promises that worker threads settle.

use std::{
   cmp::Reverse,
   collections::BinaryHeap,
   sync::{
      Condvar,
      Mutex,
      PoisonError,
      atomic::{
         AtomicBool,
         AtomicU64,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
   },
};

use rquickjs::{
   CatchResultExt,
   Ctx,
   Exception,
   Value,
};

use crate::{
   discord,
   fetch,
   js::{
      Engine,
      Registry,
      http::response_object,
      saw,
   },
   permissions::Links,
   store,
};

impl Engine {
   /// Settles promise `id` with what its worker thread produced, and runs what
   /// awaited it.
   pub fn settle(&self, id: u64, settlement: Settlement) {
      self.with(|ctx| {
         let Some((resolve, reject)) = ctx
            .userdata::<Registry<'_>>()
            .and_then(|registry| registry.pending.borrow_mut().remove(&id))
            .map(|(_, resolve, reject)| (resolve, reject))
         else {
            return;
         };
         let value: rquickjs::Result<Result<Value<'_>, String>> = match settlement {
            Settlement::Fetch(Ok(response)) => {
               response_object(&ctx, &response).map(|object| Ok(object.into_value()))
            },
            Settlement::Discord(Ok(response)) => {
               // Discord's reply, so its URLs vouch for a plugin's rewrites as the
               // app's own replies do. Not its errors, which quote back whatever
               // value the plugin sent.
               if (200..300).contains(&response.status) {
                  saw(&ctx, &Links::scan(&String::from_utf8_lossy(&response.body)));
               }
               response_object(&ctx, &response).map(|object| Ok(object.into_value()))
            },
            Settlement::Int(Ok(number)) => Ok(Ok(Value::new_int(ctx.clone(), number))),
            Settlement::Int(Err(err)) => Ok(Err(err)),
            Settlement::Text(Ok(text)) => {
               rquickjs::String::from_str(ctx.clone(), &text).map(|text| Ok(text.into_value()))
            },
            Settlement::Rows(Ok(rows)) => {
               serde_json::to_string(&rows)
                  .map_err(|err| Exception::throw_internal(&ctx, &err.to_string()))
                  .and_then(|json| ctx.json_parse(json))
                  .map(Ok)
            },
            Settlement::Json(Ok(Some(json))) | Settlement::Command(Ok(json)) => {
               ctx.json_parse(json).map(Ok)
            },
            Settlement::Json(Ok(None)) => Ok(Ok(Value::new_null(ctx.clone()))),
            Settlement::Fetch(Err(err)) => Ok(Err(err.to_string())),
            Settlement::Discord(Err(err))
            | Settlement::Text(Err(err))
            | Settlement::Command(Err(err)) => Ok(Err(err.to_string())),
            Settlement::Rows(Err(err)) | Settlement::Json(Err(err)) => Ok(Err(err.to_string())),
         };
         let outcome = value.and_then(|value| {
            match value {
               Ok(value) => resolve.call::<_, ()>((value,)),
               Err(message) => {
                  reject.call::<_, ()>((Exception::from_message(ctx.clone(), &message)?,))
               },
            }
         });
         if let Err(err) = outcome.catch(&ctx) {
            log::error!("settling promise {id}: {err}");
         }
      });
   }
}

/// What a worker thread hands back to settle a promise with.
pub enum Settlement {
   Fetch(Result<fetch::Response, fetch::FetchError>),
   Discord(Result<fetch::Response, discord::DiscordError>),
   Text(Result<String, discord::DiscordError>),
   Int(Result<i32, String>),
   Rows(Result<store::Rows, store::StoreError>),
   Json(Result<Option<String>, store::StoreError>),
   /// A command's `ok` value as JSON.
   Command(Result<String, discord::DiscordError>),
}

/// Runs `work` on its own thread and returns a promise it settles. The
/// resolvers stay in the registry, since JS values cannot leave the thread
/// that holds the engine, under `owner` so unloading it drops them.
pub fn in_background<'js>(
   ctx: &Ctx<'js>,
   owner: &str,
   work: impl FnOnce() -> Settlement + Send + 'static,
) -> rquickjs::Result<rquickjs::Promise<'js>> {
   let (promise, id) = pending(ctx, owner)?;
   std::thread::spawn(move || {
      let settlement = work();
      if let Some(engine) = crate::engine() {
         engine.settle(id, settlement);
      }
   });
   Ok(promise)
}

/// A promise registered for [`Engine::settle`], and its id.
fn pending<'js>(ctx: &Ctx<'js>, owner: &str) -> rquickjs::Result<(rquickjs::Promise<'js>, u64)> {
   static NEXT: AtomicU64 = AtomicU64::new(0);

   let (promise, resolve, reject) = ctx.promise()?;
   let id = NEXT.fetch_add(1, Ordering::Relaxed);
   ctx.userdata::<Registry<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no registry"))?
      .pending
      .borrow_mut()
      .insert(id, (owner.to_owned(), resolve, reject));
   Ok((promise, id))
}

/// Due times of the promises [`after`] returned, by id.
static TIMERS: Mutex<BinaryHeap<Reverse<(Instant, u64)>>> = Mutex::new(BinaryHeap::new());
static TIMER_WAKE: Condvar = Condvar::new();
static TIMER_STARTED: AtomicBool = AtomicBool::new(false);

/// A promise that resolves to `null` after `delay`. One thread waits for every
/// such promise, so plugins sleeping at once cost no thread each.
pub fn after<'js>(
   ctx: &Ctx<'js>,
   owner: &str,
   delay: Duration,
) -> rquickjs::Result<rquickjs::Promise<'js>> {
   // Started before the promise exists, so a thread that cannot start throws
   // here rather than leaving a promise nothing settles.
   if !TIMER_STARTED.swap(true, Ordering::AcqRel) {
      let started = std::thread::Builder::new()
         .name("phoenix-timers".to_owned())
         .spawn(settle_due);
      if let Err(err) = started {
         TIMER_STARTED.store(false, Ordering::Release);
         return Err(Exception::throw_message(
            ctx,
            &format!("timers: could not start their thread: {err}"),
         ));
      }
   }
   let (promise, id) = pending(ctx, owner)?;
   let due = Instant::now().checked_add(delay).unwrap_or_else(Instant::now);
   TIMERS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .push(Reverse((due, id)));
   TIMER_WAKE.notify_one();
   Ok(promise)
}

/// Settles each timer once it is due, waiting until the next one otherwise.
fn settle_due() {
   let mut timers = TIMERS.lock().unwrap_or_else(PoisonError::into_inner);
   loop {
      let now = Instant::now();
      let mut due = Vec::new();
      while let Some(&Reverse((at, id))) = timers.peek()
         && at <= now
      {
         timers.pop();
         due.push(id);
      }
      if !due.is_empty() {
         drop(timers);
         if let Some(engine) = crate::engine() {
            for id in due {
               engine.settle(id, Settlement::Json(Ok(None)));
            }
         }
         timers = TIMERS.lock().unwrap_or_else(PoisonError::into_inner);
         continue;
      }
      timers = match timers.peek() {
         Some(&Reverse((at, _))) => {
            TIMER_WAKE
               .wait_timeout(timers, at.saturating_duration_since(now))
               .unwrap_or_else(PoisonError::into_inner)
               .0
         },
         None => TIMER_WAKE.wait(timers).unwrap_or_else(PoisonError::into_inner),
      };
   }
}
