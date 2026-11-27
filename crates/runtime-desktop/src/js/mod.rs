//! The desktop share of the `phoenix` object: `phoenix.ui` and message
//! accessories, and `phoenix.native` for the console.

pub mod commands;
pub mod fonts;
pub mod native;
pub mod themes;
pub mod ui;

use std::{
   cell::RefCell,
   collections::HashMap,
};

use phoenix_ui_desktop::Shortcut;
use rquickjs::{
   Ctx,
   Function,
   JsLifetime,
};

use crate::{
   accessories,
   call_idle::{
      self,
      Timer,
   },
   composer,
   context_menu,
   counts::{
      self,
      Count,
   },
   double_click,
   emoji,
   forward,
   js::{
      commands::Registered,
      ui::{
         DesktopAccessory,
         DesktopAction,
         DesktopMember,
      },
   },
   members::Place,
   shortcuts,
   sidebar,
   spotify::{
      self,
      Tweak,
   },
   sticker,
   toolbar,
};

/// What plugins registered with the desktop UI, kept in the runtime's userdata
/// next to the core's registry.
#[derive(Default)]
pub struct Desktop<'js> {
   /// `phoenix.ui.addMessageAccessory` callbacks.
   pub accessories:      RefCell<Vec<DesktopAccessory<'js>>>,
   /// `phoenix.ui.addMemberDecoration` callbacks.
   pub members:          RefCell<Vec<DesktopMember<'js>>>,
   /// `phoenix.ui.addMessageHeaderDecoration` callbacks.
   pub headers:          RefCell<Vec<DesktopMember<'js>>>,
   /// `phoenix.ui.addMessageAction` callbacks.
   pub actions:          RefCell<Vec<DesktopAction<'js>>>,
   /// Active `phoenix.ui.openPage` button callbacks, keyed by button ID.
   pub page_buttons:     RefCell<HashMap<u64, (u64, String, Function<'js>)>>,
   /// `phoenix.ui.notify` `onPress` callbacks, keyed by banner ID.
   pub notices:          RefCell<HashMap<u64, (String, Function<'js>)>>,
   /// `phoenix.messages.onEmojiPick` handlers by id and plugin. Desktop has
   /// no picker presses to give them, only the unlock they turn on.
   pub emoji_pickers:    RefCell<Vec<(u64, String)>>,
   /// `phoenix.messages.onStickerPick` handlers by id and plugin.
   pub sticker_pickers:  RefCell<Vec<(u64, String, Function<'js>)>>,
   /// `phoenix.ui.onEvent("rowDoubleTap")` handlers by id and plugin.
   pub double_taps:      RefCell<Vec<(u64, String, Function<'js>)>>,
   /// `phoenix.ui.hide` holds by id and plugin.
   pub hides:            RefCell<Vec<(u64, String)>>,
   /// `phoenix.ui.collapseChannels` holds by id and plugin.
   pub collapses:        RefCell<Vec<(u64, String)>>,
   /// `phoenix.ui.disableForwardFollow` holds by id and plugin.
   pub forward_follows:  RefCell<Vec<(u64, String)>>,
   /// `phoenix.spotify` holds by id, plugin and tweak.
   pub spotify:          RefCell<Vec<(u64, String, Tweak)>>,
   /// `phoenix.ui.addHeaderButton` handlers by id and plugin.
   pub header_buttons:   RefCell<Vec<(u64, String, Function<'js>)>>,
   /// `phoenix.ui.addComposerButton` presses by id and plugin.
   pub composer_buttons: RefCell<Vec<(u64, String, Function<'js>)>>,
   /// `phoenix.ui.addShortcut` handlers by id and plugin.
   pub shortcuts:        RefCell<Vec<(u64, String, Shortcut, Function<'js>)>>,
   /// `phoenix.ui.hideCount` holds by id and plugin.
   pub hidden_counts:    RefCell<Vec<(u64, String, Count)>>,
   /// `phoenix.ui.disableCallIdle` holds by id and plugin.
   pub idle_off:         RefCell<Vec<(u64, String, Timer)>>,
   /// `phoenix.commands.register` commands by lowercase name.
   pub commands:         RefCell<HashMap<String, Registered<'js>>>,
}

impl<'js> Desktop<'js> {
   /// The decorations `place` draws.
   pub const fn decorations(&self, place: Place) -> &RefCell<Vec<DesktopMember<'js>>> {
      match place {
         Place::Members => &self.members,
         Place::Headers => &self.headers,
      }
   }
}

// SAFETY: every field is bound to 'js, and Changed swaps exactly that lifetime.
unsafe impl<'js> JsLifetime<'js> for Desktop<'js> {
   type Changed<'to> = Desktop<'to>;
}

/// Drops the accessories, member decorations, context menu actions, open page
/// callbacks, banners, emoji pick handlers, hidden counts, call idle holds,
/// sidebar collapses, header and composer buttons and shortcuts `name`
/// registered and redraws Discord without them.
pub fn unload(ctx: &Ctx<'_>, name: &str) {
   let Some(registry) = ctx.userdata::<Desktop<'_>>() else {
      return;
   };
   let mut list = registry.accessories.borrow_mut();
   let before = list.len();
   list.retain(|entry| entry.plugin != name);
   if list.len() != before {
      accessories::set_active_count(list.len());
      accessories::clear_cache();
      accessories::invalidate_discord_windows();
   }
   drop(list);

   for place in [Place::Members, Place::Headers] {
      let mut decorations = registry.decorations(place).borrow_mut();
      let before = decorations.len();
      decorations.retain(|entry| entry.plugin != name);
      if decorations.len() != before {
         place.decorations().set_active_count(decorations.len());
         place.decorations().clear_cache();
         accessories::invalidate_discord_windows();
      }
   }

   let mut actions = registry.actions.borrow_mut();
   let before_actions = actions.len();
   actions.retain(|entry| entry.plugin != name);
   if actions.len() != before_actions {
      let meta = actions.iter().map(DesktopAction::meta).collect();
      context_menu::sync_actions(meta);
   }
   drop(actions);

   registry
      .page_buttons
      .borrow_mut()
      .retain(|_, (_, plugin, _)| plugin != name);
   registry
      .notices
      .borrow_mut()
      .retain(|_, (plugin, _)| plugin != name);
   crate::notices::remove_plugin(name);

   let mut pickers = registry.emoji_pickers.borrow_mut();
   pickers.retain(|(_, plugin)| plugin != name);
   emoji::unlock(!pickers.is_empty());
   drop(pickers);

   registry.sticker_pickers.borrow_mut().retain(|(_, plugin, _)| plugin != name);
   sticker::set_active(!registry.sticker_pickers.borrow().is_empty());

   let mut taps = registry.double_taps.borrow_mut();
   taps.retain(|(_, plugin, _)| plugin != name);
   double_click::set_active(!taps.is_empty());
   drop(taps);

   registry.hides.borrow_mut().retain(|(_, plugin)| {
      let held = plugin == name;
      if held {
         composer::hide_gift(false);
      }
      !held
   });

   release_holds(&registry.collapses, name, || {
      sidebar::collapse(false);
   });
   release_holds(&registry.forward_follows, name, || {
      forward::disable(false);
   });

   registry.spotify.borrow_mut().retain(|&(_, ref plugin, tweak)| {
      let held = plugin == name;
      if held {
         spotify::release(tweak);
      }
      !held
   });

   registry
      .header_buttons
      .borrow_mut()
      .retain(|(id, plugin, _)| {
         let held = plugin == name;
         if held {
            toolbar::remove(*id);
         }
         !held
      });

   ui::unload_composer_buttons(&registry, name);

   registry
      .shortcuts
      .borrow_mut()
      .retain(|(_, plugin, shortcut, _)| {
         let held = plugin == name;
         if held {
            shortcuts::release(*shortcut);
         }
         !held
      });

   let mut hidden = registry.hidden_counts.borrow_mut();
   hidden.retain(|(_, plugin, _)| plugin != name);
   counts::hide(hidden.iter().map(|&(.., count)| count));

   let mut off = registry.idle_off.borrow_mut();
   off.retain(|(_, plugin, _)| plugin != name);
   call_idle::turn_off(off.iter().map(|&(.., timer)| timer));

   commands::unload(&registry, name);
   fonts::unload(name);
   themes::unload(name);
}

fn release_holds(holds: &RefCell<Vec<(u64, String)>>, name: &str, mut release: impl FnMut()) {
   holds.borrow_mut().retain(|(_, plugin)| {
      let held = plugin == name;
      if held {
         release();
      }
      !held
   });
}
