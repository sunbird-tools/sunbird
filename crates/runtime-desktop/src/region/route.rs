#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
   Opening,
   Established,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
   Outside,
   Direct,
}

const OUTSIDE: &[&str] = &[
   "discord.com",
   "discordapp.com",
   "discord.gg",
   "latency.discord.media",
];

const DECIDES: &[&str] = &["discord.com", "gateway.discord.gg", "latency.discord.media"];

const STATUS: &str = "status.discord.com";

pub fn decide(host: &str, phase: Phase) -> Route {
   if phase == Phase::Established {
      return Route::Direct;
   }
   let host = normalize(host);
   if !well_formed(&host) {
      return Route::Direct;
   }
   if OUTSIDE.iter().any(|domain| matches_domain(&host, domain)) {
      Route::Outside
   } else {
      Route::Direct
   }
}

pub fn decides(host: &str) -> bool {
   let host = normalize(host);
   if !well_formed(&host) || matches_domain(&host, STATUS) {
      return false;
   }
   DECIDES.iter().any(|domain| matches_domain(&host, domain)) || gateway(&host)
}

fn gateway(host: &str) -> bool {
   matches_domain(host, "discord.gg")
      && host
         .split('.')
         .next()
         .is_some_and(|label| label.starts_with("gateway"))
}

fn normalize(host: &str) -> String {
   host.trim_end_matches('.').to_ascii_lowercase()
}

fn well_formed(host: &str) -> bool {
   !host.is_empty()
      && host.len() <= 253
      && host
         .bytes()
         .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn matches_domain(host: &str, domain: &str) -> bool {
   host == domain
      || host
         .strip_suffix(domain)
         .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
   use super::{
      Phase,
      Route,
      decide,
      decides,
   };

   const DISCORD: &[&str] = &[
      "discord.com",
      "gateway.discord.gg",
      "gateway-us-east1-b.discord.gg",
      "latency.discord.media",
      "cdn.discordapp.com",
      "status.discord.com",
      "discord.gg",
      "DISCORD.COM.",
   ];

   #[test]
   fn opening_sends_discord_outside() {
      for host in DISCORD {
         assert_eq!(decide(host, Phase::Opening), Route::Outside, "{host}");
      }
   }

   #[test]
   fn voice_stays_direct_while_opening() {
      for host in [
         "c-gru17-851904d3.discord.media",
         "c-gru18-6fa2a6cb.discord.media",
         "discord.media",
         "media.discordapp.net",
      ] {
         assert_eq!(decide(host, Phase::Opening), Route::Direct, "{host}");
      }
   }

   #[test]
   fn other_hosts_stay_direct() {
      for host in ["google.com", "api.spotify.com", "discord.com.evil.net"] {
         assert_eq!(decide(host, Phase::Opening), Route::Direct, "{host}");
      }
   }

   #[test]
   fn suffix_is_not_a_match() {
      for host in ["notdiscord.com", "mydiscord.gg", "xdiscord.media"] {
         assert_eq!(decide(host, Phase::Opening), Route::Direct, "{host}");
      }
   }

   #[test]
   fn malformed_names_stay_direct() {
      for host in ["evil.com\0.discord.com", "evil.com\n.discord.com", "a b.discord.com", ""] {
         assert_eq!(decide(host, Phase::Opening), Route::Direct, "{host:?}");
      }
   }

   #[test]
   fn established_is_always_direct() {
      for host in DISCORD {
         assert_eq!(decide(host, Phase::Established), Route::Direct, "{host}");
      }
   }

   #[test]
   fn only_region_hosts_hold_the_window() {
      for host in [
         "discord.com",
         "Discord.com",
         "gateway.discord.gg",
         "gateway-us-east1-b.discord.gg",
         "latency.discord.media",
      ] {
         assert!(decides(host), "{host}");
      }
      for host in [
         "cdn.discordapp.com",
         "c-gru17-851904d3.discord.media",
         "status.discord.com",
         "discord.gg",
         "discordapp.com",
         "remote-auth-gateway.discord.gg",
         "google.com",
      ] {
         assert!(!decides(host), "{host}");
      }
   }
}
