const KEY = "enabled";
const LIMIT = 2000;
// Desktop draws composer icons from SVG, not Mana's `res://` names: an eye,
// struck through while spoiler mode is off.
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
const EYE =
  "M12 4.5C6.9 4.5 3 7.9 1.3 11.3a1.6 1.6 0 0 0 0 1.4C3 16.1 6.9 19.5 12 19.5s9-3.4 10.7-6.8a1.6 1.6 0 0 0 0-1.4C21 7.9 17.1 4.5 12 4.5Z" +
  "M12 16.5a4.5 4.5 0 1 1 0-9 4.5 4.5 0 0 1 0 9Z" +
  "M12 14.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Z";
const icon = (on: boolean) => (DESKTOP ? glyph(EYE, !on) : "res://ic_mana_image_spoiler");
const TINT_ON = 0xff5865f2;

let on = phoenix.storage.get<boolean>(KEY) ?? false;

const syncButton = () => {
  button.update({
    icon: icon(on),
    label: on ? "Spoiler mode (on)" : "Spoiler mode (off)",
    tint: on ? TINT_ON : null,
  });
};

const settings = phoenix.settings.define({
  enabled: {
    type: "switch",
    title: "Spoiler mode",
    description: "Wrap outgoing messages and attachments in spoilers automatically",
    default: on,
    onChange(value) {
      on = value;
      phoenix.storage.set(KEY, on);
      syncButton();
    },
  },
});

on = settings.enabled;

const wrapText = (text: string): string | undefined => {
  if (!text.trim() || text.includes("||")) return;
  if (text.length + 4 > LIMIT) {
    phoenix.ui.toast("Message too long to wrap in spoilers");
    return;
  }
  return `||${text}||`;
};

const button = phoenix.ui.addComposerButton({
  icon: icon(on),
  label: on ? "Spoiler mode (on)" : "Spoiler mode (off)",
  tint: on ? TINT_ON : undefined,
  onPress() {
    on = !on;
    phoenix.storage.set(KEY, on);
    syncButton();
  },
});

phoenix.messages.onSend((text) => {
  if (!on) return;
  return wrapText(text);
});

phoenix.discord.onRequest(
  { method: "POST", pathPrefix: "/channels/", pathSuffix: "/messages" },
  (req) => {
    if (!on) return;
    const payload = req.json<Phoenix.SendPayload>();
    if (!payload) return;
    let changed = false;
    if (typeof payload.content === "string") {
      const wrapped = wrapText(payload.content);
      if (wrapped !== undefined) {
        payload.content = wrapped;
        changed = true;
      }
    }
    if (Array.isArray(payload.attachments)) {
      for (const attachment of payload.attachments) {
        if (typeof attachment.filename === "string" && !attachment.filename.startsWith("SPOILER_")) {
          attachment.filename = `SPOILER_${attachment.filename}`;
          changed = true;
        }
      }
    }
    return changed ? { body: payload } : undefined;
  },
);
