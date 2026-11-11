/**
 * Subagents for Pi, run by Raku.
 *
 * Pi has no way to delegate work to another agent. This extension registers a
 * `subagent` tool whose calls Raku carries out: Raku starts a child session
 * of its own, on whichever provider and model the chosen agent names, shows
 * it to the user, and returns the child's final message as the tool result.
 *
 * The call travels in-band. RPC mode forwards an extension's `input` dialog to
 * the host as an `extension_ui_request` and resolves it with the host's
 * answer, so a request titled `raku:subagent` whose placeholder is the call
 * reaches Raku with no socket, port, or token. The dialog has no timeout, and
 * the tool's abort signal resolves it early when the user stops the turn;
 * Raku stops the child in that case.
 *
 * The agents on offer come from Raku's settings through `RAKU_SUBAGENTS`.
 * Raku does not load this extension into a subagent's own session, which is
 * what keeps delegation one level deep.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

/** Must match `PI_SUBAGENT_REQUEST_TITLE` in Raku's Pi driver. */
const REQUEST_TITLE = "raku:subagent";

interface Agent {
  name: string;
  description: string;
}

function configuredAgents(): Agent[] {
  try {
    const parsed: unknown = JSON.parse(process.env.RAKU_SUBAGENTS ?? "[]");
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (agent): agent is Agent => typeof agent?.name === "string" && typeof agent?.description === "string",
    );
  } catch {
    return [];
  }
}

export default function (pi: ExtensionAPI) {
  const agents = configuredAgents();
  if (agents.length === 0) return;

  pi.registerTool({
    name: "subagent",
    label: "Subagent",
    description: [
      "Delegate a task to a subagent that works in its own context window and returns only its final report.",
      "Use it for work whose intermediate steps you do not need to see: broad searches, reading many files to answer one question, or a self-contained piece of a larger change.",
      "The subagent starts with no knowledge of this conversation, so the prompt must be self-contained: say what to do, what you already know, and what to report back.",
      "Call it several times in one message to run subagents in parallel.",
      "When the user names an agent with @name, delegate to that agent.",
      "",
      "Available agents:",
      ...agents.map((agent) => `- ${agent.name}: ${agent.description}`),
    ].join("\n"),
    promptSnippet: "Delegate a task to a subagent with its own context window",
    parameters: Type.Object({
      agent: Type.String({
        description: `Which agent to run. One of: ${agents.map((agent) => agent.name).join(", ")}.`,
      }),
      description: Type.String({
        description: "A short label for the task, 3-5 words, shown to the user.",
      }),
      prompt: Type.String({
        description: "The complete task for the subagent, including everything it needs to know.",
      }),
    }),
    executionMode: "parallel",

    async execute(toolCallId, params, signal, _onUpdate, ctx) {
      const answer = await ctx.ui.input(
        REQUEST_TITLE,
        JSON.stringify({
          callId: toolCallId,
          agent: params.agent,
          description: params.description,
          prompt: params.prompt,
        }),
        { signal },
      );
      if (answer === undefined) {
        throw new Error(
          signal?.aborted ? "The subagent was stopped." : "Raku did not run the subagent.",
        );
      }
      // Raku reports every outcome as `<subagent … state="…">`; anything but
      // a completed run is a failed tool call.
      if (!/^<subagent\b[^>]*\bstate="completed"/.test(answer)) {
        throw new Error(answer);
      }
      return { content: [{ type: "text", text: answer }], details: {} };
    },
  });
}
