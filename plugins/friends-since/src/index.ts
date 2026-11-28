phoenix.ui.addProfileSection({
  title: "Friends since",
  async render({ userId }) {
    if (!userId) return null;

    const [relationship] = await phoenix.stores.sql<{ since: string | null }>(
      `SELECT json_extract(CAST(v AS TEXT), '$.since') AS since
         FROM relationships
        WHERE k = ?
          AND json_extract(CAST(v AS TEXT), '$.type') = 1
        LIMIT 1`,
      userId.padStart(20, "0"),
    );
    if (!relationship?.since) return null;

    const since = new Date(relationship.since);
    return Number.isFinite(since.getTime()) ? since.toISOString().slice(0, 10) : null;
  },
});
