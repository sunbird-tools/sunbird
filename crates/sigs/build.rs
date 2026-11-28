//! Hashes the locators' sources, and the decoders in `phoenix-hook` they read
//! code through, into `PHOENIX_SIGS_SOURCE`, so a saved locator result is only
//! reused by the code that found it.

use std::{
   fs,
   path::{
      Path,
      PathBuf,
   },
};

fn main() {
   let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
   let roots = [manifest.join("src"), manifest.join("../hook/src")];
   let mut files = Vec::new();
   for root in &roots {
      println!("cargo:rerun-if-changed={}", root.display());
      collect(root, &mut files);
   }
   files.sort();
   let hash = files.iter().fold(0xCBF2_9CE4_8422_2325_u64, |hash, path| {
      let name = path
         .strip_prefix(&manifest)
         .unwrap_or(path)
         .to_string_lossy();
      let bytes = fs::read(path).unwrap_or_default();
      fnv(fnv(hash, name.as_bytes()), &bytes)
   });
   println!("cargo:rustc-env=PHOENIX_SIGS_SOURCE={hash:016x}");
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
   let Ok(entries) = fs::read_dir(dir) else {
      return;
   };
   for entry in entries.flatten() {
      let path = entry.path();
      if path.is_dir() {
         collect(&path, files);
      } else if path.extension().is_some_and(|ext| ext == "rs") {
         files.push(path);
      }
   }
}

fn fnv(hash: u64, bytes: &[u8]) -> u64 {
   bytes.iter().fold(hash, |hash, &byte| {
      (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3)
   })
}
