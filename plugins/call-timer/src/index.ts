let call: { guildId: string | null; channelId: string; startedAt: number } | undefined;
let currentUserId: string | undefined;

// The same voice state is sent again when muting, deafening, or starting video.
phoenix.gateway.onSend?.(4, ({ guild_id, channel_id }) => {
  if (channel_id === null) {
    call = undefined;
  } else if (typeof channel_id === "string" && (call?.channelId !== channel_id || call.guildId !== guild_id)) {
    call = { guildId: guild_id, channelId: channel_id, startedAt: Date.now() };
  }
});

if (phoenix.gateway.ready) {
  phoenix.on(phoenix.gateway.ready, ({ user }) => {
    currentUserId = user?.id;
    call = undefined;
  });
}

const voiceStateUpdate = phoenix.gateway.voiceStateUpdate;
if (voiceStateUpdate) {
  phoenix.on(voiceStateUpdate, (payload) => {
    const state = payload as { user_id?: string; channel_id?: string | null } | null;
    if (currentUserId && state?.user_id === currentUserId && state.channel_id === null) {
      call = undefined;
    }
  });
}

function elapsedText(): string {
  if (!call) return "Join a voice channel after enabling Call Timer.";

  const elapsed = Math.max(0, Math.floor((Date.now() - call.startedAt) / 1000));
  const hours = Math.floor(elapsed / 3600);
  const minutes = Math.floor((elapsed % 3600) / 60);
  const seconds = elapsed % 60;
  const clock = [hours, minutes, seconds].map((part) => String(part).padStart(2, "0")).join(":");
  return `Voice connection: ${clock}`;
}

phoenix.settings.define({
  elapsed: {
    type: "button",
    title: "Show call duration",
    onPress: () => phoenix.ui.toast(elapsedText()),
  },
});

phoenix.commands.register({
  name: "calltimer",
  description: "Show how long you have been in your current voice channel",
  execute(_args, ctx) {
    currentUserId = ctx.currentUserId ?? currentUserId;
    ctx.reply(elapsedText());
  },
});
