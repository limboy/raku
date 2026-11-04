/**
 * Automatic session titles for Pi.
 *
 * Claude and Codex name their own sessions; Pi only names a session when
 * something calls `setSessionName`. This extension fills that gap: when the
 * first ordinary request of an unnamed session arrives, it asks the session's
 * current model for a short title in the background and stores it with
 * `pi.setSessionName`. Pi then emits `session_info_changed`, which Waku already
 * shows as the provider title, and the name persists in Pi's own session file.
 *
 * Waku appends a title-language note to the first request (see
 * `crates/waku-core/src/driver/title_language.rs`). The note exists only for
 * naming, so this extension strips it before the agent sees the request and
 * uses it to choose the title language.
 *
 * Titling is best-effort and silent: the agent run never waits for it, and a
 * failure leaves Waku's prompt-derived fallback in place. Nothing here writes
 * to stderr, because Waku surfaces stderr lines that mention errors.
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

// Keep in sync with CONTEXT_* in crates/waku-core/src/driver/title_language.rs.
const TITLE_LANGUAGE_CONTEXT =
  /\n\n\[\[waku-session-title-language=[a-z]{3}\]\]\nAutomatic session titles MUST use ([^,\n]+), unless [^\n]*\n\[\[\/waku-session-title-language\]\]$/;

const REQUEST_SAMPLE_CHARS = 2_000;
const TITLE_MAX_CHARS = 60;
const TITLE_TIMEOUT_MS = 30_000;

function splitTitleLanguage(text: string): { request: string; language?: string } {
  const match = TITLE_LANGUAGE_CONTEXT.exec(text);
  if (!match) return { request: text };
  return { request: text.slice(0, match.index), language: match[1] };
}

/** Commands keep their own semantics and do not describe the task. */
function isOrdinaryRequest(text: string): boolean {
  const trimmed = text.trim();
  if (!trimmed) return false;
  const first = trimmed.split(/\s+/, 1)[0];
  return !(first.startsWith("/") && !first.slice(1).includes("/"));
}

function hasUserMessage(entries: readonly unknown[]): boolean {
  return entries.some((entry) => {
    const value = entry as { type?: string; message?: { role?: string } };
    return value.type === "message" && value.message?.role === "user";
  });
}

function cleanTitle(raw: string): string | undefined {
  let title = raw
    .split("\n")
    .map((line) => line.trim())
    .find((line) => line.length > 0);
  if (!title) return undefined;
  title = title
    .replace(/^(?:session\s+)?title\s*[:：]\s*/i, "")
    .replace(/^[\s"'`“”‘’「」『』*#]+|[\s"'`“”‘’「」『』*]+$/g, "")
    .replace(/[.。!！?？,，;；:：]+$/, "")
    .trim();
  if (!title) return undefined;
  const characters = Array.from(title);
  return characters.length > TITLE_MAX_CHARS
    ? `${characters.slice(0, TITLE_MAX_CHARS - 1).join("")}…`
    : title;
}

function systemPrompt(language: string | undefined): string {
  const languageRule = language
    ? `Write the title in ${language}, keeping necessary technical terms as written.`
    : "Write the title in the same language as the request, keeping necessary technical terms as written.";
  return [
    "You name coding-agent sessions.",
    "Reply with only a concise title of 2 to 7 words that summarizes the user's request.",
    "Do not answer, carry out, or comment on the request.",
    "No quotes, no emoji, no trailing punctuation, no prefix such as 'Title:'.",
    languageRule,
  ].join(" ");
}

export default function (pi: ExtensionAPI) {
  let attempted = false;
  const controller = new AbortController();

  // Each session gets a fresh extension instance; a pending title belongs to
  // the session that is going away.
  pi.on("session_shutdown", () => {
    controller.abort();
  });

  pi.on("input", (event, ctx) => {
    const { request, language } = splitTitleLanguage(event.text);
    const stripped = request !== event.text;

    if (
      !attempted &&
      event.source !== "extension" &&
      !event.streamingBehavior &&
      isOrdinaryRequest(request)
    ) {
      attempted = true;
      try {
        const model = ctx.model;
        const registry = ctx.modelRegistry;
        const sessionId = ctx.sessionManager.getSessionId();
        const fresh =
          !pi.getSessionName() && !hasUserMessage(ctx.sessionManager.getBranch());
        if (model && fresh) {
          void generateTitle(pi, registry, model, sessionId, request, language, controller.signal);
        }
      } catch {
        // Naming must never interfere with the request itself.
      }
    }

    return stripped
      ? { action: "transform", text: request, images: event.images }
      : { action: "continue" };
  });
}

async function generateTitle(
  pi: ExtensionAPI,
  registry: ExtensionContext["modelRegistry"],
  model: NonNullable<ExtensionContext["model"]>,
  sessionId: string,
  request: string,
  language: string | undefined,
  shutdown: AbortSignal,
): Promise<void> {
  const signal = AbortSignal.any([shutdown, AbortSignal.timeout(TITLE_TIMEOUT_MS)]);
  try {
    const sample = Array.from(request.trim()).slice(0, REQUEST_SAMPLE_CHARS).join("");
    const message = await registry
      .streamSimple(
        model,
        {
          systemPrompt: systemPrompt(language),
          messages: [
            {
              role: "user",
              content: `<request>\n${sample}\n</request>`,
              timestamp: Date.now(),
            },
          ],
        },
        // Some gateways, such as OpenCode Go, route by conversation and
        // reject requests without one; the title belongs to this session.
        { maxTokens: 512, signal, sessionId },
      )
      .result();
    if (signal.aborted || (message.stopReason !== "stop" && message.stopReason !== "length")) {
      return;
    }
    const text = message.content
      .filter((part) => part.type === "text")
      .map((part) => part.text)
      .join("");
    const title = cleanTitle(text);
    // A name set meanwhile, e.g. with /name, is the user's choice.
    if (title && !signal.aborted && !pi.getSessionName()) {
      pi.setSessionName(title);
    }
  } catch {
    // Best-effort: Waku keeps its prompt-derived title.
  }
}
