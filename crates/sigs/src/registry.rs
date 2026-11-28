use std::collections::{
   BTreeMap,
   HashMap,
   HashSet,
};

use phoenix_hook::image::Image;

use crate::{
   Found,
   GATEWAY_HANDLER_FIELD,
   index::{
      Isa,
      Op,
      Reg,
      Value,
      op,
   },
};

/// Every Phoenix build has this event, so its entry seeds the walk.
const SEED: &str = "MESSAGE_CREATE";
/// Seeds the walk over the session/domain reducers (`READY_SUPPLEMENTAL`,
/// `RESUMED`, etc.) whose slice sits separately from the main registry or
/// domain table.
const DOMAIN_SEED: &str = "READY_SUPPLEMENTAL";
/// An event every client handles. macOS Phoenix keeps it, with others, in a
/// flat table apart from the domain tables, which is walked only when the
/// walks above miss it.
const SPLIT_SEED: &str = "MESSAGE_UPDATE";
const STRIDE: usize = 3 * size_of::<usize>();
/// Stride of a `libdiscord` domain reducer descriptor:
/// `{domain_name_ptr, domain_name_len, domain_id, slice_ptr, slice_len}`.
const DOMAIN_STRIDE: usize = 5 * size_of::<usize>();
const MAX_NAME: usize = 64;

#[derive(Debug, flub::Error)]
pub enum Error {
   #[error("no gateway table holds {seed}")]
   NotFound { seed: &'static str },
   #[error("{} gateway tables hold {seed}", .slots.len())]
   Ambiguous {
      seed:  &'static str,
      slots: Vec<usize>,
   },
}

/// A gateway event that exists in the loaded target. Only
/// [`GatewayRegistry`] makes these, so holding one proves the event is live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId(usize);

impl EventId {
   /// Position in [`GatewayRegistry::iter`] order.
   #[must_use]
   pub const fn index(self) -> usize {
      self.0
   }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GatewayEntry {
   pub name:    String,
   /// All registry table slots holding this event's `{name_ptr, name_len,
   /// handler}` record. Android has one slot per event; desktop builds can
   /// emit duplicate copies of the static domain table across codegen units.
   pub slots:   Vec<usize>,
   pub handler: usize,
}

/// The target's gateway registry, walked from [`SEED`].
///
/// The walk crosses either a flat `{name_ptr, name_len, handler}` table
/// (Android) or `libdiscord`'s 40-byte per-domain descriptor table (Desktop),
/// plus any separate [`DOMAIN_SEED`] slice (`READY_SUPPLEMENTAL`, etc.).
#[derive(Default)]
pub struct GatewayRegistry {
   entries: Vec<GatewayEntry>,
   by_name: HashMap<String, EventId>,
}

impl GatewayRegistry {
   pub fn locate(image: &Image<'_>) -> Result<Self, Error> {
      let mut entries = locate_primary(image)?;
      match walk_flat(image, DOMAIN_SEED) {
         Ok(domain) => merge_entries(&mut entries, domain),
         Err(Error::NotFound { .. }) => {},
         Err(err) => return Err(err),
      }

      if !entries.iter().any(|entry| entry.name == SPLIT_SEED) {
         for split in walk_split(image) {
            // macOS dispatches MESSAGE_CREATE through the split table, whose
            // handler differs from the domain table's, which never runs.
            entries.retain(|known| {
               !split
                  .iter()
                  .any(|entry| entry.name == known.name && entry.handler != known.handler)
            });
            merge_entries(&mut entries, split);
         }
      }

      Ok(Self::from_entries(entries))
   }

   /// The registry [`Self::entries`] came from, on the same build.
   #[must_use]
   pub fn from_entries(entries: Vec<GatewayEntry>) -> Self {
      let by_name = entries
         .iter()
         .enumerate()
         .map(|(index, entry)| (entry.name.clone(), EventId(index)))
         .collect();
      Self { entries, by_name }
   }

   #[must_use]
   pub fn entries(&self) -> &[GatewayEntry] {
      &self.entries
   }

   #[must_use]
   pub fn id(&self, name: &str) -> Option<EventId> {
      self.by_name.get(name).copied()
   }

   /// # Panics
   ///
   /// When `id` came from another registry, which one process never has.
   #[must_use]
   pub fn entry(&self, id: EventId) -> &GatewayEntry {
      &self.entries[id.0]
   }

   pub fn iter(&self) -> impl Iterator<Item = (EventId, &GatewayEntry)> {
      self
         .entries
         .iter()
         .enumerate()
         .map(|(index, entry)| (EventId(index), entry))
   }

   /// The first word of a handler's `Ok`, the constant most handlers store
   /// through the hidden return pointer. Its niche moves between builds, and
   /// an `Ok` Phoenix reads as `Err` crashes it.
   pub fn ok_word(&self, image: &Image<'_>, isa: Isa) -> Found<u64> {
      let functions = image.functions();
      let mut votes = BTreeMap::<u64, usize>::new();
      let handlers = self
         .entries
         .iter()
         .map(|entry| {
            // Thumb handler pointers carry bit 0.
            if matches!(isa, Isa::Thumb2) { entry.handler & !1 } else { entry.handler }
         })
         .collect::<HashSet<usize>>();
      for handler in handlers {
         let at = functions.partition_point(|&(start, _)| start <= handler);
         let Some(&(start, end)) = at.checked_sub(1).map(|at| &functions[at]) else {
            continue;
         };
         if start != handler {
            continue;
         }
         for word in stored_results(&op::decode(image, isa, start, end), isa) {
            *votes.entry(word).or_default() += 1;
         }
      }
      let mut ranked = votes.into_iter().collect::<Vec<(u64, usize)>>();
      ranked.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
      match ranked[..] {
         [] => Err("no handler stores a constant result".to_owned()),
         [(word, count), (other, next), ..] if count < 2 * next => Err(format!(
            "handler results split between {word:#x} ({count}) and {other:#x} ({next})"
         )),
         [(word, _), ..] => Ok(word),
      }
   }
}

/// The distinct pointer-sized constants `steps` store at offset 0 of the value
/// the hidden return pointer points at, following the pointer through moves
/// and stack spills.
fn stored_results(steps: &[op::Step], isa: Isa) -> HashSet<u64> {
   let width = if matches!(isa, Isa::Thumb2) { 4 } else { 8 };
   let sp = isa.sp();
   let mut sret = HashSet::from([isa.sret()]);
   let mut spilled = HashSet::<isize>::new();
   let mut imms = HashMap::<Reg, u64>::new();
   let mut words = HashSet::new();
   let forget = |reg: Reg, sret: &mut HashSet<Reg>, imms: &mut HashMap<Reg, u64>| {
      sret.remove(&reg);
      imms.remove(&reg);
   };
   for step in steps {
      match step.op {
         Op::Imm { reg, value } => {
            sret.remove(&reg);
            imms.insert(reg, value);
         },
         Op::Move { dst, src } => {
            let (is_sret, imm) = (sret.contains(&src), imms.get(&src).copied());
            forget(dst, &mut sret, &mut imms);
            if is_sret {
               sret.insert(dst);
            }
            if let Some(imm) = imm {
               imms.insert(dst, imm);
            }
         },
         Op::Store {
            value: Value::Reg(reg),
            base,
            offset,
            size,
         } if base == sp && size == width => {
            if sret.contains(&reg) {
               spilled.insert(offset);
            } else {
               spilled.remove(&offset);
            }
         },
         Op::Store {
            value,
            base,
            offset: 0,
            size,
         } if size == width && sret.contains(&base) => {
            let word = match value {
               Value::Imm(word) => Some(word),
               Value::Reg(reg) => imms.get(&reg).copied(),
            };
            words.extend(word);
         },
         Op::StorePair {
            values: [first, _],
            base,
            offset: 0,
            size,
         } if size == width && sret.contains(&base) => words.extend(imms.get(&first).copied()),
         Op::Load {
            reg,
            base,
            offset,
            size,
         } => {
            forget(reg, &mut sret, &mut imms);
            if base == sp && size == width && spilled.contains(&offset) {
               sret.insert(reg);
            }
         },
         Op::Offset { reg, .. } => forget(reg, &mut sret, &mut imms),
         Op::LoadPair { regs, .. } => {
            for reg in regs {
               forget(reg, &mut sret, &mut imms);
            }
         },
         Op::Call(_) => {
            let clobbered = isa.caller_saved();
            sret.retain(|&reg| clobbered & (1 << reg) == 0);
            imms.retain(|&reg, _| clobbered & (1 << reg) == 0);
         },
         _ => {},
      }
   }
   words
}

/// Finds the primary event table(s) containing [`SEED`]. Supports both a
/// single flat table (Android) and one or more per-domain descriptor tables
/// (Desktop `libdiscord`).
fn locate_primary(image: &Image<'_>) -> Result<Vec<GatewayEntry>, Error> {
   let mut seeds = table_entries(image, SEED, GATEWAY_HANDLER_FIELD);
   seeds.sort_unstable();
   seeds.dedup();
   if seeds.is_empty() {
      return Err(Error::NotFound { seed: SEED });
   }

   // Try walking domain descriptor tables anchored at each seed slice first.
   let mut domain_entries = Vec::new();
   let mut all_domains = true;
   for &(slot, _) in &seeds {
      let slice_start = walk_slice_start(image, slot);
      let Some(found) = walk_domains_from_slice(image, slice_start) else {
         all_domains = false;
         break;
      };
      merge_entries(&mut domain_entries, found);
   }
   if all_domains && !domain_entries.is_empty() {
      return Ok(domain_entries);
   }

   match seeds.as_slice() {
      [(slot, _)] => Ok(walk_from_slot(image, *slot)),
      _ => {
         Err(Error::Ambiguous {
            seed:  SEED,
            slots: seeds.iter().map(|(slot, _)| *slot).collect(),
         })
      },
   }
}

fn merge_entries(target: &mut Vec<GatewayEntry>, incoming: Vec<GatewayEntry>) {
   for entry in incoming {
      if let Some(existing) = target.iter_mut().find(|known| known.name == entry.name) {
         if existing.handler == entry.handler {
            for slot in entry.slots {
               if !existing.slots.contains(&slot) {
                  existing.slots.push(slot);
               }
            }
         }
      } else {
         target.push(entry);
      }
   }
}

fn entry(image: &Image<'_>, slot: usize) -> Option<GatewayEntry> {
   let ptr_size = image.pointer_width;
   let name = image.read_usize(slot)?;
   let len = image.read_usize(slot + ptr_size)?;
   let handler = image.read_usize(slot + 2 * ptr_size)?;
   if !(1..=MAX_NAME).contains(&len) || !image.is_function(handler) {
      return None;
   }
   let name = image.read(name, len)?;
   if !name
      .iter()
      .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
   {
      return None;
   }
   Some(GatewayEntry {
      name: String::from_utf8(name.to_vec()).ok()?,
      slots: vec![slot],
      handler,
   })
}

fn walk_slice_start(image: &Image<'_>, start: usize) -> usize {
   let stride = 3 * image.pointer_width;
   let mut first = start;
   while let Some(previous) = first.checked_sub(stride)
      && entry(image, previous).is_some()
   {
      first = previous;
   }
   first
}

fn walk_from_slot(image: &Image<'_>, start: usize) -> Vec<GatewayEntry> {
   let stride = 3 * image.pointer_width;
   let first = walk_slice_start(image, start);
   (0..)
      .map(|index| first + index * stride)
      .map_while(|slot| entry(image, slot))
      .collect()
}

fn walk_flat(image: &Image<'_>, seed: &'static str) -> Result<Vec<GatewayEntry>, Error> {
   let mut seeds = table_entries(image, seed, 2 * image.pointer_width);
   seeds.sort_unstable();
   seeds.dedup();
   match seeds.as_slice() {
      [(slot, _)] => Ok(walk_from_slot(image, *slot)),
      [] => Err(Error::NotFound { seed }),
      _ => {
         Err(Error::Ambiguous {
            seed,
            slots: seeds.iter().map(|(slot, _)| *slot).collect(),
         })
      },
   }
}

/// Every flat table holding [`SPLIT_SEED`]. macOS keeps two copies of it,
/// each naming the event through its own copy of the string, and dispatches
/// `MESSAGE_DELETE` through the second.
fn walk_split(image: &Image<'_>) -> Vec<Vec<GatewayEntry>> {
   let ptr_size = image.pointer_width;
   let mut slots = image
      .find_bytes_iter(SPLIT_SEED.as_bytes())
      .flat_map(|string| image.pointers_to(string))
      .filter(|&slot| {
         image.read_usize(slot + ptr_size) == Some(SPLIT_SEED.len())
            && image
               .read_usize(slot + 2 * ptr_size)
               .is_some_and(|code| image.is_function(code))
      })
      .collect::<Vec<_>>();
   slots.sort_unstable();
   slots.dedup();
   slots
      .into_iter()
      .map(|slot| walk_from_slot(image, slot))
      .collect()
}

/// Checks whether `slice_start` is referenced by a 5-word `libdiscord` domain
/// descriptor `{name_ptr, name_len, domain_id, slice_ptr, slice_len}` and, if
/// so, walks the entire contiguous domain table.
fn walk_domains_from_slice(image: &Image<'_>, slice_start: usize) -> Option<Vec<GatewayEntry>> {
   let ptr_size = image.pointer_width;
   let domain_stride = 5 * ptr_size;
   let slice_field = 3 * ptr_size;

   let anchor = image
      .pointers_to(slice_start)
      .into_iter()
      .filter_map(|ptr_slot| ptr_slot.checked_sub(slice_field))
      .find(|&desc| domain_entries(image, desc).is_some())?;

   let mut first = anchor;
   while let Some(prev) = first.checked_sub(domain_stride)
      && domain_entries(image, prev).is_some()
   {
      first = prev;
   }

   let mut out = Vec::new();
   let mut domains = 0usize;
   let mut cur = first;
   while let Some(slice_entries) = domain_entries(image, cur) {
      domains += 1;
      merge_entries(&mut out, slice_entries);
      cur += domain_stride;
   }
   (domains >= 2 && !out.is_empty()).then_some(out)
}

/// Parses one `libdiscord` domain descriptor at `addr`:
/// `{name_ptr, name_len, domain_id, slice_ptr, slice_len}`.
fn domain_entries(image: &Image<'_>, addr: usize) -> Option<Vec<GatewayEntry>> {
   let ptr_size = image.pointer_width;
   let name_ptr = image.read_usize(addr)?;
   let name_len = image.read_usize(addr + ptr_size)?;
   let domain_id = image.read_usize(addr + 2 * ptr_size)?;
   let slice_ptr = image.read_usize(addr + 3 * ptr_size)?;
   let slice_len = image.read_usize(addr + 4 * ptr_size)?;

   if !(1..=MAX_NAME).contains(&name_len) || domain_id > 1024 || slice_len > 256 {
      return None;
   }
   let name = image.read(name_ptr, name_len)?;
   if !name
      .iter()
      .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
   {
      return None;
   }
   let stride = 3 * ptr_size;
   let mut out = Vec::with_capacity(slice_len);
   for index in 0..slice_len {
      out.push(entry(image, slice_ptr + index * stride)?);
   }
   Some(out)
}

/// `(slot, code)` for every `{name ptr, name len, ...}` entry named `key` whose
/// pointer `field` bytes in starts a function.
fn table_entries(image: &Image<'_>, key: &str, field: usize) -> Vec<(usize, usize)> {
   let ptr_size = image.pointer_width;
   let field = if field == GATEWAY_HANDLER_FIELD {
      2 * ptr_size
   } else {
      field
   };
   let mut out = Vec::new();
   for string in image.find_bytes_iter(key.as_bytes()) {
      for slot in image.pointers_to(string) {
         if image.read_usize(slot + ptr_size) != Some(key.len()) {
            continue;
         }
         let Some(code) = image.read_usize(slot + field) else {
            continue;
         };
         if image.is_function(code) {
            out.push((slot, code));
         }
      }
      if !out.is_empty() {
         break;
      }
   }
   // Keep compile-time constants referenced on 64-bit targets.
   let _ = (STRIDE, DOMAIN_STRIDE);
   out
}
