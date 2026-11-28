//! What the locators found on this Discord build, kept in the Sunbird home so
//! the next start reuses it instead of reading the 500 MB binary again, which
//! is most of the time Sunbird adds to Discord's start.
//!
//! The saved results are keyed to the exact Discord file and to the locators'
//! own sources ([`phoenix_sigs::SOURCE`]), so an update to either reads the
//! binary again once. Addresses are saved with the image's base and moved to
//! the new one on load, since ASLR picks a fresh base each start.

use std::{
   cell::OnceCell,
   fs,
   ops::Deref,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Mutex,
      PoisonError,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::{
   Found,
   index::{
      Index,
      Isa,
   },
};
use serde::{
   Deserialize,
   Serialize,
   de::DeserializeOwned,
};
use serde_json::{
   Map,
   Value,
};

const FILE: &str = "locators.json";

/// The index of Discord's code, built the first time a locator that reads it
/// runs: on a start with every result saved, never.
pub struct LazyIndex<'img> {
   image: &'img Image<'img>,
   index: OnceCell<Index<'img>>,
}

impl<'img> LazyIndex<'img> {
   pub const fn new(image: &'img Image<'img>) -> Self {
      Self {
         image,
         index: OnceCell::new(),
      }
   }

   /// [`phoenix_sigs::locate::hookable`], which on x86-64 reads only the
   /// image, so a warm start never builds the index for it.
   pub fn hookable(&self, func: usize) -> Found<String> {
      #[cfg(target_arch = "x86_64")]
      return phoenix_sigs::desktop::entry_moved(self.image, func)
         .map(|moved| format!("{func:#x} ({moved} bytes moved)"));
      #[cfg(not(target_arch = "x86_64"))]
      phoenix_sigs::locate::hookable(self, func)
   }
}

impl<'img> Deref for LazyIndex<'img> {
   type Target = Index<'img>;

   fn deref(&self) -> &Index<'img> {
      self.index.get_or_init(|| {
         let started = std::time::Instant::now();
         let index = Index::new(self.image, Isa::HOST);
         log::info!(
            "locators: indexed Discord's code in {:?}",
            started.elapsed()
         );
         index
      })
   }
}

/// The saved results, as `locators.json` holds them.
#[derive(Serialize, Deserialize)]
struct Saved {
   key:     String,
   /// The image's address range the addresses were found in.
   base:    u64,
   end:     u64,
   results: Map<String, Value>,
}

struct Cache {
   path:    PathBuf,
   saved:   Saved,
   changed: bool,
   reused:  usize,
   found:   usize,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn cache() -> std::sync::MutexGuard<'static, Option<Cache>> {
   CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Loads what was saved for `image` from `home`, moved to its base.
pub fn load(image: &Image<'_>, home: &Path) {
   let Some((base, end)) = span(image) else {
      return;
   };
   let key = key(image);
   let path = home.join(FILE);
   let results = fs::read(&path)
      .ok()
      .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
      .filter(|saved| saved.key == key)
      .map_or_else(Map::new, |mut saved| {
         let moved = base.wrapping_sub(saved.base);
         if moved != 0 {
            for value in saved.results.values_mut() {
               rebase(value, saved.base..saved.end, moved);
            }
         }
         saved.results
      });
   *cache() = Some(Cache {
      path,
      saved: Saved {
         key,
         base,
         end,
         results,
      },
      changed: false,
      reused: 0,
      found: 0,
   });
}

/// What `locate` finds, saved under `name`: the saved result when there is
/// one for this build, else `locate`'s, which is saved for the next start.
/// A miss is saved too, so a locator this build does not match is not run
/// again either.
pub fn get<T: Serialize + DeserializeOwned>(
   name: &'static str,
   locate: impl FnOnce() -> Found<T>,
) -> Found<T> {
   let saved = cache()
      .as_ref()
      .and_then(|cache| cache.saved.results.get(name).cloned());
   if let Some(saved) = saved
      && let Ok(found) = serde_json::from_value::<Result<T, String>>(saved)
   {
      if let Some(cache) = cache().as_mut() {
         cache.reused += 1;
      }
      return found;
   }
   let found = locate();
   if let Some(cache) = cache().as_mut()
      && let Ok(value) = serde_json::to_value(&found)
   {
      cache.saved.results.insert(name.to_owned(), value);
      cache.changed = true;
      cache.found += 1;
   }
   found
}

/// Writes what changed for the next start, and logs how many results were
/// reused.
pub fn save() {
   let Some(cache) = cache().take() else {
      return;
   };
   log::info!(
      "locators: {} reused, {} found on this build",
      cache.reused,
      cache.found
   );
   if !cache.changed {
      return;
   }
   let Ok(bytes) = serde_json::to_vec(&cache.saved) else {
      return;
   };
   // Written aside and moved, so a crash mid-write never leaves half a file.
   let tmp = cache.path.with_extension("json.tmp");
   if let Err(err) = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, &cache.path)) {
      log::warn!("locators: could not save {}: {err}", cache.path.display());
   }
}

/// The image's address range.
fn span(image: &Image<'_>) -> Option<(u64, u64)> {
   let base = image.segments.iter().map(|segment| segment.addr).min()?;
   let end = image
      .segments
      .iter()
      .map(|segment| segment.addr + segment.bytes.len())
      .max()?;
   Some((u64::try_from(base).ok()?, u64::try_from(end).ok()?))
}

/// This Discord file and these locators: the file's size and modification
/// time, its unwind table or build ID, and the locators' source hash.
fn key(image: &Image<'_>) -> String {
   let file = fs::metadata(&image.path).ok();
   let size = file.as_ref().map_or(0, fs::Metadata::len);
   let modified = file
      .and_then(|file| file.modified().ok())
      .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
      .map_or(0, |time| time.as_nanos());
   let build = image.build_id.as_ref().map_or_else(
      || {
         // PE images have no build ID; their unwind table changes with every
         // build.
         image
            .pdata
            .and_then(|(_, addr, len)| image.read(addr, len))
            .map_or(0, fnv)
      },
      |id| fnv(id.as_bytes()),
   );
   format!("{}:{size}:{modified}:{build}", phoenix_sigs::SOURCE)
}

fn fnv(bytes: &[u8]) -> u64 {
   bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
      (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3)
   })
}

/// Moves every number in `value` that lies inside `old` by `moved`: the saved
/// addresses. Offsets and sizes are far smaller than any image address, so
/// none of them falls inside.
fn rebase(value: &mut Value, old: std::ops::Range<u64>, moved: u64) {
   match value {
      Value::Number(number) => {
         if let Some(addr) = number.as_u64()
            && old.contains(&addr)
         {
            *number = addr.wrapping_add(moved).into();
         }
      },
      Value::Array(items) => {
         for item in items {
            rebase(item, old.clone(), moved);
         }
      },
      Value::Object(fields) => {
         for field in fields.values_mut() {
            rebase(field, old.clone(), moved);
         }
      },
      Value::Null | Value::Bool(_) | Value::String(_) => {},
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn rebase_moves_only_addresses() {
      let mut value = serde_json::json!({
         "Ok": { "func": 0x1_4000_1000_u64, "offset": 0x88, "list": [0x1_4000_2000_u64, 3] }
      });
      rebase(&mut value, 0x1_4000_0000..0x1_5000_0000, 0x10_0000);
      assert_eq!(
         value,
         serde_json::json!({
            "Ok": { "func": 0x1_4010_1000_u64, "offset": 0x88, "list": [0x1_4010_2000_u64, 3] }
         })
      );
   }
}
