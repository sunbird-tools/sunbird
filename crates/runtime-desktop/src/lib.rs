//! phoenix on Desktop Phoenix: the access gate, the HTTP transport hooks, the
//! Sunbird pages in User Settings, message accessories, member list
//! decorations and the tray.

#![cfg(any(
   windows,
   target_os = "macos",
   all(target_os = "linux", target_arch = "x86_64")
))]

mod accessories;
mod badges;
mod call_idle;
mod client_commands;
mod commands;
mod composer;
mod config;
mod context_menu;
mod counts;
mod discord;
mod discord_version;
mod double_click;
mod emoji;
mod fetch;
mod gateway_send;
mod gpui;
mod gpui_font;
mod gpui_theme;
mod headers;
mod health;
mod icons;
mod js;
pub mod mana_theme;
mod members;
mod message;
mod modal;
mod notices;
mod onboarding;
mod os;
mod ready;
mod recovery;
#[cfg(windows)]
mod restart;
mod settings;
mod shortcuts;
mod sidebar;
mod spotify;
mod staff;
mod sticker;
mod toolbar;

use std::{
   ffi::c_void,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Arc,
      Mutex,
      PoisonError,
   },
   time::Duration,
};

use phoenix_hook::image::Image;
use phoenix_runtime::{
   Error,
   Notice,
   Platform,
   Plugin,
   Started,
   discord::{
      Authorization,
      DiscordError,
   },
   fetch::{
      FetchError,
      Request,
      Response,
   },
   permissions::Permissions,
   remote::{
      self,
      Done,
   },
   store::{
      Rows,
      StoreError,
   },
};
use phoenix_sigs::{
   index::{
      Index,
      Isa,
   },
   registry::GatewayRegistry,
};
use rquickjs::{
   Ctx,
   Object,
};

struct Desktop;

/// A `POST /ui/choose` or `POST /ui/sheet` body.
#[derive(serde::Deserialize)]
struct DialogRequest {
   title:     Option<String>,
   message:   Option<String>,
   text:      Option<String>,
   monospace: Option<bool>,
   highlight: Option<bool>,
   options:   Option<Vec<DialogItem>>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum DialogItem {
   Label(String),
   Item {
      label:  String,
      #[serde(default)]
      danger: bool,
   },
}

#[derive(serde::Serialize)]
struct DialogAnswer {
   index: i32,
}

/// `GET /health`: what each patch came to, and whether the first-launch
/// modal's watchdog saw Discord draw.
#[derive(serde::Serialize)]
struct HealthReply {
   patches:        Vec<health::Patch>,
   window_seen:    bool,
   modal_rendered: bool,
}

/// Whether Discord's code is x86-64, which every locator in
/// `phoenix_sigs::desktop` and `http` decodes. macOS Phoenix is arm64 only, so
/// there they are never run, an x86 patch can never land in arm64 code, and
/// what they locate stays off.
pub(crate) const X86_CODE: bool = cfg!(all(target_arch = "x86_64", not(target_os = "macos")));

/// Hooks `target` with `detour` and returns the trampoline as `F`, or logs the
/// error after `what`.
///
/// # Safety
///
/// Runs before Discord's threads reach `target`, `detour` has its ABI, and `F`
/// is its function pointer type.
pub(crate) unsafe fn hook<F: Copy>(
   image: &Image<'static>,
   target: usize,
   detour: usize,
   what: &str,
) -> Option<F> {
   const { assert!(size_of::<F>() == size_of::<usize>()) };
   // SAFETY: forwarded from the caller.
   match unsafe { image.hook_entry(target, detour) } {
      // SAFETY: the trampoline runs `target`'s displaced instructions, so it
      // has `F`'s ABI.
      Ok(trampoline) => Some(unsafe { std::mem::transmute_copy::<usize, F>(&trampoline) }),
      Err(err) => {
         log::warn!("{what}: {err}");
         None
      },
   }
}

/// Hooks an sret function `target` with `detour` (written in the x86-64
/// convention where the return slot is the first argument) and returns the
/// trampoline as `F`, wrapping both ends with arm64 sret thunks on macOS.
///
/// # Safety
///
/// Runs before Discord's threads reach `target`, `detour` has its x86-style
/// sret ABI, and `F` is its function pointer type.
pub(crate) unsafe fn hook_sret<F: Copy>(
   image: &Image<'static>,
   target: usize,
   detour: usize,
   what: &str,
) -> Option<F> {
   const { assert!(size_of::<F>() == size_of::<usize>()) };
   // SAFETY: forwarded from the caller.
   #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
   let hooked = unsafe { image.hook_sret_entry(target, detour) };
   // SAFETY: forwarded from the caller.
   #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
   let hooked = unsafe { image.hook_entry(target, detour) };
   let orig = match hooked {
      Ok(orig) => orig,
      Err(err) => {
         log::warn!("{what}: {err}");
         return None;
      },
   };
   // SAFETY: `orig` runs `target`'s displaced instructions with `F`'s ABI.
   Some(unsafe { std::mem::transmute_copy::<usize, F>(&orig) })
}

/// The target and its swapped gateway registry, once [`prepare`] hooked them.
static PREPARED: Mutex<Option<(Image<'static>, &'static GatewayRegistry)>> = Mutex::new(None);

/// Hooks Discord from an APC queued by `DllMain` on Windows (outside the
/// loader lock, since indexing joins threads) or the constructor on Linux and
/// macOS, before its own code runs.
///
/// Desktop Phoenix settles on the access gate in its first frames and stops
/// reading the gateway registry and HTTP vtables once started, so hooks swapped
/// later from [`start`]'s thread never run. [`start`] picks up what this
/// hooked.
pub fn prepare() -> Result<(), Error> {
   use health::{
      critical,
      feature,
      require,
   };

   phoenix_runtime::set_platform(&Desktop);
   let image = phoenix_runtime::target()?;
   let ix = Index::new(&image, Isa::HOST);
   critical("access_gate", open_access_gate(&image, &ix));
   open_gate_extras(&image);
   settings::install(&image, &ix);
   critical(
      "gpui",
      require(gpui::located(), "GPUI builders did not decode"),
   );
   critical(
      "settings",
      require(
         settings::hooked(),
         "the User Settings section render is not hooked",
      ),
   );
   #[cfg(windows)]
   feature("restart", || restart::install(&image));
   feature("staff", || staff::prepare(&ix));
   modal::install(&image);
   let modal = require(modal::hooked(), "Workspace::render_overlays is not hooked");
   health::record("notices", health::Kind::Feature, modal.clone());
   critical("modal", modal);
   feature("context_menu", || context_menu::install(&image, &ix));
   feature("double_click", || double_click::pane::install(&ix));
   feature("client_commands", || client_commands::install(&image, &ix));
   // Before accessories hooks `MessageRow::render`, whose prologue the header
   // locators check.
   let header = X86_CODE.then(|| headers::locate(&image, &ix));
   feature("accessories", || accessories::install(&image, &ix));
   feature("members", || members::install(&image));
   if let Some(header) = header {
      feature("headers", || headers::install(&image, header));
      feature("message", || message::install(&image));
   } else {
      #[cfg(target_os = "macos")]
      feature("headers", || headers::arm64::install(&image));
   }
   feature("counts", || counts::install(&image, &ix));
   feature("icons", || icons::install(&image));
   feature("composer", || composer::install(&image));
   feature("composer_buttons", || composer::buttons::install(&image, &ix));
   feature("spotify", || spotify::install(&ix));
   feature("sidebar", || sidebar::install(&image, &ix));
   feature("toolbar", || toolbar::install(&image, &ix));
   feature("badges", || badges::install(&image, &ix));
   feature("emoji", || emoji::install(&image, &ix));
   feature("sticker", || sticker::install(&image, &ix));
   feature("call_idle_dm", || call_idle::install_dm(&image, &ix));
   feature("call_idle_afk", || call_idle::install_guild_afk(&image, &ix));
   feature("gpui_font", || gpui_font::install(&image, &ix));
   feature("mana_theme", || mana_theme::install(&image, &ix));
   feature("gpui_theme", || gpui_theme::install(&image, &ix));
   feature("os_redraw", || <os::Os as os::DesktopOs>::install(&image));
   #[cfg(target_os = "linux")]
   feature("shortcuts", shortcuts::linux::install);
   let registry = phoenix_runtime::hook(&image);
   critical(
      "gateway",
      registry.as_ref().map(|_| ()).map_err(ToString::to_string),
   );
   if registry.is_ok() {
      #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
      feature("direct_calls", || route_direct_calls(&image));
      discord::install(&image);
      critical(
         "discord",
         require(discord::hooked(), "the HTTP transport is not hooked"),
      );
      feature("gateway_send", || gateway_send::install(&image));
      feature("optimistic_delete", || {
         accessories::install_optimistic_delete(&image, &ix);
      });
      feature("ready", || ready::install(&image, &ix));
   }
   #[cfg(target_os = "macos")]
   feature("message_writer", || message::writer::install(&image));
   health::log_summary();
   // The indexes are tens of MB and nothing reads them after startup.
   drop(ix);
   phoenix_sigs::desktop::release_call_index();
   phoenix_hook::arm64::release_ref_index();
   *PREPARED.lock().unwrap_or_else(PoisonError::into_inner) = Some((image, registry?));
   Ok(())
}

/// Opens the access gate for testing Discord without the plugin runtime.
pub fn open_gate_only() -> Result<(), Error> {
   phoenix_runtime::set_platform(&Desktop);
   let image = phoenix_runtime::target()?;
   let _ = open_access_gate(&image, &Index::new(&image, Isa::HOST));
   open_gate_extras(&image);
   #[cfg(windows)]
   restart::install(&image);
   Ok(())
}

/// Starts phoenix inside Desktop Phoenix, on what [`prepare`] hooked.
pub fn start(home: PathBuf, bundled: Vec<Plugin>) -> Result<Started, Error> {
   mana_theme::load_persisted(&home);
   // `recovery` stages plugins for the loader it installs on every OS.
   phoenix_runtime::update::adopt_staged(&home, &home.join("plugins"));
   let prepared = PREPARED
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .take();
   if let Some((image, registry)) = prepared {
      return phoenix_runtime::start_hooked(&Desktop, image, registry, home, bundled);
   }
   let started = (|| {
      let image = phoenix_runtime::target()?;
      let registry = phoenix_runtime::hook(&image)?;
      discord::install(&image);
      gateway_send::install(&image);
      phoenix_runtime::start_hooked(&Desktop, image, registry, home, bundled)
   })();
   // Without a runtime `started` never runs, so the offer to update is made
   // here.
   if started.is_err() && !health::snapshot().critical_failures().is_empty() {
      recovery::start();
   }
   started
}

/// Desktop Phoenix shows "This app isn't available for your account" from
/// `libdiscord_ops_surfaces::client_access::ClientAccessGate`, and blocks the
/// login on it. One function reads it (1 blocked, 2 no gate, also what a
/// missing value reads as), and the gate is only considered while it is not 2:
/// its small caller is `signed_in && access(..) != 2`. This makes it return 2.
/// Discord settles on the gate within its first frames, so the loader calls
/// this from `DllMain` or the constructor, before Discord's own code.
fn open_access_gate(image: &Image<'static>, ix: &Index<'_>) -> Result<(), String> {
   /// `mov al, 2; ret`
   const ALLOWED: [u8; 3] = [0xB0, 0x02, 0xC3];

   let allowed = if X86_CODE {
      &ALLOWED[..]
   } else {
      &phoenix_sigs::macos::NO_GATE[..]
   };
   let access = phoenix_sigs::locate::access_gate(ix)
      .inspect_err(|err| log::warn!("access gate: {err}, leaving it"))?
      .start;
   // SAFETY: Discord's own code, written from `DllMain` or the constructor
   // before it runs.
   match unsafe { image.write_code(access, allowed) } {
      Ok(()) => {
         log::info!("access gate: open");
         Ok(())
      },
      Err(err) => {
         log::error!("access gate: {err}");
         Err(err.to_string())
      },
   }
}

/// What opens beside the gate: message alerts a gated client drops, and on
/// macOS the webview stacking hCaptcha needs.
fn open_gate_extras(image: &Image<'static>) {
   if X86_CODE {
      health::feature("notification_gate", || open_notification_gate(image));
   } else {
      health::feature("notification_gate", || close_enforced(image));
      health::feature("webview_pages", || keep_pages_over_frame(image));
   }
}

/// Makes macOS's `client_access::enforced` answer false. `push_access` asks it
/// without the access function and drops every message alert for a gated
/// account, notifications and their sounds included.
fn close_enforced(image: &Image<'static>) {
   let enforced = phoenix_sigs::macos::access_read(image)
      .and_then(|read| phoenix_sigs::macos::access_enforced(image, read));
   // SAFETY: as for the access function in `open_access_gate`.
   match enforced.map(|at| unsafe { image.write_code(at, &phoenix_sigs::macos::NOT_ENFORCED) }) {
      Ok(Ok(())) => log::info!("access gate: not enforced, message alerts are on"),
      Ok(Err(err)) => log::error!("access gate: {err}"),
      Err(err) => log::warn!("access gate: {err}, message alerts stay off"),
   }
}

/// Keeps macOS embedded webviews above `GPUIView` so mouse clicks reach
/// interactive modals like hCaptcha (release builds ignore
/// `LIBDISCORD_MAC_PAGES_OVER_FRAME`).
fn keep_pages_over_frame(image: &Image<'static>) {
   let func = match phoenix_sigs::macos::pages_under_frame(image) {
      Ok(func) => func,
      Err(err) => {
         log::warn!("webview pages_under_frame: {err}, leaving it");
         return;
      },
   };
   // SAFETY: Discord's own code, written from the constructor before it runs.
   match unsafe { image.write_code(func, &phoenix_sigs::macos::PAGES_OVER_FRAME) } {
      Ok(()) => log::info!("webview: pages_under_frame disabled @ {func:#x}"),
      Err(err) => log::error!("webview pages_under_frame: {err}"),
   }
}

/// Hooks every gateway handler's entry directly on arm64: some hosts call a
/// handler directly past the registry (macOS deletes your own message that
/// way). Each stub passes its own trampoline in `x3` so events missing from the
/// registry still fall back to Discord's original handler.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn route_direct_calls(image: &Image<'static>) {
   let detour = phoenix_runtime::gateway::entry_detour_with_fallback();
   let handlers = phoenix_runtime::gateway::handlers();
   let mut hooked = 0;
   for &handler in &handlers {
      // SAFETY: runs from `prepare` before Discord's threads; the fallback
      // stub passes `trampoline` in `x3` and preserves `x0..=x2`, and the
      // registry is pointed at the trampoline before anything can call either.
      match unsafe { image.hook_entry_with_fallback(handler, detour) } {
         Ok(trampoline) => {
            phoenix_runtime::gateway::call_handler_through(handler, trampoline);
            hooked += 1;
         },
         Err(err) => log::warn!("desktop: gateway handler {handler:#x} not hooked: {err}"),
      }
   }
   log::info!(
      "desktop: {hooked} of {} gateway handlers hooked for direct calls",
      handlers.len()
   );
}

/// Makes the `surfaces` notification provider decline message alerts, which it
/// claims and returns empty on a gated client, so the planner's fallback builds
/// the real one. The x86 counterpart of macOS's [`close_enforced`].
fn open_notification_gate(image: &Image<'static>) {
   let decline: &[u8] = match phoenix_hook::x86::Abi::of(image) {
      phoenix_hook::x86::Abi::SysV => &[0x31, 0xC0, 0xC3],
      phoenix_hook::x86::Abi::Win64 => &[0x31, 0xC0, 0x90],
   };
   let gate = match phoenix_sigs::desktop::notification_gate(image) {
      Ok(gate) => gate,
      Err(err) => {
         log::warn!("notification gate: {err}, leaving it");
         return;
      },
   };
   // SAFETY: Discord's own code, written from `DllMain` or the constructor
   // before it runs.
   match unsafe { image.write_code(gate, decline) } {
      Ok(()) => log::info!("notification gate: open"),
      Err(err) => log::error!("notification gate: {err}"),
   }
}

impl Platform for Desktop {
   fn send(&self, request: &Request) -> Result<Response, FetchError> {
      fetch::send(request)
   }

   fn query(&self, database: &Path, sql: &str, args: &[String]) -> Result<Rows, StoreError> {
      phoenix_runtime::store::sqlite(database, sql, args)
   }

   fn discord(
      &self,
      method: &str,
      path: &str,
      headers: Vec<(String, String)>,
      body: Vec<u8>,
   ) -> Result<Response, DiscordError> {
      discord::send(method, path, headers, body)
   }

   fn authorize(&self, request: &Authorization) -> Result<String, DiscordError> {
      discord::authorize(request)
   }

   fn command(&self, name: &str, request: &str) -> Result<String, DiscordError> {
      client_commands::run(request).map_err(|err| {
         match err {
            client_commands::Error::Unavailable => DiscordError::NoCommands,
            client_commands::Error::Pending => {
               DiscordError::CommandPending {
                  name: name.to_owned(),
               }
            },
            client_commands::Error::Reply => {
               DiscordError::Command {
                  name:    name.to_owned(),
                  message: "the reply is not UTF-8".to_owned(),
               }
            },
         }
      })
   }

   fn init(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
      ctx.store_userdata(js::Desktop::default())
         .map_err(|err| rquickjs::Exception::throw_internal(ctx, &err.to_string()))?;
      Ok(())
   }

   fn api<'js>(
      &self,
      ctx: &Ctx<'js>,
      plugin: &str,
      _permissions: &Permissions,
      api: &Object<'js>,
      messages: &Object<'js>,
   ) -> rquickjs::Result<()> {
      js::ui::desktop_patch_api(ctx, plugin, api, messages)
   }

   fn console<'js>(&self, ctx: &Ctx<'js>, phoenix: &Object<'js>) -> rquickjs::Result<()> {
      phoenix.set("native", js::native::object(ctx)?)
   }

   fn unload(&self, ctx: &Ctx<'_>, plugin: &str) {
      js::unload(ctx, plugin);
   }

   fn staff(&self, image: &'static Image<'static>) {
      staff::start(image);
   }

   fn gateway(&self, store: *mut c_void) {
      // SAFETY: every registry handler takes the store state first, and this
      // runs inside one.
      unsafe { staff::apply(store) };
   }

   fn started(&self, _first_run: bool, _plugins: &[String]) {
      let Some(state) = phoenix_runtime::state() else {
         return;
      };
      let _ = std::fs::create_dir_all(state.plugins_dir());
      // Any local process could reach the shell's `/eval` and the session, so
      // it only listens once DevTools is opened, and only for holders of this
      // start's token. `SUNBIRD_DEVTOOLS` starts it at launch, for `dev.ps1`.
      remote::require_token(&state.plugins_dir().with_file_name("debug-shell-token"));
      if std::env::var_os("SUNBIRD_DEVTOOLS").is_some() {
         start_shell();
      }
      phoenix_ui_desktop::start_tray(phoenix_ui_desktop::TrayConfig {
         devtools_url:     Arc::new(start_shell),
         plugins_dir:      state.plugins_dir(),
         on_reload_all:    Some(Arc::new(phoenix_runtime::reload_all)),
         on_check_updates: Some(Arc::new(phoenix_runtime::update::check_now)),
      });
      let ready_body = format!(
         "Open User Settings -> Sunbird, or press {} for DevTools",
         phoenix_ui_desktop::DEVTOOLS_SHORTCUT
      );
      phoenix_ui_desktop::notify("Sunbird ready", Some(&ready_body));
      std::thread::spawn(discord_version::check);
      let config = config::load();
      if config.auto_update {
         std::thread::spawn(|| {
            phoenix_runtime::update::check_and_notify();
            recovery::refresh_launcher();
         });
      }
      if !health::snapshot().critical_failures().is_empty() {
         recovery::start();
      } else if !config.onboarded {
         onboarding::start();
      }
   }

   fn refresh(&self) {
      modal::force_redraw();
   }

   fn toast(&self, message: &str) {
      phoenix_ui_desktop::toast(message);
   }

   fn notify(&self, notice: Notice<'_>) {
      match notice {
         Notice::UpdateAvailable(update) => {
            let mut parts = Vec::new();
            if let Some(version) = &update.loader {
               parts.push(format!("Sunbird {version} (restart to update)"));
            }
            if !update.plugins.is_empty() {
               parts.push(format!("plugins: {}", update.plugins.join(", ")));
            }
            let body = if parts.is_empty() {
               "Open Settings -> Sunbird".to_owned()
            } else {
               parts.join(" · ")
            };
            phoenix_ui_desktop::notify("Sunbird update available", Some(&body));
         },
         Notice::PluginsUpdated(names) if !names.is_empty() => {
            let body = format!("Reloaded: {}", names.join(", "));
            phoenix_ui_desktop::notify("Plugins updated", Some(&body));
         },
         Notice::PluginsUpdated(_) => {},
      }
   }

   #[cfg(target_os = "macos")]
   fn updates_loader(&self) -> bool {
      macos_loader_path().is_some()
   }

   #[cfg(target_os = "macos")]
   fn update_loader(
      &self,
      slot: phoenix_runtime::update::Slot,
      version: &str,
      update: &phoenix_runtime::update::Update,
   ) {
      let msg = update_macos_loader(&slot, version, update).unwrap_or_else(|msg| {
         log::error!("loader update: {msg}");
         format!("Loader update failed: {msg}")
      });
      slot.finish(Some(msg.clone()));
      phoenix_ui_desktop::toast(&msg);
   }

   fn route(&self, request: &remote::Request) -> Option<remote::Reply> {
      let html = || {
         (
            "200 OK",
            "text/html; charset=utf-8",
            phoenix_ui_desktop::DASHBOARD_HTML.as_bytes().to_vec(),
         )
      };
      match (request.method.as_str(), request.path.as_str()) {
         ("GET", "/ui") => Some(html()),
         ("GET", "/") if request.accept.contains("text/html") => Some(html()),
         ("POST", "/ui/devtools") => {
            phoenix_ui_desktop::open_devtools();
            Some(remote::ok_json(&Done::Started))
         },
         ("POST", "/ui/choose") => Some(dialog(&request.body, false)),
         ("POST", "/ui/sheet") => Some(dialog(&request.body, true)),
         ("GET", "/health") => {
            Some(remote::ok_json(&HealthReply {
               patches:        health::snapshot().patches,
               window_seen:    modal::window_seen(),
               modal_rendered: modal::rendered(),
            }))
         },
         // A libdiscord command (`{"type": name, ...args}`) and its JSON reply,
         // for developing plugins. Like `/eval`, it acts as the account.
         ("POST", "/commands") => Some(command_route(&request.body)),
         ("GET", "/themes/tokens") => {
            let names = remote::query_param(&request.query, "names").and_then(|raw| {
               let list: Vec<String> = raw
                  .split(',')
                  .map(|part| part.trim().to_owned())
                  .filter(|part| !part.is_empty())
                  .collect();
               (!list.is_empty()).then_some(list)
            });
            Some(remote::ok_json(&mana_theme::debug_tokens(names.as_deref())))
         },
         ("POST", "/themes/reload") => {
            #[derive(serde::Serialize)]
            struct ReloadReply {
               reloaded: bool,
               active:   bool,
            }
            if mana_theme::is_supported() {
               match mana_theme::reload_active() {
                  Ok(reloaded) => {
                     Some(remote::ok_json(&ReloadReply {
                        reloaded: reloaded.is_some(),
                        active:   reloaded.unwrap_or(false),
                     }))
                  },
                  Err(err) => Some(remote::err("400 Bad Request", &err)),
               }
            } else {
               Some(remote::err(
                  "503 Service Unavailable",
                  "themes are not supported on this build",
               ))
            }
         },
         ("GET", "/themes/quick-css") => {
            #[derive(serde::Serialize)]
            struct QuickCssReply {
               quick_css: String,
            }
            Some(remote::ok_json(&QuickCssReply {
               quick_css: mana_theme::quick_css(),
            }))
         },
         ("POST", "/themes/quick-css") => {
            #[derive(serde::Deserialize)]
            struct QuickCssBody {
               css: String,
            }
            #[derive(serde::Serialize)]
            struct QuickCssSaved {
               saved: bool,
            }
            if !mana_theme::is_supported() {
               return Some(remote::err(
                  "503 Service Unavailable",
                  "themes are not supported on this build",
               ));
            }
            let Ok(parsed) = serde_json::from_slice::<QuickCssBody>(&request.body) else {
               return Some(remote::err(
                  "400 Bad Request",
                  "expected JSON body {\"css\": \"...\"}",
               ));
            };
            match mana_theme::set_quick_css(&parsed.css) {
               Ok(()) => Some(remote::ok_json(&QuickCssSaved { saved: true })),
               Err(err) => Some(remote::err("409 Conflict", &err)),
            }
         },
         #[cfg(all(target_os = "macos", debug_assertions))]
         ("GET", "/ui/screen") => Some(screen()),
         #[cfg(all(target_os = "macos", debug_assertions))]
         ("POST", "/ui/input") => Some(input(&request.body)),
         _ => None,
      }
   }

   fn routes(&self) -> &'static str {
      if cfg!(all(target_os = "macos", debug_assertions)) {
         ", GET /ui, POST /ui/devtools, POST /ui/{choose,sheet}, GET /health, POST /commands, GET \
          /themes/tokens, POST /themes/reload, GET /themes/quick-css, POST /themes/quick-css, GET \
          /ui/screen, POST /ui/input"
      } else {
         ", GET /ui, POST /ui/devtools, POST /ui/{choose,sheet}, GET /health, POST /commands, GET \
          /themes/tokens, POST /themes/reload, GET /themes/quick-css, POST /themes/quick-css"
      }
   }
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn screen() -> remote::Reply {
   phoenix_ui_desktop::input::screenshot().map_or_else(
      || remote::err("503 Service Unavailable", "no window to capture"),
      |png| ("200 OK", "image/png", png),
   )
}

/// One input for `POST /ui/input`, in points from the window's top left.
#[cfg(all(target_os = "macos", debug_assertions))]
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Input {
   Click {
      x:     f64,
      y:     f64,
      #[serde(default)]
      count: u8,
      #[serde(default)]
      right: bool,
   },
   Scroll {
      x:  f64,
      y:  f64,
      #[serde(default)]
      dx: i32,
      dy: i32,
   },
   Text {
      text: String,
   },
   Key {
      key:  KeyName,
      #[serde(default)]
      mods: Vec<Modifier>,
   },
}

#[cfg(all(target_os = "macos", debug_assertions))]
#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Modifier {
   Shift,
   Control,
   Option,
   Command,
}

/// A named key, or one character like `"k"`.
#[cfg(all(target_os = "macos", debug_assertions))]
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum KeyName {
   Named(NamedKey),
   Char(char),
}

#[cfg(all(target_os = "macos", debug_assertions))]
#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum NamedKey {
   Return,
   Tab,
   Escape,
   Backspace,
   Delete,
   Left,
   Right,
   Down,
   Up,
   Home,
   End,
   PageUp,
   PageDown,
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn input(body: &[u8]) -> remote::Reply {
   use phoenix_ui_desktop::input::{
      Key,
      modifier,
   };
   let Ok(input) = serde_json::from_slice::<Input>(body) else {
      return remote::err(
         "400 Bad Request",
         "expected a click, scroll, text or key input",
      );
   };
   let done = match input {
      Input::Click { x, y, count, right } => phoenix_ui_desktop::input::click(x, y, count, right),
      Input::Scroll { x, y, dx, dy } => phoenix_ui_desktop::input::scroll(x, y, dx, dy),
      Input::Text { text } => phoenix_ui_desktop::input::type_text(&text),
      Input::Key { key, mods } => {
         let key = match key {
            KeyName::Char(char) => Key::Char(char),
            KeyName::Named(named) => {
               match named {
                  NamedKey::Return => Key::Return,
                  NamedKey::Tab => Key::Tab,
                  NamedKey::Escape => Key::Escape,
                  NamedKey::Backspace => Key::Backspace,
                  NamedKey::Delete => Key::Delete,
                  NamedKey::Left => Key::Left,
                  NamedKey::Right => Key::Right,
                  NamedKey::Down => Key::Down,
                  NamedKey::Up => Key::Up,
                  NamedKey::Home => Key::Home,
                  NamedKey::End => Key::End,
                  NamedKey::PageUp => Key::PageUp,
                  NamedKey::PageDown => Key::PageDown,
               }
            },
         };
         let flags = mods.into_iter().fold(0, |flags, held| {
            flags
               | match held {
                  Modifier::Shift => modifier::SHIFT,
                  Modifier::Control => modifier::CONTROL,
                  Modifier::Option => modifier::OPTION,
                  Modifier::Command => modifier::COMMAND,
               }
         });
         phoenix_ui_desktop::input::press(key, flags)
      },
   };
   if done {
      remote::ok_json(&Done::Started)
   } else {
      remote::err("503 Service Unavailable", "no window to send input to")
   }
}

/// Opens a native choose or sheet dialog for the dashboard and blocks this
/// request's thread until it is answered.
/// Starts the debug shell if it isn't running and returns the `DevTools` URL.
fn start_shell() -> String {
   remote::set_flag(true);
   remote::ensure_started();
   remote::dashboard_url()
}

fn dialog(body: &[u8], sheet: bool) -> remote::Reply {
   let Ok(request) = serde_json::from_slice::<DialogRequest>(body) else {
      return remote::err("400 Bad Request", "expected a JSON dialog request");
   };
   let options = request
      .options
      .unwrap_or_default()
      .into_iter()
      .map(|item| {
         match item {
            DialogItem::Label(label) => {
               modal::ChoiceOption {
                  label,
                  danger: false,
               }
            },
            DialogItem::Item { label, danger } => modal::ChoiceOption { label, danger },
         }
      })
      .collect::<Vec<_>>();
   let content = if sheet {
      request.text.map(|text| {
         modal::ModalSection::Code {
            title: None,
            text,
            monospace: request.monospace.unwrap_or_default(),
            highlight: request.highlight.unwrap_or_default(),
         }
      })
   } else {
      request
         .message
         .map(|text| modal::ModalSection::Text { title: None, text })
   };
   if content.is_none() && options.is_empty() {
      return remote::err(
         "400 Bad Request",
         if sheet {
            "a sheet needs text"
         } else {
            "a choose dialog needs a message or options"
         },
      );
   }
   let title = request.title.unwrap_or_else(|| "Sunbird".to_owned());
   let index = modal::ask(title, content, options, Duration::from_secs(120));
   remote::ok_json(&DialogAnswer { index })
}

#[cfg(target_os = "macos")]
fn macos_loader_path() -> Option<PathBuf> {
   let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
   // SAFETY: queries the Mach-O image that holds this function.
   let ok = unsafe { libc::dladdr(macos_loader_path as *const c_void, info.as_mut_ptr()) };
   if ok == 0 {
      return None;
   }
   // SAFETY: `dladdr` returned non-zero, so `info` is initialized.
   let info = unsafe { info.assume_init() };
   if info.dli_fname.is_null() {
      return None;
   }
   // SAFETY: `dli_fname` is a NUL-terminated path from dyld.
   let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
      .to_str()
      .ok()?;
   let path = PathBuf::from(path);
   // `sunbird-macos.sh` writes `version` beside the dylib it installs, and a
   // dev build injected from `target/` has none, so updates leave it alone.
   (path.is_file() && path.with_file_name("version").is_file()).then_some(path)
}

#[cfg(target_os = "macos")]
fn update_macos_loader(
   slot: &phoenix_runtime::update::Slot,
   version: &str,
   update: &phoenix_runtime::update::Update,
) -> Result<String, String> {
   use phoenix_runtime::update;

   slot.step(format!("Downloading Sunbird {version}..."));
   recovery::install_loader(version, update)?;
   let failed_plugins = update::stage_plugins(version, update);
   Ok(if failed_plugins.is_empty() {
      format!("Updated to Sunbird {version}. Restart Sunbird to apply.")
   } else {
      format!(
         "Updated loader to Sunbird {version}, but failed to stage {} plugin(s) ({}). Restart \
          Sunbird to apply.",
         failed_plugins.len(),
         failed_plugins.join("; ")
      )
   })
}

/// Runs the JSON command in `body` for the debug shell's `POST /commands`.
fn command_route(body: &[u8]) -> remote::Reply {
   match client_commands::run(&String::from_utf8_lossy(body)) {
      Ok(reply) => {
         serde_json::from_str::<serde_json::Value>(&reply).map_or_else(
            |_| remote::err("502 Bad Gateway", "the command answered with invalid JSON"),
            |reply| remote::ok_json(&reply),
         )
      },
      Err(err) => remote::err("409 Conflict", &format!("{err:?}")),
   }
}
