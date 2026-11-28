// Like Vencord's MemberCount: a server's online and total members at the
// bottom of its tooltip in the server list, a green dot then a grey ring.
//
// The total comes from the client's own store. The online count needs a
// request, so it is asked for only when the pointer stays on a server, and kept
// for a few minutes.

interface Counts {
  members?: number;
  online?: number;
  /** When `online` came, 0 before it has. */
  onlineAt: number;
}

/** How long an online count is kept before the tooltip asks again. */
const FRESH_MS = 5 * 60_000;
/** How long the pointer stays on a server before its online count is asked for. */
const HOVER_MS = 1000;

/** Discord's online and offline status colors. */
const ONLINE = 0xff23a55a;
const OFFLINE = 0xff80848e;
const DOT =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="7"/></svg>';
const RING =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill-rule="evenodd" d="M12 5a7 7 0 1 0 0 14a7 7 0 1 0 0-14zm0 2.6a4.4 4.4 0 1 1 0 8.8a4.4 4.4 0 1 1 0-8.8z"/></svg>';

/**
 * A no-break space. French takes it too, since Discord's font draws the narrow
 * one with no width.
 */
const NBSP = String.fromCharCode(0xa0);

/** Thousands separators by language, for the user's Discord language. */
const SEPARATORS: Record<string, string> = {
  fr: NBSP,
  cs: NBSP,
  fi: NBSP,
  hu: NBSP,
  no: NBSP,
  pl: NBSP,
  ru: NBSP,
  sv: NBSP,
  uk: NBSP,
  bg: NBSP,
  da: ".",
  de: ".",
  el: ".",
  es: ".",
  hr: ".",
  id: ".",
  it: ".",
  lt: ".",
  nl: ".",
  pt: ".",
  ro: ".",
  tr: ".",
  vi: ".",
};

/** Each server's counts, so its tooltip shows them at once the next time. */
const counts = new Map<string, Counts>();
/** Servers being counted, so a tooltip opening again does not count twice. */
const counting = new Set<string>();
let separator = ",";
/** Whether `separator` follows Discord's language yet. */
let localized = false;

/** A number field of `record`, or null. */
function numberOf(record: unknown, key: string): number | null {
  if (typeof record !== "object" || record === null) return null;
  const value = (record as Record<string, unknown>)[key];
  return typeof value === "number" ? value : null;
}

const grouped = (n: number) => String(n).replace(/\B(?=(\d{3})+(?!\d))/g, separator);

const tooltip = phoenix.ui.addServerTooltip?.({
  // Runs when a server's tooltip opens, and again on `update`.
  render({ guildId }) {
    const found = counts.get(guildId);
    if (!counting.has(guildId) && Date.now() - (found?.onlineAt ?? 0) > FRESH_MS) {
      void count(guildId);
    }
    if (found?.members === undefined) return null;
    const total = [
      { icons: [{ svg: RING, color: OFFLINE }] },
      { text: grouped(found.members), color: OFFLINE },
    ];
    if (found.online === undefined) return total;
    return [
      { icons: [{ svg: DOT, color: ONLINE }] },
      { text: grouped(found.online), color: ONLINE },
      // A wider gap between the two counts than within each.
      { text: "  " },
      ...total,
    ];
  },
});

/**
 * Discord's language, for the thousands separator, from the client itself.
 * The client is up once a tooltip opens, not yet when the plugin loads.
 */
async function localize(): Promise<void> {
  const answer = await phoenix.discord.command("get_locale");
  const locale = (answer as { locale?: unknown } | null)?.locale;
  const language = typeof locale === "string" ? locale.split("-")[0] : "";
  separator = SEPARATORS[language] ?? ",";
  localized = true;
}

/**
 * The total from the client's own store at once, then the online count from
 * the server's profile, which Discord's own Server Profile settings read, only
 * once the pointer stays on the server for {@link HOVER_MS}.
 */
async function count(guildId: string): Promise<void> {
  counting.add(guildId);
  try {
    if (!localized) await localize();
    const local = await phoenix.discord.command("guild_member_counts", { guild_id: guildId });
    const found = counts.get(guildId) ?? { onlineAt: 0 };
    found.members = numberOf(local, "members") ?? found.members;
    // The store knows who is online only in servers whose member list is open.
    const online = numberOf(local, "online");
    if (online !== null) {
      found.online = online;
      found.onlineAt = Date.now();
    }
    counts.set(guildId, found);
    tooltip?.update?.(guildId);
    if (online !== null) return;

    await phoenix.sleep(HOVER_MS);
    if (tooltip?.shown?.() !== guildId) return;
    const res = await phoenix.discord.get(`/guilds/${guildId}/profile`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const profile = res.json<unknown>();
    found.members = numberOf(profile, "member_count") ?? found.members;
    found.online = numberOf(profile, "online_count") ?? found.online;
    found.onlineAt = Date.now();
    tooltip?.update?.(guildId);
  } catch (err) {
    phoenix.log.warn(`member-count: ${err}`);
  } finally {
    counting.delete(guildId);
  }
}

if (!tooltip) phoenix.log.info("member-count: this Sunbird can't add to server tooltips");
