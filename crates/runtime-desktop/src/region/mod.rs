mod hook;
mod http;
mod pool;
mod route;
mod socks;

use std::{
   collections::HashMap,
   env,
   fs,
   net::IpAddr,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Mutex,
      OnceLock,
   },
   time::{
      Duration,
      Instant,
   },
};

use pool::Proxy;
use route::Phase;

const PLUGIN: &str = "screen-share";
const QUIET: Duration = Duration::from_secs(30);
const CAP: Duration = Duration::from_secs(120);

enum Mode {
   Auto,
   On,
   Off,
}

struct Note {
   host: String,
   at:   Instant,
}

struct Session {
   phase:       Phase,
   proxies:     Vec<Proxy>,
   notes:       HashMap<IpAddr, Note>,
   sockets:     Vec<usize>,
   last_region: Option<Instant>,
   opened:      Option<Instant>,
}

fn session() -> &'static Mutex<Session> {
   static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();
   SESSION.get_or_init(|| {
      Mutex::new(Session {
         phase:       Phase::Opening,
         proxies:     Vec::new(),
         notes:       HashMap::new(),
         sockets:     Vec::new(),
         last_region: None,
         opened:      None,
      })
   })
}

pub fn arm() {
   {
      let mut session = lock();
      if session.proxies.is_empty() || session.opened.is_some() {
         return;
      }
      session.opened = Some(Instant::now());
      session.phase = Phase::Opening;
   }
   std::thread::spawn(|| {
      let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(window));
   });
}

pub fn start() {
   let home = home();
   match read_mode(&home.join("plugins.json")) {
      Mode::Off => return,
      Mode::On => log::info!("screen share: opening through an outside address"),
      Mode::Auto => {
         match probe() {
            Some(region) if region == "brazil" => {
               log::info!("screen share: region brazil, opening through an outside address");
            },
            Some(region) => {
               log::info!("screen share: region {region}, leaving the session alone");
               return;
            },
            None => {
               log::warn!("screen share: could not read the voice region");
               return;
            },
         }
      },
   }
   let proxies = pool::fill(&home);
   if proxies.is_empty() {
      log::warn!("screen share: no outside address worked");
      return;
   }
   log::info!(
      "screen share: outside address in {}",
      proxies[0].region
   );
   {
      let mut session = lock();
      session.proxies = proxies;
   }
   if !hook::install() {
      log::warn!("screen share: could not hook the network stack");
      let mut session = lock();
      session.proxies.clear();
      return;
   }
   pool::refresh_later(home, |proxies| {
      let mut session = lock();
      if session.phase == Phase::Opening {
         session.proxies = proxies;
      }
   });
}

fn probe() -> Option<String> {
   let body = http::get("https://latency.discord.media/rtc", 3_000)?;
   pool::first_region(&body)
}

fn window() {
   loop {
      std::thread::sleep(Duration::from_secs(1));
      let (opened, last_region, established) = {
         let session = lock();
         (
            session.opened.unwrap_or_else(Instant::now),
            session.last_region,
            session.phase == Phase::Established,
         )
      };
      if established {
         return;
      }
      let quiet = last_region.is_some_and(|at| at.elapsed() >= QUIET);
      if quiet || opened.elapsed() >= CAP {
         finish(quiet);
         return;
      }
   }
}

fn finish(quiet: bool) {
   let sockets = {
      let mut session = lock();
      if session.phase == Phase::Established {
         return;
      }
      session.phase = Phase::Established;
      session.notes.clear();
      std::mem::take(&mut session.sockets)
   };
   hook::shutdown_all(&sockets);
   if quiet {
      log::info!("screen share: session kept, back on the direct path");
   } else {
      log::info!("screen share: back on the direct path");
   }
}

fn note(host: &str, ip: IpAddr) {
   if route::decide(host, Phase::Opening) != route::Route::Outside {
      return;
   }
   let mut session = lock();
   if session.phase != Phase::Opening {
      return;
   }
   session.notes.insert(
      ip,
      Note {
         host: host.to_owned(),
         at:   Instant::now(),
      },
   );
}

fn host_for(ip: IpAddr) -> Option<String> {
   let session = lock();
   if session.phase != Phase::Opening {
      return None;
   }
   let host = session
      .notes
      .get(&ip)
      .filter(|note| note.at.elapsed() <= CAP)
      .map(|note| note.host.clone());
   drop(session);
   host
}

fn proxies(limit: usize) -> Vec<String> {
   lock()
      .proxies
      .iter()
      .take(limit)
      .map(|proxy| proxy.addr.clone())
      .collect()
}

fn fail(addr: &str) {
   let mut session = lock();
   if let Some(proxy) = session.proxies.iter_mut().find(|proxy| proxy.addr == addr) {
      proxy.fails += 1;
   }
   session.proxies.retain(|proxy| proxy.fails < 2);
}

fn track(sock: usize, host: &str) {
   let mut session = lock();
   if session.phase != Phase::Opening {
      return;
   }
   session.sockets.push(sock);
   if route::decides(host) {
      session.last_region = Some(Instant::now());
   }
}

fn lock() -> std::sync::MutexGuard<'static, Session> {
   session()
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn home() -> PathBuf {
   if let Some(home) = env::var_os("PHOENIX_HOME") {
      return PathBuf::from(home);
   }
   env::var_os("LOCALAPPDATA")
      .map_or_else(env::temp_dir, PathBuf::from)
      .join("DiscordPhoenix")
      .join("sunbird")
}

fn read_mode(path: &Path) -> Mode {
   let Ok(bytes) = fs::read(path) else {
      return Mode::Auto;
   };
   let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
      return Mode::Auto;
   };
   if value
      .get("disabled")
      .and_then(serde_json::Value::as_array)
      .is_some_and(|names| names.iter().any(|name| name.as_str() == Some(PLUGIN)))
   {
      return Mode::Off;
   }
   match value
      .pointer("/settings/screen-share/mode")
      .and_then(serde_json::Value::as_str)
   {
      Some("on") => Mode::On,
      Some("off") => Mode::Off,
      _ => Mode::Auto,
   }
}

#[cfg(test)]
mod tests {
   use std::io::Write;

   use super::read_mode;

   #[test]
   fn mode_follows_the_plugin_choice() {
      let dir = std::env::temp_dir().join(format!(
         "sunbird-screen-share-{}",
         std::process::id()
      ));
      let _ = std::fs::create_dir_all(&dir);
      let path = dir.join("plugins.json");
      let mut file = std::fs::File::create(&path).unwrap();
      write!(
         file,
         r#"{{"disabled":[],"settings":{{"screen-share":{{"mode":"off"}}}}}}"#
      )
      .unwrap();
      assert!(matches!(read_mode(&path), super::Mode::Off));
      write_file(
         &path,
         r#"{"disabled":["screen-share"],"settings":{"screen-share":{"mode":"on"}}}"#,
      );
      assert!(matches!(read_mode(&path), super::Mode::Off));
      write_file(&path, r#"{"disabled":[],"settings":{}}"#);
      assert!(matches!(read_mode(&path), super::Mode::Auto));
      let _ = std::fs::remove_dir_all(dir);
   }

   fn write_file(path: &std::path::Path, text: &str) {
      std::fs::write(path, text).unwrap();
   }
}
