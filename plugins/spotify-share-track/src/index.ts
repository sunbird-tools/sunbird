interface ActivitySession {
  activities?: Array<{
    name?: string;
    type?: number;
    sync_id?: string;
    timestamps?: { end?: number };
  }>;
}

let track: { id: string; end: number } | undefined;
const noTrack = "No current Spotify track was found in your Discord activity.";

function readSessions(payload: unknown): void {
  track = undefined;
  if (!Array.isArray(payload)) return;

  const now = Date.now();
  for (const session of payload as ActivitySession[]) {
    if (!Array.isArray(session?.activities)) continue;
    for (const activity of session.activities) {
      const id = activity?.sync_id;
      const end = activity?.timestamps?.end;
      if (activity?.name === "Spotify" && activity.type === 2
          && typeof id === "string" && /^[A-Za-z0-9]{22}$/.test(id)
          && typeof end === "number" && Number.isFinite(end) && end > now) {
        track = { id, end };
        return;
      }
    }
  }
}

if (phoenix.gateway.ready) {
  phoenix.on(phoenix.gateway.ready, (payload) => {
    readSessions((payload as Phoenix.Ready & { sessions?: unknown }).sessions);
  });
}

const sessionsReplace = phoenix.gateway.sessionsReplace;
if (sessionsReplace) phoenix.on(sessionsReplace, readSessions);

function trackUrl(): string | undefined {
  return track && Date.now() < track.end ? `https://open.spotify.com/track/${track.id}` : undefined;
}

phoenix.settings.define({
  copy: {
    type: "button",
    title: "Copy current Spotify track link",
    onPress() {
      const url = trackUrl();
      if (url) phoenix.ui.copy(url);
      phoenix.ui.toast(url ? "Spotify track link copied." : noTrack);
    },
  },
});

phoenix.commands.register({
  name: "track",
  description: "Share the Spotify track currently shown by your Discord activity",
  execute(_args, ctx) {
    const url = trackUrl();
    if (url) return url;
    ctx.reply(noTrack);
  },
});
