//! Locators written once per source tree on [`Index`] queries. Each runs on
//! every binary of its [`Family`], whatever the ISA.

#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod call_idle;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod channel;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod chat;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod clickable;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod commands;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod composer;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod counts;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod dismiss_stack;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod message;
pub mod menu;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod ready;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod settings;
pub mod spotify;
pub mod staff;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod sticker;

#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod forward;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod gpui;
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub mod mana_theme;

use crate::{
   Found,
   index::{
      Func,
      Index,
      Op,
      Step,
   },
};

/// One instruction's bytes as Discord ships them and as patched. An x86 patch
/// is one byte and an arm64 one a whole aligned instruction, so a thread at
/// `site` runs the old or the new instruction whole.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodePatch {
   pub site:    usize,
   pub len:     usize,
   pub shipped: [u8; 4],
   pub patched: [u8; 4],
}

impl CodePatch {
   pub(crate) fn at(ix: &Index<'_>, site: usize, patched: &[u8]) -> Found<Self> {
      let len = patched.len();
      let shipped = ix
         .image
         .read(site, len)
         .ok_or_else(|| format!("{site:#x} is unmapped"))?;
      let mut patch = Self {
         site,
         len,
         shipped: [0; 4],
         patched: [0; 4],
      };
      patch.shipped[..len].copy_from_slice(shipped);
      patch.patched[..len].copy_from_slice(patched);
      Ok(patch)
   }
}

/// Binaries built from one Rust tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
   /// `libdiscord/desktop`: Windows, Linux and macOS Desktop Phoenix.
   Desktop,
   /// `libdiscord/crates` behind the wasm host: Android and iOS.
   Mobile,
}

pub mod mobile;

const DESKTOP: &[Family] = &[Family::Desktop];
const ALL: &[Family] = &[Family::Desktop, Family::Mobile];

/// A locator, the families it reads, and what it found as text.
pub struct Locator {
   pub name:     &'static str,
   pub families: &'static [Family],
   pub run:      for<'ix> fn(&'ix Index<'ix>) -> Found<String>,
}

pub const LOCATORS: &[Locator] = &[
   Locator {
      name:     "access gate",
      families: DESKTOP,
      run:      |ix| access_gate(ix).map(|func| format!("{func:?}")),
   },
   Locator {
      name:     "media fetch",
      families: DESKTOP,
      run:      |ix| media_fetch(ix).map(|func| format!("{func:?}")),
   },
   Locator {
      name:     "spotify auto pause",
      families: DESKTOP,
      run:      |ix| spotify::auto_pause(ix).map(|patch| format!("{patch:x?}")),
   },
   Locator {
      name:     "spotify idle activity",
      families: DESKTOP,
      run:      |ix| spotify::idle_activity(ix).map(|patch| format!("{patch:x?}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "gpui builders",
      families: DESKTOP,
      run:      |ix| gpui::gpui(ix).map(|found| format!("{found:x?}")),
   },
   #[cfg(any(target_arch = "x86_64", target_os = "macos"))]
   Locator {
      name:     "composer buttons",
      families: DESKTOP,
      run:      |ix| composer::composer_buttons(ix).map(|func| format!("{func:#x}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "sticker send",
      families: DESKTOP,
      run:      |ix| sticker::sticker_send(ix).and_then(|func| hookable(ix, func)),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "sticker gates",
      families: DESKTOP,
      run:      |ix| sticker::locked_gates(ix).map(|patches| format!("{patches:x?}")),
   },
   Locator {
      name:     "menu submenu",
      families: DESKTOP,
      run:      |ix| menu::submenu(ix).map(|layout| format!("{layout:x?}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "pane commands",
      families: DESKTOP,
      run:      |ix| chat::pane_commands(ix).map(|found| format!("{found:x?}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "clickable",
      families: DESKTOP,
      run:      |ix| clickable::clickable(ix).map(|found| format!("{found:x?}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "commands",
      families: DESKTOP,
      run:      |ix| commands::commands(ix).map(|found| format!("{found:x?}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "client new",
      families: DESKTOP,
      run:      |ix| commands::client_new(ix).map(|func| format!("{func:#x}")),
   },
   Locator {
      name:     "staff flag",
      families: ALL,
      run:      |ix| staff::staff(ix).map(|staff| staff.to_string()),
   },
   Locator {
      name:     "emoji check",
      families: ALL,
      run:      |ix| {
         emoji_check(ix)
            .map(|check| format!("{:#x}, intention +{:#x}", check.func, check.intention))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "message row",
      families: DESKTOP,
      run:      |ix| message::message_row(ix).map(|row| format!("{row:x?}")),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "message body",
      families: DESKTOP,
      run:      |ix| {
         message::message_row(ix)
            .and_then(|row| message::message_body(ix, row.func))
            .map(|body| format!("{body:x?}"))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "settings label",
      families: DESKTOP,
      run:      |ix| {
         settings::settings_label(ix).map(|label| {
            format!(
               "{:#x} (id {:#x}, suffix {:#x})",
               label.name, label.id, label.suffix
            )
         })
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "settings visibility",
      families: DESKTOP,
      run:      |ix| {
         settings::settings_visibility(ix).map(|guards| {
            guards
               .iter()
               .map(|guard| format!("{:#x} (imm8 {:#x})", guard.cmp, guard.imm8()))
               .collect::<Vec<_>>()
               .join(", ")
         })
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "settings section",
      families: DESKTOP,
      run:      |ix| {
         settings::settings_section(ix)
            .map(|section| format!("{:#x} (page +{:#x})", section.func, section.page_offset))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "settings sidebar rows",
      families: DESKTOP,
      run:      |ix| {
         let section = settings::settings_section(ix)?;
         let rows = settings::settings_sidebar_rows(ix, section.page_offset)?;
         Ok(format!(
            "{rows:x?}, hooks {}, {}, {}",
            hookable(ix, rows.sidebar)?,
            hookable(ix, rows.selected).or_else(|err| {
               match (rows.selected_via, ix.isa) {
                  (Some(slot), _) => Ok(format!("through {slot:#x}")),
                  (None, crate::index::Isa::Aarch64) => {
                     Ok(format!("call {:#x} rewritten", rows.selected_call))
                  },
                  (None, _) => Err(err),
               }
            })?,
            hookable(ix, rows.push)?
         ))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "settings title",
      families: DESKTOP,
      run:      |ix| {
         let section = settings::settings_section(ix)?;
         let title = settings::settings_title(ix, section.page_offset)?;
         Ok(format!(
            "name {:#x}, translate {:#x}, header {}, call {:#x}",
            title.name,
            title.translate,
            hookable(ix, title.header)?,
            title.call
         ))
      },
   },
   #[cfg(any(target_arch = "x86_64", target_os = "macos"))]
   Locator {
      name:     "settings page changed",
      families: DESKTOP,
      run:      |ix| {
         settings::settings_section(ix)
            .and_then(|section| settings::settings_page_changed(ix, section.page_offset))
            .and_then(|func| hookable(ix, func))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "mana theme",
      families: DESKTOP,
      run:      |ix| {
         mana_theme::mana_theme(ix).and_then(|found| {
            Ok(format!(
               "set_global {}, pool {:#x}, sync {:#x}, drop {:#x}, size {:#x}, get_mut {:#x}, \
                colors {:#x}, {:x?}",
               hookable(ix, found.set_global)?,
               found.sync_pool,
               found.sync_theme,
               found.drop_in_place,
               found.size,
               found.get_mut,
               found.colors_size,
               found.layout,
            ))
         })
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "scene draw",
      families: DESKTOP,
      run:      |ix| {
         mana_theme::scene_draw(ix).and_then(|found| {
            Ok(format!(
               "draw {}, quads +{:#x}, shadows {}",
               hookable(ix, found.draw)?,
               found.quads,
               found
                  .shadows
                  .map_or_else(|| "off".to_owned(), |at| format!("+{at:#x}"))
            ))
         })
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "optimistic delete",
      families: DESKTOP,
      run:      |ix| {
         settings::optimistic_delete_of(ix).map(|found| {
            let deletes = found.deletes.iter().map(|func| format!("{func:#x}"));
            format!(
               "{} (sibling {:#x})",
               deletes.collect::<Vec<_>>().join(", "),
               found.sibling
            )
         })
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "call idle",
      families: DESKTOP,
      run:      |ix| {
         let dm = call_idle::dm_idle(ix)?;
         let afk = call_idle::guild_afk(ix)?;
         Ok(format!(
            "dm {}, afk move {}, afk disconnect {}",
            hookable(ix, dm)?,
            hookable(ix, afk.move_poll)?,
            hookable(ix, afk.disconnect_poll)?
         ))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "pending counts",
      families: DESKTOP,
      run:      |ix| {
         let found = counts::pending_counts(ix)?;
         let badges = found
            .badges
            .iter()
            .map(|&badge| hookable(ix, badge))
            .collect::<Found<Vec<String>>>()?;
         Ok(format!(
            "badges {}, friend requests {}",
            badges.join(", "),
            hookable(ix, found.friend_requests)?
         ))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "home count",
      families: DESKTOP,
      run:      |ix| {
         let found = counts::pending_counts(ix)?;
         hookable(ix, counts::home_count(ix, found.friend_requests)?)
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "home badge",
      families: DESKTOP,
      run:      |ix| {
         let found = counts::pending_counts(ix)?;
         hookable(ix, counts::home_badge(ix, found.friend_requests)?)
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "relationship counts",
      families: DESKTOP,
      run:      |ix| {
         counts::relationship_counts(ix)?
            .into_iter()
            .map(|tally| hookable(ix, tally))
            .collect::<Found<Vec<String>>>()
            .map(|tallies| tallies.join(", "))
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "channel sidebar",
      families: DESKTOP,
      run:      |ix| {
         let found = channel::channel_sidebar(ix)?;
         Ok(format!(
            "width {}, column {} (call {:#x}), publish {}, covered {}, rail {:#x}",
            hookable(ix, found.width)?,
            hookable(ix, found.column)?,
            found.column_site,
            hookable(ix, found.publish)?,
            hookable(ix, found.covered)?,
            found.rail
         ))
      },
   },
   #[cfg(any(target_arch = "x86_64", target_os = "macos"))]
   Locator {
      name:     "channel toolbar",
      families: DESKTOP,
      run:      |ix| {
         let found = channel::channel_toolbar(ix)?;
         Ok(format!(
            "button {}, append {}, id {:#x}, sizes {:#x}/{:#x}, click +{:#x}",
            hookable(ix, found.button)?,
            hookable(ix, found.append)?,
            found.members_id,
            found.button_size,
            found.toolbar_size,
            found.on_click
         ))
      },
   },
   #[cfg(any(target_arch = "x86_64", target_os = "macos"))]
   Locator {
      name:     "ready text",
      families: DESKTOP,
      run:      |ix| {
         let parse = ready_parse(ix)?;
         hookable(ix, ready::ready_text(ix, &parse)?)
      },
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "ready session",
      families: DESKTOP,
      run:      |ix| {
         let parse = ready_parse(ix)?;
         let session = ready::ready_session(ix, &parse)?;
         Ok(format!(
            "{} (kind +{:#x} = {}, raw +{:#x})",
            hookable(ix, session.func)?,
            session.kind,
            session.raw_kind,
            session.raw
         ))
      },
   },
   #[cfg(any(target_arch = "x86_64", target_os = "macos"))]
   Locator {
      name:     "resolve font",
      families: DESKTOP,
      run:      |ix| resolve_font(ix).and_then(|func| hookable(ix, func.start)),
   },
   #[cfg(any(
      target_arch = "x86_64",
      target_os = "macos",
      all(target_os = "linux", target_arch = "aarch64")
   ))]
   Locator {
      name:     "forward follow",
      families: DESKTOP,
      run:      |ix| {
         forward::forward_follow(ix).and_then(|found| {
            let nav = hookable(ix, found.navigate)?;
            match found.clips {
               Some(clips) => Ok(format!("{nav}, clips {}", hookable(ix, clips)?)),
               None => Ok(nav),
            }
         })
      },
   },
];

/// GPUI's `TextSystem::resolve_font(&self, font: &Font) -> FontId`, the one
/// caller of the cold path that panics with `"failed to resolve font"`. Every
/// font passes through it before GPUI's caches.
pub fn resolve_font<'ix>(ix: &'ix Index<'ix>) -> Found<Func<'ix>> {
   ix.literal(b"failed to resolve font")
      .callers()
      .one("resolve_font")
}

/// The `ClientAccessGate` read the runtime makes answer "no gate". The
/// blocked page calls the `signed_in && access(..) != 2` stub, whose first
/// call is the read, and then the read itself.
pub fn access_gate<'ix>(ix: &'ix Index<'ix>) -> Found<Func<'ix>> {
   const STUB: usize = 64;

   let page = ix
      .literal(b"desktop_access_gate_blocked")
      .one("access gate page")?;
   let reads = page
      .calls()
      .windows(2)
      .filter_map(|pair| {
         let stub = ix.func(pair[0]).ok()?;
         (stub.size() <= STUB && stub.calls().first() == Some(&pair[1]))
            .then(|| ix.func(pair[1]).ok())
            .flatten()
      })
      .collect::<Vec<Func<'ix>>>();

   crate::one("access gate read", &reads)
}

/// `libdiscord_media::fetch`, the function that enters its tracing span.
pub fn media_fetch<'ix>(ix: &'ix Index<'ix>) -> Found<Func<'ix>> {
   ix.tracing("libdiscord_media::fetch")
      .one("libdiscord_media::fetch")
}

/// `getEmojiUnavailableReason(&Emoji, &EmojiContext)`, a leaf that answers 3
/// for profiles before anything else.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct EmojiCheck {
   pub func:      usize,
   /// `EmojiContext.intention`, 3 (`CHAT`) for the message box.
   pub intention: usize,
}

/// Every build opens it `intention = ctx.intention; if intention | 2 == 14
/// { return 3 }`, a byte load off the second argument, a compare with 14, a
/// branch and a return of 3 within 8 instructions.
pub fn emoji_check(ix: &Index<'_>) -> Found<EmojiCheck> {
   const HEAD: usize = 8;
   let context = ix.isa.arg(1).ok_or("no second argument register")?;
   let opens = |step: &Step| matches!(step.op, Op::Load { base, size: 1, .. } if base == context);
   // Rejecting on the first instruction keeps the scan of every function to one decode each.
   let checks = ix
      .heads(1, |head| head.first().is_some_and(opens))
      .filter(|func| {
         let head = func.head(HEAD);
         let ops = head.iter().skip(1).map(|step| step.op);
         ops.clone().any(|op| matches!(op, Op::Cmp { value: 14, .. }))
            && ops.clone().any(|op| matches!(op, Op::Imm { value: 3, .. }))
            && ops.clone().any(|op| op == Op::Ret)
            && ops.clone().any(|op| matches!(op, Op::Branch(_)))
      });
   let func = checks.one("emoji check")?;
   let Some(Op::Load { offset, .. }) = func.steps().first().map(|step| step.op) else {
      return Err(format!("emoji check at {func:?} reads no context byte"));
   };
   Ok(EmojiCheck {
      func:      func.start,
      intention: usize::try_from(offset)
         .map_err(|_| format!("emoji check at {func:?} reads a negative offset"))?,
   })
}

/// [`ready::ready_parse`] from the registry's READY handler.
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
fn ready_parse<'ix>(ix: &'ix Index<'ix>) -> Found<Func<'ix>> {
   let registry = ix.registry()?;
   let id = registry.id("READY").ok_or("the registry has no READY")?;
   ready::ready_parse(ix, registry.entry(id).handler)
}

/// `func` with the whole instructions an x86-64 entry hook displaces there.
/// arm64 hooks always displace four.
#[cfg(any(
   target_arch = "x86_64",
   target_os = "macos",
   all(target_os = "linux", target_arch = "aarch64")
))]
pub fn hookable(ix: &Index<'_>, func: usize) -> Found<String> {
   match ix.isa {
      crate::index::Isa::X86_64 { .. } => {
         crate::desktop::entry_moved(ix.image, func)
            .map(|moved| format!("{func:#x} ({moved} bytes moved)"))
      },
      _ => entry_hookable(ix, func).map(|()| format!("{func:#x}")),
   }
}

/// Whether sunhook can hook `func`'s entry. On arm64 it jumps back from the
/// moved opening through `x17`, or `x16` when the opening sets `x17`, so an
/// opening that sets both has no register left for the jump.
pub fn entry_hookable(ix: &Index<'_>, func: usize) -> Found<()> {
   /// The instructions an arm64 entry hook displaces.
   const DISPLACED: usize = 4;
   /// sunhook's jump when no island is in reach, as on macOS.
   const JUMP: usize = 20;
   /// `x16` and `x17`.
   const SCRATCH: u32 = 3 << 16;
   if ix.isa != crate::index::Isa::Aarch64 {
      return Ok(());
   }
   let opened = ix.func(func)?;
   if opened.end - opened.start < JUMP {
      return Err(format!(
         "{func:#x} is {} bytes, shorter than sunhook's {JUMP}-byte jump",
         opened.end - opened.start
      ));
   }
   let sets = opened
      .head(DISPLACED)
      .iter()
      .fold(0, |seen, step| seen | step.writes & SCRATCH);
   if sets == SCRATCH {
      return Err(format!(
         "{func:#x} sets x16 and x17 as it opens, leaving sunhook no register to jump back through"
      ));
   }
   Ok(())
}
