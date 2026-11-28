//! One query interface over every Discord binary.
//!
//! Desktop Phoenix on Windows, Linux and macOS is one Rust tree, and so are
//! the Android and iOS hosts. A locator written against what that source
//! forces into the binary (string literals, panic [`Location`]s, tracing
//! callsites, the call graph) then runs on every build of the family, and only
//! the last step, reading an offset or immediate out of an instruction, sees
//! the ISA, through [`Op`].
//!
//! [`Index`] decodes every function once on all cores and keeps two sorted
//! reference tables, calls and address loads, so each query is a binary
//! search.

pub mod flow;
pub mod op;
mod thumb;

use std::{
   ops::Range,
   sync::OnceLock,
};

use phoenix_hook::image::Image;

pub use crate::index::op::{
   Isa,
   Native,
   Op,
   Reg,
   Step,
   Value,
};
use crate::{
   Found,
   one,
   registry::GatewayRegistry,
};

/// How far before a match a string literal can start. Rust merges literals
/// into one blob, so code loads the start of the literal, not the match.
const LITERAL_REACH: usize = 512;

/// The longest `&str` the string table records.
const MAX_STR: usize = 4096;

/// `(target << 32 | site)` as offsets from the image base, sorted.
struct Refs(Vec<u64>);

struct Tables {
   calls: Refs,
   addrs: Refs,
}

/// A `&'static str` stored in data: the slot holding its pointer, and the
/// bytes it points at.
#[derive(Clone, Copy, Debug)]
pub struct StrSlot {
   pub slot: usize,
   pub ptr:  usize,
   pub len:  usize,
}

pub struct Index<'img> {
   pub image: &'img Image<'img>,
   pub isa:   Isa,
   functions: Vec<(usize, usize)>,
   base:      usize,
   tables:    OnceLock<Tables>,
   strs:      OnceLock<Vec<StrSlot>>,
   registry:  OnceLock<Found<GatewayRegistry>>,
}

impl<'img> Index<'img> {
   #[must_use]
   pub fn new(image: &'img Image<'img>, isa: Isa) -> Self {
      let base = image
         .segments
         .iter()
         .map(|segment| segment.addr)
         .min()
         .unwrap_or(0);
      let mut index = Self {
         image,
         isa,
         functions: image.functions(),
         base,
         tables: OnceLock::new(),
         strs: OnceLock::new(),
         registry: OnceLock::new(),
      };
      index.add_leaves();
      index
   }

   /// Win64 leaf functions need no unwind data, so `.pdata` leaves them out.
   /// A call target outside every listed function starts one, which ends at
   /// the next listed start.
   fn add_leaves(&mut self) {
      let mut tables = self.build_tables(&self.functions, true);
      let mut leaves = tables
         .calls
         .0
         .iter()
         .map(|&r| self.base + (r >> 32) as usize)
         .filter(|&target| self.function(target).is_none() && self.image.is_code(target))
         .collect::<Vec<usize>>();
      leaves.sort_unstable();
      leaves.dedup();
      let mut added = Vec::with_capacity(leaves.len());
      for (at, &start) in leaves.iter().enumerate() {
         let next_listed = self.functions[self
            .functions
            .partition_point(|&(listed, _)| listed <= start)..]
            .first()
            .map_or(usize::MAX, |&(listed, _)| listed);
         let next_leaf = leaves.get(at + 1).copied().unwrap_or(usize::MAX);
         let segment_end = self
            .image
            .segments
            .iter()
            .find(|segment| segment.executable && segment.contains(start))
            .map_or(start, |segment| segment.addr + segment.bytes.len());
         let end = next_listed.min(next_leaf).min(segment_end);
         if end > start {
            added.push((start, end));
         }
      }
      if !added.is_empty() {
         let more = self.build_tables(&added, false);
         for (into, from) in [
            (&mut tables.calls, more.calls),
            (&mut tables.addrs, more.addrs),
         ] {
            into.0.extend(from.0);
            into.0.sort_unstable();
         }
         self.functions.extend(added);
         self.functions.sort_unstable();
      }
      drop(self.tables.set(tables));
   }

   #[must_use]
   pub const fn function_count(&self) -> usize {
      self.functions.len()
   }

   /// The function holding `addr`.
   #[must_use]
   pub fn function(&self, addr: usize) -> Option<Func<'_>> {
      let index = self
         .functions
         .partition_point(|&(start, _)| start <= addr)
         .checked_sub(1)?;
      let (start, end) = self.functions[index];
      (addr < end).then_some(Func {
         ix: self,
         start,
         end,
      })
   }

   /// The function starting at `start`.
   pub fn func(&self, start: usize) -> Found<Func<'_>> {
      self
         .function(start)
         .filter(|func| func.start == start)
         .ok_or_else(|| format!("{start:#x} starts no function"))
   }

   fn funcs(&self, sites: impl IntoIterator<Item = usize>) -> Funcs<'_> {
      let mut list = sites
         .into_iter()
         .filter_map(|site| self.function(site))
         .collect::<Vec<Func<'_>>>();
      list.sort_unstable_by_key(|func| func.start);
      list.dedup_by_key(|func| func.start);
      Funcs { list }
   }

   /// The gateway registry, walked once (it scans the whole image).
   pub fn registry(&self) -> Found<&GatewayRegistry> {
      self
         .registry
         .get_or_init(|| GatewayRegistry::locate(self.image).map_err(|err| err.to_string()))
         .as_ref()
         .map_err(Clone::clone)
   }

   fn tables(&self) -> &Tables {
      self
         .tables
         .get_or_init(|| self.build_tables(&self.functions, true))
   }

   fn build_tables(&self, functions: &[(usize, usize)], seed: bool) -> Tables {
      if self.isa == Isa::Aarch64 {
         return self.build_tables_aarch64(functions, seed);
      }
      let pack = |site: usize, target: usize| -> Option<u64> {
         let site = u32::try_from(site.checked_sub(self.base)?).ok()?;
         let target = u32::try_from(target.checked_sub(self.base)?).ok()?;
         Some(u64::from(target) << 32 | u64::from(site))
      };
      let (mut calls, mut addrs) = (Vec::new(), Vec::new());
      for (part_calls, part_addrs) in across(functions, |part| {
         let (mut calls, mut addrs) = (Vec::new(), Vec::new());
         for &(start, end) in part {
            for step in op::decode(self.image, self.isa, start, end) {
               match step.op {
                  Op::Call(target) => calls.extend(pack(step.at, target)),
                  Op::Jump(target) if !(start..end).contains(&target) => {
                     calls.extend(pack(step.at, target));
                  },
                  Op::Addr { target, .. } | Op::Global { target, .. } => {
                     addrs.extend(pack(step.at, target));
                  },
                  _ => {},
               }
            }
         }
         (calls, addrs)
      }) {
         calls.extend(part_calls);
         addrs.extend(part_addrs);
      }
      calls.sort_unstable();
      addrs.sort_unstable();
      Tables {
         calls: Refs(calls),
         addrs: Refs(addrs),
      }
   }

   fn build_tables_aarch64(&self, functions: &[(usize, usize)], seed: bool) -> Tables {
      let max_end = self
         .image
         .segments
         .iter()
         .map(|seg| seg.addr.saturating_add(seg.bytes.len()))
         .max()
         .unwrap_or(self.base);
      let max_rva = max_end.saturating_sub(self.base);
      let shift = max_rva.bit_width().saturating_sub(4);
      let seed_arm64 = seed && u32::try_from(max_rva).is_ok();
      let parts = across(functions, |part| {
         let mut calls: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
         let mut addrs: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
         let mut arm64_refs: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
         let mut all_valid = true;
         for &(start, end) in part {
            let Some(code) = self.image.read(start, end.saturating_sub(start)) else {
               all_valid = false;
               continue;
            };
            let exec_seg = seed_arm64
               .then(|| {
                  self
                     .image
                     .segments
                     .iter()
                     .find(|seg| seg.executable && seg.contains(start))
               })
               .flatten();
            let seed_this = exec_seg.is_some_and(|seg| {
               let aligned = (start - seg.addr).is_multiple_of(4);
               all_valid &= aligned;
               aligned
            });
            let warm_prefix = if seed_this {
               self.exec_warm_prefix(start)
            } else {
               &[]
            };
            aarch64_table_refs(
               code,
               warm_prefix,
               start,
               end,
               self.base,
               max_end,
               shift,
               seed_this,
               &mut calls,
               &mut addrs,
               &mut arm64_refs,
            );
         }
         (calls, addrs, arm64_refs, all_valid)
      });
      let mut calls_buckets: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
      let mut addrs_buckets: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
      let mut arm64_buckets: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
      let mut can_seed = seed_arm64;
      for (c, a, r, all_read) in parts {
         can_seed &= all_read;
         for b in 0..16 {
            calls_buckets[b].extend_from_slice(&c[b]);
            addrs_buckets[b].extend_from_slice(&a[b]);
            if can_seed {
               arm64_buckets[b].extend_from_slice(&r[b]);
            }
         }
      }
      if can_seed {
         self.scan_aarch64_exec_gaps(functions, max_end, shift, &mut arm64_buckets);
      }
      let threads = std::thread::available_parallelism().map_or(1, usize::from);
      let calls = sort_and_flatten(calls_buckets, threads);
      let addrs = sort_and_flatten(addrs_buckets, threads);
      if can_seed {
         let mut refs = sort_and_flatten(arm64_buckets, threads);
         refs.dedup();
         phoenix_hook::arm64::seed_ref_index(self.image, refs);
      }
      Tables {
         calls: Refs(calls),
         addrs: Refs(addrs),
      }
   }

   /// Returns up to `PAIR_REACH` instructions immediately before `start` inside
   /// the same executable segment so seeded arm64 `adrp` lookback matches
   /// `arm64::scan_chunk_refs`.
   fn exec_warm_prefix(&self, start: usize) -> &[u8] {
      let max_bytes = op::PAIR_REACH * 4;
      let Some(seg) = self
         .image
         .segments
         .iter()
         .find(|seg| seg.executable && seg.contains(start))
      else {
         return &[];
      };
      let offset = start - seg.addr;
      let warm_start = offset.saturating_sub(max_bytes);
      // Keep 4-byte instruction alignment relative to `seg.addr`.
      let aligned_start = warm_start + ((offset - warm_start) % 4);
      &seg.bytes[aligned_start..offset]
   }

   /// Scans executable bytes outside `functions` into `arm64_buckets` so the
   /// seeded `arm64::REF_INDEX` covers the exact same executable segments as
   /// `arm64::scan_chunk_refs`.
   fn scan_aarch64_exec_gaps(
      &self,
      functions: &[(usize, usize)],
      max_end: usize,
      shift: u32,
      arm64_buckets: &mut [Vec<u64>; 16],
   ) {
      let mut dummy_calls: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
      let mut dummy_addrs: [Vec<u64>; 16] = std::array::from_fn(|_| Vec::new());
      for seg in self.image.segments.iter().filter(|seg| seg.executable) {
         let seg_start = seg.addr;
         let seg_end = seg.addr.saturating_add(seg.bytes.len());
         let mut cursor = seg_start;
         for &(fn_start, fn_end) in functions {
            if fn_end <= seg_start {
               continue;
            }
            if fn_start >= seg_end {
               break;
            }
            if fn_start > cursor {
               let code = &seg.bytes[cursor - seg_start..fn_start - seg_start];
               let warm_prefix = self.exec_warm_prefix(cursor);
               aarch64_table_refs(
                  code,
                  warm_prefix,
                  cursor,
                  fn_start,
                  self.base,
                  max_end,
                  shift,
                  true,
                  &mut dummy_calls,
                  &mut dummy_addrs,
                  arm64_buckets,
               );
            }
            cursor = cursor.max(fn_end - (fn_end - seg_start) % 4);
         }
         if cursor < seg_end {
            let code = &seg.bytes[cursor - seg_start..];
            let warm_prefix = self.exec_warm_prefix(cursor);
            aarch64_table_refs(
               code,
               warm_prefix,
               cursor,
               seg_end,
               self.base,
               max_end,
               shift,
               true,
               &mut dummy_calls,
               &mut dummy_addrs,
               arm64_buckets,
            );
         }
      }
   }

   /// How many calls and address loads the tables hold.
   pub fn ref_counts(&self) -> (usize, usize) {
      let tables = self.tables();
      (tables.calls.0.len(), tables.addrs.0.len())
   }

   /// Builds the reference tables now, which every query below needs.
   pub fn warm(&self) {
      self.tables();
      self.strs();
   }

   /// `(site, target)` for every reference in `refs` landing in `targets`.
   fn range(&self, refs: &Refs, targets: Range<usize>) -> impl Iterator<Item = (usize, usize)> {
      let rva = |addr: usize| {
         u64::try_from(addr.saturating_sub(self.base))
            .unwrap_or(u64::MAX)
            .min(u64::from(u32::MAX) + 1)
      };
      let (low, high) = (rva(targets.start), rva(targets.end));
      let start = refs.0.partition_point(|&r| r >> 32 < low);
      let end = refs.0.partition_point(|&r| r >> 32 < high);
      refs.0[start..end].iter().map(|&r| {
         (
            self.base + (r & 0xFFFF_FFFF) as usize,
            self.base + (r >> 32) as usize,
         )
      })
   }

   /// The functions that load an address in `targets`.
   #[must_use]
   pub fn loaders(&self, targets: Range<usize>) -> Funcs<'_> {
      self.funcs(
         self
            .range(&self.tables().addrs, targets)
            .map(|(site, _)| site),
      )
   }

   /// The sites that load an address in `targets`, with the address.
   pub fn loads_of(&self, targets: Range<usize>) -> Vec<(usize, usize)> {
      self.range(&self.tables().addrs, targets).collect()
   }

   /// The call sites of `callee`.
   pub fn call_sites(&self, callee: usize) -> Vec<usize> {
      self
         .range(&self.tables().calls, callee..callee + 1)
         .map(|(site, _)| site)
         .collect()
   }

   /// The functions whose first `steps` instructions pass `keep`, checked on
   /// all cores.
   #[must_use]
   pub fn heads(&self, steps: usize, keep: impl Fn(&[Step]) -> bool + Sync) -> Funcs<'_> {
      let bytes = steps
         * match self.isa {
            Isa::Aarch64 | Isa::Thumb2 => 4,
            #[cfg(any(
               target_arch = "x86_64",
               target_os = "macos",
               all(target_os = "linux", target_arch = "aarch64")
            ))]
            Isa::X86_64 { .. } => 15,
         };
      let starts = across(&self.functions, |part| {
         part
            .iter()
            .filter(|&&(start, end)| {
               keep(&op::decode_head(self.image, self.isa, start, end.min(start + bytes), steps))
            })
            .map(|&(start, _)| start)
            .collect::<Vec<_>>()
      })
      .into_iter()
      .flatten()
      .collect::<Vec<usize>>();
      self.funcs(starts)
   }

   /// Like [`Self::heads`], but inspects up to `max_words` (capped at 64) raw
   /// `AArch64` instruction words on the stack with zero heap allocations.
   #[must_use]
   pub fn heads_arm64_words(
      &self,
      max_words: usize,
      keep: impl Fn(&[u32]) -> bool + Sync,
   ) -> Funcs<'_> {
      let count = max_words.min(64);
      let starts = across(&self.functions, |part| {
         let mut out = Vec::new();
         for &(start, end) in part {
            let bytes = end.saturating_sub(start).min(count * 4);
            let Some(code) = self.image.read(start, bytes) else {
               continue;
            };
            let chunks = code.as_chunks::<4>().0;
            let mut words = [0u32; 64];
            for (dst, &raw) in words.iter_mut().zip(chunks.iter()) {
               *dst = u32::from_le_bytes(raw);
            }
            if keep(&words[..chunks.len()]) {
               out.push(start);
            }
         }
         out
      })
      .into_iter()
      .flatten()
      .collect::<Vec<usize>>();
      self.funcs(starts)
   }

   /// Where `needle` sits in any segment.
   #[must_use]
   pub fn text(&self, needle: &[u8]) -> Vec<usize> {
      self.image.find_bytes(needle)
   }

   /// The functions that load the string literal holding `needle`. Each hit
   /// counts the loads of the closest literal start at or before it.
   #[must_use]
   pub fn literal(&self, needle: &[u8]) -> Funcs<'_> {
      let mut sites = Vec::new();
      for hit in self.text(needle) {
         let loads = self.loads_of(hit.saturating_sub(LITERAL_REACH)..hit + 1);
         let Some(closest) = loads.iter().map(|&(_, target)| target).max() else {
            continue;
         };
         sites.extend(
            loads
               .iter()
               .filter(|&&(_, target)| target == closest)
               .map(|&(site, _)| site),
         );
      }
      self.funcs(sites)
   }

   /// Every `(ptr, len)` pair in data that points at UTF-8 bytes in the image,
   /// sorted by `ptr`.
   pub fn strs(&self) -> &[StrSlot] {
      self.strs.get_or_init(|| {
         let width = self.image.pointer_width;
         let threads = std::thread::available_parallelism().map_or(1, usize::from);
         let mut found = Vec::new();
         for segment in self
            .image
            .segments
            .iter()
            .filter(|segment| !segment.executable)
         {
            let words = segment.bytes.len() / width;
            let count = words.saturating_sub(1);
            if count == 0 {
               continue;
            }
            let scan_range = |from: usize, to: usize| {
               let mut part = Vec::new();
               for index in from..to {
                  let at = index * width;
                  let mut raw_len = [0u8; 8];
                  raw_len[..width].copy_from_slice(&segment.bytes[at + width..at + 2 * width]);
                  let len = usize::try_from(u64::from_le_bytes(raw_len)).unwrap_or(usize::MAX);
                  if !(1..=MAX_STR).contains(&len) {
                     continue;
                  }
                  let mut raw_ptr = [0u8; 8];
                  raw_ptr[..width].copy_from_slice(&segment.bytes[at..at + width]);
                  let ptr = usize::try_from(u64::from_le_bytes(raw_ptr)).unwrap_or(usize::MAX);
                  if let Some(bytes) = self.image.read(ptr, len)
                     && std::str::from_utf8(bytes).is_ok()
                  {
                     part.push(StrSlot {
                        slot: segment.addr + at,
                        ptr,
                        len,
                     });
                  }
               }
               part
            };
            if threads > 1 && count >= 65_536 {
               let chunk = count.div_ceil(threads).max(16_384);
               let parts: Vec<Vec<StrSlot>> = std::thread::scope(|scope| {
                  #[expect(
                     clippy::needless_collect,
                     reason = "spawn every chunk worker before joining"
                  )]
                  let handles: Vec<_> = (0..count)
                     .step_by(chunk)
                     .map(|from| {
                        let to = (from + chunk).min(count);
                        let spawned = std::thread::Builder::new()
                           .spawn_scoped(scope, move || scan_range(from, to));
                        (from, to, spawned)
                     })
                     .collect();
                  handles
                     .into_iter()
                     .map(|(from, to, spawned)| {
                        spawned.map_or_else(
                           |_| scan_range(from, to),
                           |handle| {
                              handle
                                 .join()
                                 .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                           },
                        )
                     })
                     .collect()
               });
               for part in parts {
                  found.extend(part);
               }
            } else {
               found.extend(scan_range(0, count));
            }
         }
         found.sort_unstable_by_key(|slot| (slot.ptr, slot.slot));
         found
      })
   }

   /// The stored `&str`s whose bytes hold `addr`.
   pub fn strs_over(&self, addr: usize) -> impl Iterator<Item = StrSlot> + '_ {
      let strs = self.strs();
      let end = strs.partition_point(|slot| slot.ptr <= addr);
      let start = strs.partition_point(|slot| slot.ptr + MAX_STR <= addr);
      strs[start..end]
         .iter()
         .copied()
         .filter(move |slot| addr < slot.ptr + slot.len)
   }

   /// The bytes of a stored `&str`.
   #[must_use]
   pub fn str_of(&self, slot: StrSlot) -> &str {
      self
         .image
         .read(slot.ptr, slot.len)
         .and_then(|bytes| std::str::from_utf8(bytes).ok())
         .unwrap_or_default()
   }

   /// Every panic [`Location`] in the image.
   pub fn locations(&self) -> impl Iterator<Item = Location> + '_ {
      let width = self.image.pointer_width;
      self.strs().iter().filter_map(move |&file| {
         let bytes = self.image.read(file.ptr, file.len)?;
         if !bytes.ends_with(b".rs") {
            return None;
         }
         let line = self.image.read_u32(file.slot + 2 * width)?;
         let column = self.image.read_u32(file.slot + 2 * width + 4)?;
         ((1..1_000_000).contains(&line) && (1..1_000).contains(&column)).then_some(Location {
            file,
            line,
            column,
         })
      })
   }

   /// The [`Location`] stored at `addr`, if one is.
   #[must_use]
   pub fn location_at(&self, addr: usize) -> Option<Location> {
      let width = self.image.pointer_width;
      let file = StrSlot {
         slot: addr,
         ptr:  self.image.read_usize(addr)?,
         len:  self.image.read_usize(addr + width)?,
      };
      if file.len > MAX_STR || !self.image.read(file.ptr, file.len)?.ends_with(b".rs") {
         return None;
      }
      let line = self.image.read_u32(addr + 2 * width)?;
      let column = self.image.read_u32(addr + 2 * width + 4)?;
      ((1..1_000_000).contains(&line) && (1..1_000).contains(&column)).then_some(Location {
         file,
         line,
         column,
      })
   }

   /// The functions that can panic at a [`Location`] in a file whose path
   /// ends in `suffix`, `/` matching either separator.
   #[must_use]
   pub fn source(&self, suffix: &str) -> Funcs<'_> {
      let sites = self
         .locations()
         .filter(|location| location.ends_with(self, suffix))
         .flat_map(|location| {
            self
               .loads_of(location.file.slot..location.file.slot + 1)
               .into_iter()
               .map(|(site, _)| site)
         })
         .collect::<Vec<usize>>();
      self.funcs(sites)
   }

   /// The functions that enter a tracing span or event whose target is
   /// `target`, like `libdiscord_media::fetch`, through the callsite its
   /// metadata names.
   #[must_use]
   pub fn tracing(&self, target: &str) -> Funcs<'_> {
      let width = self.image.pointer_width;
      let mut sites = Vec::new();
      for hit in self.text(target.as_bytes()) {
         for slot in self.strs_over(hit) {
            if slot.ptr != hit || slot.len != target.len() {
               continue;
            }
            // The metadata's field set names its callsite static, and the
            // callsite points back at the metadata, which opens a few words
            // before `name: &str`. Code loads the callsite.
            let name = slot.slot.saturating_sub(2 * width);
            let opens = name.saturating_sub(6 * width)..name + 1;
            let back = |callsite: usize| {
               (0..6).any(|word| {
                  self
                     .image
                     .read_usize(callsite + word * width)
                     .is_some_and(|meta| opens.contains(&meta))
               })
            };
            for word in (name..name + 14 * width).step_by(width) {
               let Some(callsite) = self.image.read_usize(word).filter(|&at| back(at)) else {
                  continue;
               };
               sites.extend(
                  self
                     .loads_of(callsite..callsite + 1)
                     .into_iter()
                     .map(|(site, _)| site),
               );
            }
         }
      }
      self.funcs(sites)
   }
}

/// Sorts 16 range-partitioned `u64` buckets (in parallel when `threads > 1`)
/// and flattens them into one globally sorted `Vec<u64>`.
fn sort_and_flatten(mut buckets: [Vec<u64>; 16], threads: usize) -> Vec<u64> {
   if threads <= 1 {
      for bucket in &mut buckets {
         bucket.sort_unstable();
      }
   } else {
      std::thread::scope(|scope| {
         let mut handles = Vec::new();
         for bucket in &mut buckets {
            if bucket.len() > 1 {
               let ptr = bucket.as_mut_ptr() as usize;
               let len = bucket.len();
               match std::thread::Builder::new().spawn_scoped(scope, move || {
                  // SAFETY: each bucket's buffer is disjoint and lives for the scope.
                  unsafe { std::slice::from_raw_parts_mut(ptr as *mut u64, len) }.sort_unstable();
               }) {
                  Ok(handle) => handles.push(handle),
                  Err(_) => bucket.sort_unstable(),
               }
            }
         }
         for handle in handles {
            if let Err(panic) = handle.join() {
               std::panic::resume_unwind(panic);
            }
         }
      });
   }
   let total: usize = buckets.iter().map(Vec::len).sum();
   let mut out = Vec::with_capacity(total);
   for bucket in buckets {
      out.extend_from_slice(&bucket);
   }
   out
}

/// Zero-allocation `AArch64` call and address-load extractor for [`Index::build_tables`].
/// Matches [`op::decode`] for `Op::Call`, out-of-function `Op::Jump`, `Op::Addr`
/// (`adrp`, `adrp+add`), and `Op::Global` (`adrp+ldr`/`adrp+str`), and also populates
/// `arm64_refs` (`adrp+add`, `adrp+ldr/str`, `bl`, `b`) so `phoenix_hook::arm64::code_refs`
/// never has to scan `__text` a second time.
#[expect(
   clippy::too_many_arguments,
   reason = "hot inner loop avoiding struct overhead"
)]
fn aarch64_table_refs(
   code: &[u8],
   warm_prefix: &[u8],
   start: usize,
   end: usize,
   base: usize,
   max_end: usize,
   shift: u32,
   seed_arm64: bool,
   calls: &mut [Vec<u64>; 16],
   addrs: &mut [Vec<u64>; 16],
   arm64_refs: &mut [Vec<u64>; 16],
) {
   const fn sign_extend(value: u32, bits: u32) -> isize {
      let shift = 32 - bits;
      (((value << shift).cast_signed()) >> shift) as isize
   }
   const fn rel(pc: usize, imm: u32, bits: u32) -> usize {
      pc.wrapping_add_signed(sign_extend(imm, bits) * 4)
   }
   let push = |buckets: &mut [Vec<u64>; 16], site: usize, target: usize| {
      if let (Some(s), Some(t)) = (site.checked_sub(base), target.checked_sub(base))
         && let (Ok(s32), Ok(t32)) = (u32::try_from(s), u32::try_from(t))
      {
         let b = (t >> shift).min(15);
         buckets[b].push((u64::from(t32) << 32) | u64::from(s32));
      }
   };
   let warm_words = warm_prefix.as_chunks::<4>().0;
   let warm_len = warm_words.len();
   let mut pages = [(usize::MAX, 0usize); 32];
   let mut arm64_pages = [(usize::MAX, 0usize); 32];
   if seed_arm64 && warm_len > 0 {
      let warm_start = start - warm_len * 4;
      for (idx, &raw) in warm_words.iter().enumerate() {
         let word = u32::from_le_bytes(raw);
         if word & 0x9F00_0000 == 0x9000_0000 {
            let rd = (word & 0x1F) as usize;
            let immlo = (word >> 29) & 3;
            let immhi = (word >> 5) & 0x7_FFFF;
            let imm = sign_extend((immhi << 2) | immlo, 21) << 12;
            let pc = warm_start + idx * 4;
            arm64_pages[rd] = (idx, (pc & !0xFFF).wrapping_add_signed(imm));
         }
      }
   }
   for (index, &raw) in code.as_chunks::<4>().0.iter().enumerate() {
      let pc = start + index * 4;
      let arm64_idx = warm_len + index;
      let word = u32::from_le_bytes(raw);
      if word & 0x9F00_0000 == 0x9000_0000 {
         let rd = (word & 0x1F) as usize;
         let immlo = (word >> 29) & 3;
         let immhi = (word >> 5) & 0x7_FFFF;
         let imm = sign_extend((immhi << 2) | immlo, 21) << 12;
         let page = (pc & !0xFFF).wrapping_add_signed(imm);
         pages[rd] = (index, page);
         arm64_pages[rd] = (arm64_idx, page);
         push(addrs, pc, page);
      } else if word & 0x7F80_0000 == 0x1100_0000 {
         let rn = ((word >> 5) & 0x1F) as usize;
         let shift_bit = (word >> 22) & 1;
         let imm = (((word >> 10) & 0xFFF) as usize) << (12 * shift_bit);
         let (set, page) = pages[rn];
         if set != usize::MAX && index - set <= op::PAIR_REACH {
            push(addrs, pc, page.wrapping_add(imm));
         }
         if seed_arm64 {
            let (a_set, a_page) = arm64_pages[rn];
            if a_set != usize::MAX && arm64_idx - a_set <= op::PAIR_REACH {
               let target = a_page.wrapping_add(imm);
               if target >= base && target < max_end {
                  push(arm64_refs, pc, target);
               }
            }
         }
      } else if word & 0x3F00_0000 == 0x3900_0000 && ((word >> 22) & 3) <= 1 {
         let rn = ((word >> 5) & 0x1F) as usize;
         let size = 1usize << (word >> 30);
         let offset = ((word >> 10) & 0xFFF) as usize * size;
         let (set, page) = pages[rn];
         if set != usize::MAX && index - set <= op::PAIR_REACH {
            push(addrs, pc, page.wrapping_add(offset));
         }
         if seed_arm64 {
            let (a_set, a_page) = arm64_pages[rn];
            if a_set != usize::MAX && arm64_idx - a_set <= op::PAIR_REACH {
               let target = a_page.wrapping_add(offset);
               if target >= base && target < max_end {
                  push(arm64_refs, pc, target);
               }
            }
         }
      } else if word & 0xFC00_0000 == 0x9400_0000 {
         let target = rel(pc, word & 0x03FF_FFFF, 26);
         push(calls, pc, target);
         if seed_arm64 && target >= base && target < max_end {
            push(arm64_refs, pc, target);
         }
      } else if word & 0xFC00_0000 == 0x1400_0000 {
         let target = rel(pc, word & 0x03FF_FFFF, 26);
         if !(start..end).contains(&target) {
            push(calls, pc, target);
         }
         if seed_arm64 && target >= base && target < max_end {
            push(arm64_refs, pc, target);
         }
      }
   }
}

/// `work` over `functions` split across the cores, in order. Targets with one
/// thread, wasm among them, run it inline.
#[expect(
   clippy::needless_collect,
   reason = "every chunk spawns before the first join"
)]
fn across<T: Send>(
   functions: &[(usize, usize)],
   work: impl Fn(&[(usize, usize)]) -> T + Sync,
) -> Vec<T> {
   let threads = std::thread::available_parallelism().map_or(1, usize::from);
   if threads <= 1 {
      return vec![work(functions)];
   }
   let chunk = functions.len().div_ceil(threads).max(1);
   let work = &work;
   std::thread::scope(|scope| {
      let handles = functions
         .chunks(chunk)
         .map(|part| scope.spawn(move || work(part)))
         .collect::<Vec<_>>();
      handles
         .into_iter()
         .map(|handle| {
            handle
               .join()
               .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
         })
         .collect()
   })
}

/// A `core::panic::Location`, `{ file: &str, line: u32, col: u32 }`.
#[derive(Clone, Copy, Debug)]
pub struct Location {
   pub file:   StrSlot,
   pub line:   u32,
   pub column: u32,
}

impl Location {
   fn ends_with(&self, ix: &Index<'_>, suffix: &str) -> bool {
      let file = ix.str_of(self.file).as_bytes();
      file.len() >= suffix.len()
         && file[file.len() - suffix.len()..]
            .iter()
            .zip(suffix.as_bytes())
            .all(|(&have, &want)| have == want || (want == b'/' && have == b'\\'))
   }
}

/// A function the unwind table bounds.
#[derive(Clone, Copy)]
pub struct Func<'ix> {
   ix:        &'ix Index<'ix>,
   pub start: usize,
   pub end:   usize,
}

impl std::fmt::Debug for Func<'_> {
   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      write!(f, "{:#x}", self.start)
   }
}

impl<'ix> Func<'ix> {
   #[must_use]
   pub const fn size(&self) -> usize {
      self.end - self.start
   }

   #[must_use]
   pub fn steps(&self) -> Vec<Step> {
      op::decode(self.ix.image, self.ix.isa, self.start, self.end)
   }

   /// The first `count` steps, decoding no further.
   #[must_use]
   pub fn head(&self, count: usize) -> Vec<Step> {
      op::decode_head(self.ix.image, self.ix.isa, self.start, self.end, count)
   }

   /// The functions this one calls, in instruction order, tail jumps
   /// included. On arm64 the outliner turns repeated sequences into a few
   /// frameless instructions ending in `b T` or `ret`: a call to one counts as
   /// a call to `T`, or as none.
   #[must_use]
   pub fn calls(&self) -> Vec<usize> {
      self
         .calls_at()
         .into_iter()
         .map(|(_, callee)| callee)
         .collect()
   }

   /// [`Func::calls`] with the address of each call site.
   #[must_use]
   pub fn calls_at(&self) -> Vec<(usize, usize)> {
      self
         .steps()
         .iter()
         .filter_map(|step| {
            let callee = match step.op {
               Op::Call(target) => self.through_outlined(target),
               Op::Jump(target) if !(self.start..self.end).contains(&target) => Some(target),
               _ => None,
            }?;
            Some((step.at, callee))
         })
         .collect()
   }

   pub(crate) fn through_outlined(&self, target: usize) -> Option<usize> {
      self.outlined(target, 6).map_or(Some(target), |stub| {
         match stub.last().map(|step| step.op) {
            Some(Op::Jump(tail)) => Some(tail),
            _ => None,
         }
      })
   }

   /// The steps of the arm64 outlined stub of at most `most` instructions at
   /// `target`, through its closing `b T` or `ret`, or `None` when `target` is
   /// a function of its own.
   pub(crate) fn outlined(&self, target: usize, most: usize) -> Option<Vec<Step>> {
      if self.ix.isa != Isa::Aarch64 {
         return None;
      }
      let callee = self.ix.function(target)?;
      if callee.size() > most * 4 {
         return None;
      }
      let mut steps = op::decode(self.ix.image, self.ix.isa, target, callee.end);
      let end = steps
         .iter()
         .position(|step| matches!(step.op, Op::Jump(_) | Op::Ret | Op::Call(_) | Op::Branch(_)))?;
      if !matches!(steps[end].op, Op::Jump(_) | Op::Ret) {
         return None;
      }
      steps.truncate(end + 1);
      Some(steps)
   }

   /// [`Func::steps`] with each arm64 outlined stub's instructions in place of
   /// the call or tail jump to it. A stub a call reaches that ends in `b T`
   /// becomes a call to `T`, and one ending in `ret` returns to the caller.
   /// macOS setters open with a 7-instruction stub saving their arguments.
   #[must_use]
   pub fn flat_steps(&self) -> Vec<Step> {
      const MOST: usize = 12;
      let mut flat = Vec::new();
      for step in self.steps() {
         let (target, called) = match step.op {
            Op::Call(target) => (target, true),
            Op::Jump(target) if !(self.start..self.end).contains(&target) => (target, false),
            _ => {
               flat.push(step);
               continue;
            },
         };
         let Some(mut stub) = self.outlined(target, MOST) else {
            flat.push(step);
            continue;
         };
         if let Some(last) = stub.last_mut() {
            match last.op {
               Op::Jump(tail) if called => last.op = Op::Call(tail),
               Op::Ret if called => {
                  stub.pop();
               },
               _ => {},
            }
         }
         flat.extend(stub);
      }
      flat
   }

   /// The addresses this function loads, in instruction order.
   #[must_use]
   pub fn loads(&self) -> Vec<usize> {
      self
         .steps()
         .iter()
         .filter_map(|step| {
            match step.op {
               Op::Addr { target, .. } | Op::Global { target, .. } => Some(target),
               _ => None,
            }
         })
         .collect()
   }

   #[must_use]
   pub fn callers(&self) -> Funcs<'ix> {
      self.ix.funcs(self.ix.call_sites(self.start))
   }

   /// The function after this one in the image.
   #[must_use]
   pub fn next(&self) -> Option<Self> {
      let at = self
         .ix
         .functions
         .partition_point(|&(start, _)| start < self.end);
      let &(start, end) = self.ix.functions.get(at)?;
      Some(Self {
         ix: self.ix,
         start,
         end,
      })
   }

   /// The `n`th function this one calls.
   pub fn call(&self, n: usize) -> Found<Self> {
      let callee = *self
         .calls()
         .get(n)
         .ok_or_else(|| format!("{self:?} makes no call {n}"))?;
      self.ix.func(callee)
   }
}

/// A set of functions, sorted by start, each once.
pub struct Funcs<'ix> {
   list: Vec<Func<'ix>>,
}

impl<'ix> Funcs<'ix> {
   #[must_use]
   pub fn filter(self, keep: impl Fn(&Func<'ix>) -> bool) -> Self {
      Self {
         list: self.list.into_iter().filter(|func| keep(func)).collect(),
      }
   }

   /// The only function, or how many there were.
   pub fn one(&self, what: &str) -> Found<Func<'ix>> {
      one(what, &self.list)
   }

   /// Every function that calls one of these.
   #[must_use]
   pub fn callers(&self) -> Self {
      let Some(first) = self.list.first() else {
         return Self { list: Vec::new() };
      };
      first.ix.funcs(
         self
            .list
            .iter()
            .flat_map(|func| first.ix.call_sites(func.start)),
      )
   }

   /// These functions that also appear in `other`.
   #[must_use]
   pub fn and(self, other: &Self) -> Self {
      self.filter(|func| other.list.iter().any(|them| them.start == func.start))
   }

   #[must_use]
   pub const fn len(&self) -> usize {
      self.list.len()
   }

   #[must_use]
   pub const fn is_empty(&self) -> bool {
      self.list.is_empty()
   }

   pub fn iter(&self) -> impl Iterator<Item = &Func<'ix>> {
      self.list.iter()
   }
}

impl<'ix> IntoIterator for Funcs<'ix> {
   type IntoIter = std::vec::IntoIter<Func<'ix>>;
   type Item = Func<'ix>;

   fn into_iter(self) -> Self::IntoIter {
      self.list.into_iter()
   }
}
