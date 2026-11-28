//! Discord's built-in alternate launcher icons, exposed in Sunbird Settings.

#[cfg(target_os = "android")] use std::ffi::CString;

pub struct LauncherIcon {
   pub id:       &'static str,
   pub label:    &'static str,
   pub resource: &'static str,
}

/// The icons this Discord build ships as disabled activity aliases, in the
/// order Phoenix's own `AppIcon` module uses. Treat, Trick and Zombie have
/// aliases but no shipped icon resources here.
pub const ICONS: &[LauncherIcon] = &[
   LauncherIcon {
      id:       "AppIcon",
      label:    "Default",
      resource: "images_native_custom_app_icons_appicon_appicon",
   },
   LauncherIcon {
      id:       "HexagonIcon",
      label:    "Vice City Theme",
      resource: "images_native_custom_app_icons_hexagonicon_hexagonicon",
   },
   LauncherIcon {
      id:       "BrandInvertedIcon",
      label:    "Brand Inverted",
      resource: "images_native_custom_app_icons_brandinvertedicon_brandinvertedicon",
   },
   LauncherIcon {
      id:       "MatteDarkIcon",
      label:    "Goth",
      resource: "images_native_custom_app_icons_mattedarkicon_mattedarkicon",
   },
   LauncherIcon {
      id:       "MatteLightIcon",
      label:    "Matte Light",
      resource: "images_native_custom_app_icons_mattelighticon_mattelighticon",
   },
   LauncherIcon {
      id:       "BrandDarkIcon",
      label:    "Brand Dark",
      resource: "images_native_custom_app_icons_branddarkicon_branddarkicon",
   },
   LauncherIcon {
      id:       "PastelIcon",
      label:    "Pastel",
      resource: "images_native_custom_app_icons_pastelicon_pastelicon",
   },
   LauncherIcon {
      id:       "PirateIcon",
      label:    "Pirate",
      resource: "images_native_custom_app_icons_pirateicon_pirateicon",
   },
   LauncherIcon {
      id:       "CamoIcon",
      label:    "Camo",
      resource: "images_native_custom_app_icons_camoicon_camoicon",
   },
   LauncherIcon {
      id:       "SunsetIcon",
      label:    "Sunset",
      resource: "images_native_custom_app_icons_sunseticon_sunseticon",
   },
   LauncherIcon {
      id:       "GalaxyIcon",
      label:    "Galaxy",
      resource: "images_native_custom_app_icons_galaxyicon_galaxyicon",
   },
   LauncherIcon {
      id:       "Y2KIcon",
      label:    "Y2K",
      resource: "images_native_custom_app_icons_y2kicon_y2kicon",
   },
   LauncherIcon {
      id:       "CherryBlossomIcon",
      label:    "Cherry Blossom",
      resource: "images_native_custom_app_icons_cherryblossomicon_cherryblossomicon",
   },
   LauncherIcon {
      id:       "BeanieIcon",
      label:    "Beanie",
      resource: "images_native_custom_app_icons_beanieicon_beanieicon",
   },
   LauncherIcon {
      id:       "GamingIcon",
      label:    "Gaming",
      resource: "images_native_custom_app_icons_gamingicon_gamingicon",
   },
   LauncherIcon {
      id:       "CircuitIcon",
      label:    "Circuit",
      resource: "images_native_custom_app_icons_circuiticon_circuiticon",
   },
   LauncherIcon {
      id:       "HoloWavesIcon",
      label:    "Holo Waves",
      resource: "images_native_custom_app_icons_holowavesicon_holowavesicon",
   },
   LauncherIcon {
      id:       "BlushIcon",
      label:    "Blush",
      resource: "images_native_custom_app_icons_blushicon_blushicon",
   },
   LauncherIcon {
      id:       "AngryIcon",
      label:    "Angry",
      resource: "images_native_custom_app_icons_angryicon_angryicon",
   },
   LauncherIcon {
      id:       "MangaIcon",
      label:    "Manga",
      resource: "images_native_custom_app_icons_mangaicon_mangaicon",
   },
   LauncherIcon {
      id:       "ControllerIcon",
      label:    "Controller",
      resource: "images_native_custom_app_icons_controllericon_controllericon",
   },
   LauncherIcon {
      id:       "MushroomIcon",
      label:    "Mushroom",
      resource: "images_native_custom_app_icons_mushroomicon_mushroomicon",
   },
   LauncherIcon {
      id:       "BlurpleTwilightIcon",
      label:    "Blurple Twilight",
      resource: "images_native_custom_app_icons_blurpletwilighticon_blurpletwilighticon",
   },
   LauncherIcon {
      id:       "InRainbowsIcon",
      label:    "In Rainbows",
      resource: "images_native_custom_app_icons_inrainbowsicon_inrainbowsicon",
   },
   LauncherIcon {
      id:       "MidnightPrismIcon",
      label:    "Midnight Prism",
      resource: "images_native_custom_app_icons_midnightprismicon_midnightprismicon",
   },
   LauncherIcon {
      id:       "ColorWaveIcon",
      label:    "Color Wave",
      resource: "images_native_custom_app_icons_colorwaveicon_colorwaveicon",
   },
];

/// The same preview image Phoenix's own App Icon settings row uses.
pub fn preview(resource: &str) -> String {
   format!("res://{resource}")
}

#[cfg(target_os = "android")]
fn suffix(icon: &LauncherIcon) -> &str {
   if icon.id == "AppIcon" {
      "Default"
   } else {
      icon.id.strip_suffix("Icon").unwrap_or(icon.id)
   }
}

#[cfg(target_os = "android")]
#[expect(
   clippy::too_many_lines,
   reason = "JNI setup and error checks stay beside each call"
)]
fn set_components(selected: &LauncherIcon) -> Result<(), String> {
   use jni_sys::jvalue;

   use crate::jvm::{
      clear_exception,
      env,
      jni,
      method,
      take_exception,
   };

   let Some(env) = env() else {
      return Err("no Java VM".to_owned());
   };
   // SAFETY: `env` belongs to this thread, every reference stays in the local
   // frame pushed below, and Java exceptions are checked after each JNI call.
   unsafe {
      if jni!(env, PushLocalFrame, 64) != 0 {
         clear_exception(env);
         return Err("no JNI local frame".to_owned());
      }
      let result = (|| {
         let thread_class = jni!(env, FindClass, c"android/app/ActivityThread".as_ptr());
         if thread_class.is_null() {
            return Err(take_exception(env).unwrap_or_else(|| "ActivityThread missing".to_owned()));
         }
         let current_application = crate::jvm::static_method(
            env,
            thread_class,
            c"currentApplication",
            c"()Landroid/app/Application;",
         )
         .map_err(|err| err.to_string())?;
         let context = jni!(
            env,
            CallStaticObjectMethodA,
            thread_class,
            current_application,
            std::ptr::null()
         );
         if context.is_null() || jni!(env, ExceptionCheck) {
            return Err(take_exception(env).unwrap_or_else(|| "Application missing".to_owned()));
         }
         let context_class = jni!(env, GetObjectClass, context);
         let get_pm = method(
            env,
            context_class,
            c"getPackageManager",
            c"()Landroid/content/pm/PackageManager;",
         )
         .map_err(|err| err.to_string())?;
         let pm = jni!(env, CallObjectMethodA, context, get_pm, std::ptr::null());
         if pm.is_null() || jni!(env, ExceptionCheck) {
            return Err(take_exception(env).unwrap_or_else(|| "PackageManager missing".to_owned()));
         }
         let component_class = jni!(env, FindClass, c"android/content/ComponentName".as_ptr());
         if component_class.is_null() {
            return Err(take_exception(env).unwrap_or_else(|| "ComponentName missing".to_owned()));
         }
         let init = method(
            env,
            component_class,
            c"<init>",
            c"(Landroid/content/Context;Ljava/lang/String;)V",
         )
         .map_err(|err| err.to_string())?;
         let pm_class = jni!(env, GetObjectClass, pm);
         let set_enableds = method(
            env,
            pm_class,
            c"setComponentEnabledSettings",
            c"(Ljava/util/List;)V",
         )
         .map_err(|err| err.to_string())?;
         let setting_class = jni!(
            env,
            FindClass,
            c"android/content/pm/PackageManager$ComponentEnabledSetting".as_ptr()
         );
         if setting_class.is_null() {
            return Err(
               take_exception(env).unwrap_or_else(|| "ComponentEnabledSetting missing".to_owned()),
            );
         }
         let setting_init = method(
            env,
            setting_class,
            c"<init>",
            c"(Landroid/content/ComponentName;II)V",
         )
         .map_err(|err| err.to_string())?;
         let list_class = jni!(env, FindClass, c"java/util/ArrayList".as_ptr());
         if list_class.is_null() {
            return Err(take_exception(env).unwrap_or_else(|| "ArrayList missing".to_owned()));
         }
         let list_init =
            method(env, list_class, c"<init>", c"()V").map_err(|err| err.to_string())?;
         let list_add = method(env, list_class, c"add", c"(Ljava/lang/Object;)Z")
            .map_err(|err| err.to_string())?;
         let settings = jni!(env, NewObjectA, list_class, list_init, std::ptr::null());
         if settings.is_null() || jni!(env, ExceptionCheck) {
            return Err(take_exception(env).unwrap_or_else(|| "ArrayList failed".to_owned()));
         }
         for icon in ICONS {
            let class = format!("com.discord.main.Main{}", suffix(icon));
            let class = CString::new(class).map_err(|_| "class name has a NUL".to_owned())?;
            let class = jni!(env, NewStringUTF, class.as_ptr());
            let args = [jvalue { l: context }, jvalue { l: class }];
            let component = jni!(env, NewObjectA, component_class, init, args.as_ptr());
            jni!(env, DeleteLocalRef, class);
            if component.is_null() || jni!(env, ExceptionCheck) {
               return Err(
                  take_exception(env).unwrap_or_else(|| "ComponentName failed".to_owned()),
               );
            }
            let state = if icon.id == selected.id {
               1
            } else if icon.id == "AppIcon" {
               2
            } else {
               0
            };
            // Android 16 rolls launcher aliases back while their process
            // survives.
            let args = [jvalue { l: component }, jvalue { i: state }, jvalue {
               i: 0,
            }];
            let setting = jni!(env, NewObjectA, setting_class, setting_init, args.as_ptr());
            jni!(env, DeleteLocalRef, component);
            if setting.is_null() || jni!(env, ExceptionCheck) {
               return Err(
                  take_exception(env)
                     .unwrap_or_else(|| "ComponentEnabledSetting failed".to_owned()),
               );
            }
            let args = [jvalue { l: setting }];
            jni!(env, CallBooleanMethodA, settings, list_add, args.as_ptr());
            jni!(env, DeleteLocalRef, setting);
            if jni!(env, ExceptionCheck) {
               return Err(
                  take_exception(env).unwrap_or_else(|| "ArrayList.add failed".to_owned()),
               );
            }
         }
         let args = [jvalue { l: settings }];
         jni!(env, CallVoidMethodA, pm, set_enableds, args.as_ptr());
         if jni!(env, ExceptionCheck) {
            return Err(
               take_exception(env)
                  .unwrap_or_else(|| "setComponentEnabledSettings failed".to_owned()),
            );
         }
         Ok(())
      })();
      jni!(env, PopLocalFrame, std::ptr::null_mut());
      result
   }
}

#[cfg(target_os = "ios")]
fn set_components(_selected: &LauncherIcon) -> Result<(), String> {
   Err("launcher icons are Android-only for now".to_owned())
}

pub fn choose(index: usize) {
   let Some(icon) = ICONS.get(index) else {
      return;
   };
   let result = set_components(icon).or_else(|_| {
      crate::host::send(
         crate::host::Method::AppIconSet,
         &[
            (
               crate::host::Field::Id,
               phoenix_wire::Value::Str(icon.id.to_owned()),
            ),
            (crate::host::Field::Req, phoenix_wire::Value::I32(1)),
         ],
         Vec::new(),
      )
      .map_err(|err| err.to_string())
   });
   match result {
      Ok(()) => {
         phoenix_runtime::set_app_icon(icon.id);
         log::info!("launcher icon: {}", icon.id);
      },
      Err(err) => log::error!("launcher icon: {err}"),
   }
}
