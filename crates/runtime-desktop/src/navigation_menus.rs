use std::{
   ffi::c_void,
   sync::{
      OnceLock,
      PoisonError,
      RwLock,
   },
};

use phoenix_hook::image::Image;
use phoenix_sigs::locate::surfaces::{
   Menu,
   menus,
};

use crate::context_menu::{
   self,
   ActionMeta,
   DropMenuItem,
   MenuEdit,
   RawMenuVec,
};

type Collect = unsafe extern "C" fn(*mut c_void, *mut RawMenuVec) -> *mut c_void;
static COLLECT: OnceLock<Collect> = OnceLock::new();
static PATCHED: OnceLock<Vec<Menu>> = OnceLock::new();
static ACTIONS: RwLock<Vec<(Menu, ActionMeta)>> = RwLock::new(Vec::new());
static ITEM_DROP: OnceLock<DropMenuItem> = OnceLock::new();
static EDITS: RwLock<Vec<(u64, Menu, Vec<MenuEdit>)>> = RwLock::new(Vec::new());

pub fn install(image: &Image<'static>, ix: &crate::located::LazyIndex<'_>, item_size: usize) {
   let found = match crate::located::get("surfaces::menus", || menus(ix, item_size)) {
      Ok(found) => found,
      Err(err) => {
         log::warn!("plugin context menus: {err}");
         return;
      },
   };
   // SAFETY: locator validates the shared collector's Vec argument and element
   // size.
   let original = unsafe { std::mem::transmute::<usize, Collect>(found.finish) };
   if COLLECT.set(original).is_err() {
      return;
   }
   if let Some(drop_item) = found.drop_item {
      // SAFETY: the locator verifies the collector's menu item destructor.
      let _ = ITEM_DROP.set(unsafe { std::mem::transmute::<usize, DropMenuItem>(drop_item) });
   }
   let patched = found
      .calls
      .into_iter()
      .filter_map(|call| {
         let menu = call.menu;
         let detour = match menu {
            Menu::Server => server as *const (),
            Menu::Channel => channel as *const (),
            Menu::User => user as *const (),
            Menu::Message => return None,
         };
         // SAFETY: startup is single-threaded, and the prefix adds the decoded
         // id in unused R8.
         match unsafe { image.hook_call(call.site, &call.prefix, detour.addr()) } {
            Ok(_) => Some(menu),
            Err(err) => {
               log::warn!("{} context menu: {err}", menu.name());
               None
            },
         }
      })
      .collect();
   let _ = PATCHED.set(patched);
}

pub fn ready(menu: Menu) -> bool {
   if menu == Menu::Message {
      context_menu::message_menu_ready()
   } else {
      PATCHED.get().is_some_and(|patched| patched.contains(&menu))
   }
}

#[cfg(test)]
pub fn test_slot() {
   let _ = PATCHED.set(vec![Menu::User]);
}

pub fn add(menu: Menu, action: ActionMeta) {
   let mut actions = ACTIONS.write().unwrap_or_else(PoisonError::into_inner);
   actions.push((menu, action));
   actions.sort_by_key(|(_, action)| (action.priority, action.id));
}

pub fn remove(id: u64) {
   ACTIONS
      .write()
      .unwrap_or_else(PoisonError::into_inner)
      .retain(|(_, action)| action.id != id);
   EDITS
      .write()
      .unwrap_or_else(PoisonError::into_inner)
      .retain(|(edit, ..)| *edit != id);
}

pub fn can_hide() -> bool {
   ITEM_DROP.get().is_some()
}

pub fn has_actions(menu: Menu) -> bool {
   ACTIONS
      .read()
      .unwrap_or_else(PoisonError::into_inner)
      .iter()
      .any(|(kind, _)| *kind == menu)
}

pub fn add_edits(id: u64, menu: Menu, edits: Vec<MenuEdit>) {
   EDITS
      .write()
      .unwrap_or_else(PoisonError::into_inner)
      .push((id, menu, edits));
}

pub unsafe fn apply(menu: Menu, items: *mut RawMenuVec, target: u64) {
   let _ = std::panic::catch_unwind(|| {
      let edits = EDITS
         .read()
         .unwrap_or_else(PoisonError::into_inner)
         .iter()
         .filter(|(_, kind, _)| *kind == menu)
         .flat_map(|(_, _, edits)| edits.iter().cloned())
         .collect::<Vec<_>>();
      let actions = ACTIONS
         .read()
         .unwrap_or_else(PoisonError::into_inner)
         .iter()
         .filter(|(kind, _)| *kind == menu)
         .map(|(_, action)| action.clone())
         .collect::<Vec<_>>();
      // SAFETY: items is the live Vec passed to the validated native collector.
      unsafe {
         context_menu::edit_navigation_menu(items, &edits, ITEM_DROP.get().copied());
         context_menu::append_navigation_actions(items, &actions, menu, target);
      }
   });
}

unsafe fn collect(
   menu: Menu,
   output: *mut c_void,
   items: *mut RawMenuVec,
   target: u64,
) -> *mut c_void {
   let original = *COLLECT.wait();
   // SAFETY: items is the live Vec passed to the validated native collector.
   unsafe { apply(menu, items, target) };
   // SAFETY: forward the original owned Vec and output slot to the collector.
   unsafe { original(output, items) }
}

unsafe extern "C" fn server(
   output: *mut c_void,
   items: *mut RawMenuVec,
   target: u64,
) -> *mut c_void {
   // SAFETY: the server call site passes these native collector arguments.
   unsafe { collect(Menu::Server, output, items, target) }
}

unsafe extern "C" fn channel(
   output: *mut c_void,
   items: *mut RawMenuVec,
   target: u64,
) -> *mut c_void {
   // SAFETY: the channel call site passes these native collector arguments.
   unsafe { collect(Menu::Channel, output, items, target) }
}

unsafe extern "C" fn user(output: *mut c_void, items: *mut RawMenuVec, target: u64) -> *mut c_void {
   // SAFETY: the user call site passes these native collector arguments.
   unsafe { collect(Menu::User, output, items, target) }
}
