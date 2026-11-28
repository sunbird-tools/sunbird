//! When a critical patch failed, Discord most likely updated past this
//! Sunbird. This asks, through native dialogs that need none of the patches,
//! whether to fetch a newer Sunbird, installs its loader over the running one
//! and restarts Discord through the launcher.
//!
//! It also brings the installed launcher up to the site's, for installs from
//! before launchers replaced themselves.

use std::{
   collections::BTreeMap,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Mutex,
      PoisonError,
      atomic::{
         AtomicBool,
         Ordering,
      },
   },
};

use phoenix_runtime::update::{
   self,
   Update,
   VERSION,
};
use phoenix_ui_desktop::{
   alert,
   confirm,
};

const TITLE: &str = "Sunbird - Patch Error";

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Runs the recovery dialogs on a thread of their own, once per process.
pub fn start() {
   if RUNNING.swap(true, Ordering::AcqRel) {
      return;
   }
   let _ = std::thread::Builder::new()
      .name("sunbird-recovery".to_owned())
      .spawn(run);
}

fn run() {
   let failed = crate::health::snapshot()
      .critical_failures()
      .iter()
      .map(|patch| {
         format!(
            "- {}: {}",
            patch.name,
            patch.error.as_deref().unwrap_or("failed")
         )
      })
      .collect::<Vec<_>>()
      .join("\n");
   let asked = format!(
      "Sunbird {VERSION} could not patch this Discord build:\n\n{failed}\n\nDiscord most likely \
       updated before Sunbird did. Check for a Sunbird update and install it now?"
   );
   if !confirm(TITLE, &asked) {
      log::info!("recovery: update declined");
      return;
   }
   let update = match update::check_blocking() {
      Ok(update) => update,
      Err(err) => {
         alert(
            TITLE,
            &format!(
               "The update check failed: {err}\n\nTry again later from the Sunbird tray menu."
            ),
         );
         return;
      },
   };
   let Some((version, update)) = update.and_then(|update| Some((update.loader.clone()?, update)))
   else {
      alert(
         TITLE,
         &format!(
            "Sunbird {VERSION} is already the latest release. Discord likely updated before a \
             Sunbird build for it shipped.\n\nUntil one does, start Sunbird in access-gate-only \
             mode (--gate-only) to use Discord without plugins, or wait for an update."
         ),
      );
      return;
   };
   if let Err(err) = install_loader(&version, &update) {
      log::error!("recovery: {err}");
      alert(
         TITLE,
         &format!("Installing Sunbird {version} failed: {err}"),
      );
      return;
   }
   let failed_plugins = update::stage_plugins(&version, &update);
   if !failed_plugins.is_empty() {
      log::warn!(
         "recovery: plugins not staged: {}",
         failed_plugins.join("; ")
      );
   }
   // An old Windows launcher ignores `--wait-pid` and would find this
   // Discord still running.
   refresh_launcher();
   log::info!("recovery: installed Sunbird {version}, restarting");
   if let Err(err) = restart() {
      alert(
         TITLE,
         &format!(
            "Sunbird updated to {version}, but Discord could not be restarted ({err}). Quit \
             Discord and open Sunbird again."
         ),
      );
   }
}

/// Spawns the launcher to start Discord again once this process exits, then
/// exits. Returns only when it could not.
fn restart() -> Result<(), String> {
   let launcher = phoenix_ui_desktop::launcher().ok_or("the Sunbird launcher is not installed")?;
   spawn_after_exit(&launcher, std::process::id())
      .map_err(|err| format!("{}: {err}", launcher.display()))?;
   std::process::exit(0);
}

#[cfg(windows)]
fn spawn_after_exit(launcher: &Path, pid: u32) -> std::io::Result<()> {
   std::process::Command::new(launcher)
      .arg("--wait-pid")
      .arg(pid.to_string())
      .spawn()
      .map(drop)
}

#[cfg(unix)]
fn spawn_after_exit(launcher: &Path, pid: u32) -> std::io::Result<()> {
   use std::{
      os::unix::process::CommandExt as _,
      process::{
         Command,
         Stdio,
      },
   };

   let script = if cfg!(target_os = "macos") {
      r#"while kill -0 "$1" 2>/dev/null; do sleep 0.1; done; exec /usr/bin/open -n -a "$2""#
   } else {
      r#"while kill -0 "$1" 2>/dev/null; do sleep 0.1; done; exec "$2""#
   };
   Command::new("/bin/sh")
      .args(["-c", script, "_"])
      .arg(pid.to_string())
      .arg(launcher)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      // Its own group, so nothing that ends Discord's ends it too.
      .process_group(0)
      .spawn()
      .map(drop)
}

/// [`update_launcher`] with the last check's CRC-32s, only logged.
pub fn refresh_launcher() {
   match update_launcher(&update::launcher_crcs()) {
      Ok(true) => log::info!("launcher: replaced with the site's"),
      Ok(false) => {},
      Err(err) => log::warn!("launcher: {err}"),
   }
}

/// Replaces the installed launcher when the site lists a different one.
/// True when it did. A loader without the launcher's version file beside it
/// is a dev build, whose launcher is left alone.
pub fn update_launcher(crcs: &BTreeMap<String, u32>) -> Result<bool, String> {
   // The background check and recovery may both get here, and they share the
   // temporary file names.
   static ONE: Mutex<()> = Mutex::new(());
   let Some(&expected) = crcs.get(LAUNCHER) else {
      return Ok(false);
   };
   // A macOS dev build's loader has no path to report.
   let Ok(loader) = loaded_path() else {
      return Ok(false);
   };
   let home = loader
      .parent()
      .ok_or("the loader path has no parent directory")?;
   if !home.join(STAMP).is_file() {
      return Ok(false);
   }
   let launcher = phoenix_ui_desktop::launcher().ok_or("the Sunbird launcher is not installed")?;
   let _one = ONE.lock().unwrap_or_else(PoisonError::into_inner);
   replace_launcher(home, &launcher, expected)
}

#[cfg(windows)]
const LAUNCHER: &str = "sunbird-windows-x86_64.exe";
#[cfg(target_os = "linux")]
const LAUNCHER: &str = "sunbird-linux.sh";
#[cfg(target_os = "macos")]
const LAUNCHER: &str = "sunbird-macos.sh";

#[cfg(windows)]
fn replace_launcher(_home: &Path, launcher: &Path, expected: u32) -> Result<bool, String> {
   let have =
      std::fs::read(launcher).map_err(|err| format!("read {}: {err}", launcher.display()))?;
   if crc32fast::hash(&have) == expected {
      return Ok(false);
   }
   let bytes = update::download_verified(LAUNCHER, b"MZ", expected)?;
   swap(launcher, &bytes)?;
   // Held only when a launcher still runs; its next start removes it then.
   let _ = std::fs::remove_file(launcher.with_file_name("sunbird.exe.old"));
   Ok(true)
}

/// Runs the site's installer with `--launcher-only`, which rewrites the
/// launcher and records the installer's CRC-32 in `installer.crc`.
#[cfg(unix)]
fn replace_launcher(home: &Path, launcher: &Path, expected: u32) -> Result<bool, String> {
   use std::process::{
      Command,
      Stdio,
   };

   let have = std::fs::read_to_string(home.join("installer.crc")).unwrap_or_default();
   if have.trim() == expected.to_string() {
      return Ok(false);
   }
   let script = if cfg!(target_os = "macos") {
      // Sunbird.app in /Applications belongs to the admin who installed it.
      let contents = launcher.join("Contents");
      if !writable(&contents) || !writable(&contents.join("MacOS")) {
         return Ok(false);
      }
      contents.join("MacOS/sunbird")
   } else {
      launcher.to_path_buf()
   };
   let text = std::fs::read_to_string(&script)
      .map_err(|err| format!("read {}: {err}", script.display()))?;
   let discord = script_value(&text, "discord")
      .ok_or_else(|| format!("{} names no Discord", script.display()))?;
   let bytes = update::download_verified(LAUNCHER, b"#!/bin/sh", expected)?;
   let tmp = home.join(format!("installer.{}.sh", std::process::id()));
   std::fs::write(&tmp, &bytes).map_err(|err| format!("write {}: {err}", tmp.display()))?;
   let mut command = Command::new("/bin/sh");
   command
      .arg(&tmp)
      .arg("--launcher-only")
      .arg(&discord)
      // Not this process's loader in the installer and the tools it runs.
      .env_remove("LD_PRELOAD")
      .env_remove("DYLD_INSERT_LIBRARIES")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null());
   if let Some(site) = script_value(&text, "site") {
      command.env("SUNBIRD_SITE", site);
   }
   let status = command.status();
   let _ = std::fs::remove_file(&tmp);
   match status {
      Ok(status) if status.success() => Ok(true),
      Ok(status) => Err(format!("the launcher installer failed ({status})")),
      Err(err) => Err(format!("run the launcher installer: {err}")),
   }
}

#[cfg(target_os = "macos")]
fn writable(dir: &Path) -> bool {
   use std::os::unix::ffi::OsStrExt as _;

   std::ffi::CString::new(dir.as_os_str().as_bytes()).is_ok_and(|path| {
      // SAFETY: `path` is NUL-terminated and outlives the call.
      unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
   })
}

#[cfg(target_os = "linux")]
const fn writable(_dir: &Path) -> bool {
   true
}

/// `key`'s value in a launcher `sunbird-{linux,macos}.sh` generated, where
/// it is written single-quoted, with `'\''` for each quote inside.
#[cfg(any(unix, test))]
fn script_value(script: &str, key: &str) -> Option<String> {
   let mut rest = script
      .lines()
      .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))?;
   let mut value = String::new();
   while !rest.is_empty() {
      if let Some(after) = rest.strip_prefix("\\'") {
         value.push('\'');
         rest = after;
         continue;
      }
      let inner = rest.strip_prefix('\'')?;
      let end = inner.find('\'')?;
      value.push_str(&inner[..end]);
      rest = &inner[end + 1..];
   }
   Some(value)
}

/// Writes `bytes` beside `dest` and renames it over `dest`. The mapped old
/// file stays valid for this process, since the name now points at a new one.
#[cfg(unix)]
fn swap_in(
   dest: &Path,
   bytes: &[u8],
   prepare: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
   let name = dest.file_name().ok_or("the loader path has no file name")?;
   let tmp = dest.with_file_name(format!(
      "{}.next.{}",
      name.to_string_lossy(),
      std::process::id()
   ));
   std::fs::write(&tmp, bytes).map_err(|err| format!("write {}: {err}", tmp.display()))?;
   let swapped = prepare(&tmp).and_then(|()| {
      std::fs::rename(&tmp, dest).map_err(|err| format!("replace {}: {err}", dest.display()))
   });
   if swapped.is_err() {
      let _ = std::fs::remove_file(&tmp);
   }
   swapped
}

/// Downloads loader `version` and puts it where the running one was loaded
/// from, stamping the version file the launcher reads. A loader without that
/// file was not installed by a launcher (a dev build), so it is left alone.
pub fn install_loader(version: &str, update: &Update) -> Result<(), String> {
   let dest = loaded_path()?;
   let dir = dest
      .parent()
      .ok_or("the loader path has no parent directory")?;
   let stamp = dir.join(STAMP);
   if !stamp.is_file() {
      return Err(format!(
         "{} was not installed by the Sunbird launcher, so it is left alone",
         dest.display()
      ));
   }
   let bytes = update::download_loader(ASSET, MAGIC, version, update)?;
   swap(&dest, &bytes)?;
   std::fs::write(&stamp, format!("{version}\n"))
      .map_err(|err| format!("write {}: {err}", stamp.display()))
}

#[cfg(windows)]
const ASSET: &str = "phoenix-windows-x86_64.dll";
#[cfg(windows)]
const MAGIC: &[u8] = b"MZ";
/// `crates/inject` keeps the installed loader's version here.
#[cfg(windows)]
const STAMP: &str = "version.txt";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: &str = "libphoenix-linux-x86_64.so";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET: &str = "libphoenix-linux-aarch64.so";
#[cfg(target_os = "linux")]
const MAGIC: &[u8] = b"\x7fELF";

#[cfg(target_os = "macos")]
const ASSET: &str = "libphoenix-macos-arm64.dylib";
/// A 64-bit Mach-O.
#[cfg(target_os = "macos")]
const MAGIC: &[u8] = &[0xCF, 0xFA, 0xED, 0xFE];

/// `sunbird-{linux,macos}.sh` keep the installed loader's version here.
#[cfg(unix)]
const STAMP: &str = "version";

#[cfg(target_os = "linux")]
fn swap(dest: &Path, bytes: &[u8]) -> Result<(), String> {
   swap_in(dest, bytes, |_| Ok(()))
}

/// Signed ad hoc like `sunbird-macos.sh` leaves it, or dyld refuses it.
#[cfg(target_os = "macos")]
fn swap(dest: &Path, bytes: &[u8]) -> Result<(), String> {
   swap_in(dest, bytes, |tmp| {
      let signed = std::process::Command::new("/usr/bin/codesign")
         .args(["--force", "--sign", "-"])
         .arg(tmp)
         .status()
         .is_ok_and(|status| status.success());
      if signed {
         Ok(())
      } else {
         Err("codesign failed".to_owned())
      }
   })
}

/// Windows lets a loaded DLL be renamed but not overwritten, so it moves
/// aside to `.old`, and back when the new one can't take its place.
#[cfg(windows)]
fn swap(dest: &Path, bytes: &[u8]) -> Result<(), String> {
   let name = dest
      .file_name()
      .ok_or("the loader path has no file name")?
      .to_string_lossy();
   let fresh = dest.with_file_name(format!("{name}.{}.new", std::process::id()));
   let old = dest.with_file_name(format!("{name}.old"));
   std::fs::write(&fresh, bytes).map_err(|err| format!("write {}: {err}", fresh.display()))?;
   let _ = std::fs::remove_file(&old);
   if let Err(err) = std::fs::rename(dest, &old) {
      let _ = std::fs::remove_file(&fresh);
      return Err(format!("move {} aside: {err}", dest.display()));
   }
   if let Err(err) = std::fs::rename(&fresh, dest) {
      let _ = std::fs::remove_file(&fresh);
      let restored = std::fs::rename(&old, dest).is_ok();
      return Err(format!(
         "replace {}: {err}{}",
         dest.display(),
         if restored {
            ""
         } else {
            " (the old loader is at .old)"
         }
      ));
   }
   Ok(())
}

/// The file this loader was mapped from.
#[cfg(windows)]
fn loaded_path() -> Result<PathBuf, String> {
   use std::{
      ffi::{
         OsString,
         c_void,
      },
      os::windows::ffi::OsStringExt as _,
   };

   const FROM_ADDRESS: u32 = 0x4;
   const UNCHANGED_REFCOUNT: u32 = 0x2;

   #[link(name = "kernel32")]
   unsafe extern "system" {
      fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
      fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
   }

   let mut module = std::ptr::null_mut();
   // SAFETY: with `FROM_ADDRESS` the name is an address inside this module,
   // and `UNCHANGED_REFCOUNT` takes no reference that would need releasing.
   let found = unsafe {
      GetModuleHandleExW(
         FROM_ADDRESS | UNCHANGED_REFCOUNT,
         (loaded_path as *const ()).cast::<u16>(),
         &raw mut module,
      )
   } != 0;
   if !found {
      return Err(format!(
         "GetModuleHandleExW: {}",
         std::io::Error::last_os_error()
      ));
   }
   let mut name = vec![0u16; 32_768];
   let capacity = u32::try_from(name.len()).unwrap_or(u32::MAX);
   // SAFETY: `module` is this loaded module and `name` holds `capacity` units.
   let len = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), capacity) };
   if len == 0 || len >= capacity {
      return Err(format!(
         "GetModuleFileNameW: {}",
         std::io::Error::last_os_error()
      ));
   }
   name.truncate(len as usize);
   Ok(PathBuf::from(OsString::from_wide(&name)))
}

#[cfg(target_os = "linux")]
fn loaded_path() -> Result<PathBuf, String> {
   use std::os::unix::ffi::OsStrExt as _;

   let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
   // SAFETY: queries the shared object that holds this function.
   let ok = unsafe { libc::dladdr(loaded_path as *const libc::c_void, info.as_mut_ptr()) };
   // SAFETY: `info` was zeroed, so every field reads either way.
   let info = unsafe { info.assume_init() };
   if ok == 0 || info.dli_fname.is_null() {
      return Err("dladdr found no file for the loader".to_owned());
   }
   // SAFETY: `dli_fname` is a NUL-terminated path from the dynamic linker.
   let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
   Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

#[cfg(target_os = "macos")]
fn loaded_path() -> Result<PathBuf, String> {
   crate::macos_loader_path()
      .ok_or_else(|| "could not locate the loaded libphoenix.dylib".to_owned())
}

#[cfg(test)]
mod tests {
   #[test]
   fn reads_values_from_generated_launchers() {
      let linux = "#!/bin/sh\nhome='/home/a/.local/share/sunbird'\nsite='https://assets.sunbird.tools'\n\
                   fetch() { curl; }\ndiscord='/home/a/Apps/it'\\''s Discord.AppImage'\npids= sunbird=\n";
      assert_eq!(
         super::script_value(linux, "discord").as_deref(),
         Some("/home/a/Apps/it's Discord.AppImage")
      );
      assert_eq!(
         super::script_value(linux, "site").as_deref(),
         Some("https://assets.sunbird.tools")
      );
      let macos = "#!/bin/sh\nhome='/Users/a/Library/Application \
                   Support/Sunbird'\ndiscord='/Applications/Discord \
                   Phoenix.app'\ncopy=\"$home/Sunbird.app\"\n      source=$discord\n";
      assert_eq!(
         super::script_value(macos, "discord").as_deref(),
         Some("/Applications/Discord Phoenix.app")
      );
      assert_eq!(super::script_value(macos, "missing"), None);
      assert_eq!(
         super::script_value("discord=$(q \"$discord\")\n", "discord"),
         None
      );
   }
}
