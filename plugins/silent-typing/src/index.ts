const KEY = "enabled";
const TINT_ON = 0xff5865f2;
// Desktop draws composer icons from SVG, a keyboard. Phones have no Mana keyboard icon, so they keep the setting.
const DESKTOP = phoenix.platform === "windows" || phoenix.platform === "linux" || phoenix.platform === "macos";
// A filled glyph filling the chat bar's 20px box, a little past Mana's 16px
// since an outline reads lighter, or the same struck through with a gap cut
// around the line, as Discord draws its off states.
const glyph = (d: string, struck: boolean) =>
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g>' +
  (struck
    ? '<defs><mask id="cut"><rect width="24" height="24" fill="#fff"/><path d="M3 3 21 21" stroke="#000" stroke-width="5" stroke-linecap="round"/></mask></defs><g mask="url(#cut)">'
    : "<g>") +
  `<path fill="#000" fill-rule="evenodd" d="${d}"/></g>` +
  (struck ? '<path d="M3 3 21 21" stroke="#000" stroke-width="2" stroke-linecap="round"/>' : "") +
  "</g></svg>";
const KEYBOARD =
  "M3.5 4.5h17a2.5 2.5 0 0 1 2.5 2.5v10a2.5 2.5 0 0 1-2.5 2.5h-17A2.5 2.5 0 0 1 1 17V7a2.5 2.5 0 0 1 2.5-2.5Z" +
  // Five keys, four keys offset under them, then the space bar.
  "M4 7.5h2v2H4Z M7.5 7.5h2v2h-2Z M11 7.5h2v2h-2Z M14.5 7.5h2v2h-2Z M18 7.5h2v2h-2Z" +
  "M5.75 11h2v2h-2Z M9.25 11h2v2h-2Z M12.75 11h2v2h-2Z M16.25 11h2v2h-2Z" +
  "M7 14.5h10v2H7Z";
// Struck through while typing is hidden, the way others see it.
const icon = (on: boolean) => glyph(KEYBOARD, on);

let on = phoenix.storage.get<boolean>(KEY) ?? true;

const look = () => ({
  icon: icon(on),
  label: on ? "Silent typing (on)" : "Silent typing (off)",
  tint: on ? TINT_ON : null,
});

const button = DESKTOP
  ? phoenix.ui.addComposerButton({
      ...look(),
      tint: on ? TINT_ON : undefined,
      onPress() {
        on = !on;
        phoenix.storage.set(KEY, on);
        button?.update(look());
      },
    })
  : undefined;

const settings = phoenix.settings.define({
  enabled: {
    type: "switch",
    title: "Silent typing",
    description: "Keep others from seeing that you are typing",
    default: on,
    onChange(value) {
      on = value;
      phoenix.storage.set(KEY, on);
      button?.update(look());
    },
  },
});

on = settings.enabled;

phoenix.discord.onRequest(
  { method: "POST", pathPrefix: "/channels/", pathSuffix: "/typing" },
  () => (on ? false : undefined),
);
