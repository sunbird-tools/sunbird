//! Cross-platform `ManaTheme` semantic theming engine for Desktop Phoenix.
//!
//! Hooks `gpui::App::set_global::<ManaTheme>` on macOS, Windows, and Linux,
//! self-tests Discord's `SemanticColors::get_mut` against a scratch table
//! sized from the binary, snapshots Discord's unmodified `ManaTheme` with
//! balanced `Arc` refcounts, mutates an 8-byte-aligned `SemanticColors` table
//! from pre-resolved palettes into a fixed rotating ring of owned buffers, and
//! installs it via tag 0 (static reference) so GPUI's `PartialEq` invalidates
//! cached views with zero atomic refcount overhead or unbounded heap leaks.

use std::{
   cell::Cell,
   collections::BTreeMap,
   ffi::c_void,
   fs,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Condvar,
      LazyLock,
      Mutex,
      OnceLock,
      PoisonError,
      atomic::{
         AtomicBool,
         AtomicU32,
         AtomicU64,
         AtomicUsize,
         Ordering,
      },
   },
   time::{
      Duration,
      SystemTime,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::{
   index::Index,
   locate::mana_theme::{
      self,
      ManaThemeLayout,
   },
};
use phoenix_themes::{
   Color,
   ResolvedPalette,
   ThemeConfig,
   ThemeMode,
   ThemeSpec,
   load_active_spec,
   load_config,
   read_contained_theme,
   save_config,
   valid_theme_file,
};
use serde::Serialize;

/// Number of rotating owned `SemanticColors` buffers kept alive for tag-0
/// publication. Consecutive `set_global` calls always receive distinct
/// pointers so GPUI's `PartialEq` invalidates cached views while total memory
/// stays strictly bounded.
const COLOR_RING_SLOTS: usize = 8;

type SetGlobalFn = unsafe extern "C" fn(*mut c_void, *mut u8);
type SyncPoolFn = unsafe extern "C" fn(*const c_void, *mut c_void) -> bool;
type SyncThemeFn = unsafe extern "C" fn(*mut c_void);
type DropInPlaceFn = unsafe extern "C" fn(*mut u8);
type GetMutFn = unsafe extern "C" fn(*mut u8, *const u8, usize) -> *mut u32;

struct Hooks {
   orig_set_global: SetGlobalFn,
   sync_pool:       SyncPoolFn,
   sync_theme:      SyncThemeFn,
   drop_in_place:   DropInPlaceFn,
   get_mut:         GetMutFn,
   size:            usize,
   colors_size:     usize,
   layout:          ManaThemeLayout,
   /// Harmless static placeholder installed into `BaseSnapshot`'s tag-0 slot
   /// so snapshotting never allocates or leaks a color table on Discord theme
   /// switches.
   dummy_colors:    Box<[u64]>,
}

impl Hooks {
   fn dummy_colors_ptr(&self) -> usize {
      self.dummy_colors.as_ptr().cast::<u8>() as usize
   }
}

/// Published atomically right after `hook_entry` so an early detour can always
/// forward `incoming` even before `HOOKS` is populated.
static ORIG_SET_GLOBAL: AtomicUsize = AtomicUsize::new(0);
static HOOKS: OnceLock<Hooks> = OnceLock::new();
static PENDING_APPLY: AtomicBool = AtomicBool::new(false);
/// Lock-free active `--radius-scale` factor (`f32::NAN` bits = unset), synced
/// from the resolved active mode's palette.
static RADIUS_SCALE: AtomicU32 = AtomicU32::new(f32::NAN.to_bits());

/// Lock-free cached `SunbirdPalette` RGBA words, refreshed once whenever
/// `active_colors` changes so GPUI render passes never lock `STATE` or call
/// `SemanticColors::get_mut`.
static PALETTE_CACHE: [AtomicU32; 9] = {
   let d = SunbirdPalette::DEFAULT.to_rgba_words();
   [
      AtomicU32::new(d[0]),
      AtomicU32::new(d[1]),
      AtomicU32::new(d[2]),
      AtomicU32::new(d[3]),
      AtomicU32::new(d[4]),
      AtomicU32::new(d[5]),
      AtomicU32::new(d[6]),
      AtomicU32::new(d[7]),
      AtomicU32::new(d[8]),
   ]
};

thread_local! {
   static IN_DETOUR: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard that restores `IN_DETOUR` even if an observer or drop unwinds.
struct DetourGuard(bool);

impl DetourGuard {
   fn enter() -> Self {
      let prev = IN_DETOUR.with(|flag| flag.replace(true));
      Self(prev)
   }
}

impl Drop for DetourGuard {
   fn drop(&mut self) {
      IN_DETOUR.with(|flag| flag.set(self.0));
   }
}

/// 8-byte-aligned owned byte buffer for dynamically sized `ManaTheme` and
/// `SemanticColors` copies.
#[derive(Clone)]
struct AlignedBuf {
   words: Vec<u64>,
   len:   usize,
}

impl AlignedBuf {
   fn filled(len: usize, fill: u8) -> Self {
      let word = u64::from_ne_bytes([fill; 8]);
      Self {
         words: vec![word; len.div_ceil(8)],
         len,
      }
   }

   fn from_bytes(bytes: &[u8]) -> Self {
      let mut words = vec![0u64; bytes.len().div_ceil(8)];
      // SAFETY: `words` has at least `bytes.len()` writable bytes.
      unsafe {
         std::ptr::copy_nonoverlapping(bytes.as_ptr(), words.as_mut_ptr().cast::<u8>(), bytes.len());
      }
      Self {
         words,
         len: bytes.len(),
      }
   }

   const fn len(&self) -> usize {
      self.len
   }

   const fn as_bytes(&self) -> &[u8] {
      // SAFETY: `self.len <= self.words.len() * 8`.
      unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.len) }
   }

   const fn as_bytes_mut(&mut self) -> &mut [u8] {
      // SAFETY: `self.len <= self.words.len() * 8`.
      unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(), self.len) }
   }

   const fn as_mut_ptr(&mut self) -> *mut u8 {
      self.words.as_mut_ptr().cast::<u8>()
   }
}

/// Pre-resolved dark and light palettes so the UI-thread detours never parse
/// CSS or expand `var()` chains under the state lock.
struct CachedSpec {
   dark:  ResolvedPalette,
   light: ResolvedPalette,
}

impl CachedSpec {
   fn from_spec(spec: &ThemeSpec) -> Option<Self> {
      let dark = spec.resolve(ThemeMode::Dark);
      let light = spec.resolve(ThemeMode::Light);
      (dark.is_active() || light.is_active()).then_some(Self { dark, light })
   }

   const fn for_mode(&self, base_mode: ThemeMode) -> &ResolvedPalette {
      match base_mode {
         ThemeMode::Dark => &self.dark,
         ThemeMode::Light => &self.light,
      }
   }
}

struct BaseSnapshot {
   /// Unmodified `ManaTheme` bytes with `colors_ref` pointed at
   /// `dummy_colors`, `pre_filter` preserved intact (with its `Arc` refcount
   /// balanced), and `gradient_tag` cleared to `gradient_none`.
   raw:                    AlignedBuf,
   /// Unmodified `SemanticColors` table.
   colors:                 AlignedBuf,
   /// Detected dark/light mode of Discord's base theme.
   base_mode:              ThemeMode,
   /// Discord's original `Option<GradientTheme>` discriminant byte so clearing
   /// a Sunbird theme restores Nitro gradients cleanly.
   orig_gradient_tag:      u8,
   /// Discord's original static (`tag == 0`) `SemanticColors` pointer when the
   /// base theme was static, restored on clear so Discord's own pointer is back.
   orig_static_colors_ptr: Option<usize>,
}

struct ThemeState {
   home:              Option<PathBuf>,
   config:            ThemeConfig,
   active_spec:       Option<CachedSpec>,
   /// Monotonic commit counter advanced under `STATE` on every theme/watch
   /// mutation so stale reloads or overlapping applies cannot win out of order.
   generation:        u64,
   /// Whether an ephemeral JS override (`setCss`) is
   /// currently displayed on screen instead of the persisted `themes.json`
   /// stack.
   ephemeral_active:  bool,
   base:              Option<BaseSnapshot>,
   /// Whether the currently installed `ManaTheme` global is a Sunbird-patched
   /// copy (so a no-op clear does not overwrite an unmodified Nitro gradient).
   patched_installed: bool,
   /// Most recently installed `SemanticColors` bytes (themed or base), for
   /// live token inspection and Sunbird UI chrome.
   active_colors:     Option<AlignedBuf>,
   active_mode:       ThemeMode,
   /// Fixed rotating ring of 8-byte-aligned owned `SemanticColors` buffers
   /// handed to Discord via tag 0. Slots are allocated once and never move.
   color_ring:          Vec<Box<[u64]>>,
   ring_next:           usize,
   /// Last `--radius-scale` bit pattern flushed on the UI thread, so
   /// radius-only edits still trigger a GPUI window refresh.
   flushed_radius_bits: u32,
}

impl Default for ThemeState {
   fn default() -> Self {
      Self {
         home:                None,
         config:              ThemeConfig::default(),
         active_spec:         None,
         generation:          0,
         ephemeral_active:    false,
         base:                None,
         patched_installed:   false,
         active_colors:       None,
         active_mode:         ThemeMode::Dark,
         color_ring:          Vec::new(),
         ring_next:           0,
         flushed_radius_bits: f32::NAN.to_bits(),
      }
   }
}

static STATE: LazyLock<Mutex<ThemeState>> = LazyLock::new(Mutex::default);

/// Typed JSON payload returned by `/themes/tokens`.
#[derive(Serialize)]
pub struct DebugTokens {
   pub supported: bool,
   pub mode:      &'static str,
   pub active:    Option<String>,
   pub tokens:    BTreeMap<String, String>,
}

/// Bounds- and alignment-checks every pointer returned by Discord's `get_mut`
/// against `table` before any caller may read or write through it.
fn lookup_token_mut(get_mut: GetMutFn, table: &mut AlignedBuf, name: &str) -> Option<*mut u32> {
   let len = table.len();
   let base = table.as_mut_ptr() as usize;
   // SAFETY: `table` is an 8-byte-aligned `SemanticColors` buffer sized from
   // the binary and `get_mut` was self-tested during `install()`.
   let ptr = unsafe { get_mut(table.as_mut_ptr(), name.as_ptr(), name.len()) };
   let addr = ptr as usize;
   let offset = addr.checked_sub(base)?;
   if offset + 4 <= len && offset.is_multiple_of(4) {
      Some(ptr)
   } else {
      None
   }
}

/// Self-tests the located `get_mut` against a sentinel scratch table before
/// installing any hook: a known token must return a 4-aligned pointer inside
/// the decoded `colors_size` table, an unknown token must return null, and a
/// sentinel write must touch only that 4-byte slot.
fn validate_get_mut(get_mut: GetMutFn, colors_size: usize) -> bool {
   const FILL: u8 = 0x5a;
   const SENTINEL: u32 = 0xdead_beef;
   let mut table = AlignedBuf::filled(colors_size, FILL);
   let Some(slot) = lookup_token_mut(get_mut, &mut table, "background-base-low") else {
      return false;
   };
   let offset = slot as usize - table.as_mut_ptr() as usize;
   if lookup_token_mut(get_mut, &mut table, "__sunbird_nonexistent_token__").is_some() {
      return false;
   }
   // SAFETY: `lookup_token_mut` verified `slot` is 4-aligned and inside `table`.
   unsafe { slot.write(SENTINEL) };
   // SAFETY: same verified slot.
   if unsafe { slot.read() } != SENTINEL {
      return false;
   }
   let bytes = table.as_bytes();
   let before_ok = bytes[..offset].iter().all(|&b| b == FILL);
   let after_ok = bytes[offset + 4..].iter().all(|&b| b == FILL);
   before_ok && after_ok
}

const fn layout_within_bounds(size: usize, colors_size: usize, layout: ManaThemeLayout) -> bool {
   size >= 0x30
      && colors_size >= 0x400
      && colors_size.is_multiple_of(8)
      && layout.theme_ref + 16 <= size
      && layout.colors_ref + 16 <= size
      && layout.pre_filter + 16 <= size
      && layout.base_theme < size
      && layout.gradient_tag < size
}

/// Locates, self-tests, and hooks `App::set_global::<ManaTheme>` during
/// `prepare()`.
pub fn install(image: &Image<'static>, ix: &Index<'_>) {
   let found = match mana_theme::mana_theme(ix) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("mana theme: {err}, semantic themes disabled");
         return;
      },
   };
   if !layout_within_bounds(found.size, found.colors_size, found.layout) {
      log::warn!(
         "mana theme: decoded layout out of bounds (size {:#x}, colors {:#x}, {:?}), semantic \
          themes disabled",
         found.size,
         found.colors_size,
         found.layout
      );
      return;
   }
   // SAFETY: `mana_theme` verified `sync_pool`, `sync_theme`, `drop_in_place`,
   // and `get_mut` are code entrypoints; `validate_get_mut` verifies `get_mut`'s ABI.
   let (sync_pool, sync_theme, drop_in_place, get_mut): (
      SyncPoolFn,
      SyncThemeFn,
      DropInPlaceFn,
      GetMutFn,
   ) = unsafe {
      (
         std::mem::transmute::<usize, SyncPoolFn>(found.sync_pool),
         std::mem::transmute::<usize, SyncThemeFn>(found.sync_theme),
         std::mem::transmute::<usize, DropInPlaceFn>(found.drop_in_place),
         std::mem::transmute::<usize, GetMutFn>(found.get_mut),
      )
   };
   let self_test_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      validate_get_mut(get_mut, found.colors_size)
   }))
   .unwrap_or(false);
   if !self_test_ok {
      log::warn!(
         "mana theme: get_mut @ {:#x} failed scratch-table self-test, semantic themes disabled",
         found.get_mut
      );
      return;
   }
   // SAFETY: runs during `prepare` before Discord's threads reach `set_global`,
   // and `set_global_detour` matches `App::set_global::<ManaTheme>`.
   let trampoline = match unsafe {
      image.hook_entry(found.set_global, set_global_detour as *const () as usize)
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("mana theme set_global: {err}");
         return;
      },
   };
   ORIG_SET_GLOBAL.store(trampoline, Ordering::Release);
   // SAFETY: `trampoline` runs `set_global`'s displaced instructions.
   let orig_set_global = unsafe { std::mem::transmute::<usize, SetGlobalFn>(trampoline) };
   let dummy_colors = vec![0u64; found.colors_size.div_ceil(8)].into_boxed_slice();
   let _ = HOOKS.set(Hooks {
      orig_set_global,
      sync_pool,
      sync_theme,
      drop_in_place,
      get_mut,
      size: found.size,
      colors_size: found.colors_size,
      layout: found.layout,
      dummy_colors,
   });
   log::info!(
      "mana theme: hooked set_global @ {:#x} (size {:#x}, colors {:#x}, get_mut {:#x})",
      found.set_global,
      found.size,
      found.colors_size,
      found.get_mut
   );
}

/// Whether the cross-platform `ManaTheme` semantic engine is active and has at
/// least one live UI-thread drain site.
#[must_use]
pub fn is_supported() -> bool {
   HOOKS.get().is_some() && (crate::modal::hooked() || crate::settings::hooked())
}

static WATCH_GEN: AtomicU64 = AtomicU64::new(0);
static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);
static WATCH_STATE: LazyLock<(Mutex<WatchSlot>, Condvar)> =
   LazyLock::new(|| (Mutex::new(WatchSlot::default()), Condvar::new()));

/// Lightweight metadata stamp (`mtime` + byte length) used by the watcher so
/// idle polling does zero file reads or hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
   modified: SystemTime,
   len:      u64,
}

#[derive(Default)]
struct WatchSlot {
   /// Highest commit generation published to the watcher slot so an older
   /// overlapping commit can never overwrite a newer watch or clear.
   generation: u64,
   watched:    Option<WatchedTheme>,
}

#[derive(Clone, Debug)]
struct WatchedTheme {
   themes_dir: PathBuf,
   file:       String,
   persisted:  bool,
   generation: u64,
   /// Metadata stamp of the contained file that was last applied.
   stamp:      Option<FileStamp>,
   /// A changed stamp seen on the previous poll, applied once the next poll
   /// sees it unchanged.
   pending:    Option<FileStamp>,
}

/// Reads metadata for a theme file strictly contained within `themes_dir`
/// (rejecting traversal and symlinks escaping `themes_dir`) without reading
/// file contents.
fn file_stamp(themes_dir: &Path, file: &str) -> Option<FileStamp> {
   if !valid_theme_file(file) {
      return None;
   }
   let canonical_dir = fs::canonicalize(themes_dir).ok()?;
   let canonical_file = fs::canonicalize(themes_dir.join(file)).ok()?;
   if !canonical_file.starts_with(&canonical_dir) || !canonical_file.is_file() {
      return None;
   }
   let meta = fs::metadata(&canonical_file).ok()?;
   Some(FileStamp {
      modified: meta.modified().ok()?,
      len:      meta.len(),
   })
}

/// Reads and parses a contained theme file, capturing its metadata stamp
/// before the read so an edit landing during the read is never missed.
/// Rejects an empty file so a save that truncates first never wipes the
/// active theme.
fn read_theme_file_stamped(
   themes_dir: &Path,
   file: &str,
) -> Result<(ThemeSpec, Option<FileStamp>), String> {
   let stamp = file_stamp(themes_dir, file);
   let raw = read_contained_theme(themes_dir, file)?;
   if file_stamp(themes_dir, file) != stamp {
      return Err(format!("theme {file} changed while reading"));
   }
   if raw.trim().is_empty() {
      return Err(format!("theme {file} is empty"));
   }
   let spec = ThemeSpec::parse_file(file, &raw)?;
   Ok((spec, stamp))
}

fn commit_generation(state: &mut ThemeState) -> u64 {
   state.generation = state.generation.saturating_add(1);
   WATCH_GEN.store(state.generation, Ordering::Release);
   state.generation
}

/// Publishes a watch update for `gen_id`, refusing any stale commit older than
/// the slot's current generation.
fn publish_watch(gen_id: u64, next: Option<WatchedTheme>) {
   let (lock, cvar) = &*WATCH_STATE;
   let mut slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
   if gen_id < slot.generation {
      return;
   }
   let should_spawn = next.is_some();
   slot.generation = gen_id;
   slot.watched = next;
   cvar.notify_one();
   drop(slot);
   if should_spawn
      && !WATCHER_STARTED.swap(true, Ordering::AcqRel)
      && let Err(err) = std::thread::Builder::new()
         .name("sunbird-theme-watch".to_owned())
         .spawn(watch_loop)
   {
      WATCHER_STARTED.store(false, Ordering::Release);
      log::warn!("mana theme: could not spawn hot-reload watcher: {err}");
   }
}

struct WatcherExitGuard;

impl Drop for WatcherExitGuard {
   fn drop(&mut self) {
      WATCHER_STARTED.store(false, Ordering::Release);
   }
}

/// Runs one metadata check for `watched`. A changed file applies only once
/// its `mtime` and length held for a whole poll interval, so a save still
/// being written is never read.
fn poll_watched_once(watched: &WatchedTheme) {
   let current = file_stamp(&watched.themes_dir, &watched.file);
   if current.is_none() || current == watched.stamp {
      return;
   }
   let applied = if current == watched.pending {
      match reload_watched(watched) {
         Ok((_, stamp)) => Some(stamp),
         Err(err) => {
            log::warn!("mana theme: hot-reload skipped for {}: {err}", watched.file);
            Some(current)
         },
      }
   } else {
      None
   };
   let (lock, _) = &*WATCH_STATE;
   let mut slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(active) = slot
      .watched
      .as_mut()
      .filter(|active| active.generation == watched.generation)
   {
      match applied {
         Some(stamp) => {
            active.stamp = stamp;
            active.pending = None;
         },
         None => active.pending = current,
      }
   }
   drop(slot);
}

fn watch_loop() {
   const POLL_INTERVAL: Duration = Duration::from_millis(600);
   let _exit_guard = WatcherExitGuard;
   let (lock, cvar) = &*WATCH_STATE;
   loop {
      let watched = {
         let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
         while guard.watched.is_none() {
            guard = cvar.wait(guard).unwrap_or_else(PoisonError::into_inner);
         }
         let (next_guard, _) = cvar
            .wait_timeout(guard, POLL_INTERVAL)
            .unwrap_or_else(PoisonError::into_inner);
         guard = next_guard;
         guard.watched.clone()
      };
      if let Some(watched) = watched {
         poll_watched_once(&watched);
      }
   }
}

fn reload_watched(watched: &WatchedTheme) -> Result<(bool, Option<FileStamp>), String> {
   let (parsed, stamp) = read_theme_file_stamped(&watched.themes_dir, &watched.file)?;
   let combined = if watched.persisted {
      let quick_css = STATE
         .lock()
         .unwrap_or_else(PoisonError::into_inner)
         .config
         .quick_css
         .clone();
      if quick_css.trim().is_empty() {
         parsed
      } else {
         parsed.merge(&ThemeSpec::parse_css(&quick_css))
      }
   } else {
      parsed
   };
   let cached = CachedSpec::from_spec(&combined);
   let active = cached.is_some();
   let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
   if state.generation != watched.generation
      || WATCH_GEN.load(Ordering::Acquire) != watched.generation
   {
      return Ok((false, stamp));
   }
   sync_radius_scale_for_state(&state, cached.as_ref());
   state.active_spec = cached;
   drop(state);
   request_apply();
   Ok((active, stamp))
}

fn make_watch(
   themes_dir: PathBuf,
   file: &str,
   persisted: bool,
   generation: u64,
   stamp: Option<FileStamp>,
) -> Option<WatchedTheme> {
   valid_theme_file(file).then(|| {
      WatchedTheme {
         themes_dir,
         file: file.to_owned(),
         persisted,
         generation,
         stamp,
         pending: None,
      }
   })
}

/// Loads `<home>/themes.json` and the active theme file on boot so themes
/// apply on frame 1 when Discord initializes `ManaTheme`.
pub fn load_persisted(home: &Path) {
   let config_path = home.join("themes.json");
   let themes_dir = home.join("themes");
   let config = load_config(&config_path);
   let initial_stamp = config
      .active
      .as_deref()
      .and_then(|file| file_stamp(&themes_dir, file));
   let loaded = load_active_spec(&themes_dir, &config);
   let cached = loaded.as_ref().and_then(CachedSpec::from_spec);
   if loaded.is_some() && cached.is_none() {
      log::warn!(
         "mana theme: saved theme {:?} sets no supported theme tokens, ignoring",
         config.active
      );
   }
   let active_file = config.active.clone();
   let (gen_id, should_flush) = {
      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      let gen_id = commit_generation(&mut state);
      state.home = Some(home.to_owned());
      state.config = config;
      state.ephemeral_active = false;
      sync_radius_scale_for_state(&state, cached.as_ref());
      state.active_spec = cached;
      (gen_id, state.base.is_some())
   };
   let watch = active_file
      .as_deref()
      .and_then(|file| make_watch(themes_dir, file, true, gen_id, initial_stamp));
   publish_watch(gen_id, watch);
   if should_flush {
      request_apply();
   }
}

/// Applies raw `.theme.css` text live without changing persisted `themes.json`
/// (used by `phoenix.themes.setCss`).
pub fn set_css(css: &str) -> bool {
   let cached = CachedSpec::from_spec(&ThemeSpec::parse_css(css));
   let active = cached.is_some();
   let gen_id = {
      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      let gen_id = commit_generation(&mut state);
      state.ephemeral_active = true;
      sync_radius_scale_for_state(&state, cached.as_ref());
      state.active_spec = cached;
      gen_id
   };
   publish_watch(gen_id, None);
   request_apply();
   active
}

/// Applies a theme file from `themes_dir` and saves it as the active theme in
/// `themes.json`, so [`load_persisted`] brings it back at startup (used by JS
/// `phoenix.themes.apply`).
pub fn apply_file(themes_dir: &Path, file: &str) -> Result<bool, String> {
   let (parsed, stamp) = read_theme_file_stamped(themes_dir, file)?;
   if !parsed.is_active() {
      return Err(format!("theme {file} sets no supported tokens"));
   }
   let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
   let combined = if state.config.quick_css.trim().is_empty() {
      parsed
   } else {
      parsed.merge(&ThemeSpec::parse_css(&state.config.quick_css))
   };
   let cached = CachedSpec::from_spec(&combined);
   let gen_id = commit_generation(&mut state);
   state.ephemeral_active = false;
   state.config.active = Some(file.to_owned());
   sync_radius_scale_for_state(&state, cached.as_ref());
   state.active_spec = cached;
   let save = state
      .home
      .as_ref()
      .map(|home| (home.join("themes.json"), state.config.clone()));
   drop(state);
   let watch = make_watch(themes_dir.to_owned(), file, true, gen_id, stamp);
   publish_watch(gen_id, watch);
   request_apply();
   if let Some((path, config)) = save {
      save_config(&path, &config).map_err(|err| format!("saving themes.json: {err}"))?;
   }
   Ok(true)
}

/// Drops the saved active theme from `themes.json`, keeping quick CSS, so a
/// theme a plugin replaced or cleared stays off at the next startup.
pub fn forget_saved() {
   let save = {
      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      let had = state.config.active.take().is_some();
      state
         .home
         .as_ref()
         .filter(|_| had)
         .map(|home| (home.join("themes.json"), state.config.clone()))
   };
   if let Some((path, config)) = save
      && let Err(err) = save_config(&path, &config)
   {
      log::warn!("mana theme: saving themes.json: {err}");
   }
}

/// Drops a plugin's ephemeral override and restores whatever `themes.json` has
/// active (or Discord's unmodified palette if none).
pub fn restore_persisted() {
   let (home, cfg) = {
      let state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      (state.home.clone(), state.config.clone())
   };
   let initial_stamp = home
      .as_ref()
      .zip(cfg.active.as_deref())
      .and_then(|(h, file)| file_stamp(&h.join("themes"), file));
   let cached = home
      .as_ref()
      .and_then(|h| load_active_spec(&h.join("themes"), &cfg))
      .and_then(|s| CachedSpec::from_spec(&s));
   let gen_id = {
      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      let gen_id = commit_generation(&mut state);
      state.ephemeral_active = false;
      sync_radius_scale_for_state(&state, cached.as_ref());
      state.active_spec = cached;
      gen_id
   };
   let watch = home
      .as_ref()
      .zip(cfg.active.as_deref())
      .and_then(|(h, file)| make_watch(h.join("themes"), file, true, gen_id, initial_stamp));
   publish_watch(gen_id, watch);
   request_apply();
}

/// Updates persisted `quick_css` in `themes.json` and re-applies it layered on
/// top of any active persisted theme. Leaves an active ephemeral JS override
/// untouched on screen while still persisting `quick_css`.
pub fn set_quick_css(css: &str) -> Result<(), String> {
   for _ in 0..3 {
      let (home, mut cfg) = {
         let state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
         let Some(home) = state.home.clone() else {
            return Err("sunbird home directory is not initialized".to_owned());
         };
         (home, state.config.clone())
      };
      css.clone_into(&mut cfg.quick_css);
      let themes_dir = home.join("themes");
      let initial_stamp = cfg
         .active
         .as_deref()
         .and_then(|file| file_stamp(&themes_dir, file));
      let cached = load_active_spec(&themes_dir, &cfg).and_then(|s| CachedSpec::from_spec(&s));

      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      if state.home.as_deref() != Some(home.as_path()) || state.config.active != cfg.active {
         continue;
      }
      css.clone_into(&mut state.config.quick_css);
      let save = (home.join("themes.json"), state.config.clone());
      let apply_now = if state.ephemeral_active {
         None
      } else {
         let gen_id = commit_generation(&mut state);
         sync_radius_scale_for_state(&state, cached.as_ref());
         state.active_spec = cached;
         let watch = cfg
            .active
            .as_deref()
            .and_then(|file| make_watch(themes_dir, file, true, gen_id, initial_stamp));
         Some((gen_id, watch))
      };
      drop(state);
      if let Some((gen_id, watch)) = apply_now {
         publish_watch(gen_id, watch);
         request_apply();
      }
      if let Err(err) = save_config(&save.0, &save.1) {
         log::warn!("mana theme: saving themes.json: {err}");
         return Err(format!("saving themes.json: {err}"));
      }
      return Ok(());
   }
   log::warn!("mana theme: set_quick_css aborted after concurrent theme switches");
   Err("concurrent theme switch while saving quick CSS".to_owned())
}

/// Returns the currently persisted `quick_css` string from `themes.json`.
#[must_use]
pub fn quick_css() -> String {
   STATE
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .config
      .quick_css
      .clone()
}

/// Immediately reloads the currently watched theme file from disk.
///
/// Used by `POST /themes/reload` and `phoenix.themes.reload()`. Returns
/// `Ok(None)` without disturbing an inline `setCss` override when no file is
/// watched, or `Ok(Some(active))` when a watched file was reloaded.
pub fn reload_active() -> Result<Option<bool>, String> {
   let watched = WATCH_STATE
      .0
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .watched
      .clone();
   let Some(watched) = watched else {
      return Ok(None);
   };
   let (active, stamp) = reload_watched(&watched)?;
   let mut slot = WATCH_STATE
      .0
      .lock()
      .unwrap_or_else(PoisonError::into_inner);
   if let Some(w) = slot.watched.as_mut()
      && w.generation == watched.generation
   {
      w.stamp = stamp;
      w.pending = None;
   }
   drop(slot);
   Ok(Some(active))
}

fn sync_radius_scale(scale: Option<f32>) -> u32 {
   let bits = scale
      .filter(|s| s.is_finite())
      .map_or(f32::NAN.to_bits(), f32::to_bits);
   RADIUS_SCALE.store(bits, Ordering::Relaxed);
   bits
}

fn sync_radius_scale_for_state(state: &ThemeState, cached: Option<&CachedSpec>) {
   let base_mode = state
      .base
      .as_ref()
      .map_or(state.active_mode, |base| base.base_mode);
   let scale = cached.and_then(|c| c.for_mode(base_mode).radius_scale);
   let _ = sync_radius_scale(scale);
}

/// Lock-free active `--radius-scale` multiplier, or `None` when unset.
#[must_use]
pub fn radius_scale() -> Option<f32> {
   let val = f32::from_bits(RADIUS_SCALE.load(Ordering::Relaxed));
   val.is_finite().then_some(val)
}

/// Live semantic colors for Sunbird's own GPUI chrome (settings cards,
/// dividers, modals, code blocks, and notification banners).
#[derive(Clone, Copy, Debug)]
pub struct SunbirdPalette {
   pub bg_surface: Color,
   pub bg_code:    Color,
   pub border:     Color,
   pub text:       Color,
   pub text_muted: Color,
   pub accent:     Color,
   pub danger:     Color,
   pub positive:   Color,
   /// Discord's own interactive icons, like the chat bar's.
   pub icon:       Color,
}

impl SunbirdPalette {
   const DEFAULT: Self = Self {
      bg_surface: Color::rgb(0x2B, 0x2D, 0x31),
      bg_code:    Color::rgb(0x12, 0x14, 0x1A),
      border:     Color::rgb(0x3F, 0x41, 0x47),
      text:       Color::rgb(0xF2, 0xF3, 0xF5),
      text_muted: Color::rgb(0xB5, 0xBA, 0xC1),
      accent:     Color::rgb(0x58, 0x65, 0xF2),
      danger:     Color::rgb(0xFF, 0x7B, 0x72),
      positive:   Color::rgb(0x7E, 0xE7, 0x87),
      icon:       Color::rgb(0xB5, 0xBA, 0xC1),
   };

   const fn to_rgba_words(self) -> [u32; 9] {
      [
         self.bg_surface.to_rgba_u32(),
         self.bg_code.to_rgba_u32(),
         self.border.to_rgba_u32(),
         self.text.to_rgba_u32(),
         self.text_muted.to_rgba_u32(),
         self.accent.to_rgba_u32(),
         self.danger.to_rgba_u32(),
         self.positive.to_rgba_u32(),
         self.icon.to_rgba_u32(),
      ]
   }
}

impl Default for SunbirdPalette {
   fn default() -> Self {
      Self::DEFAULT
   }
}

/// Converts a `phoenix_themes::Color` to GPUI's `[h, s, l, a]` tuple, with an
/// optional alpha override.
#[must_use]
pub fn color_to_hsla(color: Color, alpha: Option<f32>) -> [f32; 4] {
   let hsla = color.to_hsla();
   [hsla.h, hsla.s, hsla.l, alpha.unwrap_or(hsla.a)]
}

/// Packs a `phoenix_themes::Color` as `0xAARRGGBB`.
#[must_use]
pub const fn color_to_argb(color: Color) -> u32 {
   ((color.a as u32) << 24)
      | ((color.r as u32) << 16)
      | ((color.g as u32) << 8)
      | (color.b as u32)
}

/// Lock-free read of Sunbird's cached live UI palette (zero mutex or `get_mut`
/// work on the render thread).
#[must_use]
pub fn sunbird_palette() -> SunbirdPalette {
   SunbirdPalette {
      bg_surface: Color::from_rgba_u32(PALETTE_CACHE[0].load(Ordering::Relaxed)),
      bg_code:    Color::from_rgba_u32(PALETTE_CACHE[1].load(Ordering::Relaxed)),
      border:     Color::from_rgba_u32(PALETTE_CACHE[2].load(Ordering::Relaxed)),
      text:       Color::from_rgba_u32(PALETTE_CACHE[3].load(Ordering::Relaxed)),
      text_muted: Color::from_rgba_u32(PALETTE_CACHE[4].load(Ordering::Relaxed)),
      accent:     Color::from_rgba_u32(PALETTE_CACHE[5].load(Ordering::Relaxed)),
      danger:     Color::from_rgba_u32(PALETTE_CACHE[6].load(Ordering::Relaxed)),
      positive:   Color::from_rgba_u32(PALETTE_CACHE[7].load(Ordering::Relaxed)),
      icon:       Color::from_rgba_u32(PALETTE_CACHE[8].load(Ordering::Relaxed)),
   }
}

fn store_palette_cache(pal: SunbirdPalette) {
   for (slot, word) in PALETTE_CACHE.iter().zip(pal.to_rgba_words()) {
      slot.store(word, Ordering::Relaxed);
   }
}

fn refresh_palette_cache(hooks: &Hooks, colors: &AlignedBuf) {
   let fallback = SunbirdPalette::default();
   let mut table = colors.clone();
   let mut pick = |names: &[&str], default: Color| -> Color {
      for name in names {
         if let Some(ptr) = lookup_token_mut(hooks.get_mut, &mut table, name) {
            // SAFETY: `lookup_token_mut` verified `ptr` is 4-aligned inside `table`.
            return Color::from_rgba_u32(unsafe { ptr.read() });
         }
      }
      default
   };
   store_palette_cache(SunbirdPalette {
      bg_surface: pick(
         &["background-surface-high", "background-base-lower"],
         fallback.bg_surface,
      ),
      bg_code:    pick(
         &["background-base-lowest", "background-base-lower"],
         fallback.bg_code,
      ),
      border:     pick(&["border-subtle", "border-normal"], fallback.border),
      text:       pick(&["text-default", "text-normal"], fallback.text),
      text_muted: pick(&["text-muted", "text-subtle"], fallback.text_muted),
      accent:     pick(&["background-brand", "brand-500"], fallback.accent),
      danger:     pick(
         &["text-feedback-critical", "status-danger"],
         fallback.danger,
      ),
      positive:   pick(
         &["text-feedback-positive", "status-positive"],
         fallback.positive,
      ),
      icon:       pick(
         &["interactive-icon-default", "interactive-normal"],
         fallback.icon,
      ),
   });
}

/// Currently persisted active theme filename from `themes.json`, if any.
#[must_use]
pub fn active_file() -> Option<String> {
   STATE
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .config
      .active
      .clone()
}

/// Reads a single live semantic token color by its Mana name (e.g.
/// `"background-base-low"`).
#[must_use]
pub fn read_token(name: &str) -> Option<Color> {
   let hooks = HOOKS.get()?;
   let mut table = STATE
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .active_colors
      .clone()?;
   let ptr = lookup_token_mut(hooks.get_mut, &mut table, name)?;
   // SAFETY: `lookup_token_mut` verified `ptr` is 4-aligned and inside `table`.
   Some(Color::from_rgba_u32(unsafe { ptr.read() }))
}

/// Snapshot of live theme status and key resolved tokens for `/themes/tokens`.
#[must_use]
pub fn debug_tokens(requested: Option<&[String]>) -> DebugTokens {
   const DEFAULT_TOKENS: &[&str] = &[
      "background-base-lowest",
      "background-base-lower",
      "background-base-low",
      "background-surface-high",
      "background-surface-higher",
      "background-brand",
      "text-default",
      "text-strong",
      "text-muted",
      "text-brand",
      "border-subtle",
      "border-normal",
      "button-filled-brand-background",
      "button-filled-brand-text",
      "chat-input-container-background",
   ];

   let (mode, active) = {
      let state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      (
         match state.active_mode {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
         },
         state.config.active.clone(),
      )
   };

   let mut tokens = BTreeMap::new();
   if let Some(names) = requested.filter(|slice| !slice.is_empty()) {
      for name in names {
         if let Some(color) = read_token(name) {
            tokens.insert(name.clone(), color.to_hex());
         }
      }
   } else {
      for &name in DEFAULT_TOKENS {
         if let Some(color) = read_token(name) {
            tokens.insert(name.to_owned(), color.to_hex());
         }
      }
   }

   DebugTokens {
      supported: is_supported(),
      mode,
      active,
      tokens,
   }
}

fn request_apply() {
   if HOOKS.get().is_some() {
      PENDING_APPLY.store(true, Ordering::Release);
      crate::modal::force_redraw();
   }
}

/// Drains any pending off-thread theme change on the GPUI UI thread with live
/// `&mut App` (`app`). Called from `overlay_render_detour` and `section_detour`.
///
/// # Safety
///
/// `app` must be null or a valid live `&mut gpui::App` pointer on the UI thread.
pub unsafe fn flush_pending(app: *mut c_void) {
   if app.is_null() || !PENDING_APPLY.load(Ordering::Acquire) {
      return;
   }
   let Some(hooks) = HOOKS.get() else {
      return;
   };
   if !PENDING_APPLY.swap(false, Ordering::AcqRel) {
      return;
   }

   let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      flush_pending_inner(hooks, app);
   }));
   if result.is_err() {
      log::error!("mana theme: panic inside flush_pending");
   }
}

fn flush_pending_inner(hooks: &Hooks, app: *mut c_void) {
   let built = {
      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      let Some(base) = &state.base else {
         return;
      };
      let base_mode = base.base_mode;
      let (colors, mode, hits, radius) =
         build_active_colors(hooks, base, state.active_spec.as_ref());
      let radius_bits = sync_radius_scale(radius);
      let radius_changed = radius_bits != state.flushed_radius_bits;
      // Avoid replacing an unmodified theme on a no-op clear or a theme whose
      // custom variables matched zero Mana tokens and did not change radius.
      if hits == 0 && !state.patched_installed && !radius_changed {
         refresh_palette_cache(hooks, &colors);
         state.flushed_radius_bits = radius_bits;
         state.active_colors = Some(colors);
         state.active_mode = base_mode;
         return;
      }
      let base_raw = base.raw.clone();
      if radius_changed {
         // A redraw request does not show new radii. `force_redraw` and GPUI's
         // `App::refresh_windows` both repaint nothing until some primitive
         // changes. A table one bit off in every color repaints this frame
         // like a color edit, and the next flush installs the real one.
         let mut nudged = colors;
         for word in &mut nudged.words {
            *word ^= 0x0000_0001_0000_0001;
         }
         let theme = make_patched_theme(hooks, &mut state, &base_raw, &nudged);
         state.flushed_radius_bits = radius_bits;
         state.patched_installed = true;
         drop(state);
         PENDING_APPLY.store(true, Ordering::Release);
         return install_theme(hooks, app, theme);
      }
      let patched = if hits > 0 {
         make_patched_theme(hooks, &mut state, &base_raw, &colors)
      } else {
         let orig_grad = base.orig_gradient_tag;
         let orig_static = base.orig_static_colors_ptr;
         make_restored_theme(hooks, &mut state, &base_raw, &colors, orig_grad, orig_static)
      };
      refresh_palette_cache(hooks, &colors);
      state.flushed_radius_bits = radius_bits;
      state.active_colors = Some(colors);
      state.active_mode = mode;
      state.patched_installed = hits > 0;
      drop(state);
      patched
   };
   install_theme(hooks, app, built);
}

/// Installs `theme` as the `ManaTheme` global on the UI thread's `app`.
fn install_theme(hooks: &Hooks, app: *mut c_void, mut theme: AlignedBuf) {
   let _guard = DetourGuard::enter();
   // SAFETY: `app` is a live `&mut gpui::App` on the UI thread, and `theme` is
   // a valid owned `ManaTheme` whose `Arc` refcounts were balanced by
   // `clone_base_raw`.
   unsafe { (hooks.orig_set_global)(app, theme.as_mut_ptr()) };
   // Discord switches themes with `set_global`, `sync_theme`, then
   // `sync_pool`. When `sync_pool` returns false Discord redraws every window,
   // which `force_redraw` does here either way.
   // SAFETY: `app` is a live `&mut gpui::App` on the UI thread.
   unsafe { (hooks.sync_theme)(app) };
   // SAFETY: `app` is a live `&mut gpui::App` on the UI thread; null `arg0`
   // means no per-window override (matching every Discord caller).
   unsafe { (hooks.sync_pool)(std::ptr::null(), app) };
   crate::modal::force_redraw();
}

unsafe extern "C" fn set_global_detour(app: *mut c_void, incoming: *mut u8) {
   let Some(hooks) = HOOKS.get() else {
      // Wait briefly if another thread entered the hook in the tiny window
      // between `hook_entry` writing the jump and storing the trampoline.
      let mut orig = ORIG_SET_GLOBAL.load(Ordering::Acquire);
      while orig == 0 {
         std::thread::yield_now();
         orig = ORIG_SET_GLOBAL.load(Ordering::Acquire);
      }
      // SAFETY: `orig` is the published trampoline for `set_global`.
      let orig_fn: SetGlobalFn = unsafe { std::mem::transmute::<usize, SetGlobalFn>(orig) };
      unsafe { orig_fn(app, incoming) };
      return;
   };
   if incoming.is_null() || IN_DETOUR.with(Cell::get) {
      // SAFETY: forwarding untouched arguments to original `set_global`.
      unsafe { (hooks.orig_set_global)(app, incoming) };
      return;
   }

   let mut consumed = false;
   let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      handle_set_global(hooks, app, incoming, &mut consumed);
   }));
   if result.is_err() {
      log::error!("mana theme: panic inside set_global_detour");
      if !consumed {
         let _guard = DetourGuard::enter();
         // SAFETY: `incoming` was neither dropped nor forwarded yet.
         unsafe { (hooks.orig_set_global)(app, incoming) };
      }
   }
}

fn valid_theme_ref(hooks: &Hooks, incoming: &[u8]) -> bool {
   let tag = read_u64(incoming, hooks.layout.theme_ref);
   match tag {
      0 => true,
      1 => read_usize(incoming, hooks.layout.theme_ref + 8) != 0,
      _ => false,
   }
}

/// Returns whether `incoming`'s `colors_ref` points to one of Sunbird's own
/// published `color_ring` slots (or `dummy_colors`), which happens when
/// Discord clones the currently installed `ManaTheme` to update a non-color
/// field and passes it back through `set_global`.
fn is_own_color_ptr(hooks: &Hooks, state: &ThemeState, incoming: &[u8]) -> bool {
   let tag = read_u64(incoming, hooks.layout.colors_ref);
   if tag != 0 {
      return false;
   }
   let ptr = read_usize(incoming, hooks.layout.colors_ref + 8);
   ptr == hooks.dummy_colors_ptr()
      || state
         .color_ring
         .iter()
         .any(|slot| slot.as_ptr().cast::<u8>() as usize == ptr)
}

fn handle_set_global(hooks: &Hooks, app: *mut c_void, incoming: *mut u8, consumed: &mut bool) {
   // SAFETY: `incoming` points to a live `hooks.size`-byte `ManaTheme`.
   let incoming_slice = unsafe { std::slice::from_raw_parts(incoming, hooks.size) };
   let decoded = valid_theme_ref(hooks, incoming_slice)
      .then(|| read_semantic_colors(hooks, incoming_slice))
      .flatten();

   let Some(incoming_colors) = decoded else {
      let old_base = {
         let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
         state.active_colors = None;
         state.patched_installed = false;
         store_palette_cache(SunbirdPalette::default());
         sync_radius_scale(None);
         state.base.take()
      };
      if let Some(mut old) = old_base {
         // SAFETY: `old.raw` is an owned snapshot with balanced refcounts.
         unsafe { (hooks.drop_in_place)(old.raw.as_mut_ptr()) };
      }
      *consumed = true;
      let _guard = DetourGuard::enter();
      // SAFETY: unknown `ThemeRef` or `SemanticColorsRef` tag; pass through.
      unsafe { (hooks.orig_set_global)(app, incoming) };
      return;
   };

   let snapshot_raw = snapshot_base_raw(hooks, incoming_slice);
   let incoming_mode = detect_mode(hooks, &incoming_colors);
   let incoming_grad = incoming_slice[hooks.layout.gradient_tag];
   let incoming_static_ptr = (read_u64(incoming_slice, hooks.layout.colors_ref) == 0)
      .then(|| read_usize(incoming_slice, hooks.layout.colors_ref + 8))
      .filter(|&ptr| ptr != 0);

   let (old_base, patched) = {
      let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
      let own_ptr = is_own_color_ptr(hooks, &state, incoming_slice);
      let mut snapshot_raw = snapshot_raw;
      let old = if own_ptr && state.base.is_some() {
         // Preserve Discord's unmodified base colors and saved `pre_filter`
         // when Discord re-submits a clone of our already-patched `ManaTheme`,
         // refreshing only the remaining non-color fields in `raw`.
         let prev = state.base.as_mut().expect("checked is_some");
         let pf = hooks.layout.pre_filter;
         if read_u64(snapshot_raw.as_bytes(), pf) == u64::MAX {
            snapshot_raw.as_bytes_mut()[pf..pf + 16]
               .copy_from_slice(&prev.raw.as_bytes()[pf..pf + 16]);
            write_u64(prev.raw.as_bytes_mut(), pf, u64::MAX);
            write_usize(prev.raw.as_bytes_mut(), pf + 8, 0);
         }
         let prev_raw = std::mem::replace(&mut prev.raw, snapshot_raw);
         Some(prev_raw)
      } else {
         state
            .base
            .replace(BaseSnapshot {
               raw:                    snapshot_raw,
               colors:                 incoming_colors,
               base_mode:              incoming_mode,
               orig_gradient_tag:      incoming_grad,
               orig_static_colors_ptr: incoming_static_ptr,
            })
            .map(|b| b.raw)
      };

      let base = state.base.as_ref().expect("just inserted or preserved");
      let base_mode = base.base_mode;
      let base_raw = base.raw.clone();
      let orig_grad = base.orig_gradient_tag;
      let orig_static = base.orig_static_colors_ptr;
      let (colors, mode, hits, radius) =
         build_active_colors(hooks, base, state.active_spec.as_ref());
      sync_radius_scale(radius);
      let patched = if hits > 0 {
         Some(make_patched_theme(hooks, &mut state, &base_raw, &colors))
      } else if own_ptr {
         Some(make_restored_theme(
            hooks,
            &mut state,
            &base_raw,
            &colors,
            orig_grad,
            orig_static,
         ))
      } else {
         None
      };
      refresh_palette_cache(hooks, &colors);
      state.active_colors = Some(colors);
      state.active_mode = if hits > 0 { mode } else { base_mode };
      state.patched_installed = hits > 0;
      PENDING_APPLY.store(false, Ordering::Release);
      drop(state);
      (old, patched)
   };

   if let Some(mut old_raw) = old_base {
      // SAFETY: `old_raw` is an owned snapshot with balanced refcounts.
      unsafe { (hooks.drop_in_place)(old_raw.as_mut_ptr()) };
   }

   *consumed = true;
   if let Some(mut patched) = patched {
      // Drop Discord's original `incoming` via its own destructor (cleanly
      // freeing its `pre_filter` and `SemanticColorsRef`).
      // SAFETY: `incoming` was owned by this call and is not forwarded.
      unsafe { (hooks.drop_in_place)(incoming) };
      let _guard = DetourGuard::enter();
      // SAFETY: `patched` is a valid owned `ManaTheme`.
      unsafe { (hooks.orig_set_global)(app, patched.as_mut_ptr()) };
   } else {
      let _guard = DetourGuard::enter();
      // SAFETY: forwarding Discord's unmodified `incoming`.
      unsafe { (hooks.orig_set_global)(app, incoming) };
   }
}

/// Copies the `SemanticColors` table out of `incoming`'s `SemanticColorsRef`.
fn read_semantic_colors(hooks: &Hooks, incoming: &[u8]) -> Option<AlignedBuf> {
   let tag = read_u64(incoming, hooks.layout.colors_ref);
   let ptr = read_usize(incoming, hooks.layout.colors_ref + 8);
   if ptr == 0 {
      return None;
   }
   let data_ptr = match tag {
      0 => ptr,
      1 => ptr.checked_add(hooks.layout.arc_theme_colors)?,
      2 => ptr.checked_add(hooks.layout.arc_colors_data)?,
      _ => return None,
   };
   let mut out = AlignedBuf::filled(hooks.colors_size, 0);
   // SAFETY: `data_ptr` points to Discord's live `SemanticColors` table.
   unsafe {
      std::ptr::copy_nonoverlapping(data_ptr as *const u8, out.as_mut_ptr(), hooks.colors_size);
   }
   Some(out)
}

/// Detects whether Discord's base `SemanticColors` table is dark or light by
/// comparing `background-base-low` and `text-default` WCAG luminance.
fn detect_mode(hooks: &Hooks, base_colors: &AlignedBuf) -> ThemeMode {
   let mut scratch = base_colors.clone();
   let read_lum = |table: &mut AlignedBuf, name: &str| -> Option<f32> {
      let ptr = lookup_token_mut(hooks.get_mut, table, name)?;
      // SAFETY: `lookup_token_mut` verified `ptr` is 4-aligned and inside `table`.
      Some(Color::from_rgba_u32(unsafe { ptr.read() }).relative_luminance())
   };
   let Some(bg_lum) = read_lum(&mut scratch, "background-base-low") else {
      return ThemeMode::Dark;
   };
   let is_light = read_lum(&mut scratch, "text-default")
      .map_or(bg_lum > 0.35, |text_lum| bg_lum > text_lum);
   if is_light {
      ThemeMode::Light
   } else {
      ThemeMode::Dark
   }
}

/// Builds a self-contained `BaseSnapshot` byte buffer from `incoming`:
/// increments `ThemeRef` and `pre_filter` `Arc` strong counts when present,
/// points `colors_ref` at `dummy_colors`, and clears `gradient_tag` to
/// `gradient_none`. `pre_filter` is kept intact so clearing a theme restores it.
fn snapshot_base_raw(hooks: &Hooks, incoming: &[u8]) -> AlignedBuf {
   let mut raw = AlignedBuf::from_bytes(incoming);
   bump_theme_ref_arc(hooks, raw.as_bytes());
   bump_pre_filter_arc(hooks, raw.as_bytes());

   let dummy_ptr = hooks.dummy_colors_ptr();
   let bytes = raw.as_bytes_mut();
   write_u64(bytes, hooks.layout.colors_ref, 0);
   write_usize(bytes, hooks.layout.colors_ref + 8, dummy_ptr);
   bytes[hooks.layout.gradient_tag] = hooks.layout.gradient_none;
   raw
}

/// Clones a `BaseSnapshot`'s raw `ManaTheme` bytes, incrementing its `ThemeRef`
/// `Arc` strong count if tag 1 so the clone is independently droppable.
fn clone_base_raw(hooks: &Hooks, base: &AlignedBuf) -> AlignedBuf {
   bump_theme_ref_arc(hooks, base.as_bytes());
   base.clone()
}

fn bump_theme_ref_arc(hooks: &Hooks, raw: &[u8]) {
   let tag = read_u64(raw, hooks.layout.theme_ref);
   if tag == 1 {
      let ptr = read_usize(raw, hooks.layout.theme_ref + 8);
      if ptr != 0 {
         // SAFETY: tag 1 `ThemeRef` holds a live `ArcInner` whose first word is
         // the `AtomicUsize` strong count.
         let strong = unsafe { &*(ptr as *const AtomicUsize) };
         strong.fetch_add(1, Ordering::Relaxed);
      }
   }
}

fn bump_pre_filter_arc(hooks: &Hooks, raw: &[u8]) {
   let tag = read_u64(raw, hooks.layout.pre_filter);
   if tag == 1 || tag == 2 {
      let ptr = read_usize(raw, hooks.layout.pre_filter + 8);
      if ptr != 0 {
         // SAFETY: tag 1/2 `SemanticColorsRef` holds a live `ArcInner` whose
         // first word is the `AtomicUsize` strong count.
         let strong = unsafe { &*(ptr as *const AtomicUsize) };
         strong.fetch_add(1, Ordering::Relaxed);
      }
   }
}

/// Clones `base_raw` with balanced `ThemeRef` refcounts, clears `pre_filter`
/// to `None` (without bumping it), and installs `colors` into the next
/// `color_ring` slot as tag 0.
fn make_patched_theme(
   hooks: &Hooks,
   state: &mut ThemeState,
   base_raw: &AlignedBuf,
   colors: &AlignedBuf,
) -> AlignedBuf {
   let mut patched = clone_base_raw(hooks, base_raw);
   let published = publish_color_table(state, colors);
   let bytes = patched.as_bytes_mut();
   write_u64(bytes, hooks.layout.colors_ref, 0);
   write_usize(bytes, hooks.layout.colors_ref + 8, published as usize);
   write_u64(bytes, hooks.layout.pre_filter, u64::MAX);
   write_usize(bytes, hooks.layout.pre_filter + 8, 0);
   patched
}

/// Clones `base_raw` and restores Discord's original `pre_filter`, Nitro
/// gradient tag, and static `SemanticColors` pointer (when static).
fn make_restored_theme(
   hooks: &Hooks,
   state: &mut ThemeState,
   base_raw: &AlignedBuf,
   colors: &AlignedBuf,
   orig_gradient_tag: u8,
   orig_static_colors_ptr: Option<usize>,
) -> AlignedBuf {
   let mut restored = clone_base_raw(hooks, base_raw);
   bump_pre_filter_arc(hooks, restored.as_bytes());
   let colors_ptr = orig_static_colors_ptr
      .unwrap_or_else(|| publish_color_table(state, colors) as usize);
   let bytes = restored.as_bytes_mut();
   write_u64(bytes, hooks.layout.colors_ref, 0);
   write_usize(bytes, hooks.layout.colors_ref + 8, colors_ptr);
   bytes[hooks.layout.gradient_tag] = orig_gradient_tag;
   restored
}

/// Copies `table` into the next slot of `state.color_ring`, allocating up to
/// [`COLOR_RING_SLOTS`] stable 8-byte-aligned heap buffers once and reusing
/// them round-robin so every `set_global` receives a distinct pointer with
/// zero unbounded heap leak.
fn publish_color_table(state: &mut ThemeState, table: &AlignedBuf) -> *const u8 {
   let words_len = table.words.len();
   let slot_idx = state.ring_next;
   state.ring_next = (slot_idx + 1) % COLOR_RING_SLOTS;
   if slot_idx >= state.color_ring.len() {
      state
         .color_ring
         .push(vec![0u64; words_len].into_boxed_slice());
   }
   let slot = &mut state.color_ring[slot_idx];
   slot[..words_len].copy_from_slice(&table.words);
   slot.as_ptr().cast::<u8>()
}

/// Applies pre-resolved `spec` (if present) onto `base.colors` via Discord's
/// `get_mut`, returning the mutated table, effective mode, count of matched
/// Mana tokens, and resolved `--radius-scale`.
fn build_active_colors(
   hooks: &Hooks,
   base: &BaseSnapshot,
   spec: Option<&CachedSpec>,
) -> (AlignedBuf, ThemeMode, usize, Option<f32>) {
   let mut table = base.colors.clone();
   let Some(spec) = spec else {
      return (table, base.base_mode, 0, None);
   };
   let resolved = spec.for_mode(base.base_mode);
   let mut hits = 0usize;
   for (name, color) in &resolved.tokens {
      if let Some(ptr) = lookup_token_mut(hooks.get_mut, &mut table, name) {
         // SAFETY: `lookup_token_mut` verified `ptr` is 4-aligned and inside `table`.
         unsafe { ptr.write(color.to_rgba_u32()) };
         hits += 1;
      }
   }
   (table, resolved.mode, hits, resolved.radius_scale)
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
   u64::from_le_bytes(buf[at..at + 8].try_into().unwrap_or_default())
}

fn write_u64(buf: &mut [u8], at: usize, value: u64) {
   buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_usize(buf: &[u8], at: usize) -> usize {
   usize::from_le_bytes(
      buf[at..at + std::mem::size_of::<usize>()]
         .try_into()
         .unwrap_or_default(),
   )
}

fn write_usize(buf: &mut [u8], at: usize, value: usize) {
   buf[at..at + std::mem::size_of::<usize>()].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
   use std::sync::{
      Mutex,
      PoisonError,
   };

   use phoenix_themes::{
      ThemeConfig,
      load_config,
      save_config,
   };

   use super::{
      STATE,
      WATCH_STATE,
      apply_file,
      commit_generation,
      load_persisted,
      poll_watched_once,
      publish_watch,
      quick_css,
      radius_scale,
      reload_active,
      set_css,
      set_quick_css,
      sync_radius_scale,
   };

   static TEST_LOCK: Mutex<()> = Mutex::new(());

   fn reset_for_test() {
      let gen_id = {
         let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
         let next_gen = commit_generation(&mut state);
         state.home = None;
         state.config = ThemeConfig::default();
         state.active_spec = None;
         state.ephemeral_active = false;
         drop(state);
         sync_radius_scale(None);
         next_gen
      };
      publish_watch(gen_id, None);
   }

   #[test]
   fn hot_reload_reapplies_edits_and_survives_malformed_file() {
      let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
      reset_for_test();
      let tmp = std::env::temp_dir().join(format!("sunbird-watch-test-{}", std::process::id()));
      let themes_dir = tmp.join("themes");
      let _ = std::fs::remove_dir_all(&tmp);
      std::fs::create_dir_all(&themes_dir).expect("create temp themes dir");

      let path = themes_dir.join("live.theme.json");
      std::fs::write(
         &path,
         r##"{"name":"Live","radius_scale":1.25,"tokens":{"background-base-low":"#111111"}}"##,
      )
      .expect("write initial json");

      assert!(apply_file(&themes_dir, "live.theme.json").expect("apply initial"));
      assert_eq!(radius_scale(), Some(1.25));

      std::fs::write(
         &path,
         r##"{"name":"Live","radius_scale":1.75,"tokens":{"background-base-low":"#222222"}}"##,
      )
      .expect("write updated json");
      assert_eq!(reload_active().expect("reload updated"), Some(true));
      assert_eq!(radius_scale(), Some(1.75));

      // Simulate a partial/corrupt mid-save write: reload fails cleanly without
      // clobbering the active radius scale.
      std::fs::write(&path, "{ incomplete json").expect("write malformed json");
      assert!(reload_active().is_err());
      assert_eq!(radius_scale(), Some(1.75));

      // Same protection for an empty `.theme.css` mid-save write.
      let css_path = themes_dir.join("live.theme.css");
      std::fs::write(&css_path, ":root { --radius-scale: 1.3; }").expect("write css");
      assert!(apply_file(&themes_dir, "live.theme.css").expect("apply css"));
      assert_eq!(radius_scale(), Some(1.3));
      std::fs::write(&css_path, "").expect("write empty css");
      assert!(reload_active().is_err());
      assert_eq!(radius_scale(), Some(1.3));

      // Inline set_css clears the watch; reload_active must return `Ok(None)`
      // without clobbering the inline override.
      assert!(set_css(":root { --radius-scale: 1.4; }"));
      assert_eq!(reload_active().expect("no-op reload on inline css"), None);
      assert_eq!(radius_scale(), Some(1.4));

      reset_for_test();
      let _ = std::fs::remove_dir_all(&tmp);
   }

   #[test]
   fn metadata_poll_detects_edit_without_reading_when_unchanged() {
      let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
      reset_for_test();
      let tmp = std::env::temp_dir().join(format!("sunbird-poll-test-{}", std::process::id()));
      let themes_dir = tmp.join("themes");
      let _ = std::fs::remove_dir_all(&tmp);
      std::fs::create_dir_all(&themes_dir).expect("create temp themes dir");

      let path = themes_dir.join("auto.theme.css");
      std::fs::write(&path, ":root { --radius-scale: 1.1; }").expect("write initial css");
      assert!(apply_file(&themes_dir, "auto.theme.css").expect("apply initial"));
      assert_eq!(radius_scale(), Some(1.1));

      let watched = || {
         WATCH_STATE
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .watched
            .clone()
            .expect("watched slot populated")
      };
      // Unchanged metadata must skip reload immediately.
      poll_watched_once(&watched());
      assert_eq!(radius_scale(), Some(1.1));

      // Different byte length changes the metadata stamp even within the same
      // filesystem timestamp granularity. The first poll only notes the new
      // stamp, and the next one applies it once it held.
      std::fs::write(&path, ":root { --radius-scale: 1.9; /* edited */ }")
         .expect("write edited css");
      poll_watched_once(&watched());
      assert_eq!(radius_scale(), Some(1.1));
      poll_watched_once(&watched());
      assert_eq!(radius_scale(), Some(1.9));

      reset_for_test();
      let _ = std::fs::remove_dir_all(&tmp);
   }

   #[test]
   fn quick_css_persists_and_layers_over_active_theme() {
      let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
      reset_for_test();
      let tmp = std::env::temp_dir().join(format!("sunbird-qcss-test-{}", std::process::id()));
      let themes_dir = tmp.join("themes");
      let _ = std::fs::remove_dir_all(&tmp);
      std::fs::create_dir_all(&themes_dir).expect("create temp themes dir");

      std::fs::write(
         themes_dir.join("base.theme.css"),
         ":root { --bg-primary: #10141e; --radius-scale: 1.2; }",
      )
      .expect("write base css");
      save_config(
         &tmp.join("themes.json"),
         &ThemeConfig {
            active:    Some("base.theme.css".to_owned()),
            quick_css: String::new(),
         },
      )
      .expect("save initial config");

      load_persisted(&tmp);
      assert_eq!(radius_scale(), Some(1.2));

      set_quick_css(":root { --radius-scale: 1.8; }").expect("set quick css");
      assert_eq!(quick_css(), ":root { --radius-scale: 1.8; }");
      assert_eq!(radius_scale(), Some(1.8));

      let persisted = load_config(&tmp.join("themes.json"));
      assert_eq!(persisted.active.as_deref(), Some("base.theme.css"));
      assert_eq!(persisted.quick_css, ":root { --radius-scale: 1.8; }");

      // While an ephemeral JS override is active, set_quick_css persists the
      // text but leaves the ephemeral override on screen.
      assert!(set_css(":root { --radius-scale: 0.7; }"));
      set_quick_css(":root { --radius-scale: 2.0; }").expect("persist quick css");
      assert_eq!(radius_scale(), Some(0.7));
      assert_eq!(
         load_config(&tmp.join("themes.json")).quick_css,
         ":root { --radius-scale: 2.0; }"
      );

      reset_for_test();
      let _ = std::fs::remove_dir_all(&tmp);
   }
}
