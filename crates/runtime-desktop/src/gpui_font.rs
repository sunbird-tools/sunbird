//! Font overrides: Discord's own families swapped for installed ones.
//!
//! GPUI's `TextSystem::resolve_font(&self, font: &Font)` is hooked; on entry,
//! before its caches, a `gg sans` or `ABC Ginto` family becomes the main font
//! and `gg sans mono` the code font, so a change shows once text is laid out
//! again. Families the system can't find fall back to its UI font, as GPUI
//! does for any font.

use std::sync::{
   PoisonError,
   RwLock,
};

/// The main and code families, empty for Discord's own.
static FONTS: RwLock<(&'static str, &'static str)> = RwLock::new(("", ""));

/// Sets both families, `""` keeping Discord's.
pub fn set(main: &str, mono: &str) {
   let main = crate::gpui::intern_str(main.trim());
   let mono = crate::gpui::intern_str(mono.trim());
   *FONTS.write().unwrap_or_else(PoisonError::into_inner) = (main, mono);
}

pub fn clear() {
   set("", "");
}

pub fn is_supported() -> bool {
   hook::installed()
}

pub fn install(image: &phoenix_hook::image::Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   hook::install(image, ix);
}

/// Installed font families by the name GPUI's text system finds them by,
/// sorted.
pub fn families() -> Vec<String> {
   #[cfg(windows)]
   let mut out = dwrite::families();
   #[cfg(target_os = "macos")]
   let mut out = coretext::families();
   #[cfg(target_os = "linux")]
   let mut out = fontconfig::families();
   // Some fonts name their family with nothing printable.
   out.retain(|name| !name.trim().is_empty());
   out.sort_unstable_by_key(|name| name.to_lowercase());
   out.dedup();
   out
}

/// The system font collection's family names, the ones GPUI's DirectWrite
/// text system finds by name, through raw COM vtables.
#[cfg(windows)]
mod dwrite {
   type Com = *mut *const usize;

   #[repr(C)]
   struct Guid(u32, u16, u16, [u8; 8]);

   static IID_IDWRITE_FACTORY: Guid = Guid(0xB859_EE5A, 0xD838, 0x4B5B, [
      0xA2, 0xE8, 0x1A, 0xDC, 0x7D, 0x93, 0xDB, 0x48,
   ]);

   #[link(name = "dwrite")]
   unsafe extern "system" {
      fn DWriteCreateFactory(kind: u32, iid: *const Guid, factory: *mut Com) -> i32;
   }

   /// Method `index` of `this`'s vtable, as `F`.
   ///
   /// # Safety
   ///
   /// `this` is a live COM object whose method at `index` has the signature
   /// `F`.
   unsafe fn method<F: Copy>(this: Com, index: usize) -> F {
      // SAFETY: forwarded from the caller; a vtable slot is a function pointer.
      unsafe { std::mem::transmute_copy(&*(*this).add(index)) }
   }

   /// # Safety
   ///
   /// `this` is a live COM object the caller owns a reference to.
   unsafe fn release(this: Com) {
      // SAFETY: `IUnknown::Release` is slot 2 of every COM vtable.
      unsafe { method::<unsafe extern "system" fn(Com) -> u32>(this, 2)(this) };
   }

   /// The `en-us` name of `names`, or its first.
   ///
   /// # Safety
   ///
   /// `names` is a live `IDWriteLocalizedStrings`.
   unsafe fn english(names: Com) -> Option<String> {
      // SAFETY: slots 4, 7 and 8 are `FindLocaleName`, `GetStringLength` and
      // `GetString`, and every buffer holds what they write.
      unsafe {
         let find: unsafe extern "system" fn(Com, *const u16, *mut u32, *mut i32) -> i32 =
            method(names, 4);
         let len: unsafe extern "system" fn(Com, u32, *mut u32) -> i32 = method(names, 7);
         let get: unsafe extern "system" fn(Com, u32, *mut u16, u32) -> i32 = method(names, 8);
         let locale: Vec<u16> = "en-us".encode_utf16().chain([0]).collect();
         let (mut index, mut exists) = (0, 0);
         if find(names, locale.as_ptr(), &raw mut index, &raw mut exists) < 0 || exists == 0 {
            index = 0;
         }
         let mut n = 0;
         if len(names, index, &raw mut n) < 0 {
            return None;
         }
         let mut buf = vec![0u16; n as usize + 1];
         if get(names, index, buf.as_mut_ptr(), n + 1) < 0 {
            return None;
         }
         buf.truncate(n as usize);
         String::from_utf16(&buf).ok()
      }
   }

   /// Installed font families by their English name.
   pub(super) fn families() -> Vec<String> {
      let mut out = Vec::new();
      let mut factory: Com = std::ptr::null_mut();
      // SAFETY: a shared factory (kind 0) written to `factory` on success.
      if unsafe { DWriteCreateFactory(0, &raw const IID_IDWRITE_FACTORY, &raw mut factory) } < 0
         || factory.is_null()
      {
         return out;
      }
      // SAFETY: the slots are `IDWriteFactory::GetSystemFontCollection` (3),
      // `IDWriteFontCollection::GetFontFamilyCount` (3) and `GetFontFamily`
      // (4), and `IDWriteFontFamily::GetFamilyNames` (6); each object got is
      // released.
      unsafe {
         let collection_of: unsafe extern "system" fn(Com, *mut Com, i32) -> i32 =
            method(factory, 3);
         let mut collection: Com = std::ptr::null_mut();
         if collection_of(factory, &raw mut collection, 0) >= 0 && !collection.is_null() {
            let count: unsafe extern "system" fn(Com) -> u32 = method(collection, 3);
            let family_at: unsafe extern "system" fn(Com, u32, *mut Com) -> i32 =
               method(collection, 4);
            for i in 0..count(collection) {
               let mut family: Com = std::ptr::null_mut();
               if family_at(collection, i, &raw mut family) < 0 || family.is_null() {
                  continue;
               }
               let names_of: unsafe extern "system" fn(Com, *mut Com) -> i32 = method(family, 6);
               let mut names: Com = std::ptr::null_mut();
               if names_of(family, &raw mut names) >= 0 && !names.is_null() {
                  out.extend(english(names));
                  release(names);
               }
               release(family);
            }
            release(collection);
         }
         release(factory);
      }
      out
   }
}

/// CoreText's family names, the ones GPUI's text system finds by name.
#[cfg(target_os = "macos")]
mod coretext {
   use std::ffi::{
      c_char,
      c_void,
   };

   type CfRef = *const c_void;

   const UTF8: u32 = 0x0800_0100;

   #[link(name = "CoreText", kind = "framework")]
   unsafe extern "C" {
      fn CTFontManagerCopyAvailableFontFamilyNames() -> CfRef;
   }

   #[link(name = "CoreFoundation", kind = "framework")]
   unsafe extern "C" {
      fn CFArrayGetCount(array: CfRef) -> isize;
      fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
      fn CFStringGetLength(string: CfRef) -> isize;
      fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
      fn CFStringGetCString(string: CfRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
      fn CFRelease(object: CfRef);
   }

   /// # Safety
   ///
   /// `string` is a live `CFString`.
   unsafe fn text(string: CfRef) -> Option<String> {
      // SAFETY: `buf` holds the most UTF-8 bytes `string` can take plus the NUL
      // `CFStringGetCString` writes.
      unsafe {
         let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(string), UTF8) + 1;
         let mut buf = vec![0u8; usize::try_from(size).ok()?];
         if CFStringGetCString(string, buf.as_mut_ptr().cast(), size, UTF8) == 0 {
            return None;
         }
         let len = buf.iter().position(|&byte| byte == 0)?;
         buf.truncate(len);
         String::from_utf8(buf).ok()
      }
   }

   pub(super) fn families() -> Vec<String> {
      // SAFETY: the copied array is a live `CFArray` of `CFString`s, released
      // once read.
      unsafe {
         let names = CTFontManagerCopyAvailableFontFamilyNames();
         if names.is_null() {
            return Vec::new();
         }
         let out = (0..CFArrayGetCount(names))
            .filter_map(|index| text(CFArrayGetValueAtIndex(names, index)))
            // Names starting with a dot are the system's hidden UI fonts.
            .filter(|name| !name.starts_with('.'))
            .collect();
         CFRelease(names);
         out
      }
   }
}

/// fontconfig's family names, loaded with `dlopen` so a system without it
/// only lists nothing.
#[cfg(target_os = "linux")]
mod fontconfig {
   use std::ffi::{
      CStr,
      c_char,
      c_int,
      c_void,
   };

   use crate::fetch::curl::sym;

   type Pattern = *mut c_void;
   type ObjectSet = *mut c_void;

   #[repr(C)]
   struct FontSet {
      nfont: c_int,
      _size: c_int,
      fonts: *const Pattern,
   }

   const FAMILY: &CStr = c"family";
   const MATCH: c_int = 0;

   pub(super) fn families() -> Vec<String> {
      // SAFETY: each symbol is read with its type from `fontconfig.h`, and
      // every object created is destroyed. The library stays loaded.
      unsafe {
         let lib = libc::dlopen(c"libfontconfig.so.1".as_ptr(), libc::RTLD_NOW);
         if lib.is_null() {
            log::warn!("desktop fonts: libfontconfig.so.1 is not installed, no fonts listed");
            return Vec::new();
         }
         let Some(names) = (|| {
            let pattern_create: unsafe extern "C" fn() -> Pattern = sym(lib, c"FcPatternCreate")?;
            let pattern_destroy: unsafe extern "C" fn(Pattern) = sym(lib, c"FcPatternDestroy")?;
            let set_create: unsafe extern "C" fn() -> ObjectSet = sym(lib, c"FcObjectSetCreate")?;
            let set_add: unsafe extern "C" fn(ObjectSet, *const c_char) -> c_int =
               sym(lib, c"FcObjectSetAdd")?;
            let set_destroy: unsafe extern "C" fn(ObjectSet) = sym(lib, c"FcObjectSetDestroy")?;
            let list: unsafe extern "C" fn(*mut c_void, Pattern, ObjectSet) -> *mut FontSet =
               sym(lib, c"FcFontList")?;
            let list_destroy: unsafe extern "C" fn(*mut FontSet) = sym(lib, c"FcFontSetDestroy")?;
            let get: unsafe extern "C" fn(
               Pattern,
               *const c_char,
               c_int,
               *mut *const c_char,
            ) -> c_int = sym(lib, c"FcPatternGetString")?;

            let pattern = pattern_create();
            let objects = set_create();
            set_add(objects, FAMILY.as_ptr());
            // A null config lists the current one, loading it on first use.
            let fonts = list(std::ptr::null_mut(), pattern, objects);
            set_destroy(objects);
            pattern_destroy(pattern);
            if fonts.is_null() {
               return Some(Vec::new());
            }
            let count = usize::try_from((*fonts).nfont).unwrap_or(0);
            let patterns = if count == 0 || (*fonts).fonts.is_null() {
               &[][..]
            } else {
               std::slice::from_raw_parts((*fonts).fonts, count)
            };
            let names = patterns
               .iter()
               .filter_map(|&font| {
                  let mut name = std::ptr::null();
                  // The first family is the font's own, later ones are
                  // localized.
                  (get(font, FAMILY.as_ptr(), 0, &raw mut name) == MATCH && !name.is_null())
                     .then(|| CStr::from_ptr(name).to_string_lossy().into_owned())
               })
               .collect();
            list_destroy(fonts);
            Some(names)
         })() else {
            log::warn!("desktop fonts: libfontconfig.so.1 lacks a listing symbol");
            return Vec::new();
         };
         names
      }
   }
}

mod hook {
   use std::sync::{
      OnceLock,
      PoisonError,
   };

   use phoenix_hook::image::Image;
   use phoenix_sigs::locate;

   use super::FONTS;
   use crate::gpui::SharedString;

   /// `TextSystem::resolve_font(&self, &Font) -> FontId`.
   type ResolveFn = unsafe extern "C" fn(*const u8, *mut SharedString) -> usize;

   static ORIGINAL: OnceLock<ResolveFn> = OnceLock::new();

   #[derive(Clone, Copy)]
   enum Role {
      Main,
      Mono,
   }

   impl Role {
      fn of(family: &str) -> Option<Self> {
         if family.starts_with("gg sans mono") {
            Some(Self::Mono)
         } else if family.starts_with("gg sans") || family.starts_with("ABC Ginto") {
            Some(Self::Main)
         } else {
            None
         }
      }
   }

   pub(super) fn installed() -> bool {
      ORIGINAL.get().is_some()
   }

   pub(super) fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
      let func = match crate::located::get("locate::resolve_font", || {
         locate::resolve_font(ix).map(|func| func.start)
      }) {
         Ok(func) => func,
         Err(err) => {
            log::warn!("desktop fonts: {err}, fonts disabled");
            return;
         },
      };
      // SAFETY: `resolve_font` found a function start, `prepare` runs before
      // Discord's render thread exists, and the detour has its ABI.
      if let Some(original) = unsafe {
         crate::hook::<ResolveFn>(
            image,
            func,
            on_resolve_font as *const () as usize,
            "desktop fonts",
         )
      } {
         let _ = ORIGINAL.set(original);
         log::info!("desktop fonts: hooked resolve_font @ {func:#x}");
      }
   }

   /// The family's text, borrowed from `family`.
   ///
   /// # Safety
   ///
   /// `family` is a live `SharedString`.
   unsafe fn text(family: &SharedString) -> Option<&str> {
      // `SharedString` tags, as [`SharedString`] documents them. Below `STATIC`
      // the tag is the length of text stored inline after it.
      let (bytes, len) = match family.tag {
         // `gg sans` and its siblings are short enough to sit inline.
         len @ 0..SharedString::STATIC => {
            let inline = std::ptr::from_ref(family).cast::<u8>().wrapping_add(1);
            (inline, len.into())
         },
         SharedString::STATIC => (family.ptr, family.len),
         // SAFETY: an `Arc<str>`'s bytes follow its two counts.
         SharedString::ARC => (unsafe { family.ptr.add(16) }, family.len),
         _ => return None,
      };
      if bytes.is_null() || len > 256 {
         return None;
      }
      // SAFETY: the string's own bytes, live as long as `family`.
      std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) }).ok()
   }

   /// `resolve_font(self, font)`: rewrites `font.family`, its first field.
   extern "C" fn on_resolve_font(system: *const u8, family: *mut SharedString) -> usize {
      let fonts = *FONTS.read().unwrap_or_else(PoisonError::into_inner);
      // A panic must not unwind into the text system.
      let _ = std::panic::catch_unwind(|| {
         if fonts == ("", "") || family.is_null() {
            return;
         }
         // SAFETY: `resolve_font` received a live `&Font` whose `family` starts
         // it.
         let Some(role) = (unsafe { text(&*family) }).and_then(Role::of) else {
            return;
         };
         let to = match role {
            Role::Main => fonts.0,
            Role::Mono => fonts.1,
         };
         if to.is_empty() {
            return;
         }
         // SAFETY: the caller's `Font` outlives the call, and a static string
         // needs no drop. An `Arc` family overwritten keeps one count it never
         // gives back, so its bytes live on, bounded by Discord's few families.
         unsafe { family.write(SharedString::from_static(to)) };
      });
      let original = *ORIGINAL.wait();
      // SAFETY: the trampoline runs the original with Discord's arguments.
      unsafe { original(system, family) }
   }
}
