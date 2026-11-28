<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/assets/sunbird-dark.svg">
  <img src=".github/assets/sunbird-light.svg" width="88" alt="">
</picture>

# Sunbird

A client modification and plugin platform for Discord Phoenix.

[sunbird.tools](https://sunbird.tools/) · [Discord](https://discord.gg/jEmw3bC2m7) · [Plugin API](crates/runtime/phoenix.d.ts) · [License](LICENSE)

</div>

Sunbird is a client modification for Discord Phoenix, Discord's native Rust client. If you know Vencord or BetterDiscord, it is the same idea for Phoenix: plugins, quality-of-life changes, custom UI, and themes, managed from settings inside the client. Underneath is a sandboxed TypeScript runtime with its own typed plugin API.

Sunbird runs on Android, iOS, Windows, Linux, and macOS (Apple silicon). No root or Xposed is needed, and on desktop Discord's own files stay untouched, so a Discord update does not undo the mod.

Phoenix is still an unreleased alpha, so today's installers also deal with its access gate and fast-changing builds. That is pre-release plumbing around the mod, not the point of Sunbird. Alpha bugs and missing Phoenix features are not Sunbird bugs, and they are not worth reporting to Discord.

## Install

Open [sunbird.tools](https://sunbird.tools/) and pick your platform. The install method differs because Phoenix is packaged differently on each one:

- **Android (9+, arm64 and armv7):** the site builds a ready-to-install Sunbird APK in your browser, either as `com.discord` or side by side as `com.discord.phoenix`, with the plugins you pick (and, if you want them, custom plugins or a Frida gadget).
- **iOS:** the site prepares a Sunbird IPA from a decrypted Discord IPA in your browser, ready to sideload with a tool such as AltStore or Sideloadly.
- **Windows (x86-64):** run `SunbirdSetup.exe` from the site. It installs the Sunbird launcher, which starts Discord with the mod loaded and updates itself on every launch. It also adds a no-plugin fallback shortcut for Discord builds Sunbird has not caught up with yet.
- **Linux (x86-64):** run the command below. It installs the Sunbird launcher and app menu entry, plus the same no-plugin fallback.

  ```sh
  curl -fsSL https://assets.sunbird.tools/plugins/sunbird-linux.sh | sh
  ```

- **macOS (Apple silicon):** run the command below. It installs a separate Sunbird app that starts a re-signed copy of Discord Phoenix with the mod loaded, keeps the copy and loader up to date, and leaves Discord's own app untouched. The copy may ask you to sign in once.

  ```sh
  curl -fsSL https://assets.sunbird.tools/plugins/sunbird-macos.sh | sh
  ```

After installing, open Discord through Sunbird and go to Settings → Sunbird. There you can toggle plugins, change their settings, install plugins from a URL or a file, edit their code, and open the JavaScript console. On desktop, `Ctrl+Shift+I` opens DevTools (`Cmd+Shift+I` on macOS).

Like other client mods, Sunbird hooks client internals, and Phoenix is moving quickly while it is in alpha. If a new Discord build misbehaves, turn plugins off. On Windows and Linux, the no-plugin fallback starts Phoenix without the runtime until Sunbird catches up.

Desktop launchers check for a newer loader before they start Discord, and plugin updates are handled in Settings. To remove Sunbird on Windows, run `%LOCALAPPDATA%\Sunbird\sunbird.exe --uninstall`. On Linux or macOS, run the install command for your platform with `sh -s -- --uninstall` at the end. On mobile, remove the Sunbird app as you would any other app.

## Plugins

Plugins are how Sunbird changes Discord. Each one is a small TypeScript bundle that Sunbird runs in QuickJS with a typed `phoenix` object. A plugin can watch or change events as Discord handles them, add UI inside Discord, keep its own settings and data, and, only with permission, talk to Discord or the web.

Manage plugins in Settings → Sunbird. You can turn a plugin on or off, open its page to change settings and see what it can reach, edit its code, apply updates, or uninstall it. The catalog installs bundled plugins from [assets.sunbird.tools](https://assets.sunbird.tools/). You can also install a plugin from a local `.js` file or an HTTPS link to one, including a GitHub file or a gist. TypeScript sources need to be bundled to JavaScript first.

The bundled plugins live in [`plugins/`](plugins/). CI bundles them with esbuild ([`web/tools/bundle-plugins.ts`](web/tools/bundle-plugins.ts)) and publishes them to the same site, so installed copies can update in place.

<details>
<summary>Bundled plugins</summary>

**Messages and media**

| Plugin | What it does |
|---|---|
| [`clear-urls`](plugins/clear-urls) | Removes tracking parameters from links you send and receive |
| [`dearrow`](plugins/dearrow) | Replaces YouTube titles and thumbnails in embeds using DeArrow |
| [`fakenitro`](plugins/fakenitro) | Sends custom and animated emojis as links, and renders fake emojis, stickers, and GIFs as real ones |
| [`fix-embeds`](plugins/fix-embeds) | Rewrites X/Twitter, Instagram, TikTok, and custom links for working embeds |
| [`message-latency`](plugins/message-latency) | Flags messages with high send latency |
| [`message-logger`](plugins/message-logger) | Keeps deleted messages and edit history |
| [`no-reply-mention`](plugins/no-reply-mention) | Stops your replies from pinging the person you reply to |
| [`silent-message-toggle`](plugins/silent-message-toggle) | Sends messages without notifying anyone, always or when they start with a prefix |
| [`silent-typing`](plugins/silent-typing) | Prevents Discord from sending typing indicators |
| [`spoiler-mode`](plugins/spoiler-mode) | Adds a toggle to automatically spoiler everything you send |
| [`text-replace`](plugins/text-replace) | Rewrites your outgoing messages by find and replace rules |
| [`translate`](plugins/translate) | Translates messages from the message menu, or every message in selected channels, using Kagi or Google |
| [`view-raw`](plugins/view-raw) | Opens a message's raw text and JSON in a viewer |

**People and servers**

| Plugin | What it does |
|---|---|
| [`global-badges`](plugins/global-badges) | Shows user badges from other Discord client mods on profiles |
| [`member-count`](plugins/member-count) | Shows how many members a server has, and how many are online, in its tooltip in the server list (Desktop) |
| [`permissions-viewer`](plugins/permissions-viewer) | Shows someone's roles and permissions on a server and in a channel |
| [`platform-indicators`](plugins/platform-indicators) | Shows which devices people are online from: desktop, mobile, web or console |
| [`reviewdb`](plugins/reviewdb) | Adds a ReviewDB tab to user profiles |
| [`show-timeout-duration`](plugins/show-timeout-duration) | Shows remaining timeout duration beside usernames |

**Interface**

| Plugin | What it does |
|---|---|
| [`collapse-channels`](plugins/collapse-channels) | Collapses the channel list to the server rail and brings it back, from a header button or a shortcut |
| [`desktop-themes`](plugins/desktop-themes) | Live `.theme.css` themes on Desktop Phoenix |
| [`hide-composer-buttons`](plugins/hide-composer-buttons) | Hides extra buttons in the message bar (Gift, Voice Message, etc.) |
| [`no-blocked-messages`](plugins/no-blocked-messages) | Hides messages from blocked (and optionally ignored) users completely |
| [`no-haptics`](plugins/no-haptics) | Disables haptic feedback and vibration across the app |
| [`no-pending-count`](plugins/no-pending-count) | Hides the badge counts of incoming friend requests and message requests (Desktop) |
| [`no-profile-themes`](plugins/no-profile-themes) | Strips profile themes, effects, avatar decorations, and name styles |
| [`taptap`](plugins/taptap) | Double-tap a message to reply, or your own to edit |

</details>

## Writing a plugin

Start from [`plugins/example`](plugins/example/src/index.ts) for a tour of the API, or write one small bundle of your own. The plugin below adds a word count under long messages. It has two settings, and it changes the message only in this client; nothing extra is sent to Discord.

```ts
// @description Adds a word count below long messages
// @version 1.0.0
// @author you
// @permissions gateway

const settings = phoenix.settings.define({
  minWords: {
    type: "number",
    title: "Minimum words",
    description: "Only mark messages with at least this many words",
    default: 80,
  },
  includeBots: {
    type: "switch",
    title: "Include bots",
    default: false,
  },
});

phoenix.hook(phoenix.gateway.messageCreate, {
  before(message) {
    if (!settings.includeBots && message.author.bot) return;

    const words = message.content.trim().split(/\s+/).filter(Boolean).length;
    const minimum = Math.max(1, Math.round(settings.minWords));
    if (words < minimum) return;

    message.content += `\n-# ${words} words`;
  },
});
```

The settings appear on the plugin's page in Sunbird Settings. Read them where you use them, as above, because each property returns the user's current choice. Use `phoenix.on(...)` when a plugin only needs to observe an event after Phoenix handles it; use `phoenix.hook(...)` when it needs to change the payload in `before`, drop it by returning `false`, or run in `after` with the result Phoenix saw.

### Plugin files

An installed plugin is one JavaScript file. Sunbird takes its name from the file name, or from the folder for entry files such as `index.js`. The `// @` lines at the top give its description, version, author, and permissions. The example above has no TypeScript-only syntax, so it can be saved as `word-count.js` and installed from a file as written. Other header lines can start a plugin turned off (`// @enabled false`) or name the oldest Sunbird it needs (`// @min-sunbird ...`).

Plugins in this repository are written in TypeScript instead:

```text
plugins/my-plugin/
  manifest.ts
  src/index.ts
```

`manifest.ts` carries the same information in typed form, and the bundler turns it into the header lines:

```ts
export default {
  description: "Adds a word count below long messages",
  version: "1.0.0",
  author: "you",
  permissions: ["gateway"],
} satisfies Phoenix.Manifest;
```

Run `node web/tools/bundle-plugins.ts --out <dir> my-plugin` to bundle one plugin for testing. On Windows, with the local debug shell enabled for the target client, `.\scripts\dev.ps1 -HotReload my-plugin` bundles it and reloads it through that shell. Leave that shell off outside development, because it can run code with your Discord session. For ordinary debugging, `phoenix.log` writes to the Console in Sunbird Settings.

### The API at a glance

[`crates/runtime/phoenix.d.ts`](crates/runtime/phoenix.d.ts) is the full reference. Most plugins only need a small part of it:

| API | Use it for |
|---|---|
| `phoenix.gateway`, `phoenix.on`, `phoenix.hook` | Watching or changing gateway events as Phoenix handles them. |
| `phoenix.ui` | Buttons, message accessories and actions, decorations, server tooltips, profile content, dialogs, pages, notifications, clipboard, and toasts. |
| `phoenix.commands` | Slash commands in the message box. |
| `phoenix.messages` | Local-only messages and interceptors for text you send. |
| `phoenix.settings`, `phoenix.storage`, `phoenix.log` | Settings controls, per-plugin JSON storage, and log lines. |
| `phoenix.stores` | Read-only SQL over Phoenix's local cache. |
| `phoenix.discord` | Discord API calls in your session, plus request and response interceptors. |
| `phoenix.fetch` | HTTP requests to other hosts. |
| `phoenix.bridge` | Android host modules by number, for low-level work. |
| `phoenix.themes` | `.theme.css` themes on Desktop Phoenix, when the current build supports them. |

Good bundled plugins to read next are [`silent-typing`](plugins/silent-typing/src/index.ts) for a four-line request interceptor, [`message-latency`](plugins/message-latency/src/index.ts) for settings with a gateway hook, [`hide-composer-buttons`](plugins/hide-composer-buttons/src/index.ts) for settings that add and remove UI as they change, and [`view-raw`](plugins/view-raw/src/index.ts) for a message action that opens a page.

### Permissions

Declare only what a plugin uses. Settings, storage, logging, commands, and high-level UI need no permission. Everything else is left out of the plugin's `phoenix` object or refused until it is declared in `// @permissions` (or `permissions` in `manifest.ts`):

- `gateway`: gateway events, and the message cache through `stores`
- `discord`: Discord API requests made as you, and request and response interceptors
- `http`: `fetch` to any host, and images and links from any host
- `messages`: reading and rewriting message and composer text through `messages`
- `hooks`: host modules through `bridge`, the raw view tree, and outgoing gateway frames through `gateway.onSend`. It includes everything above.

Each plugin page in Settings lists what the plugin can reach. An update that asks for more than the copy you have is refused until you remove the plugin and install it again.

A few notes from the type file are worth knowing before you start:

- Some members are optional or only exist on one kind of client. They are marked with `?` in `phoenix.d.ts`, so check for them before calling them.
- Use a setting with `type: "secret"` for API keys and tokens. It is shown masked and kept out of the plugin's log lines.
- A replacement gateway payload is turned back into JSON, so numbers above 2^53 lose precision. Discord IDs in the typed payloads are strings.
- Hooks and settings are removed when a plugin is turned off, reloaded, or uninstalled. If a plugin starts anything else, undo it in `phoenix.onUnload(...)` or with the remover a registration returned.
- QuickJS is not a browser. Do not assume DOM or web globals such as `URLSearchParams` exist.

## How it works

- **Loader:** [`crates/loader`](crates/loader) builds the native library that starts Sunbird inside Discord: `libplm.so` in the Android APK through a renamed `DT_NEEDED`, a dylib in the iOS app, `phoenix.dll` from the Windows launcher, `libphoenix.so` through `LD_PRELOAD` on Linux, and `libphoenix.dylib` through `DYLD_INSERT_LIBRARIES` in the signed macOS copy.
- **Runtime:** [`crates/runtime`](crates/runtime) starts QuickJS, gives each plugin its permission-scoped `phoenix` object, and evaluates the bundled and installed plugin files. The mobile and desktop runtime crates provide the platform settings, UI, and transport pieces behind that API.
- **Hooks:** gateway events are hooked by walking Phoenix's event registry by name and swapping handler pointers. On mobile, Sunbird works through Phoenix's UI-op bridge, with [sunhook](https://github.com/sunbird-tools/sunhook) inline hooks on Android and vtable swaps on iOS. On desktop it hooks GPUI builders, `libdiscord/crates/http` vtables, and native settings directly.
- **Pre-release builds:** Phoenix is currently gated and changes often. Sunbird finds the gate and its other targets from strings and code shape rather than fixed offsets, patches the gate in current builds, and ships no-plugin desktop fallbacks for builds it has not caught up with yet.
- **Signing:** Android builds are signed with the key in [`crates/patcher/keys/`](crates/patcher/keys/) using APK Signature Scheme v3, so updates install over each other and you stay signed in.
- **Updates:** CI publishes loaders and plugin bundles to assets.sunbird.tools with their checksums. Desktop launchers fetch the current loader before launch, and Settings applies plugin updates in place.

## Building from source

Most people only need the site above. To build Sunbird itself, start with the layout, then the command for your platform.

<details>
<summary>Repository layout</summary>

| Path | What |
|---|---|
| `crates/loader` | The library Discord loads (`libplm.so`, `phoenix.dll`, `libphoenix.so`, `libphoenix.dylib`). |
| `crates/runtime` | The shared QuickJS plugin runtime and the `phoenix` API. |
| `crates/runtime-mobile` | Android and iOS: the UI-op bridge, Settings pages, and UI patches. |
| `crates/runtime-desktop` | Desktop: GPUI hooks, HTTP transport hooks, Sunbird pages in User Settings, and the tray. |
| `crates/inject` | The Windows launcher (`sunbird-windows-x86_64.exe`). |
| `crates/hook`, `crates/sigs`, `crates/sigscan` | Native hooking, build-independent binary locators, and the `sigscan` CLI. |
| `crates/ui-mobile`, `crates/wire` | Mobile UI-op and bridge frame codecs. |
| `crates/ui-desktop` | Desktop host primitives: clipboard, URLs, notifications, tray, and GPUI surface hooks. |
| `crates/patcher`, `crates/patcher-web` | The APK and IPA patcher CLI, and its wasm build for the site. |
| `crates/stand-in*` | A fake gateway library for testing hooks without Discord. |
| `docs/` | Phoenix schema generated from the documented mobile build. |
| `plugins/` | Bundled TypeScript plugins. |
| `scripts/` | Desktop installers and dev build helpers. |
| `web/` | Assets CI deploys to assets.sunbird.tools. |
| `frida/` | Frida gadget bootstrap. |

</details>

<details>
<summary>Build commands</summary>

- **Android (Linux with Nix):**

  ```sh
  nix develop
  nu scripts/phoenix.nu base.apk config.arm64_v8a.apk config.xxhdpi.apk config.en.apk
  ```

  Pass `config.armeabi_v7a.apk` instead of the arm64 split for a 32-bit phone. This builds the loader and the patcher, bundles the plugins, patches the splits, and installs them with `adb`.

- **Android (Windows):** `.\scripts\dev.ps1` (needs `cargo-ndk`, the Android SDK and NDK, and a JDK).
- **Windows desktop:** `.\scripts\dev.ps1 -Desktop` for the dev loop, or `cargo build --release -p phoenix-loader -p phoenix-inject` for `phoenix.dll` and the launcher.
- **Linux desktop:** `cargo build --release -p phoenix-loader` (`target/release/libphoenix.so`).
- **macOS desktop:** `cargo build --release -p phoenix-loader` (`target/release/libphoenix.dylib`).
- **iOS:** built on macOS with Xcode for `aarch64-apple-ios` (see [`.github/workflows/ios.yml`](.github/workflows/ios.yml)).
- **Plugins and site assets:** `node web/tools/bundle-plugins.ts` and `nu scripts/site.nu web`.

The site itself lives in [sunbird-tools/site](https://github.com/sunbird-tools/site).

</details>

## Help and contributing

Questions and bug reports are welcome in [Discord](https://discord.gg/jEmw3bC2m7) and [GitHub Issues](https://github.com/sunbird-tools/sunbird/issues). For a bug report, include your platform, the Sunbird version shown in Settings, the plugin involved if there is one, and what you did before it happened. Logs help, but remove tokens, message content, and anything private first.

If you are changing code, read [AGENTS.md](AGENTS.md) first. It is the detailed map of the crates, build and debug workflows, signing and privacy rules, and code style. For Phoenix internals, start with the generated schema in [`docs/phoenix-356207.md`](docs/phoenix-356207.md) before decompiling anything again.

## Contributors

<a href="https://github.com/sunbird-tools/sunbird/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=sunbird-tools/sunbird" alt="Contributors" />
</a>

<sub>Licensed under [EUPL-1.2](LICENSE). Not affiliated with Discord.</sub>
