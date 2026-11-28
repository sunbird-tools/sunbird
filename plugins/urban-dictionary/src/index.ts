interface Definition {
  word: string;
  definition: string;
  example?: string;
  thumbs_up: number;
}

function isDefinition(value: unknown): value is Definition {
  if (typeof value !== "object" || value === null) return false;
  const entry = value as Partial<Definition>;
  return typeof entry.word === "string"
    && typeof entry.definition === "string"
    && Boolean(entry.definition.trim())
    && typeof entry.thumbs_up === "number"
    && Number.isFinite(entry.thumbs_up);
}

function plainText(text: string, max: number): string {
  const plain = text.replace(/\s+/g, " ").trim();
  const clipped = plain.length > max ? `${plain.slice(0, max - 1)}…` : plain;
  return clipped.replace(/[\\`*_{}\[\]()<>#|~]/g, "\\$&");
}

async function lookup(term: string): Promise<string> {
  if (!term || term.length > 200) return "Enter a term of up to 200 characters.";

  try {
    const query = encodeURIComponent(term).replaceAll("(", "%28").replaceAll(")", "%29");
    const response = await phoenix.fetch(`https://api.urbandictionary.com/v0/define?term=${query}`);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);

    const result = response.json<{ list?: unknown }>();
    const entries = Array.isArray(result?.list) ? result.list.filter(isDefinition) : [];
    const best = entries.reduce<Definition | undefined>(
      (current, entry) => !current || entry.thumbs_up > current.thumbs_up ? entry : current,
      undefined,
    );
    if (!best) return "No definitions found.";

    const lines = [`**${plainText(best.word, 120)}**`, plainText(best.definition, 800)];
    if (typeof best.example === "string" && best.example.trim()) {
      lines.push(`Example: ${plainText(best.example, 300)}`);
    }
    lines.push(`[More definitions](https://www.urbandictionary.com/define.php?term=${query})`);
    return lines.join("\n\n");
  } catch {
    return "Urban Dictionary is unavailable. Try again later.";
  }
}

phoenix.ui.addMessageAction({
  label: "Look up on Urban Dictionary",
  async onPress(ctx) {
    const term = ctx.message?.content.trim() ?? "";
    if (!term || term.length > 200) {
      phoenix.ui.toast("Choose a text message of up to 200 characters.");
      return;
    }
    const choice = await phoenix.ui.choose({
      title: "Send this text to Urban Dictionary?",
      message: term,
      options: ["Look up", "Cancel"],
    });
    if (choice === 0) ctx.reply(await lookup(term));
  },
});

phoenix.commands.register({
  name: "urban",
  description: "Look up a term on Urban Dictionary",
  async execute(args, ctx) {
    ctx.reply(await lookup(args.trim()));
  },
});
