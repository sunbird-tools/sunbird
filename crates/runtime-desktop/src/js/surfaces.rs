use std::sync::atomic::{
   AtomicU64,
   Ordering,
};

use phoenix_runtime::js::Engine;
use phoenix_sigs::locate::surfaces::Surface;
use rquickjs::{
   CatchResultExt,
   Ctx,
   Exception,
   Function,
   Object,
   Value,
};

use super::Desktop;
use crate::surfaces::{
   self,
   Button,
   Section,
};

pub struct Handler<'js> {
   id:       u64,
   plugin:   String,
   surface:  Surface,
   callback: Function<'js>,
}

fn find_surface(name: &str) -> Option<Surface> {
   Surface::ALL
      .into_iter()
      .find(|surface| surface.name() == name)
}

pub fn bind<'js>(ctx: &Ctx<'js>, ui: &Object<'js>, plugin: &str) -> rquickjs::Result<()> {
   let section_owner = plugin.to_owned();
   ui.set(
      "addSurfaceSection",
      Function::new(
         ctx.clone(),
         move |ctx: Ctx<'js>, name: String, options: Object<'js>| {
            section(&ctx, &section_owner, &name, &options)
         },
      )?,
   )?;
   ui.set(
      "hasSurface",
      Function::new(ctx.clone(), |name: String| {
         find_surface(&name).is_some_and(surfaces::ready)
      })?,
   )?;
   let owner = plugin.to_owned();
   ui.set(
      "addSurfaceButton",
      Function::new(
         ctx.clone(),
         move |ctx: Ctx<'js>, name: String, options: Object<'js>| {
            let surface = find_surface(&name).ok_or_else(|| {
               Exception::throw_type(&ctx, &format!("unknown ui surface: {name}"))
            })?;
            register(&ctx, &owner, surface, &options)
         },
      )?,
   )
}

fn section<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   name: &str,
   options: &Object<'js>,
) -> rquickjs::Result<Function<'js>> {
   static NEXT_ID: AtomicU64 = AtomicU64::new(1);
   let surface = match find_surface(name) {
      Some(surface @ (Surface::Profile | Surface::ServerList)) if surfaces::ready(surface) => {
         surface
      },
      _ => {
         return Err(Exception::throw_type(
            ctx,
            "this surface does not support sections",
         ));
      },
   };
   let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
   let owner = plugin.to_owned();
   let remove = Function::new(ctx.clone(), move || {
      surfaces::remove_sections(Some(id), &owner);
   })?;
   let owner = plugin.to_owned();
   remove.set(
      "update",
      Function::new(ctx.clone(), move |next: Object<'js>| {
         let section = read_section(&next, id, &owner, surface)?;
         surfaces::set_section(section, true);
         Ok::<_, rquickjs::Error>(())
      })?,
   )?;
   surfaces::set_section(read_section(options, id, plugin, surface)?, false);
   Ok(remove)
}

fn read_section(
   options: &Object<'_>,
   id: u64,
   plugin: &str,
   surface: Surface,
) -> rquickjs::Result<Section> {
   let priority = options
      .get::<_, Option<i32>>("priority")?
      .unwrap_or_default();
   let title = options
      .get::<_, Option<String>>("title")?
      .filter(|title| !title.is_empty());
   let content = options.get::<_, Value<'_>>("content")?;
   let mut pieces = Vec::new();
   if let Some(text) = title {
      pieces.push(crate::gpui::Piece {
         text,
         color: None,
         icons: Vec::new(),
      });
   }
   if let Some(content) = super::ui::parse_rendered(&content, priority) {
      pieces.push(crate::gpui::Piece {
         text:  content.text,
         color: content.color,
         icons: content.icons,
      });
   }
   Ok(Section {
      id,
      plugin: plugin.to_owned(),
      surface,
      priority,
      pieces,
   })
}

fn register<'js>(
   ctx: &Ctx<'js>,
   plugin: &str,
   surface: Surface,
   options: &Object<'js>,
) -> rquickjs::Result<Function<'js>> {
   static NEXT_ID: AtomicU64 = AtomicU64::new(1);
   if !surfaces::ready(surface) {
      return Err(Exception::throw_message(
         ctx,
         &format!("{} is unavailable", surface.name()),
      ));
   }
   let registry = ctx
      .userdata::<Desktop<'js>>()
      .ok_or_else(|| Exception::throw_internal(ctx, "no desktop registry"))?;
   let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
   let label = options.get::<_, String>("label")?;
   let priority = options
      .get::<_, Option<i32>>("priority")?
      .unwrap_or_default();
   let callback = options.get::<_, Function<'js>>("onPress")?;
   let remove = Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
      if let Some(registry) = ctx.userdata::<Desktop<'_>>() {
         registry
            .surface_buttons
            .borrow_mut()
            .retain(|entry| entry.id != id);
      }
      surfaces::remove(id);
   })?;
   remove.set(
      "update",
      Function::new(ctx.clone(), move |patch: Object<'js>| {
         let next_label = patch.get::<_, Option<String>>("label")?;
         let next_priority = patch.get::<_, Option<i32>>("priority")?;
         if let Some(mut button) = surfaces::get(id) {
            if let Some(label) = next_label {
               button.label = label;
            }
            if let Some(priority) = next_priority {
               button.priority = priority;
            }
            surfaces::set(button);
         }
         Ok::<_, rquickjs::Error>(())
      })?,
   )?;
   registry.surface_buttons.borrow_mut().push(Handler {
      id,
      surface,
      plugin: plugin.to_owned(),
      callback,
   });
   surfaces::set(Button {
      id,
      surface,
      label,
      priority,
   });
   Ok(remove)
}

pub fn unload(registry: &Desktop<'_>, plugin: &str) {
   surfaces::remove_sections(None, plugin);
   registry.surface_buttons.borrow_mut().retain(|entry| {
      if entry.plugin == plugin {
         surfaces::remove(entry.id);
         false
      } else {
         true
      }
   });
}

pub fn press(engine: &Engine, id: u64, message_id: u64) {
   engine.with(|ctx| {
      let entry = ctx.userdata::<Desktop<'_>>().and_then(|registry| {
         let handlers = registry.surface_buttons.borrow();
         let entry = handlers.iter().find(|entry| entry.id == id)?;
         Some((entry.surface, entry.plugin.clone(), entry.callback.clone()))
      });
      let Some((surface, plugin, callback)) = entry else {
         return;
      };
      let call = (|| {
         let argument = super::ui::build_message_context(&ctx, message_id, 0, message_id != 0)?;
         argument.set("surface", surface.name())?;
         if message_id == 0 {
            argument.remove("messageId")?;
            argument.remove("message")?;
            if let Some(user) = super::ui::current_user_id() {
               argument.set("currentUserId", user)?;
            }
         } else if let Some(message) = argument.get::<_, Option<Object<'_>>>("message")?
            && let Some(channel) = message.get::<_, Option<String>>("channel_id")?
         {
            argument.set("channelId", channel)?;
         }
         callback.call::<_, Value<'_>>((argument,))
      })();
      if let Err(err) = call.catch(&ctx) {
         log::error!("[{plugin}] {} button: {err}", surface.name());
      }
   });
}

#[cfg(test)]
mod tests {
   use phoenix_runtime::js::compile;
   use phoenix_sigs::registry::GatewayRegistry;
   use rquickjs::Persistent;

   use super::*;

   #[test]
   fn updates_sections_and_cleans_them_up() {
      const OWNER: &str = "section-test";
      surfaces::test_slot();
      let engine = Engine::new(&GatewayRegistry::default()).unwrap();
      engine.with(|ctx| {
         assert!(ctx.store_userdata(Desktop::default()).is_ok());
         let api = Object::new(ctx.clone()).unwrap();
         bind(&ctx, &api, OWNER).unwrap();
         let code = r#"
            const first = phoenix.addSurfaceSection("serverList", { content: "old" });
            phoenix.addSurfaceSection("serverList", { content: "last", priority: 5 });
            first.update({ title: "new", content: "body", priority: -1 });
            return () => { first(); first.update({ content: "removed" }); };
         "#;
         let remove = compile(&ctx, code)
            .unwrap()
            .call::<_, Function<'_>>((api,))
            .unwrap();
         let sections = surfaces::sections_for(OWNER);
         assert_eq!(sections.len(), 2);
         assert_eq!(sections[0].pieces[0].text, "new");
         assert_eq!(sections[0].pieces[1].text, "body");
         remove.call::<_, ()>(()).unwrap();
         let sections = surfaces::sections_for(OWNER);
         assert_eq!(sections.len(), 1);
         assert_eq!(sections[0].pieces[0].text, "last");
         unload(&ctx.userdata::<Desktop<'_>>().unwrap(), OWNER);
         assert!(surfaces::sections_for(OWNER).is_empty());
      });
   }

   #[test]
   fn registers_updates_routes_and_removes_buttons() {
      surfaces::test_slot();
      let engine = Engine::new(&GatewayRegistry::default()).unwrap();
      let (result, remove) = engine.with(|ctx| {
         assert!(ctx.store_userdata(Desktop::default()).is_ok());
         let api = Object::new(ctx.clone()).unwrap();
         bind(&ctx, &api, "test").unwrap();
         let result = Object::new(ctx.clone()).unwrap();
         api.set("result", result.clone()).unwrap();
         let script = r#"
            if (!phoenix.hasSurface("composer")) throw new Error("missing composer");
            if (phoenix.hasSurface("profile")) throw new Error("unexpected profile");
            const remove = phoenix.addSurfaceButton("composer", {
               label: "original",
               onPress(ctx) {
                  phoenix.result.composer = ctx.surface;
                  phoenix.result.composerHasMessage = "messageId" in ctx;
               },
            });
            const removeMessage = phoenix.addSurfaceButton("messageToolbar", {
               label: "message",
               onPress(ctx) {
                  phoenix.result.message = ctx.surface;
                  phoenix.result.messageId = ctx.messageId;
               },
            });
            remove.update({ label: "updated", priority: -4 });
            return () => { remove(); removeMessage(); };
         "#;
         let remove = compile(&ctx, script)
            .unwrap()
            .call::<_, Function<'_>>((api,))
            .unwrap();
         (
            Persistent::save(&ctx, result),
            Persistent::save(&ctx, remove),
         )
      });
      let (composer, message) = engine.with(|ctx| {
         let registry = ctx.userdata::<Desktop<'_>>().unwrap();
         let handlers = registry.surface_buttons.borrow();
         (handlers[0].id, handlers[1].id)
      });
      let button = surfaces::get(composer).unwrap();
      assert_eq!(button.label, "updated");
      assert_eq!(button.priority, -4);
      press(&engine, composer, 0);
      press(&engine, message, 42);
      engine.with(|ctx| {
         let received = result.restore(&ctx).unwrap();
         assert_eq!(received.get::<_, String>("composer").unwrap(), "composer");
         assert!(!received.get::<_, bool>("composerHasMessage").unwrap());
         assert_eq!(
            received.get::<_, String>("message").unwrap(),
            "messageToolbar"
         );
         assert_eq!(received.get::<_, String>("messageId").unwrap(), "42");
         remove.restore(&ctx).unwrap().call::<_, ()>(()).unwrap();
         assert!(
            ctx.userdata::<Desktop<'_>>()
               .unwrap()
               .surface_buttons
               .borrow()
               .is_empty()
         );
      });
      assert!(surfaces::get(composer).is_none());
      assert!(surfaces::get(message).is_none());
   }
}
