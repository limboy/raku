/**
 * Self-described shell commands for Pi.
 *
 * Claude Code's shell tool asks the model for a short `description` of every
 * command, and Raku shows that line in place of the raw command text. Pi's
 * built-in `bash` tool has no such field, so a run of
 * `cd /long/path && cat … | head -60` rows is all a Pi transcript shows.
 *
 * This extension re-registers `bash` as Pi's own definition plus one optional
 * `description` parameter. Execution, output, rendering, and prompt metadata
 * are Pi's; the description is only read by Raku from the stored tool call.
 *
 * It steps aside when it would change more than that: when another extension
 * already replaced `bash` (a sandbox, an SSH runner), or when the session does
 * not offer `bash` at all, because registering a tool also activates it.
 */
import { createBashToolDefinition, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const DESCRIPTION_PARAMETER = Type.Optional(
  Type.String({
    description:
      "Clear, concise description of what this command does in active voice, " +
      "5-10 words, e.g. \"Run the focused parser tests\" or \"Show working tree status\". " +
      "Describe the intent, not the command text.",
  }),
);

export default function (pi: ExtensionAPI) {
  pi.on("session_start", (_event, ctx) => {
    const bash = pi.getAllTools().find((tool) => tool.name === "bash");
    if (bash?.sourceInfo?.source !== "builtin") return;
    if (!pi.getActiveTools().includes("bash")) return;

    const builtin = createBashToolDefinition(ctx.cwd);
    pi.registerTool({
      ...builtin,
      parameters: Type.Object({
        ...builtin.parameters.properties,
        description: DESCRIPTION_PARAMETER,
      }),
      promptGuidelines: [
        ...(builtin.promptGuidelines ?? []),
        "Give every bash call a short `description` of what the command does, in 5-10 words of active voice.",
      ],
    });
  });
}
