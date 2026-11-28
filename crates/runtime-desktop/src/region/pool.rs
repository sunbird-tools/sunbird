use std::{
   collections::HashSet,
   fs,
   net::IpAddr,
   path::Path,
   sync::{
      Mutex,
      PoisonError,
      atomic::{
         AtomicBool,
         AtomicUsize,
         Ordering,
      },
   },
   thread,
   time::{
      Duration,
      Instant,
   },
};

use super::http;

const LISTS: &[&str] = &[
   "https://raw.githubusercontent.com/monosans/proxy-list/main/proxies/socks5.txt",
   "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/socks5.txt",
   "https://api.proxyscrape.com/v4/free-proxy-list/get?request=display_proxies&protocol=socks5&proxy_format=ipport&format=text",
];

const PROBE: &str = "https://latency.discord.media/rtc";
const WANT: usize = 3;
const WORKERS: usize = 16;

#[derive(Clone)]
pub struct Proxy {
   pub addr:   String,
   pub region: String,
   pub latency: Duration,
   pub fails:  u32,
}

pub fn first_region(body: &[u8]) -> Option<String> {
   let list: Vec<serde_json::Value> = serde_json::from_slice(body).ok()?;
   Some(list.first()?.get("region")?.as_str()?.to_owned())
}

pub fn fill(home: &Path) -> Vec<Proxy> {
   let cached = load(home);
   let warm = validate(&cached, Duration::from_secs(8), WANT);
   if !warm.is_empty() {
      save(home, &warm);
      return warm;
   }
   let fresh = download();
   let found = validate(&fresh, Duration::from_secs(12), WANT);
   if !found.is_empty() {
      save(home, &found);
   }
   found
}

pub fn refresh_later(home: std::path::PathBuf, replace: impl Fn(Vec<Proxy>) + Send + 'static) {
   thread::spawn(move || {
      let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
         let found = validate(&download(), Duration::from_secs(25), WANT);
         if found.is_empty() {
            return;
         }
         save(&home, &found);
         replace(found);
      }));
   });
}

fn load(home: &Path) -> Vec<String> {
   let Ok(bytes) = fs::read(home.join("screen-share-proxies.json")) else {
      return Vec::new();
   };
   serde_json::from_slice::<Vec<String>>(&bytes).unwrap_or_default()
}

fn save(home: &Path, proxies: &[Proxy]) {
   let addrs: Vec<&str> = proxies.iter().map(|proxy| proxy.addr.as_str()).collect();
   let Ok(bytes) = serde_json::to_vec(&addrs) else {
      return;
   };
   let _ = fs::create_dir_all(home);
   let _ = fs::write(home.join("screen-share-proxies.json"), bytes);
}

fn download() -> Vec<String> {
   let mut lists = Vec::new();
   for url in LISTS {
      let Some(body) = http::get(url, 8_000) else {
         continue;
      };
      let Ok(text) = String::from_utf8(body) else {
         continue;
      };
      lists.push(
         text
            .lines()
            .filter_map(parse_proxy)
            .collect::<Vec<_>>(),
      );
   }
   let mut seen = HashSet::new();
   let mut merged = Vec::new();
   let mut index = 0usize;
   loop {
      let mut progressed = false;
      for list in &lists {
         let Some(addr) = list.get(index) else {
            continue;
         };
         progressed = true;
         if seen.insert(addr.clone()) {
            merged.push(addr.clone());
         }
      }
      if !progressed {
         break;
      }
      index += 1;
   }
   merged
}

pub fn parse_proxy(line: &str) -> Option<String> {
   let line = line.trim();
   let line = line
      .strip_prefix("socks5h://")
      .or_else(|| line.strip_prefix("socks5://"))
      .unwrap_or(line);
   let (ip, port) = line.rsplit_once(':')?;
   let port: u16 = port.parse().ok()?;
   if port == 0 || ip.parse::<IpAddr>().is_err() {
      return None;
   }
   if ip.contains(':') {
      return None;
   }
   Some(line.to_owned())
}

fn validate(candidates: &[String], budget: Duration, want: usize) -> Vec<Proxy> {
   if candidates.is_empty() {
      return Vec::new();
   }
   let started = Instant::now();
   let cursor = AtomicUsize::new(0);
   let stop = AtomicBool::new(false);
   let found = Mutex::new(Vec::new());
   thread::scope(|scope| {
      for _ in 0..WORKERS {
         scope.spawn(|| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
               loop {
                  if stop.load(Ordering::Relaxed) {
                     return;
                  }
                  if started.elapsed() > budget
                     && !found.lock().unwrap_or_else(PoisonError::into_inner).is_empty()
                  {
                     return;
                  }
                  if started.elapsed() > budget.saturating_add(Duration::from_secs(8)) {
                     return;
                  }
                  let index = cursor.fetch_add(1, Ordering::Relaxed);
                  if index >= candidates.len() || index >= 240 {
                     return;
                  }
                  if let Some(proxy) = check(&candidates[index]) {
                     let mut list = found.lock().unwrap_or_else(PoisonError::into_inner);
                     list.push(proxy);
                     if list.len() >= want {
                        stop.store(true, Ordering::Relaxed);
                        return;
                     }
                  }
               }
            }));
         });
      }
   });
   let mut list = found.into_inner().unwrap_or_else(PoisonError::into_inner);
   list.sort_by_key(|proxy| proxy.latency);
   list.truncate(want);
   list
}

fn check(addr: &str) -> Option<Proxy> {
   let started = Instant::now();
   let body = http::get_via_socks(PROBE, addr, 6_000)?;
   let region = first_region(&body)?;
   if region == "brazil" {
      return None;
   }
   Some(Proxy {
      addr: addr.to_owned(),
      region,
      latency: started.elapsed(),
      fails: 0,
   })
}

#[cfg(test)]
mod tests {
   use super::{
      first_region,
      parse_proxy,
   };

   #[test]
   fn reads_the_first_region() {
      let body = br#"[{"region":"brazil","id":1},{"region":"rotterdam"}]"#;
      assert_eq!(first_region(body).as_deref(), Some("brazil"));
   }

   #[test]
   fn parses_proxy_lines() {
      assert_eq!(parse_proxy("5.255.99.75:1080").as_deref(), Some("5.255.99.75:1080"));
      assert_eq!(
         parse_proxy("socks5://1.2.3.4:1080").as_deref(),
         Some("1.2.3.4:1080")
      );
      assert!(parse_proxy("example.com:1080").is_none());
      assert!(parse_proxy("5.255.99.75").is_none());
      assert!(parse_proxy("5.255.99.999:1080").is_none());
   }
}
