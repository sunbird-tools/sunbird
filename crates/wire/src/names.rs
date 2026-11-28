//! The names of the Phoenix host calls, events and fields phoenix uses. Their
//! numbers change between Discord builds, so the runtime reads them at
//! startup, and CI checks a new build still has every one.

/// A host module's method, named `Module.method`.
#[derive(Clone, Copy)]
pub enum Method {
   NetworkingRequest,
   LinkingOpenUrl,
   SystemOverlayToast,
   ClipboardSetString,
   MediaPickerPick,
   MediaPickerRead,
   MediaPickerRelease,
   TimingSet,
   AppStateSetTheme,
   AppIconSet,
}

impl Method {
   pub const ALL: [Self; 10] = [
      Self::NetworkingRequest,
      Self::LinkingOpenUrl,
      Self::SystemOverlayToast,
      Self::ClipboardSetString,
      Self::MediaPickerPick,
      Self::MediaPickerRead,
      Self::MediaPickerRelease,
      Self::TimingSet,
      Self::AppStateSetTheme,
      Self::AppIconSet,
   ];

   #[must_use]
   pub const fn name(self) -> &'static str {
      match self {
         Self::NetworkingRequest => "Networking.request",
         Self::LinkingOpenUrl => "Linking.openURL",
         Self::SystemOverlayToast => "SystemOverlay.toast",
         Self::ClipboardSetString => "Clipboard.setString",
         Self::MediaPickerPick => "MediaPicker.pick",
         Self::MediaPickerRead => "MediaPicker.read",
         Self::MediaPickerRelease => "MediaPicker.release",
         Self::TimingSet => "Timing.set",
         Self::AppStateSetTheme => "AppState.setTheme",
         Self::AppIconSet => "AppIcon.set",
      }
   }

   #[must_use]
   pub fn module(self) -> &'static str {
      let name = self.name();
      name.split_once('.').map_or(name, |(module, _)| module)
   }
}

/// A host event Phoenix's Java side dispatches to the guest.
#[derive(Clone, Copy)]
pub enum Event {
   Http,
   Picked,
   Read,
   Timer,
   Ws,
}

impl Event {
   pub const ALL: [Self; 5] = [Self::Http, Self::Picked, Self::Read, Self::Timer, Self::Ws];

   #[must_use]
   pub const fn name(self) -> &'static str {
      match self {
         Self::Http => "@http",
         Self::Picked => "@picked",
         Self::Read => "@read",
         Self::Timer => "@timer",
         Self::Ws => "@ws",
      }
   }
}

/// A field of a host call's or event's arguments.
#[derive(Clone, Copy)]
pub enum Field {
   Id,
   Error,
   Method,
   Url,
   Headers,
   Status,
   Text,
   Kind,
   Max,
   Extensions,
   Files,
   Handle,
   Handles,
   Name,
   Denied,
   Delay,
   Dark,
   Surface,
   TextDefault,
   TextMuted,
   Ripple,
   Danger,
   Req,
}

impl Field {
   pub const ALL: [Self; 23] = [
      Self::Id,
      Self::Error,
      Self::Method,
      Self::Url,
      Self::Headers,
      Self::Status,
      Self::Text,
      Self::Kind,
      Self::Max,
      Self::Extensions,
      Self::Files,
      Self::Handle,
      Self::Handles,
      Self::Name,
      Self::Denied,
      Self::Delay,
      Self::Dark,
      Self::Surface,
      Self::TextDefault,
      Self::TextMuted,
      Self::Ripple,
      Self::Danger,
      Self::Req,
   ];

   #[must_use]
   pub const fn name(self) -> &'static str {
      match self {
         Self::Id => "id",
         Self::Error => "error",
         Self::Method => "method",
         Self::Url => "url",
         Self::Headers => "headers",
         Self::Status => "status",
         Self::Text => "text",
         Self::Kind => "kind",
         Self::Max => "max",
         Self::Extensions => "extensions",
         Self::Files => "files",
         Self::Handle => "handle",
         Self::Handles => "handles",
         Self::Name => "name",
         Self::Denied => "denied",
         Self::Delay => "delay",
         Self::Dark => "dark",
         Self::Surface => "surface",
         Self::TextDefault => "textDefault",
         Self::TextMuted => "textMuted",
         Self::Ripple => "ripple",
         Self::Danger => "danger",
         Self::Req => "req",
      }
   }
}

/// A host module whose calls phoenix matches by module number alone.
#[derive(Clone, Copy)]
pub enum Module {
   Haptics,
}

impl Module {
   pub const ALL: [Self; 1] = [Self::Haptics];

   #[must_use]
   pub const fn name(self) -> &'static str {
      match self {
         Self::Haptics => "Haptics",
      }
   }
}

/// Every name, laid out as the module and method of each [`Method`], then each
/// [`Event`], then each [`Field`], then each [`Module`].
#[must_use]
pub fn all() -> Vec<&'static str> {
   let mut names = Vec::new();
   for method in Method::ALL {
      names.extend([method.module(), method.name()]);
   }
   names.extend(Event::ALL.map(Event::name));
   names.extend(Field::ALL.map(Field::name));
   names.extend(Module::ALL.map(Module::name));
   names
}
