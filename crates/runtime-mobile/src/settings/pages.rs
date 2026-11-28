//! The main page, with its update and debug sections, and the install page.

use phoenix_runtime::{
   PluginInfo,
   update::Catalog,
};
use phoenix_ui_mobile::{
   Prop,
   templates,
};
use phoenix_wire::Value;

use crate::settings::{
   Field,
   ICON_CONSOLE,
   ICON_FILE,
   ICON_PLUGIN,
   ICON_RELOAD,
   Press,
   cards::{
      BLURPLE,
      GREEN,
      badge,
      bar,
      card,
      column,
      divider,
      grow,
      heading,
      label,
      line,
      pill,
      row,
   },
   console::log_line,
   rows::{
      InputKind,
      button_row,
      icon_row,
      section,
      switch_row,
      text_field,
      value_row,
   },
   theme::Color,
   tree::{
      Tree,
      text,
   },
};

pub fn main_sections(plugins: &[PluginInfo]) -> Vec<Tree> {
   let rows = if plugins.is_empty() {
      vec![button_row("No plugins found", ICON_PLUGIN, None, false)]
   } else {
      plugins
         .iter()
         .enumerate()
         .map(|(index, plugin)| {
            let subtitle = plugin
               .state
               .error()
               .map(|error| format!("Failed: {error}"))
               .or_else(|| plugin.header.description.clone());
            switch_row(
               Some(ICON_PLUGIN),
               &plugin.name,
               subtitle.as_deref(),
               plugin.state.is_on(),
               Press::OpenPlugin(index),
               Press::Toggle(index),
            )
         })
         .collect()
   };
   let enabled = plugins.iter().filter(|plugin| plugin.state.is_on()).count();
   updates_section()
      .into_iter()
      .chain([
         section("Plugins", rows),
         section("", vec![
            button_row("Install plugin", ICON_FILE, Some(Press::OpenInstall), true),
            button_row("Console", ICON_CONSOLE, Some(Press::OpenConsole), true),
         ]),
         debug_section(),
         section("About", vec![
            value_row("Version", phoenix_runtime::update::VERSION),
            value_row("Runtime", "rquickjs"),
            value_row("Gateway hooks", "registry pointer swap"),
            // iOS hooks nothing inline: it swaps the bridge's vtable slots.
            value_row(
               "Hook library",
               if cfg!(target_os = "ios") {
                  "vtable slot swap"
               } else {
                  "ShadowHook"
               },
            ),
            value_row("Enabled", &format!("{enabled} of {}", plugins.len())),
         ]),
      ])
      .collect()
}

/// One card for what can update: Sunbird and its plugins, and on Android
/// Discord itself.
fn updates_section() -> Option<Tree> {
   let mut items = sunbird_item().into_iter().collect::<Vec<_>>();
   if let Some(discord) = discord_item() {
      if !items.is_empty() {
         items.push(divider());
      }
      items.push(discord);
   }
   if items.is_empty() {
      return None;
   }
   let mut header = Tree::from_template(&templates().header);
   header.set(Prop::TEXT, Some(text("Updates")));
   Some(column(0.0, vec![header, card(items)]))
}

fn sunbird_item() -> Option<Tree> {
   let found = phoenix_runtime::update::found();
   let status = phoenix_runtime::update::status();
   let busy = phoenix_runtime::update::busy();
   let Some(update) = found else {
      // The line can be a success or a failure, so it gets the old warning
      // color under a neutral heading.
      let status = status?;
      return Some(column(12.0, vec![
         heading(
            badge(Color::Muted.argb(), 44.0, ICON_RELOAD),
            "Sunbird",
            phoenix_runtime::update::VERSION,
            None,
         ),
         line(&status, Color::Warn.argb()),
      ]));
   };
   let title = update.loader.as_ref().map_or_else(
      || "Plugin updates".to_owned(),
      |version| format!("Sunbird {version}"),
   );
   // The IPA is signed, so iOS takes a new loader only from a reinstall.
   let reinstall = cfg!(target_os = "ios") && update.loader.is_some();
   let subtitle = if busy {
      status.unwrap_or_else(|| "Updating...".to_owned())
   } else if reinstall {
      "Patch a new IPA on the site and install it over this one".to_owned()
   } else {
      let plugins = summary(&update.plugins);
      match (&update.loader, plugins) {
         (Some(_), Some(plugins)) => format!("New loader, and {plugins}"),
         (Some(_), None) => "A new loader, installed in-app".to_owned(),
         (None, plugins) => plugins.unwrap_or_default(),
      }
   };
   let trailing = if busy {
      label("Updating", 14.0, 700, BLURPLE)
   } else if reinstall {
      pill("Site", Press::OpenSite)
   } else {
      pill("Update", Press::ApplyUpdate)
   };
   let mut kids = vec![heading(
      badge(GREEN, 44.0, ICON_RELOAD),
      &title,
      &subtitle,
      Some(trailing),
   )];
   // Plugins update in place, so the site's full download is only for a
   // loader.
   if update.loader.is_some() && !busy && !reinstall {
      kids.push(row(8.0, vec![
         grow(label(
            "Or get the full download from the site",
            13.0,
            400,
            Color::Muted.argb(),
         )),
         pill("Site", Press::OpenSite),
      ]));
   }
   if let Some(status) = phoenix_runtime::update::status().filter(|_| !busy) {
      kids.push(line(&status, Color::Warn.argb()));
   }
   Some(column(12.0, kids))
}

/// "a, b and 3 more have updates".
fn summary(plugins: &[String]) -> Option<String> {
   match plugins {
      [] => None,
      [one] => Some(format!("{one} has an update")),
      [first, second] => Some(format!("{first} and {second} have updates")),
      [first, second, rest @ ..] => {
         Some(format!(
            "{first}, {second} and {} more have updates",
            rest.len()
         ))
      },
   }
}

/// Discord's own update, with its progress while it runs.
fn discord_item() -> Option<Tree> {
   #[cfg(target_os = "android")]
   {
      use crate::discord_update::State;
      let (title, subtitle, progress, pill_text) = match crate::discord_update::state() {
         State::Current => return None,
         State::Available(build) => {
            (
               format!("Discord {}", build.version_name),
               format!(
                  "Checked for Sunbird \u{00B7} {} MB",
                  crate::discord_update::download_size(&build) >> 20
               ),
               None,
               "View",
            )
         },
         State::Running(build, progress) => {
            let step = progress
               .steps
               .get(progress.at)
               .map_or_default(|step| step.label.clone());
            (
               format!("Discord {}", build.version_name),
               format!("{step} \u{00B7} {}%", progress.percent()),
               Some(progress.fraction()),
               "View",
            )
         },
         State::Failed { build, .. } => {
            (
               format!("Discord {}", build.version_name),
               "The update failed".to_owned(),
               None,
               "Retry",
            )
         },
         State::Handed(build) => {
            (
               format!("Discord {}", build.version_name),
               "Ready to install".to_owned(),
               None,
               "Install",
            )
         },
      };
      let mut kids = vec![heading(
         badge(BLURPLE, 44.0, ICON_RELOAD),
         &title,
         &subtitle,
         Some(pill(pill_text, Press::OpenDiscordUpdate)),
      )];
      kids.extend(progress.map(|fraction| bar(fraction, 6.0, BLURPLE)));
      Some(column(12.0, kids))
   }
   #[cfg(not(target_os = "android"))]
   None
}

/// The site's plugins, each with a switch that installs or uninstalls it.
fn listed_rows(plugins: &[PluginInfo]) -> Vec<Tree> {
   let listed = match phoenix_runtime::update::catalog() {
      Catalog::Unfetched | Catalog::Loading => {
         return vec![button_row(
            "Loading the plugin list...",
            ICON_PLUGIN,
            None,
            false,
         )];
      },
      Catalog::Failed(err) => {
         let mut line = log_line(&[(
            format!("The plugin list did not load: {err}"),
            Color::Error.argb(),
         )]);
         line.set(Prop::FONT_FAMILY, None);
         return vec![line];
      },
      Catalog::Loaded(listed) => listed,
   };
   listed
      .iter()
      .enumerate()
      .map(|(index, plugin)| {
         let on = plugins
            .iter()
            .any(|other| other.name == plugin.name && (other.installed || other.state.is_on()));
         let subtitle = plugin.warning.as_ref().map_or_else(
            || plugin.description.clone(),
            |warning| format!("{} \u{26A0} {warning}", plugin.description),
         );
         switch_row(
            Some(ICON_PLUGIN),
            &plugin.name,
            Some(&subtitle),
            on,
            Press::Listed(index),
            Press::Listed(index),
         )
      })
      .collect()
}

fn debug_section() -> Tree {
   let debug_rows = vec![
      switch_row(
         None,
         "Staff unlock",
         Some("Grant the staff flag to unlock Discord experiments"),
         phoenix_runtime::staff_enabled(),
         Press::ToggleStaff,
         Press::ToggleStaff,
      ),
      switch_row(
         None,
         "Remote debug shell",
         Some(
            "Listen on 127.0.0.1:4242 for ADB eval, logs & plugin hot-reload (adb forward \
             tcp:4242 tcp:4242)",
         ),
         phoenix_runtime::remote_shell_enabled(),
         Press::ToggleRemoteShell,
         Press::ToggleRemoteShell,
      ),
   ];
   section("Debug", debug_rows)
}

pub fn install_sections(status: Option<&(String, i64)>, plugins: &[PluginInfo]) -> Vec<Tree> {
   let mut sections = vec![
      section("From Sunbird", listed_rows(plugins)),
      section("From a URL", vec![text_field(
         Field::Install,
         Some(ICON_PLUGIN),
         "https://example.com/plugin.js",
         "",
         InputKind::Text,
      )]),
      section("From a file", vec![button_row(
         "Choose a .js file",
         ICON_FILE,
         Some(Press::PickFile),
         false,
      )]),
   ];
   if let Some((message, color)) = status {
      let mut line = log_line(&[(message.clone(), *color)]);
      line.set(Prop::FONT_FAMILY, None);
      line.set(Prop::FONT_SIZE, Some(Value::F64(14.0)));
      line.set(Prop::LINE_HEIGHT, Some(Value::F64(18.0)));
      sections.push(section("", vec![line]));
   }
   sections
}

/// Discord's alternate launcher icons, laid out like its own App Icon page.
pub fn icons_sections() -> Vec<Tree> {
   let current = phoenix_runtime::app_icon();
   let row = |(index, icon): (usize, &crate::icons::LauncherIcon)| {
      icon_row(
         icon.label,
         &crate::icons::preview(icon.resource),
         icon.id == current,
         Press::ChooseIcon(index),
      )
   };
   let limited = crate::icons::ICONS
      .iter()
      .enumerate()
      .filter(|(_, icon)| icon.id == "HexagonIcon")
      .map(row)
      .collect();
   let regular = crate::icons::ICONS
      .iter()
      .enumerate()
      .filter(|(_, icon)| icon.id != "HexagonIcon")
      .map(row)
      .collect();
   vec![section("Limited Time", limited), section("", regular)]
}
