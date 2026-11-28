//! Theme file helpers and cross-platform `--radius-scale` Scene hook.
//!
//! Color theming is handled natively through [`crate::mana_theme`] (Discord's
//! `SemanticColors` table). This module keeps the `gpui::Scene` draw hook
//! strictly for scaling `Quad::corner_radii` when a theme sets `--radius-scale`,
//! saving and restoring the original radii around `draw` so re-presented scenes
//! never compound-scale.

use std::path::{
   Path,
   PathBuf,
};

use phoenix_hook::image::Image;

/// Installs the cross-platform `--radius-scale` Scene hook during `prepare()`.
pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
   radius::install(image, ix);
}

#[must_use]
pub fn is_supported() -> bool {
   crate::mana_theme::is_supported()
}

/// Applies `.theme.css` text live. Returns whether it themes anything.
pub fn set_css(css: &str) -> bool {
   crate::mana_theme::set_css(css)
}

pub fn clear() {
   crate::mana_theme::forget_saved();
   crate::mana_theme::restore_persisted();
}

/// A `.theme.css` or `.theme.json` file in the themes folder.
pub struct ThemeFile {
   pub file:        String,
   pub name:        String,
   pub description: Option<String>,
   pub author:      Option<String>,
}

/// `*.theme.css` and `*.theme.json` files in `dir`, named by their metadata.
#[must_use]
pub fn list(dir: &Path) -> Vec<ThemeFile> {
   phoenix_themes::list_themes(dir)
      .into_iter()
      .map(|entry| {
         ThemeFile {
            file:        entry.file,
            name:        entry.meta.name,
            description: entry.meta.description,
            author:      entry.meta.author,
         }
      })
      .collect()
}

pub fn open_dir(dir: &Path) {
   let _ = std::fs::create_dir_all(dir);
   let opener = if cfg!(windows) {
      "explorer"
   } else if cfg!(target_os = "macos") {
      "open"
   } else {
      "xdg-open"
   };
   let _ = std::process::Command::new(opener)
      .arg(PathBuf::from(dir))
      .spawn();
}

mod radius {
   use std::sync::OnceLock;

   use phoenix_hook::image::Image;
   use phoenix_sigs::locate::mana_theme::{
      self,
      SceneDraw,
   };

   static SCENE: OnceLock<SceneDraw> = OnceLock::new();

   /// GPUI's `#[repr(C)]` shader structs, verified by `scene_draw`. Each
   /// `(radii, bounds)` pair names a `[f32; 4]` corner radii field and the
   /// `[x, y, w, h]` bounds that clamp it.
   struct Layout {
      size:  usize,
      radii: &'static [(usize, usize)],
   }

   const QUAD: Layout = Layout {
      size:  160,
      radii: &[(128, 8)],
   };
   /// `corner_radii` clips the blurred extent at `bounds`, and
   /// `shape_corner_radii` rounds the blurred silhouette at `shape_bounds`.
   const SHADOW: Layout = Layout {
      size:  112,
      radii: &[(24, 8), (88, 72)],
   };
   const MAX_PRIMITIVES: usize = 65_536;
   const MAX_RADIUS: f32 = 64.0;

   /// Restores every scaled radii field when `draw` returns (or unwinds), so
   /// re-presenting the same `Scene` never compounds `--radius-scale`.
   struct RadiiGuard {
      saved: Vec<(*mut u8, [f32; 4])>,
   }

   impl Drop for RadiiGuard {
      fn drop(&mut self) {
         for &(at, orig) in &self.saved {
            // SAFETY: `at` was validated as a live radii field inside one of
            // the scene's primitive lists for the duration of `draw`.
            unsafe {
               write4(std::slice::from_raw_parts_mut(at, 16), 0, orig);
            }
         }
      }
   }

   /// Reads a `Vec` in `scene` as `(cap, ptr, len)` with checked address
   /// arithmetic.
   ///
   /// # Safety
   ///
   /// `scene + at` must point to a live `Vec` (`cap`, `ptr`, `len`).
   const unsafe fn vec_at(scene: usize, at: usize) -> Option<(usize, usize, usize)> {
      let Some(addr) = scene.checked_add(at) else {
         return None;
      };
      if addr == 0 || !addr.is_multiple_of(std::mem::align_of::<usize>()) {
         return None;
      }
      // SAFETY: forwarded from the caller; Rust `Vec` layout is `cap`, `ptr`, `len`.
      unsafe {
         let words = addr as *const usize;
         Some((words.read(), words.add(1).read(), words.add(2).read()))
      }
   }

   /// Scales the radii of every primitive in the `Vec` at `scene + at`,
   /// recording the originals in `saved`.
   ///
   /// # Safety
   ///
   /// `scene + at` must be a live `Vec` of `layout` primitives not accessed
   /// concurrently.
   unsafe fn scale_list(
      scene: usize,
      at: usize,
      layout: &Layout,
      scale: f32,
      saved: &mut Vec<(*mut u8, [f32; 4])>,
   ) -> Option<()> {
      // SAFETY: forwarded from the caller.
      let (cap, ptr, len) = unsafe { vec_at(scene, at)? };
      if ptr == 0
         || len > cap
         || len > MAX_PRIMITIVES
         || !ptr.is_multiple_of(std::mem::align_of::<f32>())
      {
         return None;
      }
      let byte_len = len.checked_mul(layout.size)?;
      // SAFETY: `ptr` holds `len` live primitives of `layout.size` bytes.
      let items = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, byte_len) };
      for item in items.chunks_exact_mut(layout.size) {
         for &(radii, bounds) in layout.radii {
            let [_, _, w, h] = read4(item, bounds);
            let half_min = if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
               (0.5 * w.min(h)).min(MAX_RADIUS)
            } else {
               MAX_RADIUS
            };
            let orig = read4(item, radii);
            let scaled = orig.map(|r| {
               if r > 0.5 && r < 24.0 {
                  (r * scale).clamp(0.0, half_min)
               } else {
                  r
               }
            });
            if scaled.map(f32::to_bits) != orig.map(f32::to_bits) {
               saved.push((item[radii..].as_mut_ptr(), orig));
               write4(item, radii, scaled);
            }
         }
      }
      Some(())
   }

   /// Scales corner radii in `scene`'s quads and shadows when
   /// `--radius-scale` is active and not `1.0`, returning a [`RadiiGuard`]
   /// that restores the original values on drop.
   ///
   /// # Safety
   ///
   /// `scene` must be a live `&gpui::Scene` pointer not accessed concurrently.
   unsafe fn scale_scene_radii(scene: usize) -> Option<RadiiGuard> {
      let scale = crate::mana_theme::radius_scale()?;
      if !scale.is_finite() || (scale - 1.0).abs() <= 0.001 {
         return None;
      }
      let found = SCENE.get()?;
      if scene == 0 {
         return None;
      }
      // A panic must not unwind into the renderer.
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
         let mut saved = Vec::new();
         // SAFETY: `scene` is live for the duration of `draw`, and both
         // offsets were decoded from the renderer binary.
         unsafe {
            scale_list(scene, found.quads, &QUAD, scale, &mut saved);
            if let Some(shadows) = found.shadows {
               scale_list(scene, shadows, &SHADOW, scale, &mut saved);
            }
         }
         (!saved.is_empty()).then_some(RadiiGuard { saved })
      }))
      .ok()
      .flatten()
   }

   fn read4(buf: &[u8], at: usize) -> [f32; 4] {
      std::array::from_fn(|i| {
         f32::from_le_bytes(
            buf[at + 4 * i..at + 4 * i + 4]
               .try_into()
               .unwrap_or_default(),
         )
      })
   }

   fn write4(buf: &mut [u8], at: usize, v: [f32; 4]) {
      for (i, x) in v.iter().enumerate() {
         buf[at + 4 * i..at + 4 * i + 4].copy_from_slice(&x.to_le_bytes());
      }
   }

   /// Linux `WgpuRenderer::draw` takes all six `SysV` integer registers
   /// (`rdi..r9`) and returns a 16-byte `(rax, rdx)` pair (`Result`).
   #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
   mod abi {
      use std::sync::OnceLock;

      #[repr(C)]
      pub struct DrawRet(usize, usize);

      pub type DrawFn = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize) -> DrawRet;

      static ORIG_DRAW: OnceLock<DrawFn> = OnceLock::new();

      pub fn set_orig(trampoline: usize) {
         // SAFETY: `trampoline` runs `WgpuRenderer::draw`'s displaced prologue.
         let orig = unsafe { std::mem::transmute::<usize, DrawFn>(trampoline) };
         let _ = ORIG_DRAW.set(orig);
      }

      pub unsafe extern "C" fn draw_detour(
         a0: usize,
         a1: usize,
         a2: usize,
         a3: usize,
         a4: usize,
         a5: usize,
      ) -> DrawRet {
         let Some(orig) = ORIG_DRAW.get().copied() else {
            return DrawRet(0, 0);
         };
         // SAFETY: `a1` (`rsi`) is `&Scene`; `_guard` restores radii on exit.
         let _guard = unsafe { super::scale_scene_radii(a1) };
         // SAFETY: forwarding all six `SysV` register arguments to `orig`.
         unsafe { orig(a0, a1, a2, a3, a4, a5) }
      }
   }

   /// Windows `DirectXRenderer::draw` takes `(rcx, rdx, r8d)` and returns a
   /// pointer-sized `Result` in `rax`.
   #[cfg(all(target_arch = "x86_64", windows))]
   mod abi {
      use std::sync::OnceLock;

      pub type DrawFn = unsafe extern "C" fn(usize, usize, usize, usize) -> usize;

      static ORIG_DRAW: OnceLock<DrawFn> = OnceLock::new();

      pub fn set_orig(trampoline: usize) {
         // SAFETY: `trampoline` runs `DirectXRenderer::draw`'s displaced prologue.
         let orig = unsafe { std::mem::transmute::<usize, DrawFn>(trampoline) };
         let _ = ORIG_DRAW.set(orig);
      }

      pub unsafe extern "C" fn draw_detour(a0: usize, a1: usize, a2: usize, a3: usize) -> usize {
         let Some(orig) = ORIG_DRAW.get().copied() else {
            return 0;
         };
         // SAFETY: `a1` (`rdx`) is `&Scene`; `_guard` restores radii on exit.
         let _guard = unsafe { super::scale_scene_radii(a1) };
         // SAFETY: forwarding Win64 register arguments to `orig`.
         unsafe { orig(a0, a1, a2, a3) }
      }
   }

   /// macOS `MetalRenderer::draw(&mut self, scene: &Scene)` takes `(x0, x1)`
   /// and returns `()`.
   #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
   mod abi {
      use std::sync::OnceLock;

      pub type DrawFn = unsafe extern "C" fn(usize, usize);

      static ORIG_DRAW: OnceLock<DrawFn> = OnceLock::new();

      pub fn set_orig(trampoline: usize) {
         // SAFETY: `trampoline` runs `MetalRenderer::draw`'s displaced prologue.
         let orig = unsafe { std::mem::transmute::<usize, DrawFn>(trampoline) };
         let _ = ORIG_DRAW.set(orig);
      }

      pub unsafe extern "C" fn draw_detour(a0: usize, a1: usize) {
         let Some(orig) = ORIG_DRAW.get().copied() else {
            return;
         };
         // SAFETY: `a1` (`x1`) is `&Scene`; `_guard` restores radii on exit.
         let _guard = unsafe { super::scale_scene_radii(a1) };
         // SAFETY: forwarding `(renderer, scene)` to `orig`.
         unsafe { orig(a0, a1) };
      }
   }

   #[cfg(any(
      all(target_arch = "x86_64", any(windows, target_os = "linux")),
      all(target_os = "macos", target_arch = "aarch64")
   ))]
   pub(super) fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>) {
      let found = match crate::located::get("mana_theme::scene_draw", || mana_theme::scene_draw(ix))
      {
         Ok(found) => found,
         Err(err) => {
            log::warn!("desktop themes: radius hook disabled: {err}");
            return;
         },
      };
      // Publish `SCENE` before activating the hook.
      let _ = SCENE.set(found);
      // SAFETY: runs during `prepare` before Discord's render thread exists.
      match unsafe { image.hook_entry(found.draw, abi::draw_detour as *const () as usize) } {
         Ok(trampoline) => {
            abi::set_orig(trampoline);
            log::info!(
               "desktop themes: hooked renderer draw @ {:#x} for --radius-scale (quads +{:#x})",
               found.draw,
               found.quads
            );
         },
         Err(err) => log::warn!("desktop themes: hooking renderer draw: {err}"),
      }
   }

   #[cfg(not(any(
      all(target_arch = "x86_64", any(windows, target_os = "linux")),
      all(target_os = "macos", target_arch = "aarch64")
   )))]
   pub(super) fn install(_image: &Image<'static>, _ix: &crate::located::LazyIndex<'_>) {}
}
