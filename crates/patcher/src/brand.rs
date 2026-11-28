//! Sunbird launcher artwork replacing the alternate icons Discord ships.

struct Icon {
   stem:    &'static str,
   preview: &'static str,
   png:     &'static [u8],
   webp:    &'static [u8],
}

const PREVIEW_DIR: &str = "res/drawable-xxhdpi-v4/images_native_custom_app_icons_";
const LAUNCHER_DIR: &str = "res/mipmap-xxxhdpi-v4/";

const ICONS: &[Icon] = &[
   Icon {
      stem:    "hexagon",
      preview: "hexagonicon_hexagonicon",
      png:     include_bytes!("../brand/icons/gear-gta-vi.png"),
      webp:    include_bytes!("../brand/icons/gear-gta-vi.webp"),
   },
   Icon {
      stem:    "brand_inverted",
      preview: "brandinvertedicon_brandinvertedicon",
      png:     include_bytes!("../brand/icons/gear-inverse.png"),
      webp:    include_bytes!("../brand/icons/gear-inverse.webp"),
   },
   Icon {
      stem:    "matte_dark",
      preview: "mattedarkicon_mattedarkicon",
      png:     include_bytes!("../brand/icons/gear-mono-dark.png"),
      webp:    include_bytes!("../brand/icons/gear-mono-dark.webp"),
   },
   Icon {
      stem:    "matte_light",
      preview: "mattelighticon_mattelighticon",
      png:     include_bytes!("../brand/icons/gear-mono-light.png"),
      webp:    include_bytes!("../brand/icons/gear-mono-light.webp"),
   },
   Icon {
      stem:    "pastel",
      preview: "pastelicon_pastelicon",
      png:     include_bytes!("../brand/icons/gear-classic.png"),
      webp:    include_bytes!("../brand/icons/gear-classic.webp"),
   },
   Icon {
      stem:    "midnight_prism",
      preview: "midnightprismicon_midnightprismicon",
      png:     include_bytes!("../brand/icons/gear-deep.png"),
      webp:    include_bytes!("../brand/icons/gear-deep.webp"),
   },
   Icon {
      stem:    "sunset",
      preview: "sunseticon_sunseticon",
      png:     include_bytes!("../brand/icons/sun-gta-vi.png"),
      webp:    include_bytes!("../brand/icons/sun-gta-vi.webp"),
   },
   Icon {
      stem:    "color_wave",
      preview: "colorwaveicon_colorwaveicon",
      png:     include_bytes!("../brand/icons/sun-classic.png"),
      webp:    include_bytes!("../brand/icons/sun-classic.webp"),
   },
   Icon {
      stem:    "in_rainbows",
      preview: "inrainbowsicon_inrainbowsicon",
      png:     include_bytes!("../brand/icons/sun-inverse.png"),
      webp:    include_bytes!("../brand/icons/sun-inverse.webp"),
   },
   Icon {
      stem:    "blurple_twilight",
      preview: "blurpletwilighticon_blurpletwilighticon",
      png:     include_bytes!("../brand/icons/sun-deep.png"),
      webp:    include_bytes!("../brand/icons/sun-deep.webp"),
   },
   Icon {
      stem:    "holowaves",
      preview: "holowavesicon_holowavesicon",
      png:     include_bytes!("../brand/icons/sun-mono-dark.png"),
      webp:    include_bytes!("../brand/icons/sun-mono-dark.webp"),
   },
   Icon {
      stem:    "circuit",
      preview: "circuiticon_circuiticon",
      png:     include_bytes!("../brand/icons/sun-mono-light.png"),
      webp:    include_bytes!("../brand/icons/sun-mono-light.webp"),
   },
];

/// The replacement for a resource Discord ships, if Sunbird provides one.
pub fn replacement(name: &str) -> Option<&'static [u8]> {
   for icon in ICONS {
      if name
         .strip_prefix(PREVIEW_DIR)
         .and_then(|file| file.strip_suffix(".png"))
         == Some(icon.preview)
      {
         return Some(icon.png);
      }
      if let Some(file) = name.strip_prefix(LAUNCHER_DIR)
         && let Some(rest) = file.strip_prefix(icon.stem)
         && matches!(rest, ".webp" | "_foreground.webp" | "_round.webp")
      {
         return Some(icon.webp);
      }
   }
   None
}
