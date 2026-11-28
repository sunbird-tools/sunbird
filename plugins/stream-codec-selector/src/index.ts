// @description Allows the user to choose preferred video and audio codecs for streaming
// @version 1.0.0
// @author xx8008
// @enabled false

/*
 * Stream Codec Selector (TypeScript source for `sunbird-tools/sunbird`: plugins/stream-codec-selector/src/index.ts)
 *
 * Current runtime limitation:
 *   Discord Phoenix handles WebRTC SDP negotiation and Voice Gateway frames (OP 1/2/12)
 *   in the native Rust / media-engine layer, which is not yet exposed to QuickJS plugins.
 *   This plugin persists preferences, tracks stream/voice REST & Gateway lifecycle events,
 *   and rewrites codec/SDP fields whenever an intercepted payload exposes them.
 */

type VideoCodecChoice = "auto" | "H264" | "VP8" | "VP9" | "AV1";
type AudioCodecChoice = "auto" | "opus" | "opus-music" | "opus-red" | "G722";

interface StoredPrefs {
  videoCodec: VideoCodecChoice;
  audioCodec: AudioCodecChoice;
  forceCodec: boolean;
}

interface PluginStatus {
  state: string;
  lastAppliedVideo: string;
  lastAppliedAudio: string;
  lastEvent: string;
  lastUpdatedAt: string | null;
  patchedCount: number;
}

interface CodecEntry {
  type?: string;
  name?: string;
  [key: string]: unknown;
}

const PLUGIN_NAME = "StreamCodecSelector";
const STORAGE_KEYS = {
  PREFS: "prefs",
  STATUS: "status",
} as const;

const DEFAULT_PREFS: StoredPrefs = {
  videoCodec: "auto",
  audioCodec: "auto",
  forceCodec: false,
};

const AUDIO_CODEC_MAP: Record<AudioCodecChoice, string | null> = {
  auto: null,
  opus: "opus",
  "opus-music": "opus",
  "opus-red": "opus",
  G722: "G722",
};

const removers: Array<() => void> = [];
let unloaded = false;

function logInfo(msg: string, ...args: unknown[]): void {
  phoenix.log.info(`[${PLUGIN_NAME}] ${msg}`, ...args);
}

function logWarn(msg: string, ...args: unknown[]): void {
  phoenix.log.warn(`[${PLUGIN_NAME}] ${msg}`, ...args);
}

function storageGet<T>(key: string, fallback: T): T {
  try {
    const value = phoenix.storage.get<T>(key);
    return value !== undefined ? value : fallback;
  } catch (err) {
    logWarn(`Failed reading storage key "${key}":`, String(err));
    return fallback;
  }
}

function storageSet(key: string, value: unknown): void {
  try {
    phoenix.storage.set(key, value);
  } catch (err) {
    logWarn(`Failed writing storage key "${key}":`, String(err));
  }
}

function getStatus(): PluginStatus {
  return storageGet<PluginStatus>(STORAGE_KEYS.STATUS, {
    state: "Idle (no active stream detected)",
    lastAppliedVideo: "None",
    lastAppliedAudio: "None",
    lastEvent: "None",
    lastUpdatedAt: null,
    patchedCount: 0,
  });
}

function updateStatus(patch: Partial<PluginStatus>): PluginStatus {
  const next: PluginStatus = {
    ...getStatus(),
    ...patch,
    lastUpdatedAt: new Date().toISOString(),
  };
  storageSet(STORAGE_KEYS.STATUS, next);
  return next;
}

function showNotify(title: string, body: string): void {
  try {
    phoenix.ui.notify({ title, body, durationMs: 4500 });
  } catch {
    phoenix.ui.toast(`${title}: ${body}`);
  }
}

function syncPrefsToStorage(partial: Partial<StoredPrefs>): StoredPrefs {
  const current = storageGet<StoredPrefs>(STORAGE_KEYS.PREFS, { ...DEFAULT_PREFS });
  const next: StoredPrefs = { ...current, ...partial };
  storageSet(STORAGE_KEYS.PREFS, next);
  return next;
}

const initialStored = storageGet<StoredPrefs>(STORAGE_KEYS.PREFS, DEFAULT_PREFS);

const settings = phoenix.settings.define({
  videoCodec: {
    type: "select",
    title: "Preferred Video Codec",
    description: "Target video codec for Go Live, screen share, and camera streams.",
    options: [
      { value: "auto", label: "Auto (Discord Default)" },
      { value: "H264", label: "H.264 (AVC)" },
      { value: "VP8", label: "VP8" },
      { value: "VP9", label: "VP9" },
      { value: "AV1", label: "AV1" },
    ] as const,
    default: initialStored.videoCodec ?? "auto",
    onChange(value) {
      syncPrefsToStorage({ videoCodec: value as VideoCodecChoice });
      logInfo(`Video codec preference changed to: ${value}`);
      phoenix.ui.toast(`Video codec set to ${value}`);
    },
  },

  audioCodec: {
    type: "select",
    title: "Preferred Audio Codec",
    description: "Target audio codec for stream and voice media negotiation.",
    options: [
      { value: "auto", label: "Auto (Discord Default)" },
      { value: "opus", label: "Opus (Standard)" },
      { value: "opus-music", label: "Opus (High Bitrate / Stereo)" },
      { value: "opus-red", label: "Opus + RED (Loss Recovery)" },
      { value: "G722", label: "G.722 (Wideband Fallback)" },
    ] as const,
    default: initialStored.audioCodec ?? "auto",
    onChange(value) {
      syncPrefsToStorage({ audioCodec: value as AudioCodecChoice });
      logInfo(`Audio codec preference changed to: ${value}`);
      phoenix.ui.toast(`Audio codec set to ${value}`);
    },
  },

  forceCodec: {
    type: "switch",
    title: "Force Preferred Codecs",
    description:
      "When enabled, aggressively strips/reorders non-matching codecs in any exposed SDP or codec payload.",
    default: Boolean(initialStored.forceCodec),
    onChange(value) {
      syncPrefsToStorage({ forceCodec: value });
      logInfo(`Force codec mode ${value ? "enabled" : "disabled"}`);
    },
  },

  viewStatus: {
    type: "button",
    title: "Show Current Status",
    description: "Displays current codec selections, stream state, and last applied codecs.",
    onPress() {
      const prefs = getEffectivePrefs();
      const status = getStatus();
      const lines = [
        `Video Preference: ${prefs.videoCodec}`,
        `Audio Preference: ${prefs.audioCodec}`,
        `Force Mode: ${prefs.forceCodec ? "ON" : "OFF"}`,
        `Stream State: ${status.state}`,
        `Last Applied Video: ${status.lastAppliedVideo}`,
        `Last Applied Audio: ${status.lastAppliedAudio}`,
        `Last Event: ${status.lastEvent}`,
        `Payloads Patched: ${status.patchedCount}`,
        "",
        "Note: Full WebRTC SDP / Voice Gateway forcing requires native media-engine hooks not yet exposed by Phoenix.",
      ].join("\n");

      void phoenix.ui.choose({
        title: "Stream Codec Selector Status",
        message: lines,
        options: ["OK"],
      });
    },
  },

  resetDefaults: {
    type: "button",
    title: "Reset to Default",
    description: "Clears stored preferences and status history back to defaults.",
    onPress() {
      storageSet(STORAGE_KEYS.PREFS, { ...DEFAULT_PREFS });
      updateStatus({
        state: "Reset to defaults",
        lastAppliedVideo: "None",
        lastAppliedAudio: "None",
        lastEvent: "Manual reset",
        patchedCount: 0,
      });
      logInfo("Stored codec preferences and status reset to defaults.");
      showNotify(
        "Preferences Reset",
        "Stored state cleared. Set Video/Audio dropdowns back to Auto if needed."
      );
    },
  },
});

function getEffectivePrefs(): StoredPrefs {
  const stored = storageGet<StoredPrefs>(STORAGE_KEYS.PREFS, DEFAULT_PREFS);
  return {
    videoCodec: (settings.videoCodec as VideoCodecChoice) ?? stored.videoCodec ?? "auto",
    audioCodec: (settings.audioCodec as AudioCodecChoice) ?? stored.audioCodec ?? "auto",
    forceCodec:
      typeof settings.forceCodec === "boolean" ? settings.forceCodec : Boolean(stored.forceCodec),
  };
}

syncPrefsToStorage(getEffectivePrefs());

function rewriteSdpMediaLine(
  sdp: string,
  mediaType: "video" | "audio",
  targetCodecName: string,
  forceCodec: boolean
): { sdp: string; changed: boolean } {
  const lines = sdp.split(/\r?\n/);
  const mPrefix = `m=${mediaType} `;
  const mIndex = lines.findIndex((line) => line.startsWith(mPrefix));
  if (mIndex === -1) return { sdp, changed: false };

  const matchingPts = new Set<string>();
  const targetLower = targetCodecName.toLowerCase();
  for (let i = mIndex + 1; i < lines.length; i++) {
    if (lines[i].startsWith("m=")) break;
    const match = /^a=rtpmap:(\d+)\s+([^/\s]+)/i.exec(lines[i]);
    if (match && match[2].toLowerCase() === targetLower) {
      matchingPts.add(match[1]);
    }
  }
  if (matchingPts.size === 0) return { sdp, changed: false };

  const rtxPts = new Set<string>();
  for (let i = mIndex + 1; i < lines.length; i++) {
    if (lines[i].startsWith("m=")) break;
    const fmtpMatch = /^a=fmtp:(\d+)\s+.*apt=(\d+)/i.exec(lines[i]);
    if (fmtpMatch && matchingPts.has(fmtpMatch[2])) {
      rtxPts.add(fmtpMatch[1]);
    }
  }

  const parts = lines[mIndex].split(" ");
  if (parts.length <= 3) return { sdp, changed: false };

  const header = parts.slice(0, 3);
  const allPts = parts.slice(3);
  const preferred = allPts.filter((pt) => matchingPts.has(pt) || rtxPts.has(pt));
  const remaining = allPts.filter((pt) => !matchingPts.has(pt) && !rtxPts.has(pt));

  const nextPts = forceCodec ? preferred : [...preferred, ...remaining];
  if (nextPts.length === 0) return { sdp, changed: false };

  const nextLine = [...header, ...nextPts].join(" ");
  if (nextLine === lines[mIndex]) return { sdp, changed: false };

  lines[mIndex] = nextLine;
  return { sdp: lines.join("\r\n"), changed: true };
}

function reorderCodecEntries(
  entries: CodecEntry[],
  mediaType: "video" | "audio",
  targetName: string,
  forceCodec: boolean
): boolean {
  const targetLower = targetName.toLowerCase();
  const hasMatch = entries.some(
    (c) => c && c.type === mediaType && String(c.name ?? "").toLowerCase() === targetLower
  );
  if (!hasMatch) return false;

  entries.sort((a, b) => {
    if (a?.type !== mediaType || b?.type !== mediaType) return 0;
    const aMatch = String(a.name ?? "").toLowerCase() === targetLower ? 0 : 1;
    const bMatch = String(b.name ?? "").toLowerCase() === targetLower ? 0 : 1;
    return aMatch - bMatch;
  });

  if (forceCodec) {
    for (let i = entries.length - 1; i >= 0; i--) {
      const entry = entries[i];
      const nameLower = String(entry?.name ?? "").toLowerCase();
      if (
        entry?.type === mediaType &&
        nameLower !== targetLower &&
        !(mediaType === "video" && nameLower === "rtx")
      ) {
        entries.splice(i, 1);
      }
    }
  }
  return true;
}

function applyCodecPreferencesToPayload(
  rawPayload: unknown,
  prefs: StoredPrefs
): {
  modified: boolean;
  payload: Record<string, unknown> | null;
  appliedVideo: string | null;
  appliedAudio: string | null;
} {
  if (!rawPayload || typeof rawPayload !== "object") {
    return { modified: false, payload: null, appliedVideo: null, appliedAudio: null };
  }

  const wantedVideo = prefs.videoCodec !== "auto" ? prefs.videoCodec : null;
  const wantedAudio = AUDIO_CODEC_MAP[prefs.audioCodec] ?? null;
  if (!wantedVideo && !wantedAudio) {
    return { modified: false, payload: null, appliedVideo: null, appliedAudio: null };
  }

  let modified = false;
  let appliedVideo: string | null = null;
  let appliedAudio: string | null = null;
  const payload = { ...(rawPayload as Record<string, unknown>) };

  if (wantedVideo) {
    for (const key of ["video_codec", "preferred_video_codec", "codec"]) {
      const current = payload[key];
      if (typeof current === "string" && current.toUpperCase() !== wantedVideo) {
        payload[key] = wantedVideo;
        modified = true;
        appliedVideo = wantedVideo;
      }
    }
  }

  if (wantedAudio) {
    for (const key of ["audio_codec", "preferred_audio_codec"]) {
      const current = payload[key];
      if (typeof current === "string" && current.toLowerCase() !== wantedAudio.toLowerCase()) {
        payload[key] = wantedAudio;
        modified = true;
        appliedAudio = prefs.audioCodec;
      }
    }
  }

  if (Array.isArray(payload.codecs)) {
    const updatedCodecs = (payload.codecs as CodecEntry[]).slice();
    let arrayChanged = false;

    if (wantedVideo && reorderCodecEntries(updatedCodecs, "video", wantedVideo, prefs.forceCodec)) {
      arrayChanged = true;
      appliedVideo = wantedVideo;
    }
    if (wantedAudio && reorderCodecEntries(updatedCodecs, "audio", wantedAudio, prefs.forceCodec)) {
      arrayChanged = true;
      appliedAudio = prefs.audioCodec;
    }

    if (arrayChanged) {
      payload.codecs = updatedCodecs;
      modified = true;
    }
  }

  if (typeof payload.sdp === "string") {
    let nextSdp = payload.sdp;
    if (wantedVideo) {
      const res = rewriteSdpMediaLine(nextSdp, "video", wantedVideo, prefs.forceCodec);
      if (res.changed) {
        nextSdp = res.sdp;
        modified = true;
        appliedVideo = wantedVideo;
      }
    }
    if (wantedAudio) {
      const res = rewriteSdpMediaLine(nextSdp, "audio", wantedAudio, prefs.forceCodec);
      if (res.changed) {
        nextSdp = res.sdp;
        modified = true;
        appliedAudio = prefs.audioCodec;
      }
    }
    if (nextSdp !== payload.sdp) {
      payload.sdp = nextSdp;
    }
  }

  return { modified, payload, appliedVideo, appliedAudio };
}

function handleStreamAttempt(
  sourceLabel: string,
  payload: unknown
): { modified: boolean; nextPayload: Record<string, unknown> | null } {
  const prefs = getEffectivePrefs();
  const currentStatus = getStatus();
  const { modified, payload: nextPayload, appliedVideo, appliedAudio } =
    applyCodecPreferencesToPayload(payload, prefs);

  updateStatus({
    state: modified ? "Streaming (codec payload patched)" : "Streaming detected (REST/Gateway)",
    lastAppliedVideo: appliedVideo ?? `${prefs.videoCodec} (native engine controlled)`,
    lastAppliedAudio: appliedAudio ?? `${prefs.audioCodec} (native engine controlled)`,
    lastEvent: sourceLabel,
    patchedCount: modified ? currentStatus.patchedCount + 1 : currentStatus.patchedCount,
  });

  logInfo(
    `Stream activity via ${sourceLabel} | wanted=(video:${prefs.videoCodec}, audio:${prefs.audioCodec}, force:${prefs.forceCodec}) | patched=${modified}`
  );

  if (prefs.videoCodec !== "auto" || prefs.audioCodec !== "auto") {
    if (modified) {
      phoenix.ui.toast(`Applied codec preference (${prefs.videoCodec} / ${prefs.audioCodec})`);
    } else {
      logWarn(
        "No codec/SDP fields were present in this payload; Discord Phoenix's native media engine negotiated codecs internally."
      );
    }
  }

  return { modified, nextPayload };
}

const streamFilters: Phoenix.RequestFilter[] = [
  { pathIncludes: "/streams" },
  { pathIncludes: "/voice" },
  { pathIncludes: "/call" },
];

for (const filter of streamFilters) {
  removers.push(
    phoenix.discord.onRequest(filter, (req) => {
      if (unloaded) return;
      const bodyJson = req.json();
      const label = `${req.method} ${req.path}`;
      if (req.method === "DELETE" && req.path.includes("/streams")) {
        updateStatus({ state: "Idle (stream ended)", lastEvent: label });
        logInfo(`Stream stop request observed: ${label}`);
        return;
      }

      const { modified, nextPayload } = handleStreamAttempt(label, bodyJson);
      if (modified && nextPayload) return { body: nextPayload };
    })
  );

  removers.push(
    phoenix.discord.onResponse(filter, (res) => {
      if (unloaded) return;
      const bodyJson = res.json();
      if (!bodyJson || typeof bodyJson !== "object") return;

      const prefs = getEffectivePrefs();
      const { modified, payload: nextPayload } = applyCodecPreferencesToPayload(bodyJson, prefs);
      if (modified && nextPayload) {
        updateStatus({
          state: "Streaming (response codec patched)",
          lastEvent: `RES ${res.status} ${res.path}`,
          patchedCount: getStatus().patchedCount + 1,
        });
        logInfo(`Patched codec metadata in response from ${res.path}`);
        return { body: nextPayload };
      }
    })
  );
}

const streamSignals = [
  "streamCreate",
  "streamServerUpdate",
  "streamDelete",
  "voiceStateUpdate",
  "voiceServerUpdate",
] as const;

for (const eventName of streamSignals) {
  const sig = phoenix.gateway[eventName];
  if (!sig) continue;

  if (eventName !== "streamDelete") {
    phoenix.hook(sig, {
      before(payload) {
        if (unloaded) return;
        const { modified, nextPayload } = handleStreamAttempt(`gateway:${eventName}`, payload);
        if (modified && nextPayload) return nextPayload;
      },
    });
  } else {
    phoenix.on(sig, () => {
      if (unloaded) return;
      updateStatus({ state: "Idle (stream ended)", lastEvent: `gateway:${eventName}` });
      logInfo("Observed gateway:streamDelete");
    });
  }

  logInfo(`Registered gateway listener for "${eventName}".`);
}

if (typeof phoenix.gateway.onSend === "function") {
  removers.push(
    phoenix.gateway.onSend(4, (payload) => {
      if (unloaded) return;
      if (payload.channel_id) {
        handleStreamAttempt("gateway:op4:voiceStateUpdate", payload);
      } else {
        updateStatus({ state: "Idle (left voice)", lastEvent: "gateway:op4:leave" });
      }
    })
  );
}

const initialPrefs = getEffectivePrefs();
logInfo(
  `Loaded (video=${initialPrefs.videoCodec}, audio=${initialPrefs.audioCodec}, force=${initialPrefs.forceCodec})`
);

phoenix.onUnload(() => {
  unloaded = true;
  while (removers.length > 0) {
    removers.pop()?.();
  }
  logInfo("Unloaded cleanly.");
});
