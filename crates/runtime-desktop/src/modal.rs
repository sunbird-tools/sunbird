//! Native GPUI / Mana modal overlay, syntax-colorized code viewer, and native
//! `TextInput` / `TextArea` editors for Desktop Phoenix.
//!
//! Hooks Discord's window root render (`modal-layer-scrim` ->
//! `WindowRoot::render`) and composes Discord's own
//! `mana_html::hosted::modal::Modal`, `TextInput`, and `TextArea` natively on
//! top of the active window — no Win32 popup windows and no `WebView2`
//! overlays.

use std::{
   collections::HashMap,
   ffi::c_void,
   mem::MaybeUninit,
   ops::{
      Deref,
      DerefMut,
   },
   ptr,
   slice,
   str,
   sync::{
      Arc,
      Condvar,
      LazyLock,
      Mutex,
      MutexGuard,
      OnceLock,
      PoisonError,
      atomic::{
         AtomicBool,
         AtomicU64,
         AtomicUsize,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
   },
};

use phoenix_hook::image::Image;
use phoenix_runtime::highlight::{
   self,
   JsonKind,
   Kind,
};
use phoenix_sigs::desktop::{
   self,
   Dismiss,
   FieldLayout,
   ModalLayout,
   ScaleFadeLayout,
   layout,
};

use crate::{
   gpui::{
      self,
      BtnSize,
      BtnVariant,
      RawAnyElement,
      RawButton,
      RawDiv,
      RcFnVtbl,
      Scratch,
      SharedString,
      Style,
      W_FULL,
      argb_to_hsla,
      gpui_px,
      intern_str,
   },
   os::{
      DesktopOs,
      Os,
   },
};

const MAX_HIGHLIGHT_LINES: usize = 600;
const MONO_FONT: &str = "gg sans mono";

// GitHub-Dark and GitHub-Light syntax accent palettes (ARGB -> HSLA).
struct SyntaxColors {
   key:     u32,
   string:  u32,
   number:  u32,
   bool_:   u32,
   null:    u32,
   punct:   u32,
   keyword: u32,
   comment: u32,
}

const SYNTAX_DARK: SyntaxColors = SyntaxColors {
   key:     0xFF79_C0FF,
   string:  0xFF7E_E787,
   number:  0xFFFF_A657,
   bool_:   0xFFD2_A8FF,
   null:    0xFFFF_7B72,
   punct:   0xFF8B_949E,
   keyword: 0xFFFF_7B72,
   comment: 0xFF8B_949E,
};

const SYNTAX_LIGHT: SyntaxColors = SyntaxColors {
   key:     0xFF05_50AE,
   string:  0xFF11_6329,
   number:  0xFF95_3800,
   bool_:   0xFF66_39BA,
   null:    0xFFCF_222E,
   punct:   0xFF57_606A,
   keyword: 0xFFCF_222E,
   comment: 0xFF57_606A,
};

fn col_muted() -> u32 {
   crate::mana_theme::color_to_argb(crate::mana_theme::sunbird_palette().text_muted)
}

fn col_error() -> u32 {
   crate::mana_theme::color_to_argb(crate::mana_theme::sunbird_palette().danger)
}

fn col_ok() -> u32 {
   crate::mana_theme::color_to_argb(crate::mana_theme::sunbird_palette().positive)
}

// ---------------------------------------------------------------------------
// Public Modal Specs
// ---------------------------------------------------------------------------

/// A clickable action button inside a native GPUI page modal
/// (`phoenix.ui.openPage`).
#[derive(Clone)]
pub struct ModalButton {
   pub label:    String,
   pub danger:   bool,
   /// Drawn as the page's main action.
   pub primary:  bool,
   pub on_press: Arc<dyn Fn() + Send + Sync>,
}

/// A section inside a native GPUI page modal (`phoenix.ui.openPage`).
#[derive(Clone)]
pub enum ModalSection {
   Buttons {
      title:   Option<String>,
      buttons: Vec<ModalButton>,
   },
   Text {
      title: Option<String>,
      text:  String,
   },
   Code {
      title:     Option<String>,
      text:      String,
      monospace: bool,
      highlight: bool,
   },
   /// A Mana `Switch` row. The page is rebuilt from its spec every frame, so
   /// `on_toggle` replaces the page with `checked` flipped to show the change.
   Switch {
      id:          String,
      label:       String,
      description: Option<String>,
      checked:     bool,
      on_toggle:   Arc<dyn Fn(bool) + Send + Sync>,
   },
   /// A native text box holding `initial` when the page opens, whose text
   /// goes to `on_change` on a worker thread each time it changes. Its `id`
   /// keeps the same box across frames.
   Field {
      id:          String,
      label:       String,
      description: Option<String>,
      initial:     String,
      multiline:   bool,
      on_change:   Arc<dyn Fn(String) + Send + Sync>,
   },
}

/// Specification for a native GPUI plugin page (`phoenix.ui.openPage`).
#[derive(Clone)]
pub struct PageModalSpec {
   pub title:    String,
   pub sections: Vec<ModalSection>,
}

/// A native `phoenix.ui.choose` / `showSheet` dialog whose answer a waiting
/// worker thread reads.
pub struct Choice {
   picked: Mutex<Option<i32>>,
   done:   Condvar,
}

impl Choice {
   const fn new() -> Self {
      Self {
         picked: Mutex::new(None),
         done:   Condvar::new(),
      }
   }

   /// Records `index` unless the dialog already answered.
   pub fn pick(&self, index: i32) {
      let mut picked = self.picked.lock().unwrap_or_else(PoisonError::into_inner);
      if picked.is_none() {
         *picked = Some(index);
      }
      drop(picked);
      self.done.notify_all();
   }

   /// Waits for the picked index, or `None` when `timeout` passes first.
   pub fn wait(&self, timeout: Duration) -> Option<i32> {
      let picked = self.picked.lock().unwrap_or_else(PoisonError::into_inner);
      let (picked, _) = self
         .done
         .wait_timeout_while(picked, timeout, |picked| picked.is_none())
         .unwrap_or_else(PoisonError::into_inner);
      *picked
   }
}

/// One button of a native choose dialog.
pub struct ChoiceOption {
   pub label:  String,
   pub danger: bool,
}

#[derive(Clone)]
struct CodeEditorModal {
   plugin:       String,
   pending_init: Option<String>,
   status:       FieldStatus,
   /// The page it opened over, which comes back when it closes.
   under:        Option<(u64, PageModalSpec)>,
}

#[derive(Clone)]
enum FieldStatus {
   Idle,
   Working(String),
   Failed(String),
}

impl FieldStatus {
   fn line(&self) -> Option<(&str, u32)> {
      match self {
         Self::Idle => None,
         Self::Working(msg) => Some((msg, col_ok())),
         Self::Failed(msg) => Some((msg, col_error())),
      }
   }
}

/// A plugin's secret setting, typed into the one-line field. Other settings
/// are edited in place on the plugin's page, but a secret is never drawn, so
/// the field starts empty.
#[derive(Clone)]
pub struct SettingField {
   pub plugin: String,
   pub key:    String,
   pub title:  String,
   /// Runs on a worker thread once the value is saved.
   pub then:   Option<Arc<dyn Fn() + Send + Sync>>,
}

/// A one-line field a plugin asks for, like `phoenix.ui.addProfileTab`'s
/// input block.
#[derive(Clone)]
pub struct Prompt {
   pub title:     String,
   pub on_submit: Arc<dyn Fn(String) + Send + Sync>,
}

/// What the one-line field modal's text goes to.
#[derive(Clone)]
enum FieldTarget {
   Install,
   Setting(SettingField),
   Prompt(Prompt),
}

/// The one-line field modal: a plugin URL to install, a setting, or a prompt.
#[derive(Clone)]
struct InstallUrlModal {
   target:       FieldTarget,
   pending_init: Option<String>,
   status:       FieldStatus,
   /// The page it opened over, which comes back when it closes.
   under:        Option<(u64, PageModalSpec)>,
}

#[derive(Clone)]
enum ActiveModal {
   Page {
      id:   u64,
      spec: PageModalSpec,
   },
   /// A choose dialog, over the page it returns to when answered.
   Choice {
      spec:   PageModalSpec,
      choice: Arc<Choice>,
      under:  Option<(u64, PageModalSpec)>,
   },
   CodeEditor(CodeEditorModal),
   InstallUrl(InstallUrlModal),
}

/// The active modal copied out of the state lock for one render pass. A choose
/// dialog draws as the page it is.
enum Snapshot {
   Page(PageModalSpec),
   CodeEditor(CodeEditorModal),
   InstallUrl(InstallUrlModal),
}

// ---------------------------------------------------------------------------
// ABI Mirrors & Resolved Discord Symbols
// ---------------------------------------------------------------------------

type RawModal = Scratch<{ layout::SCRATCH_CAP }>;
type RawTextInput = Scratch<{ layout::SCRATCH_CAP }>;
type RawTextArea = Scratch<{ layout::SCRATCH_CAP }>;
type RawScaleFade = Scratch<{ layout::SCALE_FADE_CAP }>;

#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct RawEntity {
   id_lo: u32,
   id_hi: u32,
   rest:  [u64; 3],
}
const _: () = assert!(size_of::<RawEntity>() == layout::ENTITY);

#[repr(C)]
struct RawVecAny {
   cap: usize,
   ptr: *mut RawAnyElement,
   len: usize,
}

type OverlayRenderFn = unsafe extern "C" fn(
   sret: *mut RawVecAny,
   workspace: *mut c_void,
   arg3: *mut c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawVecAny;

type VecPushAnyFn = unsafe extern "C" fn(*mut RawVecAny, *mut RawAnyElement);

type ModalNewFn = unsafe extern "C" fn(*mut RawModal, *const SharedString) -> *mut RawModal;
type ModalIntoAnyFn = unsafe extern "C" fn(*mut RawAnyElement, *mut RawModal) -> *mut RawAnyElement;
type ModalChildDivFn =
   unsafe extern "C" fn(*mut RawModal, *mut RawModal, *mut RawDiv) -> *mut RawModal;
type ModalFooterBtnFn =
   unsafe extern "C" fn(*mut RawModal, *mut RawModal, *mut RawButton) -> *mut RawModal;
type DivIdFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv, *const u8, usize) -> *mut RawDiv;

type CxNewFieldFn =
   unsafe extern "C" fn(*mut RawEntity, *mut c_void, *mut c_void) -> *mut RawEntity;
type EntityMapGetFn = unsafe extern "C" fn(*mut c_void, u32, u32) -> *mut c_void;
type TextGetFn =
   unsafe extern "C" fn(*mut SharedString, *const c_void, *const c_void) -> *mut SharedString;
type ArcDropSlowFn = unsafe extern "C" fn(*const u8, usize);
type TextInputNewFn =
   unsafe extern "C" fn(*mut RawTextInput, *const RawEntity) -> *mut RawTextInput;
type TextInputIntoAnyFn =
   unsafe extern "C" fn(*mut RawAnyElement, *mut RawTextInput) -> *mut RawAnyElement;
type TextAreaNewFn = unsafe extern "C" fn(*mut RawTextArea, *const RawEntity) -> *mut RawTextArea;
type TextAreaIntoAnyFn =
   unsafe extern "C" fn(*mut RawAnyElement, *mut RawTextArea) -> *mut RawAnyElement;
type ListenerPushFn = unsafe extern "C" fn(*mut c_void, *mut u8, *const RcFnVtbl);
type IsHoveredFn = unsafe extern "C" fn(*const c_void, *const c_void) -> bool;
type AnimationFrameFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
type ScaleFadeIntoAnyFn = unsafe extern "C" fn(
   *mut RawAnyElement,
   *mut RawScaleFade,
   *const c_void,
) -> *mut RawAnyElement;

struct ScaleFadeSymbols {
   into_any:   ScaleFadeIntoAnyFn,
   caller_loc: *const c_void,
   layout:     ScaleFadeLayout,
}

struct ModalSymbols {
   vec_push_any:        VecPushAnyFn,
   modal_new:           ModalNewFn,
   modal_into_any:      ModalIntoAnyFn,
   modal_child_div:     ModalChildDivFn,
   modal_footer_button: ModalFooterBtnFn,
   div_id:              DivIdFn,
   layout:              ModalLayout,
}

/// Click-outside-to-close, wired the way Discord's own overlay `backdrop` is.
struct DismissSymbols {
   push:                 ListenerPushFn,
   is_hovered:           IsHoveredFn,
   /// `Div` offset of its `Box<Interactivity>`.
   interactivity:        usize,
   mouse_down_listeners: usize,
   propagate_event:      usize,
}

struct TextFieldSymbols {
   cx_new_single:       CxNewFieldFn,
   cx_new_multiline:    CxNewFieldFn,
   entity_map_get:      EntityMapGetFn,
   text_get:            TextGetFn,
   arc_drop_slow:       ArcDropSlowFn,
   text_input_new:      TextInputNewFn,
   text_input_into_any: TextInputIntoAnyFn,
   text_area_new:       TextAreaNewFn,
   text_area_into_any:  TextAreaIntoAnyFn,
   layout:              FieldLayout,
}

// SAFETY: resolved static function pointers in `Discord.exe`.
unsafe impl Send for ModalSymbols {}
unsafe impl Sync for ModalSymbols {}
unsafe impl Send for TextFieldSymbols {}
unsafe impl Sync for TextFieldSymbols {}
unsafe impl Send for DismissSymbols {}
unsafe impl Sync for DismissSymbols {}
unsafe impl Send for ScaleFadeSymbols {}
unsafe impl Sync for ScaleFadeSymbols {}

static ORIG_OVERLAY_RENDER: OnceLock<OverlayRenderFn> = OnceLock::new();

/// The Settings layer's render, `fn(sret, layer, window, cx) -> sret`.
type SettingsLayerFn = unsafe extern "C" fn(
   *mut RawAnyElement,
   *mut c_void,
   *mut c_void,
   *mut c_void,
) -> *mut RawAnyElement;
static ORIG_SETTINGS_LAYER: OnceLock<SettingsLayerFn> = OnceLock::new();
/// Whether the open modal is drawn inside the Settings layer, from Settings'
/// first render while it is open until it closes. GPUI renders `Workspace` and
/// Settings independently (each re-renders only when notified, and replays its
/// last frame otherwise), so deciding this per frame made the modal flash
/// between the two.
static IN_SETTINGS: AtomicBool = AtomicBool::new(false);
/// When Settings last rendered, in ms since [`epoch_ms`]'s start.
static SETTINGS_AT: AtomicU64 = AtomicU64::new(0);
/// Settings this recently rendered may still be open: the press that opens a
/// modal from it re-renders it first.
const SETTINGS_RECENT_MS: u64 = 500;
/// Whether a redraw is due once [`SETTINGS_RECENT_MS`] has passed.
static RECHECK: AtomicBool = AtomicBool::new(false);

fn epoch_ms() -> u64 {
   static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
   u64::try_from(
      EPOCH
         .get_or_init(std::time::Instant::now)
         .elapsed()
         .as_millis(),
   )
   .unwrap_or(u64::MAX)
}
static MODAL_SYM: OnceLock<ModalSymbols> = OnceLock::new();
static FIELD_SYM: OnceLock<TextFieldSymbols> = OnceLock::new();
static DISMISS_SYM: OnceLock<DismissSymbols> = OnceLock::new();
static ANIMATION_FRAME: OnceLock<AnimationFrameFn> = OnceLock::new();
static SCALE_FADE: OnceLock<ScaleFadeSymbols> = OnceLock::new();
static PUMP_RUNNING: AtomicBool = AtomicBool::new(false);
/// Set while one mouse press is dispatched if it landed on the modal card.
static PRESS_IN_CARD: AtomicBool = AtomicBool::new(false);
static WINDOW_VAL_OFFSET: AtomicUsize = AtomicUsize::new(0);

// Discord's `modal-layer-card` spring parameters (open underdamped, close overdamped).
const MODAL_SCALE_FROM: f32 = 0.90;
const OPEN_STIFFNESS: f32 = 1000.0;
const OPEN_DAMPING: f32 = 48.0;
const CLOSE_STIFFNESS: f32 = 1200.0;
const CLOSE_DAMPING: f32 = 80.0;
const SETTLE_POS: f32 = 0.0004;
const SETTLE_VEL: f32 = 0.01;
const MAX_TRANSITION_SECS: f32 = 0.45;
const FALLBACK_TICK: Duration = Duration::from_millis(16);

#[derive(Clone, Copy)]
struct Transition {
   started:   Instant,
   from:      f32,
   vel0:      f32,
   target:    f32,
   stiffness: f32,
   damping:   f32,
}

impl Transition {
   const fn open(now: Instant) -> Self {
      Self::open_from(0.0, 0.0, now)
   }

   const fn open_from(from: f32, vel0: f32, now: Instant) -> Self {
      Self {
         started: now,
         from,
         vel0,
         target: 1.0,
         stiffness: OPEN_STIFFNESS,
         damping: OPEN_DAMPING,
      }
   }

   const fn close_from(from: f32, vel0: f32, now: Instant) -> Self {
      Self {
         started: now,
         from,
         vel0,
         target: 0.0,
         stiffness: CLOSE_STIFFNESS,
         damping: CLOSE_DAMPING,
      }
   }

   fn sample(&self, now: Instant) -> (f32, f32, bool) {
      let t = now.saturating_duration_since(self.started).as_secs_f32();
      if t >= MAX_TRANSITION_SECS {
         return (self.target, 0.0, true);
      }
      let d0 = self.from - self.target;
      let k = self.stiffness;
      let c = self.damping;
      let delta = c.mul_add(c, -4.0 * k);
      let (disp, vel) = if delta < 0.0 {
         let alpha = 0.5 * c;
         let wd = (-delta).sqrt() * 0.5;
         let b = alpha.mul_add(d0, self.vel0) / wd;
         let decay = (-alpha * t).exp();
         let (sin, cos) = (wd * t).sin_cos();
         let disp = decay * d0.mul_add(cos, b * sin);
         let vel = decay
            * b.mul_add(wd, -(alpha * d0))
               .mul_add(cos, (-d0).mul_add(wd, -(alpha * b)) * sin);
         (disp, vel)
      } else if delta > 0.0 {
         let root = delta.sqrt();
         let r1 = f32::midpoint(-c, root);
         let r2 = f32::midpoint(-c, -root);
         let c1 = (-r2).mul_add(d0, self.vel0) / (r1 - r2);
         let c2 = d0 - c1;
         let e1 = (r1 * t).exp();
         let e2 = (r2 * t).exp();
         (c1.mul_add(e1, c2 * e2), (c1 * r1).mul_add(e1, c2 * r2 * e2))
      } else {
         let alpha = 0.5 * c;
         let b = alpha.mul_add(d0, self.vel0);
         let decay = (-alpha * t).exp();
         let disp = decay * b.mul_add(t, d0);
         let vel = decay * (-alpha * b).mul_add(t, alpha.mul_add(-d0, b));
         (disp, vel)
      };
      if disp.abs() <= SETTLE_POS && vel.abs() <= SETTLE_VEL {
         (self.target, 0.0, true)
      } else {
         (self.target + disp, vel, false)
      }
   }
}

struct RenderFrame {
   snapshot:    Snapshot,
   progress:    f32,
   animating:   bool,
   interactive: bool,
   instance_id: u64,
}

struct ModalState {
   active:      Option<ActiveModal>,
   closing:     Option<ActiveModal>,
   transition:  Option<Transition>,
   instance_id: u64,
}

impl ModalState {
   const fn empty() -> Self {
      Self {
         active:      None,
         closing:     None,
         transition:  None,
         instance_id: 0,
      }
   }

   const fn is_visible(&self) -> bool {
      self.active.is_some() || self.closing.is_some()
   }

   fn sample_current(&self, now: Instant) -> (f32, f32) {
      match self.transition {
         Some(tr) => {
            let (pos, vel, _) = tr.sample(now);
            (pos, vel)
         },
         None if self.active.is_some() => (1.0, 0.0),
         None => (0.0, 0.0),
      }
   }

   fn open(&mut self, modal: ActiveModal, now: Instant) {
      if self.active.is_none() {
         if let Some(tr) = self.transition {
            let (pos, vel, _) = tr.sample(now);
            self.transition = Some(Transition::open_from(pos, vel, now));
         } else {
            self.instance_id = self.instance_id.wrapping_add(1).max(1);
            self.transition = Some(Transition::open(now));
         }
      }
      self.closing = None;
      self.active = Some(modal);
   }

   fn close(&mut self, now: Instant) -> bool {
      let Some(taken) = self.active.take() else {
         return false;
      };
      if let Some(restored) = uncover(&taken) {
         self.active = Some(restored);
         self.closing = None;
      } else {
         let (pos, vel) = self.sample_current(now);
         self.closing = Some(taken);
         self.transition = Some(Transition::close_from(pos, vel, now));
      }
      true
   }

   fn render_frame(&mut self, now: Instant) -> Option<RenderFrame> {
      if let Some(active) = self.active.as_mut() {
         let (progress, animating) = match self.transition {
            Some(tr) => {
               let (pos, _, settled) = tr.sample(now);
               if settled {
                  self.transition = None;
                  (1.0, false)
               } else {
                  (pos, true)
               }
            },
            None => (1.0, false),
         };
         return Some(RenderFrame {
            snapshot: snapshot_modal(active, true),
            progress,
            animating,
            interactive: true,
            instance_id: self.instance_id,
         });
      }
      let closing = self.closing.as_mut()?;
      let (progress, settled) = self.transition.map_or((0.0, true), |tr| {
         let (pos, _, settled) = tr.sample(now);
         (pos, settled)
      });
      if settled {
         self.closing = None;
         self.transition = None;
         return None;
      }
      Some(RenderFrame {
         snapshot: snapshot_modal(closing, false),
         progress,
         animating: true,
         interactive: false,
         instance_id: self.instance_id,
      })
   }
}

fn snapshot_modal(modal: &mut ActiveModal, take_init: bool) -> Snapshot {
   match modal {
      ActiveModal::Page { spec, .. } | ActiveModal::Choice { spec, .. } => {
         Snapshot::Page(spec.clone())
      },
      ActiveModal::CodeEditor(ed) => {
         Snapshot::CodeEditor(CodeEditorModal {
            plugin:       ed.plugin.clone(),
            pending_init: if take_init {
               ed.pending_init.take()
            } else {
               None
            },
            status:       ed.status.clone(),
            under:        None,
         })
      },
      ActiveModal::InstallUrl(inst) => {
         Snapshot::InstallUrl(InstallUrlModal {
            target:       inst.target.clone(),
            pending_init: if take_init {
               inst.pending_init.take()
            } else {
               None
            },
            status:       inst.status.clone(),
            under:        None,
         })
      },
   }
}

static MODAL_OPEN: AtomicBool = AtomicBool::new(false);
static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_MODAL: LazyLock<Mutex<ModalState>> = LazyLock::new(|| Mutex::new(ModalState::empty()));

struct ActiveGuard(MutexGuard<'static, ModalState>);

impl Deref for ActiveGuard {
   type Target = ModalState;

   fn deref(&self) -> &Self::Target {
      &self.0
   }
}

impl DerefMut for ActiveGuard {
   fn deref_mut(&mut self) -> &mut Self::Target {
      &mut self.0
   }
}

impl Drop for ActiveGuard {
   /// `MODAL_OPEN` changes only under this lock, so a close racing an open
   /// cannot leave a held or closing modal undrawn.
   fn drop(&mut self) {
      MODAL_OPEN.store(self.0.is_visible(), Ordering::Release);
   }
}

fn active_modal() -> ActiveGuard {
   ActiveGuard(ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner))
}

fn schedule_next_frame(window: *mut c_void, app: *mut c_void) {
   if let Some(&anim_frame) = ANIMATION_FRAME.get()
      && !window.is_null()
      && !app.is_null()
   {
      // SAFETY: called on the GPUI main thread with live `window` and `app`.
      unsafe { anim_frame(window, app) };
      return;
   }
   if PUMP_RUNNING.swap(true, Ordering::AcqRel) {
      return;
   }
   std::thread::spawn(|| {
      loop {
         std::thread::sleep(FALLBACK_TICK);
         let keep_going = active_modal().transition.is_some();
         force_redraw();
         if !keep_going {
            break;
         }
      }
      PUMP_RUNNING.store(false, Ordering::Release);
   });
}

fn wrap_scale_fade(
   child: RawAnyElement,
   progress: f32,
   instance_id: u64,
) -> Result<RawAnyElement, RawAnyElement> {
   let Some(sf) = SCALE_FADE.get() else {
      return Err(child);
   };
   let at = &sf.layout;
   let mut raw = RawScaleFade::zeroed();
   raw.put(at.child, child);
   raw.put(at.opacity_range, [0.0f32, 1.0f32]);
   raw.put(at.label_none, 0xFFu8);
   raw.put(at.element_id + at.element_id_tag_off, at.element_id_tag);
   raw.put(at.element_id + at.element_id_key, instance_id);
   raw.put(
      at.element_id + at.element_id_str,
      SharedString::from_static("sunbird-modal-scale"),
   );
   raw.put(at.progress, progress);
   raw.put(at.scale_from, MODAL_SCALE_FROM);
   raw.put(at.origin, 0u32);
   raw.put(at.fade, 1u8);
   // SAFETY: `into_any`, `caller_loc`, and every field offset in `at` were
   // decoded from Discord's binary and bounds-checked against `SCALE_FADE_CAP`.
   let out = unsafe {
      let mut out = MaybeUninit::<RawAnyElement>::uninit();
      (sf.into_any)(out.as_mut_ptr(), &raw mut raw, sf.caller_loc);
      out.assume_init()
   };
   Ok(out)
}

/// Main-thread-owned GPUI `Entity<FieldState>` handles, lazily created on first
/// render when `window` and `app` are live.
struct FieldEntities {
   url:    Option<RawEntity>,
   code:   Option<RawEntity>,
   search: Option<RawEntity>,
}

static FIELD_ENTITIES: LazyLock<Mutex<FieldEntities>> = LazyLock::new(|| {
   Mutex::new(FieldEntities {
      url:    None,
      code:   None,
      search: None,
   })
});

/// A page's text box, with the text last seen in it, and whether it waits to
/// be filled from its section's `initial`.
struct PageFieldState {
   entity: RawEntity,
   seen:   String,
   refill: bool,
   /// Set by a write the field has not displayed yet, so the next read only
   /// records the text instead of reporting it as a user edit.
   fresh:  bool,
}

/// Page text boxes by section id, kept while Discord runs.
static PAGE_FIELDS: LazyLock<Mutex<HashMap<String, PageFieldState>>> =
   LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fills every page text box from its section's value again on its next
/// frame, for a page that opens fresh.
pub fn refill_page_fields() {
   for field in PAGE_FIELDS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .values_mut()
   {
      field.refill = true;
   }
}

/// Ensures page field `id` has an entity, returning it.
fn ensure_page_field(
   app: *mut c_void,
   window: *mut c_void,
   id: &str,
   multiline: bool,
) -> Option<RawEntity> {
   let existing = PAGE_FIELDS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get(id)
      .map(|field| field.entity);
   if let Some(entity) = existing {
      return Some(entity);
   }
   // SAFETY: called from a GPUI callback with live `app` and `window`.
   let entity = unsafe { new_field_entity(app, window, multiline, "") }?;
   {
      let mut fields = PAGE_FIELDS.lock().unwrap_or_else(PoisonError::into_inner);
      fields.entry(id.to_owned()).or_insert(PageFieldState {
         entity,
         seen: String::new(),
         refill: true,
         fresh: false,
      });
   }
   Some(entity)
}

/// Writes every page field's `initial` value before the page opens.
pub fn prime_page(spec: &PageModalSpec, app: *mut c_void, window: *mut c_void) {
   if app.is_null() || window.is_null() {
      return;
   }
   let Some(tf) = FIELD_SYM.get() else {
      return;
   };
   for section in &spec.sections {
      let ModalSection::Field {
         id,
         initial,
         multiline,
         ..
      } = section
      else {
         continue;
      };
      let Some(entity) = ensure_page_field(app, window, id, *multiline) else {
         continue;
      };
      // SAFETY: a GPUI callback on the main thread, outside any render, with
      // live `app` and `window`.
      let field = unsafe { field_ptr(app, &entity) };
      // SAFETY: `field` is the live `FieldState` behind `entity`.
      unsafe { write_field_pending(tf, field, initial) };
      let mut fields = PAGE_FIELDS.lock().unwrap_or_else(PoisonError::into_inner);
      if let Some(field) = fields.get_mut(id) {
         field.seen.clear();
         field.seen.push_str(initial);
         field.refill = false;
         field.fresh = true;
      }
   }
}

/// Which kept field entity a field draws: the modals' one-line and multiline
/// fields, or the Sunbird page's search box.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldSlot {
   Url,
   Code,
   Search,
}

/// A text box drawn on a page, and what it holds.
pub struct PageField {
   pub element: RawAnyElement,
   pub text:    String,
}

/// The Sunbird page's search box, which keeps its text while the page is
/// closed.
pub fn search_field(app: *mut c_void, window: *mut c_void) -> Option<PageField> {
   let tf = FIELD_SYM.get()?;
   // SAFETY: called from the settings section's render on the GPUI main
   // thread, with its live `app` and `window`.
   let ent =
      unsafe { ensure_field_entity(app, window, FieldSlot::Search, "Search for a plugin...") }?;
   // SAFETY: as above, and the field's entity is not leased while the section
   // renders.
   let text = unsafe { read_field_text(app, &ent) }.unwrap_or_default();
   // SAFETY: builds a hosted `TextInput` from `&Entity<FieldState>` in a
   // buffer that fits it, and moves it into an `AnyElement`.
   let element = unsafe {
      let mut ti = RawTextInput::zeroed();
      (tf.text_input_new)(&raw mut ti, &raw const ent);
      let mut any = MaybeUninit::<RawAnyElement>::uninit();
      (tf.text_input_into_any)(any.as_mut_ptr(), &raw mut ti);
      any.assume_init()
   };
   Some(PageField { element, text })
}

#[derive(Clone)]
enum ModalAction {
   Close,
   PageButton(Arc<dyn Fn() + Send + Sync>),
   Switch(Arc<dyn Fn(bool) + Send + Sync>),
   SaveCode(String),
   CopyCode,
   SubmitInstallUrl,
}

static MODAL_ACTIONS: LazyLock<Mutex<Vec<ModalAction>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn register_modal_action(action: ModalAction) -> u64 {
   let mut list = MODAL_ACTIONS.lock().unwrap_or_else(PoisonError::into_inner);
   let id = list.len() as u64;
   list.push(action);
   id
}

fn lookup_modal_action(id: u64) -> Option<ModalAction> {
   let list = MODAL_ACTIONS.lock().unwrap_or_else(PoisonError::into_inner);
   usize::try_from(id)
      .ok()
      .and_then(|idx| list.get(idx).cloned())
}

// ---------------------------------------------------------------------------
// Vtables for Modal on_close (3 args), Modal Buttons (4 args), & update_entity
// ---------------------------------------------------------------------------

static MODAL_CLOSE_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     modal_close_callback as *const c_void,
   call_mut:      modal_close_callback as *const c_void,
   call:          modal_close_callback as *const c_void,
   extra:         ptr::null(),
};

static MODAL_SWITCH_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     modal_switch_callback as *const c_void,
   call_mut:      modal_switch_callback as *const c_void,
   call:          modal_switch_callback as *const c_void,
   extra:         ptr::null(),
};

static MODAL_BTN_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     modal_button_callback as *const c_void,
   call_mut:      modal_button_callback as *const c_void,
   call:          modal_button_callback as *const c_void,
   extra:         ptr::null(),
};

// `Box<dyn Fn(&MouseDownEvent, DispatchPhase, &Hitbox, &mut Window, &mut App)>`
// vtables for zero-sized listeners, so the box holds no allocation.
static SCRIM_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     scrim_press as *const c_void,
   call_mut:      scrim_press as *const c_void,
   call:          scrim_press as *const c_void,
   extra:         ptr::null(),
};

static CARD_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     card_press as *const c_void,
   call_mut:      card_press as *const c_void,
   call:          card_press as *const c_void,
   extra:         ptr::null(),
};

static SWALLOW_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     swallow_press as *const c_void,
   call_mut:      swallow_press as *const c_void,
   call:          swallow_press as *const c_void,
   extra:         ptr::null(),
};

pub const PHASE_BUBBLE: u8 = 0;
pub const PHASE_CAPTURE: u8 = 1;

const unsafe extern "C" fn noop_drop(_data: *mut u8) {}

/// GPUI dispatches a press front to back in the capture phase, then back to
/// front in the bubble phase, so the card (painted over the scrim) bubbles
/// first. A press the card saw is kept; any other press on the scrim closes.
///
/// Both press handlers are Rust ABI like the `Fn::call` slot they fill: on
/// Linux `extern "C"` would assume the caller widened `phase`, which Rust
/// callers do not.
unsafe fn scrim_press(
   _data: *const u8,
   _event: *const c_void,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase == PHASE_CAPTURE {
         PRESS_IN_CARD.store(false, Ordering::Relaxed);
         return;
      }
      let Some(sym) = DISMISS_SYM.get() else {
         return;
      };
      // SAFETY: GPUI passes the live `&Hitbox`, `&Window` and `&mut App`.
      if phase != PHASE_BUBBLE || app.is_null() || !unsafe { (sym.is_hovered)(hitbox, window) } {
         return;
      }
      // Like Discord's backdrop, swallow the press so the chat underneath never
      // sees it (`cx.stop_propagation()`).
      // SAFETY: `propagate_event` is a `bool` in `App`, located by `sigs`.
      unsafe { app.add(sym.propagate_event).write(0) };
      if !PRESS_IN_CARD.swap(false, Ordering::Relaxed) {
         close_active();
      }
   });
}

unsafe fn card_press(
   _data: *const u8,
   _event: *const c_void,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   _app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase == PHASE_BUBBLE
         && let Some(sym) = DISMISS_SYM.get()
         // SAFETY: GPUI passes the live `&Hitbox` and `&Window`.
         && unsafe { (sym.is_hovered)(hitbox, window) }
      {
         PRESS_IN_CARD.store(true, Ordering::Relaxed);
      }
   });
}

/// Ends a press on the element after its own listeners ran, so what lies
/// under it never sees the press.
unsafe fn swallow_press(
   _data: *const u8,
   _event: *const c_void,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase != PHASE_BUBBLE || app.is_null() {
         return;
      }
      let Some(sym) = DISMISS_SYM.get() else {
         return;
      };
      // SAFETY: GPUI passes the live `&Hitbox`, `&Window` and `&mut App`, and
      // `propagate_event` is a `bool` in `App`, located by `sigs`.
      unsafe {
         if (sym.is_hovered)(hitbox, window) {
            app.add(sym.propagate_event).write(0);
         }
      }
   });
}

/// Keeps a press on `div` from reaching what is drawn under it. Children
/// bubble first, so their own listeners still run. Settings closes on a press
/// on its backdrop, which lies under anything drawn over it.
pub fn swallow_presses(div: &mut RawDiv) -> bool {
   on_mouse_down(div, &SWALLOW_PRESS_VTBL)
}

/// Pushes a zero-sized listener into `div`'s `Interactivity` mouse-down list,
/// which also makes GPUI give `div` a hitbox.
fn on_mouse_down(div: &mut RawDiv, vtbl: &'static RcFnVtbl) -> bool {
   listen_mouse_down(div, ptr::dangling_mut::<u8>(), vtbl)
}

/// Whether the pointer is over `hitbox`, for a mouse-down listener.
pub fn hovered(hitbox: *const c_void, window: *const c_void) -> bool {
   // SAFETY: GPUI passes listeners the live `&Hitbox` and `&Window`.
   DISMISS_SYM
      .get()
      .is_some_and(|sym| unsafe { (sym.is_hovered)(hitbox, window) })
}

/// [`on_mouse_down`] with `data` as the listener's `self`. A zero-sized
/// `vtbl` keeps GPUI from reading or freeing `data`, so it may be any
/// non-null value.
pub fn listen_mouse_down(div: &mut RawDiv, data: *mut u8, vtbl: &'static RcFnVtbl) -> bool {
   let Some(sym) = DISMISS_SYM.get() else {
      return false;
   };
   // SAFETY: `RawDiv` holds its `Box<Interactivity>` at `interactivity`
   // (decoded by `sigs`); `push` is Discord's own `Vec<Box<dyn ..>>::push`.
   unsafe {
      let interactivity = div
         .as_mut_ptr()
         .add(sym.interactivity)
         .cast::<*mut u8>()
         .read_unaligned();
      if interactivity.is_null() {
         return false;
      }
      (sym.push)(
         interactivity.add(sym.mouse_down_listeners).cast::<c_void>(),
         data,
         vtbl,
      );
   }
   true
}

/// `Modal::on_close` invokes `Fn::call(&self, window, cx)` — 3 arguments, no
/// click event argument.
unsafe extern "C" fn modal_close_callback(
   _self_ptr: *const u64,
   _window: *mut c_void,
   _cx: *mut c_void,
) {
   let _ = std::panic::catch_unwind(close_active);
}

/// A modal `Switch`'s `on_change`, with the Rust ABI of the `Fn::call` slot
/// it fills, like `gpui::switch_callback`.
unsafe fn modal_switch_callback(
   self_ptr: *const u64,
   checked: u8,
   _window: *mut c_void,
   _app: *mut c_void,
) {
   if self_ptr.is_null() {
      return;
   }
   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `self_ptr` points to `RcInner.action_id`.
      let id = unsafe { *self_ptr };
      if let Some(ModalAction::Switch(on_toggle)) = lookup_modal_action(id) {
         let on = checked != 0;
         std::thread::spawn(move || on_toggle(on));
      }
   });
}

/// Mana `Button::on_click` invokes `Fn::call(&self, event, window, app)` — 4
/// arguments, giving us a live `*mut App` for reading native `FieldState`s.
unsafe extern "C" fn modal_button_callback(
   self_ptr: *const u64,
   _event: *const c_void,
   _window: *mut c_void,
   app: *mut c_void,
) {
   if self_ptr.is_null() {
      return;
   }
   let _ = std::panic::catch_unwind(|| {
      if active_modal().active.is_none() {
         return;
      }
      // SAFETY: `self_ptr` points to `RcInner.action_id`.
      let id = unsafe { *self_ptr };
      let Some(action) = lookup_modal_action(id) else {
         return;
      };
      match action {
         ModalAction::Close => close_active(),
         ModalAction::PageButton(cb) => {
            std::thread::spawn(move || cb());
         },
         ModalAction::Switch(_) => {},
         ModalAction::CopyCode => {
            if let Some(code) = read_code_field(app) {
               match phoenix_ui_desktop::copy(&code) {
                  Ok(()) => phoenix_ui_desktop::toast("Copied plugin source"),
                  Err(err) => phoenix_ui_desktop::toast(&format!("Copy failed: {err}")),
               }
            }
         },
         ModalAction::SaveCode(plugin) => {
            let Some(code) = read_code_field(app) else {
               set_editor_status(FieldStatus::Failed("Could not read text field".to_owned()));
               return;
            };
            match phoenix_runtime::save_source(&plugin, code) {
               Ok(()) => {
                  close_active();
                  phoenix_ui_desktop::toast(&format!("Saved & reloaded {plugin}"));
               },
               Err(err) => {
                  set_editor_status(FieldStatus::Failed(format!("Save failed: {err}")));
               },
            }
         },
         ModalAction::SubmitInstallUrl => {
            let target = match active_modal().active.as_ref() {
               Some(ActiveModal::InstallUrl(inst)) => inst.target.clone(),
               _ => return,
            };
            match target {
               FieldTarget::Install => {},
               FieldTarget::Setting(field) => {
                  save_setting(app, &field);
                  return;
               },
               FieldTarget::Prompt(prompt) => {
                  let Some(raw) = read_url_field(app) else {
                     set_install_status(FieldStatus::Failed(
                        "Could not read the text field".to_owned(),
                     ));
                     return;
                  };
                  let text = raw.trim().to_owned();
                  if text.is_empty() {
                     return;
                  }
                  close_active();
                  std::thread::spawn(move || (prompt.on_submit)(text));
                  return;
               },
            }
            let Some(raw) = read_url_field(app) else {
               set_install_status(FieldStatus::Failed("Could not read URL field".to_owned()));
               return;
            };
            let url = raw.trim().to_owned();
            if url.is_empty() {
               set_install_status(FieldStatus::Failed(
                  "Enter an https:// link to a plugin .js file".to_owned(),
               ));
               return;
            }
            set_install_status(FieldStatus::Working(
               "Downloading and installing plugin...".to_owned(),
            ));
            std::thread::spawn(move || {
               match phoenix_runtime::install(&url) {
                  Ok(installed) => {
                     close_active();
                     phoenix_ui_desktop::toast(&installed.to_string());
                  },
                  Err(err) => {
                     set_install_status(FieldStatus::Failed(err.to_string()));
                  },
               }
            });
         },
      }
   });
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Opens a native GPUI modal for `phoenix.ui.openPage` and triggers a GPUI
/// redraw immediately.
pub fn open_page(spec: PageModalSpec) -> u64 {
   open_page_closing(spec, None)
}

type OnClose = Arc<dyn Fn() + Send + Sync>;

/// What runs when the user closes the page with this id (its Close button or
/// the scrim), but not when [`close_page`] closes it.
static ON_CLOSE: Mutex<Option<(u64, OnClose)>> = Mutex::new(None);

/// [`open_page`], running `on_close` on a worker thread when the user closes
/// it. Only the newest page keeps its `on_close`.
pub fn open_page_closing(spec: PageModalSpec, on_close: Option<OnClose>) -> u64 {
   resolve_pending_choice();
   let id = NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed);
   *ON_CLOSE.lock().unwrap_or_else(PoisonError::into_inner) = on_close.map(|run| (id, run));
   set_active(ActiveModal::Page { id, spec });
   id
}

/// Takes page `id`'s `on_close`, if it still has one.
fn take_on_close(id: u64) -> Option<OnClose> {
   let mut slot = ON_CLOSE.lock().unwrap_or_else(PoisonError::into_inner);
   if slot.as_ref().is_some_and(|(page, _)| *page == id) {
      slot.take().map(|(_, run)| run)
   } else {
      None
   }
}

/// Whether [`install`] hooked `Workspace::render_overlays`, which draws every
/// GPUI modal.
pub fn hooked() -> bool {
   ORIG_OVERLAY_RENDER.get().is_some()
}

/// Set once `Workspace::render_overlays` runs: Discord's window is drawing.
static WINDOW_SEEN: AtomicBool = AtomicBool::new(false);
/// Set once a modal was built into a frame.
static MODAL_RENDERED: AtomicBool = AtomicBool::new(false);

pub fn window_seen() -> bool {
   WINDOW_SEEN.load(Ordering::Acquire)
}

pub fn rendered() -> bool {
   MODAL_RENDERED.load(Ordering::Acquire)
}

fn set_active(modal: ActiveModal) {
   let mut guard = active_modal();
   guard.open(modal, Instant::now());
   drop(guard);
   force_redraw();
}

/// Opens a native choose or sheet dialog and returns what it will answer.
/// A `showSheet` body is a [`ModalSection::Code`]; a `choose` message is
/// [`ModalSection::Text`]. Without resolved GPUI modal symbols the dialog
/// cannot be drawn, so it answers -1 immediately.
pub fn open_choice(
   title: String,
   body: Option<ModalSection>,
   options: Vec<ChoiceOption>,
) -> Arc<Choice> {
   let choice = Arc::new(Choice::new());
   if ORIG_OVERLAY_RENDER.get().is_none() {
      choice.pick(-1);
      return choice;
   }
   resolve_pending_choice();
   let under = match active_modal().active.as_ref() {
      Some(ActiveModal::Page { id, spec }) => Some((*id, spec.clone())),
      Some(
         ActiveModal::Choice { under, .. }
         | ActiveModal::InstallUrl(InstallUrlModal { under, .. })
         | ActiveModal::CodeEditor(CodeEditorModal { under, .. }),
      ) => under.clone(),
      _ => None,
   };

   let mut sections = Vec::with_capacity(2);
   if let Some(body) = body {
      sections.push(body);
   }
   if !options.is_empty() {
      let buttons = options
         .into_iter()
         .enumerate()
         .map(|(index, option)| {
            let picked = Arc::clone(&choice);
            ModalButton {
               label:    option.label,
               danger:   option.danger,
               primary:  false,
               on_press: Arc::new(move || {
                  finish_choice(&picked, i32::try_from(index).unwrap_or(-1));
               }),
            }
         })
         .collect();
      sections.push(ModalSection::Buttons {
         title: None,
         buttons,
      });
   }
   set_active(ActiveModal::Choice {
      spec: PageModalSpec { title, sections },
      choice: Arc::clone(&choice),
      under,
   });
   choice
}

/// Opens a choose dialog and waits up to `timeout` for the picked index, -1
/// when it is dismissed or left open.
pub fn ask(
   title: String,
   body: Option<ModalSection>,
   options: Vec<ChoiceOption>,
   timeout: Duration,
) -> i32 {
   let choice = open_choice(title, body, options);
   choice.wait(timeout).unwrap_or_else(|| {
      dismiss_choice(&choice);
      -1
   })
}

/// Closes `choice`'s dialog if it is still the one on screen.
pub fn dismiss_choice(choice: &Arc<Choice>) {
   finish_choice(choice, -1);
}

/// Answers `choice` with `index` and closes its dialog, leaving any dialog
/// that already replaced it open.
fn finish_choice(choice: &Arc<Choice>, index: i32) {
   let mut guard = active_modal();
   let current = matches!(guard.active.as_ref(), Some(ActiveModal::Choice { choice: active, .. }) if Arc::ptr_eq(active, choice));
   choice.pick(index);
   if current {
      guard.close(Instant::now());
   }
   drop(guard);
   if current {
      force_redraw();
   }
}

/// Answers any open choose dialog with -1 before it is replaced.
fn resolve_pending_choice() {
   let guard = active_modal();
   if let Some(ActiveModal::Choice { choice, .. }) = guard.active.as_ref() {
      choice.pick(-1);
   }
}

/// The page a closing modal leaves on screen.
fn uncover(closing: &ActiveModal) -> Option<ActiveModal> {
   match closing {
      ActiveModal::Choice {
         under: Some((id, spec)),
         ..
      }
      | ActiveModal::InstallUrl(InstallUrlModal {
         under: Some((id, spec)),
         ..
      })
      | ActiveModal::CodeEditor(CodeEditorModal {
         under: Some((id, spec)),
         ..
      }) => {
         Some(ActiveModal::Page {
            id:   *id,
            spec: spec.clone(),
         })
      },
      _ => None,
   }
}

/// Swaps the sections of page `id`, on screen or under a choose dialog.
/// False when that page is gone.
pub fn replace_page(id: u64, spec: PageModalSpec) -> bool {
   let mut guard = active_modal();
   let slot = match guard.active.as_mut() {
      Some(
         ActiveModal::Page { id: cur, spec }
         | ActiveModal::Choice {
            under: Some((cur, spec)),
            ..
         },
      ) if *cur == id => spec,
      _ => return false,
   };
   *slot = spec;
   drop(guard);
   force_redraw();
   true
}

/// Closes an open page modal if its id still matches `id`.
pub fn close_page(id: u64) {
   let mut guard = active_modal();
   if matches!(&guard.active, Some(ActiveModal::Page { id: cur, .. }) if *cur == id)
      && guard.close(Instant::now())
   {
      drop(guard);
      force_redraw();
   }
}

/// Opens a native GPUI multiline code editor modal for `plugin`.
pub fn open_code_editor(plugin: String, initial_source: String) {
   resolve_pending_choice();
   let under = page_shown();
   set_active(ActiveModal::CodeEditor(CodeEditorModal {
      plugin,
      pending_init: Some(initial_source),
      status: FieldStatus::Idle,
      under,
   }));
}

/// Opens a native GPUI single-line URL installer modal.
pub fn open_install_url() {
   resolve_pending_choice();
   set_active(ActiveModal::InstallUrl(InstallUrlModal {
      target:       FieldTarget::Install,
      pending_init: Some(String::new()),
      status:       FieldStatus::Idle,
      under:        None,
   }));
}

/// Opens the one-line field on a plugin's secret setting.
pub fn open_setting_editor(field: SettingField) {
   resolve_pending_choice();
   let under = page_shown();
   set_active(ActiveModal::InstallUrl(InstallUrlModal {
      pending_init: Some(String::new()),
      target: FieldTarget::Setting(field),
      status: FieldStatus::Idle,
      under,
   }));
}

/// The page on screen, which a field opened over it returns to.
fn page_shown() -> Option<(u64, PageModalSpec)> {
   match active_modal().active.as_ref() {
      Some(ActiveModal::Page { id, spec }) => Some((*id, spec.clone())),
      _ => None,
   }
}

/// Opens the one-line field for `prompt`, which gets the text when it is
/// submitted.
pub fn open_prompt(prompt: Prompt) {
   resolve_pending_choice();
   set_active(ActiveModal::InstallUrl(InstallUrlModal {
      target:       FieldTarget::Prompt(prompt),
      pending_init: Some(String::new()),
      status:       FieldStatus::Idle,
      under:        None,
   }));
}

/// Stores the field's text as `field`'s value and closes the modal, or says
/// why it can't.
fn save_setting(app: *mut c_void, field: &SettingField) {
   let Some(raw) = read_url_field(app) else {
      set_install_status(FieldStatus::Failed(
         "Could not read the text field".to_owned(),
      ));
      return;
   };
   phoenix_runtime::set_setting(
      &field.plugin,
      &field.key,
      phoenix_runtime::Chosen::Text(raw.trim().to_owned()),
   );
   close_active();
   if let Some(then) = field.then.clone() {
      std::thread::spawn(move || then());
   }
}

fn close_active() {
   resolve_pending_choice();
   let mut guard = active_modal();
   if let Some(ActiveModal::Page { id, .. }) = guard.active.as_ref()
      && let Some(on_close) = take_on_close(*id)
   {
      std::thread::spawn(move || on_close());
   }
   let changed = guard.close(Instant::now());
   drop(guard);
   if changed {
      force_redraw();
   }
}

fn set_editor_status(status: FieldStatus) {
   let mut guard = active_modal();
   if let Some(ActiveModal::CodeEditor(ed)) = guard.active.as_mut() {
      ed.status = status;
   }
   drop(guard);
   force_redraw();
}

fn set_install_status(status: FieldStatus) {
   let mut guard = active_modal();
   if let Some(ActiveModal::InstallUrl(inst)) = guard.active.as_mut() {
      inst.status = status;
   }
   drop(guard);
   force_redraw();
}

/// Forces every visible GPUI window in this process to re-render, from any
/// thread.
pub fn force_redraw() {
   Os::force_redraw(WINDOW_VAL_OFFSET.load(Ordering::Acquire));
}

// ---------------------------------------------------------------------------
// Installation (`DllMain` / `crate::prepare`)
// ---------------------------------------------------------------------------

pub fn sret_wrap(addr: usize) -> Result<usize, &'static str> {
   if crate::X86_CODE {
      Ok(addr)
   } else {
      phoenix_hook::image::sret_caller(addr).ok_or("no memory for sret caller thunk")
   }
}

fn set_dismiss(found: &Dismiss, interactivity: usize) {
   // SAFETY: resolved and shape-checked against Discord by `sigs`.
   let _ = DISMISS_SYM.set(unsafe {
      DismissSymbols {
         push: std::mem::transmute::<usize, ListenerPushFn>(found.listener_push),
         is_hovered: std::mem::transmute::<usize, IsHoveredFn>(found.hitbox_is_hovered),
         interactivity,
         mouse_down_listeners: found.mouse_down_listeners,
         propagate_event: found.propagate_event,
      }
   });
}

/// Fallback on macOS if native modal resolution fails: still resolve
/// `mouse_down` so `rowDoubleTap` keeps working.
#[cfg(target_os = "macos")]
fn install_mouse_down_fallback(image: &Image<'static>, div: &desktop::DivLayout) {
   match phoenix_sigs::macos::mouse_down(image, div) {
      Ok(found) => {
         set_dismiss(&found, div.interactivity);
         log::info!("desktop mouse_down: resolved {found:x?}");
      },
      Err(err) => log::warn!("desktop mouse_down: {err}, rowDoubleTap disabled"),
   }
}

/// Locates Discord's native `Modal`, `Div::id`, `TextInput`/`TextArea`, and
/// window root render, then hooks `WindowRoot::render`.
#[expect(clippy::too_many_lines, reason = "one symbol table per located group")]
pub fn install(image: &Image<'static>) {
   let (Some(div_into_any), Some(gpui_layout)) = (gpui::div_into_any_addr(), gpui::layout()) else {
      log::warn!("desktop modal: GPUI builders unavailable, skipping");
      return;
   };
   if !gpui::supports_push_any() {
      log::warn!("desktop modal: Div::child(AnyElement) unavailable, skipping");
      return;
   }
   let located = if crate::X86_CODE {
      desktop::modal(image, div_into_any, &gpui_layout.div)
   } else {
      phoenix_sigs::macos::modal(image, div_into_any, &gpui_layout.div)
   };
   let found = match located {
      Ok(m) => m,
      Err(err) => {
         log::warn!("desktop modal: {err}, native modals disabled");
         #[cfg(target_os = "macos")]
         install_mouse_down_fallback(image, &gpui_layout.div);
         return;
      },
   };
   WINDOW_VAL_OFFSET.store(found.window_validation_offset, Ordering::Release);
   set_dismiss(&found.dismiss, gpui_layout.div.interactivity);

   let modal_sym = match (|| -> Result<ModalSymbols, &'static str> {
      let modal_new = sret_wrap(found.modal_new)?;
      let modal_into_any = sret_wrap(found.modal_into_any)?;
      let modal_child_div = sret_wrap(found.modal_child_div)?;
      let modal_footer_button = sret_wrap(found.modal_footer_button)?;
      let div_id = sret_wrap(found.div_id)?;
      // SAFETY: resolved and layout-verified against Discord.
      Ok(unsafe {
         ModalSymbols {
            vec_push_any:        std::mem::transmute::<usize, VecPushAnyFn>(found.vec_push_any),
            modal_new:           std::mem::transmute::<usize, ModalNewFn>(modal_new),
            modal_into_any:      std::mem::transmute::<usize, ModalIntoAnyFn>(modal_into_any),
            modal_child_div:     std::mem::transmute::<usize, ModalChildDivFn>(modal_child_div),
            modal_footer_button: std::mem::transmute::<usize, ModalFooterBtnFn>(
               modal_footer_button,
            ),
            div_id:              std::mem::transmute::<usize, DivIdFn>(div_id),
            layout:              found.layout,
         }
      })
   })() {
      Ok(sym) => sym,
      Err(err) => {
         log::warn!("desktop modal: {err}, native modals disabled");
         return;
      },
   };
   let _ = MODAL_SYM.set(modal_sym);

   let tf = if crate::X86_CODE {
      desktop::text_fields(image)
   } else {
      phoenix_sigs::macos::text_fields(image)
   };
   match tf {
      Ok(tf) => {
         let wrapped = (|| -> Result<TextFieldSymbols, &'static str> {
            let cx_new_single = sret_wrap(tf.cx_new_single)?;
            let cx_new_multiline = sret_wrap(tf.cx_new_multiline)?;
            let text_get = sret_wrap(tf.text_get)?;
            let text_input_new = sret_wrap(tf.text_input_new)?;
            let text_input_into_any = sret_wrap(tf.text_input_into_any)?;
            let text_area_new = sret_wrap(tf.text_area_new)?;
            let text_area_into_any = sret_wrap(tf.text_area_into_any)?;
            // SAFETY: resolved and cross-checked against Discord.
            Ok(unsafe {
               TextFieldSymbols {
                  cx_new_single:       std::mem::transmute::<usize, CxNewFieldFn>(cx_new_single),
                  cx_new_multiline:    std::mem::transmute::<usize, CxNewFieldFn>(cx_new_multiline),
                  entity_map_get:      std::mem::transmute::<usize, EntityMapGetFn>(
                     tf.entity_map_get,
                  ),
                  text_get:            std::mem::transmute::<usize, TextGetFn>(text_get),
                  arc_drop_slow:       std::mem::transmute::<usize, ArcDropSlowFn>(
                     tf.arc_drop_slow,
                  ),
                  text_input_new:      std::mem::transmute::<usize, TextInputNewFn>(text_input_new),
                  text_input_into_any: std::mem::transmute::<usize, TextInputIntoAnyFn>(
                     text_input_into_any,
                  ),
                  text_area_new:       std::mem::transmute::<usize, TextAreaNewFn>(text_area_new),
                  text_area_into_any:  std::mem::transmute::<usize, TextAreaIntoAnyFn>(
                     text_area_into_any,
                  ),
                  layout:              tf.layout,
               }
            })
         })();
         match wrapped {
            Ok(sym) => {
               let _ = FIELD_SYM.set(sym);
               log::info!(
                  "desktop modal: resolved native TextInput/TextArea (single={:#x}, multi={:#x}) \
                   {:x?}",
                  tf.cx_new_single,
                  tf.cx_new_multiline,
                  tf.layout,
               );
            },
            Err(err) => {
               log::warn!("desktop modal: native TextInput/TextArea thunk failed: {err}");
            },
         }
      },
      Err(err) => {
         log::warn!("desktop modal: native TextInput/TextArea unavailable: {err}");
      },
   }

   // SAFETY: runs from `prepare` before Discord threads execute;
   // `overlay_render_detour` matches `Workspace::render_overlays`'s 5-arg sret
   // ABI returning `Vec<AnyElement>` (via `sret_callee`/`sret_caller` on
   // arm64).
   let Some(orig) = (unsafe {
      crate::hook_sret::<OverlayRenderFn>(
         image,
         found.overlay_render,
         overlay_render_detour as *const () as usize,
         "desktop modal: overlay render hook failed",
      )
   }) else {
      return;
   };
   let _ = ORIG_OVERLAY_RENDER.set(orig);
   install_settings_layer(image);
   install_transitions(image);
   crate::notices::enable();
   log::info!(
      "desktop modal: hooked Workspace::render_overlays @ {:#x} (push={:#x}, val +{:#x}) {:x?}",
      found.overlay_render,
      found.vec_push_any,
      found.window_validation_offset,
      found.layout,
   );
}

/// Hooks the Settings layer's render, so modals opened over Settings draw
/// above it.
fn install_settings_layer(image: &Image<'static>) {
   let located = if crate::X86_CODE {
      desktop::settings_layer(image)
   } else {
      phoenix_sigs::macos::settings_layer(image)
   };
   let func = match located {
      Ok(func) => func,
      Err(err) => {
         log::warn!("desktop modal: {err}, modals stay under Settings");
         return;
      },
   };
   // SAFETY: from `prepare`, before Discord's threads run; the detour has the
   // Settings layer render's sret signature.
   if let Some(orig) = unsafe {
      crate::hook_sret::<SettingsLayerFn>(
         image,
         func,
         settings_layer_detour as *const () as usize,
         "desktop modal: Settings layer hook failed",
      )
   } {
      let _ = ORIG_SETTINGS_LAYER.set(orig);
      log::info!("desktop modal: hooked Settings layer @ {func:#x}");
   }
}

fn install_transitions(image: &Image<'static>) {
   let anim = if crate::X86_CODE {
      desktop::animation_frame(image)
   } else {
      phoenix_sigs::macos::animation_frame(image)
   };
   match anim {
      Ok(addr) => {
         // SAFETY: resolved GPUI `animation_frame(window, app)` with C/Rust ABI.
         let _ = ANIMATION_FRAME.set(unsafe {
            std::mem::transmute::<usize, AnimationFrameFn>(addr)
         });
         log::info!("desktop modal: resolved animation_frame @ {addr:#x}");
      },
      Err(err) => log::info!("desktop modal: animation_frame unavailable ({err}), using timer"),
   }

   if !crate::X86_CODE {
      match phoenix_sigs::macos::scale_fade(image) {
         Ok(sf) => {
            match sret_wrap(sf.into_any) {
               Ok(into_any) => {
                  // SAFETY: resolved `ScaleFade::into_any_element` and layout
                  // verified against Discord's binary by `sigs`.
                  let _ = SCALE_FADE.set(unsafe {
                     ScaleFadeSymbols {
                        into_any:   std::mem::transmute::<usize, ScaleFadeIntoAnyFn>(into_any),
                        caller_loc: sf.caller_loc as *const c_void,
                        layout:     sf.layout,
                     }
                  });
                  log::info!(
                     "desktop modal: resolved ScaleFade @ {:#x} {:x?}",
                     sf.into_any,
                     sf.layout,
                  );
               },
               Err(err) => log::warn!("desktop modal: ScaleFade thunk failed: {err}"),
            }
         },
         Err(err) => log::info!("desktop modal: ScaleFade unavailable ({err}), fading opacity"),
      }
   }
}

// ---------------------------------------------------------------------------
// Native FieldState Helpers (GPUI Main Thread Only)
// ---------------------------------------------------------------------------

/// Lazily initializes a single-line or multi-line `Entity<FieldState>` on the
/// GPUI main thread and writes its static placeholder.
unsafe fn ensure_field_entity(
   app: *mut c_void,
   window: *mut c_void,
   which: FieldSlot,
   placeholder: &'static str,
) -> Option<RawEntity> {
   let multiline = which == FieldSlot::Code;
   let mut guard = FIELD_ENTITIES
      .lock()
      .unwrap_or_else(PoisonError::into_inner);
   let slot = match which {
      FieldSlot::Url => &mut guard.url,
      FieldSlot::Code => &mut guard.code,
      FieldSlot::Search => &mut guard.search,
   };
   if let Some(ent) = *slot {
      return Some(ent);
   }
   // SAFETY: as this function's.
   let ent = unsafe { new_field_entity(app, window, multiline, placeholder) }?;
   *slot = Some(ent);
   drop(guard);
   Some(ent)
}

/// The live `FieldState` behind `entity`, or null.
unsafe fn field_ptr(app: *mut c_void, entity: &RawEntity) -> *mut c_void {
   let Some(tf) = FIELD_SYM.get() else {
      return ptr::null_mut();
   };
   if app.is_null() {
      return ptr::null_mut();
   }
   // SAFETY: `app` is live and the entity id came from it.
   unsafe {
      (tf.entity_map_get)(
         app.cast::<u8>()
            .add(tf.layout.app_entities)
            .cast::<c_void>(),
         entity.id_lo,
         entity.id_hi,
      )
   }
}

/// Writes `text` into the field's pending-value slot, which its `TextInput`
/// applies.
unsafe fn write_field_pending(tf: &TextFieldSymbols, field: *mut c_void, text: &str) {
   if field.is_null() || field as usize == 1 {
      return;
   }
   // SAFETY: `field` is a live `FieldState` and `pending_value` is in bounds.
   #[expect(clippy::cast_ptr_alignment, reason = "a `FieldState` slot is word aligned")]
   let slot = unsafe { field.cast::<u8>().add(tf.layout.pending_value) }.cast::<SharedString>();
   // SAFETY: `slot` is a live `Option<SharedString>`; `0xff` marks `None`.
   let previous = unsafe { slot.read_unaligned() };
   // SAFETY: writing the new value into the same slot.
   unsafe { slot.write_unaligned(SharedString::from_str(text)) };
   // SAFETY: `previous` is either `None` or the value just replaced.
   unsafe { release_shared_string(tf, &previous) };
}

/// Drops a heap-backed `SharedString` read out of a field slot.
unsafe fn release_shared_string(tf: &TextFieldSymbols, ss: &SharedString) {
    if ss.tag != SharedString::ARC || ss.ptr.is_null() {
        return;
    }
    #[expect(clippy::cast_ptr_alignment, reason = "an `Arc` header is word aligned")]
    // SAFETY: a heap `SharedString`'s `ptr` is its `ArcInner`, whose first word
    // is the strong count.
    let strong = unsafe { &*ss.ptr.cast::<AtomicUsize>() };
    if strong.fetch_sub(1, Ordering::Release) == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        // SAFETY: the last strong reference; `arc_drop_slow` is
        // `Arc<str>::drop_slow`, which takes the `ArcInner<str>` fat pointer
        // (payload pointer + payload length) and frees the allocation.
        unsafe { (tf.arc_drop_slow)(ss.ptr, ss.len) };
    }
}

/// A new single-line or multi-line `Entity<FieldState>` showing
/// `placeholder` while empty.
unsafe fn new_field_entity(
   app: *mut c_void,
   window: *mut c_void,
   multiline: bool,
   placeholder: &'static str,
) -> Option<RawEntity> {
   let tf = FIELD_SYM.get()?;
   if app.is_null() || window.is_null() {
      return None;
   }
   // SAFETY: invoked on the GPUI main thread inside `WindowRoot::render` with
   // valid `app` and `window` pointers.
   let ent = unsafe {
      let mut out = MaybeUninit::<RawEntity>::uninit();
      if multiline {
         (tf.cx_new_multiline)(out.as_mut_ptr(), app, window);
      } else {
         (tf.cx_new_single)(out.as_mut_ptr(), app, window);
      }
      let ent = out.assume_init();
      let field = (tf.entity_map_get)(
         app.cast::<u8>()
            .add(tf.layout.app_entities)
            .cast::<c_void>(),
         ent.id_lo,
         ent.id_hi,
      );
      if !field.is_null() && field as usize != 1 {
         for slot in tf.layout.placeholders {
            field
               .cast::<u8>()
               .add(slot)
               .cast::<SharedString>()
               .write_unaligned(SharedString::from_static(placeholder));
         }
      }
      ent
   };
   Some(ent)
}

unsafe fn read_field_text(app: *mut c_void, entity: &RawEntity) -> Option<String> {
    let tf = FIELD_SYM.get()?;
    if app.is_null() {
        return None;
    }
    // SAFETY: called on the GPUI main thread with a live `app`, and `entity`
    // is an `Entity<FieldState>` still mapped in `app`'s entity map.
    let field = unsafe {
        (tf.entity_map_get)(
            app.cast::<u8>()
                .add(tf.layout.app_entities)
                .cast::<c_void>(),
            entity.id_lo,
            entity.id_hi,
        )
    };
    if field.is_null() || field as usize == 1 {
        return None;
    }
    // SAFETY: `field` is a live `FieldState`; `text_get` writes an
    // initialised `SharedString` through the out-pointer on success.
    let mut ss = MaybeUninit::<SharedString>::uninit();
    unsafe { (tf.text_get)(ss.as_mut_ptr(), field, app) };
    // SAFETY: `text_get` returned normally, so `ss` is initialised.
    let ss = unsafe { ss.assume_init() };
    
    // SAFETY: `ss` is a live `SharedString` for the duration of the borrow.
    let result = unsafe { decode_shared_string(&ss) };
    
    // SAFETY: `ss` is still the value `text_get` produced and is released
    // exactly once here.
    unsafe { release_shared_string(tf, &ss) };
    
    Some(result)
}

unsafe fn decode_shared_string(ss: &SharedString) -> String {
   let tag = ss.tag;
   if tag < SharedString::STATIC {
      // SAFETY: inline bytes live at offsets `1..=tag` inside `SharedString`.
      let bytes =
         unsafe { slice::from_raw_parts(ptr::from_ref(ss).cast::<u8>().add(1), usize::from(tag)) };
      return String::from_utf8_lossy(bytes).into_owned();
   }
   if tag == SharedString::STATIC {
      if ss.ptr.is_null() || ss.len == 0 {
         return String::new();
      }
      // SAFETY: static slice `ptr..ptr + len`.
      let bytes = unsafe { slice::from_raw_parts(ss.ptr, ss.len) };
      return String::from_utf8_lossy(bytes).into_owned();
   }
   if tag == SharedString::ARC {
      if ss.ptr.is_null() || ss.len == 0 {
         return String::new();
      }
      // SAFETY: `ArcInner` has `strong: usize` (+0), `weak: usize` (+8), UTF-8
      // payload at `+0x10` for `ss.len` bytes.
      let bytes = unsafe { slice::from_raw_parts(ss.ptr.add(0x10), ss.len) };
      return String::from_utf8_lossy(bytes).into_owned();
   }
   String::new()
}

fn read_code_field(app: *mut c_void) -> Option<String> {
   let ent = FIELD_ENTITIES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .code?;
   // SAFETY: called from GPUI button callback with live `app`.
   unsafe { read_field_text(app, &ent) }
}

fn read_url_field(app: *mut c_void) -> Option<String> {
   let ent = FIELD_ENTITIES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .url?;
   // SAFETY: called from GPUI button callback with live `app`.
   unsafe { read_field_text(app, &ent) }
}

// ---------------------------------------------------------------------------
// Workspace::render_overlays Detour & Native Modal Builder
// ---------------------------------------------------------------------------

unsafe extern "C" fn overlay_render_detour(
   sret: *mut RawVecAny,
   workspace: *mut c_void,
   arg3: *mut c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawVecAny {
   let Some(orig) = ORIG_OVERLAY_RENDER.get().copied() else {
      return sret;
   };
   if !cx.is_null() {
      // SAFETY: `[cx]` is `*mut gpui::App` inside `Context<Workspace>`.
      let app = unsafe { *(cx.cast::<*mut c_void>()) };
      // SAFETY: `app` is null or a live `&mut gpui::App` on the UI thread.
      unsafe { crate::mana_theme::flush_pending(app) };
   }
   // SAFETY: always render Discord's own overlay stack first.
   let out = unsafe { orig(sret, workspace, arg3, window, cx) };
   WINDOW_SEEN.store(true, Ordering::Release);
   push_modal(sret, window, cx);
   // Banners go here even while Settings is open and draws its own copy over
   // itself: this one lies right under it, and is already there the moment
   // Settings closes instead of after `SETTINGS_RECENT_MS`.
   if !sret.is_null() {
      let _ = std::panic::catch_unwind(|| {
         if let Some(notices) = crate::notices::build() {
            push_overlay(sret, notices);
         }
      });
   }
   out
}

/// Appends `div` to the overlays Discord rendered into `sret`.
fn push_overlay(sret: *mut RawVecAny, div: RawDiv) {
   let Some(msym) = MODAL_SYM.get() else {
      return;
   };
   let mut any = MaybeUninit::<RawAnyElement>::uninit();
   gpui::write_any(div, any.as_mut_ptr());
   // SAFETY: `any` was initialized by `div_into_any` and `vec_push_any` is
   // Discord's own `Vec::<AnyElement>::push`.
   unsafe {
      let mut any = any.assume_init();
      (msym.vec_push_any)(sret, &raw mut any);
   }
}

/// Whether Settings drew within [`SETTINGS_RECENT_MS`], drawing the overlays
/// over itself. Redraws once that passes, in case it closed.
fn settings_drawn_recently() -> bool {
   let at = SETTINGS_AT.load(Ordering::Acquire);
   if at == 0 || epoch_ms().saturating_sub(at) >= SETTINGS_RECENT_MS {
      return false;
   }
   if !RECHECK.swap(true, Ordering::AcqRel) {
      std::thread::spawn(|| {
         std::thread::sleep(std::time::Duration::from_millis(SETTINGS_RECENT_MS + 100));
         RECHECK.store(false, Ordering::Release);
         force_redraw();
      });
   }
   true
}

fn push_modal(sret: *mut RawVecAny, window: *mut c_void, cx: *mut c_void) {
   if !MODAL_OPEN.load(Ordering::Acquire) {
      IN_SETTINGS.store(false, Ordering::Release);
      return;
   }
   // ponytail: closing Settings under an open modal hides the modal until
   // Settings opens again or the modal is replaced; nothing tells this render
   // apart from one where Settings only replays its last frame.
   if IN_SETTINGS.load(Ordering::Acquire) || sret.is_null() || cx.is_null() {
      return;
   }
   // Settings rendering after this in the same frame would build the modal a
   // second time, so leave it to Settings while it was just on screen, and look
   // again once that has passed in case it closed.
   // ponytail: Settings left idle past this before a modal opens from a timer
   // still gets one frame with the modal built in both places.
   if settings_drawn_recently() {
      return;
   }

   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `[cx]` is `*mut App` inside `Context<Workspace>`.
      let app = unsafe { *(cx.cast::<*mut c_void>()) };
      if let Some(scrim) = build_overlay(window, app) {
         push_overlay(sret, scrim);
         MODAL_RENDERED.store(true, Ordering::Release);
      }
   });
}

unsafe extern "C" fn settings_layer_detour(
   sret: *mut RawAnyElement,
   layer: *mut c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement {
   let Some(orig) = ORIG_SETTINGS_LAYER.get().copied() else {
      return sret;
   };
   // SAFETY: Discord's own render first, with its own arguments.
   let out = unsafe { orig(sret, layer, window, cx) };
   SETTINGS_AT.store(epoch_ms().max(1), Ordering::Release);
   let modal = MODAL_OPEN.load(Ordering::Acquire);
   if !modal {
      IN_SETTINGS.store(false, Ordering::Release);
   }
   if sret.is_null() || cx.is_null() {
      return out;
   }
   if modal {
      IN_SETTINGS.store(true, Ordering::Release);
   }
   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `[cx]` is `*mut App` inside the layer's `Context`.
      let app = unsafe { *(cx.cast::<*mut c_void>()) };
      let scrim = if modal {
         build_overlay(window, app)
      } else {
         None
      };
      // Settings draws over the workspace's overlays, so banners go on it too.
      let notices = crate::notices::build();
      if scrim.is_none() && notices.is_none() {
         return;
      }
      if scrim.is_some() {
         MODAL_RENDERED.store(true, Ordering::Release);
      }
      let Some(mut over) = gpui::raw_div() else {
         return;
      };
      gpui::style_div(&mut over, |s| {
         s.set(s.at.position, POSITION_ABSOLUTE);
         s.lengths(&s.at.inset, gpui_px(0.0));
         s.lengths(&s.at.size, W_FULL);
      });
      // SAFETY: takes the rendered Settings `AnyElement` out of `sret`; a
      // replacement is written back below either way.
      let mut settings = unsafe { ptr::read(sret) };
      if !gpui::push_any(&mut over, &mut settings) {
         // SAFETY: puts the untouched Settings element back.
         unsafe { ptr::write(sret, settings) };
         return;
      }
      for layer in [scrim, notices].into_iter().flatten() {
         over = gpui::push_div(over, layer);
      }
      gpui::write_any(over, sret);
   });
   out
}

const WHITE_SPACE_NOWRAP: u8 = 1;
const FLEX_NOWRAP: u8 = 0;
const FLEX_WRAP: u8 = 1;
const OVERFLOW_SCROLL: u8 = 3;
const POSITION_ABSOLUTE: u8 = 1;
const DISPLAY_FLEX: u8 = 1;
const ALIGN_CENTER: u8 = 4;

/// Lines the plugin code editor shows; it scrolls past them.
const EDITOR_ROWS: u8 = 30;
/// Rows of a multiline setting's field.
const SETTING_ROWS: u8 = 8;

fn set_mono_font(s: &Style, font_px: f32, line_px: f32) {
   s.length(s.at.font_size, gpui_px(font_px));
   s.length(s.at.line_height, gpui_px(line_px));
   s.shared_string(s.at.font_family, SharedString::from_static(MONO_FONT));
   s.set(s.at.white_space, WHITE_SPACE_NOWRAP);
}

fn div_with_id(msym: &ModalSymbols, mut div: RawDiv, id: &str) -> RawDiv {
   let id_s = intern_str(id);
   let mut out = RawDiv::zeroed();
   // SAFETY: `Div::id` moves `div` into `out` and enables native mouse-wheel
   // scrolling via `Interactivity.element_id`.
   unsafe { (msym.div_id)(&raw mut out, &raw mut div, id_s.as_ptr(), id_s.len()) };
   out
}

fn make_modal_button(
   id: &str,
   label: &str,
   variant: BtnVariant,
   size: BtnSize,
   action: ModalAction,
) -> Option<RawButton> {
   let action_id = register_modal_action(action);
   gpui::build_raw_button(
      id,
      label,
      variant,
      size,
      &raw const MODAL_BTN_VTBL,
      action_id,
   )
}

fn push_modal_button(
   parent: RawDiv,
   id: &str,
   label: &str,
   variant: BtnVariant,
   size: BtnSize,
   action: ModalAction,
) -> RawDiv {
   let Some(btn) = make_modal_button(id, label, variant, size, action) else {
      return parent;
   };
   gpui::push_raw_button(parent, btn)
}

fn mono_span(text: &str, argb: u32) -> Option<RawDiv> {
   let mut d = gpui::raw_div()?;
   let hsla = argb_to_hsla(argb);
   gpui::style_div(&mut d, |s| {
      set_mono_font(s, 12.5, 18.0);
      s.hsla(s.at.text_color, hsla);
      // Prevent token spans from shrinking inside flex-row lines.
      s.float(s.at.flex_shrink, 0.0);
   });
   let mut any = gpui::str_to_any(text)?;
   if !gpui::push_any(&mut d, &mut any) {
      return None;
   }
   Some(d)
}

/// `text` in `argb`, or without a color of its own when `None`, which takes
/// the modal's, light or dark with Discord's theme.
fn plain_text_div(text: &str, argb: Option<u32>, font_px: f32) -> Option<RawDiv> {
   let mut d = gpui::raw_div()?;
   gpui::style_div(&mut d, |s| {
      s.length(s.at.font_size, gpui_px(font_px));
      s.length(s.at.line_height, gpui_px(font_px + 5.0));
      if let Some(argb) = argb {
         s.hsla(s.at.text_color, argb_to_hsla(argb));
      }
   });
   let mut any = gpui::str_to_any(text)?;
   if !gpui::push_any(&mut d, &mut any) {
      return None;
   }
   Some(d)
}

/// Joins same-colored neighbors and folds whitespace into a neighbor. Every
/// span is a full GPUI `Div` rebuilt each frame, so a JSON line drops from
/// about six to three or four.
fn coalesce<'a>(tokens: impl Iterator<Item = (u32, &'a str)>) -> Vec<(u32, String)> {
   let mut spans: Vec<(u32, String)> = Vec::new();
   for (color, tok) in tokens {
      let blank = tok.trim().is_empty();
      match spans.last_mut() {
         Some((last, text)) if blank || *last == color || text.trim().is_empty() => {
            if !blank {
               *last = color;
            }
            text.push_str(tok);
         },
         _ => spans.push((color, tok.to_owned())),
      }
   }
   spans
}

fn build_code_box(
   msym: &ModalSymbols,
   box_id: &str,
   text: &str,
   monospace: bool,
   do_highlight: bool,
) -> Option<RawDiv> {
   let pal = crate::mana_theme::sunbird_palette();
   let plain = crate::mana_theme::color_to_argb(pal.text);
   let syntax = if pal.bg_code.relative_luminance() > 0.35 {
      &SYNTAX_LIGHT
   } else {
      &SYNTAX_DARK
   };
   let mut box_div = gpui::v_stack(1.0)?;
   box_div = div_with_id(msym, box_div, box_id);
   gpui::style_div(&mut box_div, |s| {
      s.solid_background(crate::mana_theme::color_to_hsla(pal.bg_code, Some(1.0)));
      s.lengths(&s.at.border_widths, gpui_px(1.0));
      s.lengths(&s.at.corner_radii, gpui_px(8.0));
      s.lengths(&s.at.padding, gpui_px(12.0));
      s.length(s.at.size[0], W_FULL);
      s.hsla(
         s.at.border_color,
         crate::mana_theme::color_to_hsla(pal.border, Some(0.35)),
      );
      s.set(s.at.overflow_x, OVERFLOW_SCROLL);
      if monospace {
         set_mono_font(s, 12.5, 18.0);
      }
   });

   let is_json = do_highlight && highlight::looks_like_json(text);
   for (idx, line) in text.split('\n').enumerate() {
      let line = line.strip_suffix('\r').unwrap_or(line);
      if line.is_empty() {
         if let Some(sp) = mono_span(" ", plain) {
            box_div = gpui::push_div(box_div, sp);
         }
         continue;
      }
      if do_highlight && idx < MAX_HIGHLIGHT_LINES {
         let Some(mut row) = gpui::h_row(0.0, false) else {
            continue;
         };
         gpui::style_div(&mut row, |s| {
            s.set(s.at.white_space, WHITE_SPACE_NOWRAP);
            s.set(s.at.flex_wrap, FLEX_NOWRAP);
         });
         let spans = if is_json {
            coalesce(highlight::json(line).into_iter().map(|(kind, tok)| {
               let color = match kind {
                  JsonKind::Key => syntax.key,
                  JsonKind::String => syntax.string,
                  JsonKind::Number => syntax.number,
                  JsonKind::Bool => syntax.bool_,
                  JsonKind::Null => syntax.null,
                  JsonKind::Punctuation => syntax.punct,
                  JsonKind::Plain => plain,
               };
               (color, tok)
            }))
         } else {
            coalesce(highlight::js(line).into_iter().map(|(kind, tok)| {
               let color = match kind {
                  Kind::Keyword => syntax.keyword,
                  Kind::String => syntax.string,
                  Kind::Number => syntax.number,
                  Kind::Comment => syntax.comment,
                  Kind::Plain => plain,
               };
               (color, tok)
            }))
         };
         for (color, text) in &spans {
            if let Some(span) = mono_span(text, *color) {
               row = gpui::push_div(row, span);
            }
         }
         box_div = gpui::push_div(box_div, row);
      } else if monospace {
         if let Some(span) = mono_span(line, plain) {
            box_div = gpui::push_div(box_div, span);
         }
      } else if let Some(div) = plain_text_div(line, Some(plain), 13.0) {
         box_div = gpui::push_div(box_div, div);
      }
   }
   Some(box_div)
}

#[expect(clippy::too_many_lines, reason = "one arm per section kind")]
fn build_page_body(
   msym: &ModalSymbols,
   spec: &PageModalSpec,
   window: *mut c_void,
   app: *mut c_void,
) -> Option<RawDiv> {
   let muted = col_muted();
   let mut body = gpui::v_stack(14.0)?;
   gpui::style_div(&mut body, |s| s.length(s.at.size[0], W_FULL));
   for (sec_idx, section) in spec.sections.iter().enumerate() {
      match section {
         ModalSection::Buttons { title, buttons } => {
            let mut block = gpui::v_stack(8.0)?;
            if let Some(t) = title
               && !t.is_empty()
               && let Some(hdr) = plain_text_div(t, Some(muted), 12.0)
            {
               block = gpui::push_div(block, hdr);
            }
            let mut row = gpui::h_row(8.0, false)?;
            gpui::style_div(&mut row, |s| s.set(s.at.flex_wrap, FLEX_WRAP));
            for (btn_idx, b) in buttons.iter().enumerate() {
               row = push_modal_button(
                  row,
                  &format!("sunbird-modal-btn-{sec_idx}-{btn_idx}"),
                  &b.label,
                  if b.danger {
                     BtnVariant::Critical
                  } else if b.primary {
                     BtnVariant::Primary
                  } else {
                     BtnVariant::Secondary
                  },
                  BtnSize::Small,
                  ModalAction::PageButton(Arc::clone(&b.on_press)),
               );
            }
            block = gpui::push_div(block, row);
            body = gpui::push_div(body, block);
         },
         ModalSection::Text { title, text } => {
            let mut block = gpui::v_stack(6.0)?;
            if let Some(t) = title
               && !t.is_empty()
               && let Some(hdr) = plain_text_div(t, Some(muted), 12.0)
            {
               block = gpui::push_div(block, hdr);
            }
            for line in text.lines() {
               if let Some(d) = plain_text_div(line, None, 13.5) {
                  block = gpui::push_div(block, d);
               }
            }
            body = gpui::push_div(body, block);
         },
         ModalSection::Code {
            title,
            text,
            monospace,
            highlight,
         } => {
            let mut block = gpui::v_stack(6.0)?;
            if let Some(t) = title
               && !t.is_empty()
               && let Some(hdr) = plain_text_div(t, Some(muted), 12.0)
            {
               block = gpui::push_div(block, hdr);
            }
            if let Some(code_box) = build_code_box(
               msym,
               &format!("sunbird-code-{sec_idx}"),
               text,
               *monospace,
               *highlight,
            ) {
               block = gpui::push_div(block, code_box);
            }
            body = gpui::push_div(body, block);
         },
         ModalSection::Switch {
            id,
            label,
            description,
            checked,
            on_toggle,
         } => {
            let action = register_modal_action(ModalAction::Switch(Arc::clone(on_toggle)));
            if let Some(row) = gpui::build_raw_switch(
               id,
               label,
               description.as_deref(),
               *checked,
               &raw const MODAL_SWITCH_VTBL,
               action,
            ) {
               body = gpui::push_div(body, row);
            }
         },
         ModalSection::Field {
            id,
            label,
            description,
            initial,
            multiline,
            on_change,
         } => {
            let field = PageFieldSpec {
               id,
               initial,
               multiline: *multiline,
               on_change,
            };
            let block = field_block(window, app, label, description.as_deref(), &field)?;
            body = gpui::push_div(body, block);
         },
      }
   }
   Some(body)
}

/// What [`page_field`] draws and reports to.
struct PageFieldSpec<'a> {
   id:        &'a str,
   initial:   &'a str,
   multiline: bool,
   on_change: &'a Arc<dyn Fn(String) + Send + Sync>,
}

/// A [`ModalSection::Field`]: its label, description, and text box.
fn field_block(
   window: *mut c_void,
   app: *mut c_void,
   label: &str,
   description: Option<&str>,
   field: &PageFieldSpec<'_>,
) -> Option<RawDiv> {
   let mut block = gpui::v_stack(6.0)?;
   if let Some(hdr) = plain_text_div(label, None, 14.0) {
      block = gpui::push_div(block, hdr);
   }
   if let Some(desc) = description.filter(|desc| !desc.is_empty())
      && let Some(line) = plain_text_div(desc, Some(col_muted()), 12.0)
   {
      block = gpui::push_div(block, line);
   }
   // SAFETY: called while the overlay renders on the GPUI main thread, with
   // that frame's live `window` and `app`.
   if let Some(mut element) = unsafe { page_field(window, app, field) } {
      let mut wrap = gpui::raw_div()?;
      gpui::style_div(&mut wrap, |s| s.length(s.at.size[0], W_FULL));
      if gpui::push_any(&mut wrap, &mut element) {
         block = gpui::push_div(block, wrap);
      }
   }
   Some(block)
}

/// Section `id`'s text box: made and filled with `initial` the first time,
/// filled again after [`refill_page_fields`], and its text handed to
/// `on_change` whenever it differs from the last frame's.
unsafe fn page_field(
   window: *mut c_void,
   app: *mut c_void,
   spec: &PageFieldSpec<'_>,
) -> Option<RawAnyElement> {
   let PageFieldSpec {
      id,
      initial,
      multiline,
      on_change,
   } = *spec;
   let tf = FIELD_SYM.get()?;

   let needs_init = {
      let fields = PAGE_FIELDS.lock().unwrap_or_else(PoisonError::into_inner);
      !fields.contains_key(id)
   };

   if needs_init {
      // SAFETY: `app` and `window` are live GPUI handles.
      let entity = unsafe { new_field_entity(app, window, multiline, "") }?;
      let mut fields = PAGE_FIELDS.lock().unwrap_or_else(PoisonError::into_inner);
      fields.insert(
         id.to_owned(),
         PageFieldState {
            entity,
            seen: String::new(),
            refill: true,
            fresh: false,
         },
      );
   }

   let (entity, refill) = {
      let mut fields = PAGE_FIELDS.lock().unwrap_or_else(PoisonError::into_inner);
      let field = fields.get_mut(id)?;
      let refill = field.refill;
      if refill {
         field.refill = false;
         field.seen.clear();
         field.fresh = true;
      }
      let entity = field.entity;
      drop(fields);
      (entity, refill)
   };

   if refill {
      // SAFETY: `app` is live and the field's entity is not leased mid-render.
      let field = unsafe { field_ptr(app, &entity) };
      // SAFETY: `field` is the live `FieldState` behind `entity`.
      unsafe { write_field_pending(tf, field, initial) };
      return Some(unsafe { page_text_input(tf, entity, multiline) });
   }

   // SAFETY: as above.
   let text = unsafe { read_field_text(app, &entity) }.unwrap_or_default();

   let changed = {
      let mut fields = PAGE_FIELDS.lock().unwrap_or_else(PoisonError::into_inner);
      let Some(field) = fields.get_mut(id) else {
         return Some(unsafe { page_text_input(tf, entity, multiline) });
      };
      let fresh = std::mem::take(&mut field.fresh);
      let changed = !fresh && text != field.seen;
      if fresh || changed {
         field.seen.clone_from(&text);
      }
      drop(fields);
      changed
   };

   if changed {
      let on_change = Arc::clone(on_change);
      std::thread::spawn(move || on_change(text));
   }

   // SAFETY: builds a hosted `TextInput`, or a `TextArea` of
   // [`SETTING_ROWS`] rows, from `&Entity<FieldState>` in a buffer that fits
   // it, and moves it into an `AnyElement`.
   Some(unsafe { page_text_input(tf, entity, multiline) })
}

unsafe fn page_text_input(
   tf: &TextFieldSymbols,
   entity: RawEntity,
   multiline: bool,
) -> RawAnyElement {
   let mut any = MaybeUninit::<RawAnyElement>::uninit();
   if multiline {
      let mut ta = RawTextArea::zeroed();
      // SAFETY: `entity` is a live handle and `ta` is a zeroed local the
      // constructor writes an initialised `RawTextArea` into.
      unsafe { (tf.text_area_new)(&raw mut ta, &raw const entity) };
      if let Some(rows) = tf.layout.text_area_rows {
         ta.put(rows, [1u8, SETTING_ROWS]);
         ta.put(rows + 2, [1u8, SETTING_ROWS]);
      }
      // SAFETY: `ta` is initialised above; `into_any` moves it into `any`.
      unsafe { (tf.text_area_into_any)(any.as_mut_ptr(), &raw mut ta) };
   } else {
      let mut ti = RawTextInput::zeroed();
      // SAFETY: `entity` is a live handle and `ti` is a zeroed local the
      // constructor writes an initialised `RawTextInput` into.
      unsafe { (tf.text_input_new)(&raw mut ti, &raw const entity) };
      // SAFETY: `ti` is initialised above; `into_any` moves it into `any`.
      unsafe { (tf.text_input_into_any)(any.as_mut_ptr(), &raw mut ti) };
   }
   // SAFETY: whichever branch ran wrote an initialised `RawAnyElement` into `any`.
   unsafe { any.assume_init() }
}

#[expect(
   clippy::too_many_lines,
   reason = "assembles the single active native Mana Modal and scrim overlay"
)]
fn build_overlay(window: *mut c_void, app: *mut c_void) -> Option<RawDiv> {
   let msym = MODAL_SYM.get()?;

   let RenderFrame {
      snapshot: modal_snapshot,
      progress,
      animating,
      interactive,
      instance_id,
   } = active_modal().render_frame(Instant::now())?;
   if animating {
      schedule_next_frame(window, app);
   }

   MODAL_ACTIONS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clear();

   let (title, subtitle, size_code, body_opt, footer_buttons): (
      String,
      Option<String>,
      u8,
      Option<RawDiv>,
      Vec<RawButton>,
   ) = match modal_snapshot {
      Snapshot::Page(spec) => {
         let body = build_page_body(msym, &spec, window, app);
         let mut footer = Vec::new();
         if let Some(close_btn) = make_modal_button(
            "sunbird-modal-close",
            "Close",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::Close,
         ) {
            footer.push(close_btn);
         }
         (spec.title, None, 3, body, footer)
      },
      Snapshot::CodeEditor(ed) => {
         let mut body = gpui::v_stack(10.0);
         if let Some(ref mut b) = body {
            gpui::style_div(b, |s| s.length(s.at.size[0], W_FULL));
            if let Some((msg, color)) = ed.status.line()
               && let Some(st) = plain_text_div(msg, Some(color), 13.0)
            {
               *b = gpui::push_div(std::mem::replace(b, RawDiv::zeroed()), st);
            }
            if let Some(tf) = FIELD_SYM.get()
               && let Some(ent) = unsafe {
                  ensure_field_entity(
                     app,
                     window,
                     FieldSlot::Code,
                     "// Plugin JavaScript source...",
                  )
               }
            {
               if let Some(init) = &ed.pending_init {
                  // SAFETY: `app` is live and `ent` is its field.
                  let field = unsafe { field_ptr(app, &ent) };
                  // SAFETY: `field` is the live `FieldState` behind `ent`.
                  unsafe { write_field_pending(tf, field, init) };
               }
               // SAFETY: constructs hosted `TextArea` from
               // `&Entity<FieldState>` in a buffer that fits it and converts to
               // `AnyElement`.
               let mut ta_any = unsafe {
                  let mut ta = RawTextArea::zeroed();
                  (tf.text_area_new)(&raw mut ta, &raw const ent);
                  // Unset, Mana shows a few lines; fix it at a page of code.
                  if let Some(rows) = tf.layout.text_area_rows {
                     ta.put(rows, [1u8, EDITOR_ROWS]);
                     ta.put(rows + 2, [1u8, EDITOR_ROWS]);
                  }
                  let mut any = MaybeUninit::<RawAnyElement>::uninit();
                  (tf.text_area_into_any)(any.as_mut_ptr(), &raw mut ta);
                  any.assume_init()
               };
               if let Some(mut wrap) = gpui::v_stack(4.0) {
                  gpui::style_div(&mut wrap, |s| {
                     s.length(s.at.size[0], W_FULL);
                     s.shared_string(s.at.font_family, SharedString::from_static(MONO_FONT));
                     s.length(s.at.font_size, gpui_px(12.5));
                  });
                  if gpui::push_any(&mut wrap, &mut ta_any) {
                     *b = gpui::push_div(std::mem::replace(b, RawDiv::zeroed()), wrap);
                  }
               }
            } else if let Some(msg) = plain_text_div(
               "Native TextArea could not be resolved on this Discord build.",
               Some(col_error()),
               13.5,
            ) {
               *b = gpui::push_div(std::mem::replace(b, RawDiv::zeroed()), msg);
            }
         }
         let mut footer = Vec::new();
         if let Some(btn) = make_modal_button(
            "sunbird-edit-cancel",
            "Cancel",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::Close,
         ) {
            footer.push(btn);
         }
         if let Some(btn) = make_modal_button(
            "sunbird-edit-copy",
            "Copy Source",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::CopyCode,
         ) {
            footer.push(btn);
         }
         if let Some(btn) = make_modal_button(
            "sunbird-edit-save",
            "Save & Reload",
            BtnVariant::Primary,
            BtnSize::Medium,
            ModalAction::SaveCode(ed.plugin.clone()),
         ) {
            footer.push(btn);
         }
         (
            format!("Edit Plugin · {}", ed.plugin),
            None,
            3,
            body,
            footer,
         )
      },
      Snapshot::InstallUrl(inst) => {
         let mut body = gpui::v_stack(10.0);
         if let Some(ref mut b) = body {
            gpui::style_div(b, |s| s.length(s.at.size[0], W_FULL));
            if let Some(tf) = FIELD_SYM.get()
               && let Some(ent) = unsafe {
                  ensure_field_entity(app, window, FieldSlot::Url, match &inst.target {
                     FieldTarget::Install => "https://example.com/plugin.js",
                     FieldTarget::Setting(_) => "Leave empty to clear",
                     FieldTarget::Prompt(_) => "",
                  })
               }
            {
               if let Some(init) = &inst.pending_init {
                  // SAFETY: `app` is live and `ent` is its field.
                  let field = unsafe { field_ptr(app, &ent) };
                  // SAFETY: `field` is the live `FieldState` behind `ent`.
                  unsafe { write_field_pending(tf, field, init) };
               }
               // SAFETY: constructs hosted `TextInput` from `&Entity<FieldState>`
               // in a buffer that fits it and converts to `AnyElement`.
               let mut ti_any = unsafe {
                  let mut any = MaybeUninit::<RawAnyElement>::uninit();
                  let mut ti = RawTextInput::zeroed();
                  (tf.text_input_new)(&raw mut ti, &raw const ent);
                  (tf.text_input_into_any)(any.as_mut_ptr(), &raw mut ti);
                  any.assume_init()
               };
               if let Some(mut wrap) = gpui::v_stack(4.0) {
                  gpui::style_div(&mut wrap, |s| s.length(s.at.size[0], W_FULL));
                  if gpui::push_any(&mut wrap, &mut ti_any) {
                     *b = gpui::push_div(std::mem::replace(b, RawDiv::zeroed()), wrap);
                  }
               }
            } else if let Some(msg) = plain_text_div(
               "Native TextInput could not be resolved on this Discord build.",
               Some(col_error()),
               13.5,
            ) {
               *b = gpui::push_div(std::mem::replace(b, RawDiv::zeroed()), msg);
            }
            if let Some((msg, color)) = inst.status.line()
               && let Some(st) = plain_text_div(msg, Some(color), 13.0)
            {
               *b = gpui::push_div(std::mem::replace(b, RawDiv::zeroed()), st);
            }
         }
         let mut footer = Vec::new();
         if let Some(btn) = make_modal_button(
            "sunbird-url-cancel",
            "Cancel",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::Close,
         ) {
            footer.push(btn);
         }
         if !matches!(inst.status, FieldStatus::Working(_))
            && let Some(btn) = make_modal_button(
               "sunbird-url-install",
               match inst.target {
                  FieldTarget::Install => "Install Plugin",
                  FieldTarget::Setting(_) => "Save",
                  FieldTarget::Prompt(_) => "Send",
               },
               BtnVariant::Primary,
               BtnSize::Medium,
               ModalAction::SubmitInstallUrl,
            )
         {
            footer.push(btn);
         }
         let (title, subtitle) = match &inst.target {
            FieldTarget::Setting(field) => (field.title.clone(), Some(field.plugin.clone())),
            FieldTarget::Prompt(prompt) => (prompt.title.clone(), None),
            FieldTarget::Install => {
               (
                  "Install Plugin from URL".to_owned(),
                  Some("Paste an https:// URL pointing to a Sunbird .js plugin bundle.".to_owned()),
               )
            },
         };
         (title, subtitle, 2, body, footer)
      },
   };

   let mut body_div = body_opt?;
   let close_rc = gpui::alloc_rc_u64(0)?;

   // Build `mana_html::hosted::modal::Modal`.
   let at = &msym.layout;
   let title_ss = SharedString::from_str(&title);
   let mut m = RawModal::zeroed();
   // SAFETY: `Modal::new` builds into a buffer that fits a `Modal`.
   unsafe { (msym.modal_new)(&raw mut m, &raw const title_ss) };
   m.put(at.scale, size_code);
   if let Some(sub) = subtitle {
      m.put(at.subtitle, SharedString::from_str(&sub));
   }
   m.put(at.on_close, close_rc);
   m.put(at.on_close + 8, &raw const MODAL_CLOSE_VTBL);
   // SAFETY: `modal_child_div`, `modal_footer_button` and `modal_into_any`
   // take the `Modal` by value and return it through buffers that fit it.
   let modal_any = unsafe {
      let mut next = RawModal::zeroed();
      (msym.modal_child_div)(&raw mut next, &raw mut m, &raw mut body_div);
      let mut m = next;

      for mut btn in footer_buttons {
         let mut next = RawModal::zeroed();
         (msym.modal_footer_button)(&raw mut next, &raw mut m, &raw mut btn);
         m = next;
      }

      let mut any = MaybeUninit::<RawAnyElement>::uninit();
      (msym.modal_into_any)(any.as_mut_ptr(), &raw mut m);
      any.assume_init()
   };

   let alpha = progress.clamp(0.0, 1.0);
   let mut scrim = gpui::raw_div()?;
   gpui::style_div(&mut scrim, |s| {
      s.set(s.at.position, POSITION_ABSOLUTE);
      s.lengths(&s.at.inset, gpui_px(0.0));
      s.lengths(&s.at.size, W_FULL);
      s.set(s.at.display, DISPLAY_FLEX);
      s.set(s.at.align_items, ALIGN_CENTER);
      s.set(s.at.justify_content, ALIGN_CENTER);
      s.solid_background([0.0, 0.0, 0.0, 0.68 * alpha]);
   });
   let mut card = gpui::raw_div()?;
   // Keep ScaleFade mounted at progress 1.0 once settled so layout does not shift.
   let mut wrapped = match wrap_scale_fade(modal_any, progress, instance_id) {
      Ok(scaled) => {
         gpui::style_div(&mut card, |s| s.set(s.at.display, DISPLAY_FLEX));
         scaled
      },
      Err(raw) => {
         gpui::style_div(&mut card, |s| {
            s.set(s.at.display, DISPLAY_FLEX);
            if alpha < 1.0 {
               s.float(s.at.opacity, alpha);
            }
         });
         raw
      },
   };
   if !gpui::push_any(&mut card, &mut wrapped) {
      return None;
   }
   if interactive {
      on_mouse_down(&mut scrim, &SCRIM_PRESS_VTBL);
      on_mouse_down(&mut card, &CARD_PRESS_VTBL);
   }

   Some(gpui::push_div(scrim, card))
}

#[cfg(test)]
mod tests {
   #[test]
   fn coalesce_folds_whitespace_and_same_colors() {
      let tokens = [
         (1, "  "),
         (2, "\"k\""),
         (3, ":"),
         (1, " "),
         (4, "1"),
         (4, "2"),
         (3, ","),
      ];
      assert_eq!(super::coalesce(tokens.into_iter()), vec![
         (2, "  \"k\"".to_owned()),
         (3, ": ".to_owned()),
         (4, "12".to_owned()),
         (3, ",".to_owned()),
      ]);
   }
}
