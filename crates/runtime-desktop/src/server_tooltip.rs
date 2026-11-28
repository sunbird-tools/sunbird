//! Rows plugins add at the bottom of a server's tooltip in the server list
//! (`phoenix.ui.addServerTooltip`).
//!
//! The detour on what draws the bottom of the tooltip, the voice channels and
//! streams that [`server_tooltip::tooltip`] finds, puts the plugins' rows
//! under it, or alone where the server has none. Each row is its pieces side
//! by side: icons or colored text. Plugins render on a worker thread and what
//! they return is kept per server, so the tooltip never waits on them.

use std::{
   collections::HashMap,
   ffi::c_void,
   sync::{
      LazyLock,
      Mutex,
      OnceLock,
      PoisonError,
      RwLock,
      atomic::{
         AtomicBool,
         AtomicIsize,
         AtomicU64,
         AtomicUsize,
         Ordering,
      },
      mpsc,
   },
   time::Instant,
};

use phoenix_hook::image::Image;
use phoenix_sigs::locate::server_tooltip;

use crate::{
   accessories::invalidate_discord_windows,
   gpui::{
      self,
      Piece,
      RawAnyElement,
   },
   members::MIN_SNOWFLAKE,
};

/// `fn(out: &mut Option<AnyElement>, &self, window, cx) -> out`.
type RenderFn = unsafe extern "C" fn(
   *mut RawAnyElement,
   *const u8,
   *mut c_void,
   *mut c_void,
) -> *mut RawAnyElement;

/// The rows' text, a little larger and bolder than the tooltip's own lines.
const FONT_PX: f32 = 16.0;
const FONT_WEIGHT: f32 = 600.0;
const MAX_CACHE_ENTRIES: usize = 4096;

/// What each plugin adds to one server's tooltip: a row of pieces each.
type Rows = Vec<Vec<Piece>>;

enum Entry {
   Pending,
   Ready(Rows),
}

static ORIGINAL: OnceLock<RenderFn> = OnceLock::new();
/// Where the tooltip's `self` keeps the server's id.
static GUILD_FIELD: AtomicIsize = AtomicIsize::new(0);
/// How many plugins add rows.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// The server whose tooltip was drawn last, the only one a change redraws.
static SHOWN: AtomicU64 = AtomicU64::new(0);
/// When it was last drawn, from [`now_ms`], 0 before any.
static SHOWN_AT: AtomicU64 = AtomicU64::new(0);
/// How long after its last draw a tooltip still counts as open.
const OPEN_MS: u64 = 250;
/// What the plugins returned per server.
static CACHE: LazyLock<RwLock<HashMap<u64, Entry>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static WORKER: OnceLock<mpsc::Sender<u64>> = OnceLock::new();
/// Held while the worker starts, so two threads never start one each.
static STARTING: Mutex<()> = Mutex::new(());
/// Whether a failed start was logged, since lookups run every frame.
static WARNED: AtomicBool = AtomicBool::new(false);
/// Bumped whenever the cache is dropped, so rows rendered before are not kept.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// How many times `update(guild)` stored rows, per server, so the worker never
/// keeps a render it started before one over the newer rows.
static UPDATES: LazyLock<Mutex<HashMap<u64, u64>>> = LazyLock::new(Mutex::default);

/// Hooks the tooltip's render, before Discord's threads run.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   if !gpui::supports_accessories() {
      log::warn!("server tooltip: GPUI text/push primitives unavailable, skipping");
      return;
   }
   // Without it, plugins' icons show as their text.
   gpui::locate_svg(image);
   let found = match crate::located::get("server_tooltip::tooltip", || server_tooltip::tooltip(ix))
   {
      Ok(found) => found,
      Err(err) => {
         log::warn!("server tooltip: {err}");
         return;
      },
   };
   GUILD_FIELD.store(found.guild, Ordering::Release);
   // SAFETY: runs before Discord's threads, and the detour has the render's
   // ABI.
   if let Some(original) = unsafe {
      crate::hook_sret::<RenderFn>(
         image,
         found.render,
         render as *const () as usize,
         "server tooltip",
      )
   } {
      let _ = ORIGINAL.set(original);
      log::info!(
         "server tooltip: hooked @ {:#x}, server id at self+{:#x}",
         found.render,
         found.guild
      );
   }
}

pub fn set_active_count(count: usize) {
   ACTIVE.store(count, Ordering::Release);
   if count > 0 {
      let _ = worker();
   }
}

/// Where the cache stands, read before a render so [`set_rendered`] drops
/// rows rendered before the cache was.
pub fn generation() -> u64 {
   GENERATION.load(Ordering::Acquire)
}

/// The server whose tooltip is open.
///
/// An open tooltip redraws every frame (70 to 150 renders every 250 ms on
/// Windows, 2026-11), and none once it closes, so one not drawn for
/// [`OPEN_MS`] has closed. A build that stopped redrawing it would read as
/// closed, which only makes a plugin wait.
pub fn shown() -> Option<u64> {
   let drawn = SHOWN_AT.load(Ordering::Acquire);
   let guild = SHOWN.load(Ordering::Acquire);
   (guild != 0 && drawn != 0 && now_ms().saturating_sub(drawn) <= OPEN_MS).then_some(guild)
}

/// Milliseconds since the first tooltip, never 0.
fn now_ms() -> u64 {
   static START: OnceLock<Instant> = OnceLock::new();
   let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
   u64::try_from(elapsed).unwrap_or(u64::MAX).max(1)
}

/// Drops every server's rows, so the plugins are asked again, and repaints
/// the tooltip on screen without them.
pub fn clear_cache() {
   if let Ok(mut cache) = CACHE.write() {
      GENERATION.fetch_add(1, Ordering::AcqRel);
      cache.clear();
   }
   invalidate_discord_windows();
}

/// Keeps what `update(guild)` rendered at `generation`.
pub fn set_rendered(guild: u64, rows: Rows, generation: u64) {
   store(guild, rows, generation, None);
}

/// How many times `update(guild)` stored rows for `guild`.
fn updates(guild: u64) -> u64 {
   UPDATES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get(&guild)
      .copied()
      .unwrap_or_default()
}

/// Keeps what the plugins returned for `guild`, rendered at `generation`, and
/// repaints when it changed the tooltip on screen. Rows rendered before the
/// cache was dropped are not kept, so the next lookup asks again, and neither
/// are the worker's once an `update(guild)` stored newer rows since it read
/// `seen`. Redrawing for every server would stall Discord when a plugin
/// updates them all at once.
fn store(guild: u64, rows: Rows, generation: u64, seen: Option<u64>) {
   let changed = CACHE.write().is_ok_and(|mut cache| {
      let mut updates = UPDATES.lock().unwrap_or_else(PoisonError::into_inner);
      if GENERATION.load(Ordering::Acquire) != generation {
         return false;
      }
      match seen {
         Some(seen) if updates.get(&guild).copied().unwrap_or_default() != seen => return false,
         Some(_) => {},
         None => *updates.entry(guild).or_default() += 1,
      }
      drop(updates);
      if cache.len() > MAX_CACHE_ENTRIES {
         cache.clear();
      }
      let same = matches!(cache.get(&guild), Some(Entry::Ready(old)) if same_rows(old, &rows));
      cache.insert(guild, Entry::Ready(rows));
      !same
   });
   if changed && SHOWN.load(Ordering::Acquire) == guild {
      invalidate_discord_windows();
   }
}

fn same_rows(a: &Rows, b: &Rows) -> bool {
   a.len() == b.len()
      && a.iter().zip(b).all(|(a, b)| {
         a.len() == b.len()
            && a
               .iter()
               .zip(b)
               .all(|(a, b)| a.text == b.text && a.color == b.color && a.icons == b.icons)
      })
}

/// The thread plugins render on, started the first time it is needed. It is
/// kept only once it runs, so a failed start is tried again.
fn worker() -> Option<&'static mpsc::Sender<u64>> {
   if let Some(tx) = WORKER.get() {
      return Some(tx);
   }
   let _starting = STARTING.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(tx) = WORKER.get() {
      return Some(tx);
   }
   let (tx, rx) = mpsc::channel::<u64>();
   match std::thread::Builder::new()
      .name("sunbird-server-tooltips".to_owned())
      .spawn(move || render_requests(&rx))
   {
      Ok(_) => Some(WORKER.get_or_init(|| tx)),
      Err(err) => {
         if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!("server tooltip: no thread to render rows on: {err}");
         }
         None
      },
   }
}

/// Renders each server asked for, the ones asked for meanwhile in one batch.
fn render_requests(rx: &mpsc::Receiver<u64>) {
   while let Ok(guild) = rx.recv() {
      let mut batch = vec![guild];
      while let Ok(more) = rx.try_recv() {
         if !batch.contains(&more) {
            batch.push(more);
         }
      }
      for guild in batch {
         if ACTIVE.load(Ordering::Acquire) == 0 {
            break;
         }
         let generation = generation();
         let seen = updates(guild);
         let rows = phoenix_runtime::engine()
            .map_or_default(|engine| crate::js::ui::render_server_tooltip(&engine, guild));
         store(guild, rows, generation, Some(seen));
      }
   }
}

/// What the plugins returned for `guild`, asking them when unknown, or again
/// when its tooltip `opened` after another's, so a plugin hears of each server
/// the pointer moves to while the cached rows draw at once.
fn lookup(guild: u64, opened: bool) -> Option<Rows> {
   if let Ok(cache) = CACHE.read()
      && let Some(entry) = cache.get(&guild)
   {
      if opened && matches!(entry, Entry::Ready(_)) {
         let _ = worker().map(|tx| tx.send(guild));
      }
      return match entry {
         Entry::Ready(rows) if !rows.is_empty() => Some(rows.clone()),
         _ => None,
      };
   }
   let ask = CACHE.write().is_ok_and(|mut cache| {
      if cache.contains_key(&guild) {
         return false;
      }
      cache.insert(guild, Entry::Pending);
      true
   });
   if ask && worker().is_none_or(|tx| tx.send(guild).is_err()) {
      // Nothing renders it, so a later lookup asks again.
      if let Ok(mut cache) = CACHE.write() {
         cache.remove(&guild);
      }
   }
   None
}

extern "C" fn render(
   out: *mut RawAnyElement,
   this: *const u8,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement {
   // The jump is written just before `ORIGINAL` is set, all before Discord
   // runs.
   let original = *ORIGINAL.wait();
   // SAFETY: the trampoline runs the original with Phoenix's arguments.
   let returned = unsafe { original(out, this, window, cx) };
   if ACTIVE.load(Ordering::Acquire) == 0 || this.is_null() {
      return returned;
   }
   // SAFETY: the tooltip's `self` keeps the server's id at the decoded field.
   let guild = unsafe {
      this
         .offset(GUILD_FIELD.load(Ordering::Acquire))
         .cast::<u64>()
         .read_unaligned()
   };
   if guild < MIN_SNOWFLAKE {
      return returned;
   }
   SHOWN_AT.store(now_ms(), Ordering::Release);
   let opened = SHOWN.swap(guild, Ordering::AcqRel) != guild;
   if let Some(rows) = lookup(guild, opened) {
      // The render draws the voice channels and streams under the line, and
      // nothing for a server without any: an element's first word is a
      // pointer, so 0 is no element.
      // SAFETY: the original filled what `out` points to.
      let drawn = unsafe { out.cast::<usize>().read_unaligned() } != 0;
      gpui::wrap_with_rows(out, drawn, &rows, FONT_PX, Some(FONT_WEIGHT));
   }
   returned
}
