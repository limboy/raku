/**
 * Structured questions for Pi, answered in Raku.
 *
 * Pi has no tool for asking the user a question. This extension registers an
 * `ask_user_question` tool shaped like Claude Code's `AskUserQuestion`, so
 * Raku can show the same question form it shows for Claude and Codex instead
 * of the model writing its question as plain text.
 *
 * The call travels in-band, the way the `subagent` extension's does: RPC mode
 * forwards an extension's `input` dialog to the host as an
 * `extension_ui_request`, so a request titled `raku:ask_user_question` whose
 * placeholder is the call reaches Raku, and the form's answers come back as
 * its value. The dialog has no timeout, and the tool's abort signal resolves
 * it early when the user stops the turn.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

/** Must match `PI_ASK_USER_REQUEST_TITLE` in Raku's Pi driver. */
const REQUEST_TITLE = "raku:ask_user_question";

interface Answer {
  questionId: string;
  answers: string[];
}

function parseAnswers(value: string): Answer[] {
  try {
    const parsed: unknown = JSON.parse(value);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (answer): answer is Answer =>
        typeof answer?.questionId === "string" && Array.isArray(answer?.answers),
    );
  } catch {
    return [];
  }
}

export default function (pi: ExtensionAPI) {
  pi.registerTool({
    name: "ask_user_question",
    label: "Ask questions",
    description: [
      "Ask the user one to four multiple-choice questions and wait for their answers.",
      "Use it when you need the user to make a decision, clarify an ambiguous request, or choose between approaches before you continue.",
      "Raku shows the questions as a form; the user picks an option or types their own answer, so do not add an \"Other\" option.",
      "If you recommend an option, list it first and add \"(Recommended)\" to its label.",
      "Do not use it for questions you can answer yourself from the code or the conversation.",
    ].join("\n"),
    promptSnippet: "Ask the user multiple-choice questions and wait for their answers",
    parameters: Type.Object({
      questions: Type.Array(
        Type.Object({
          question: Type.String({
            description: "The complete question, ending with a question mark.",
          }),
          header: Type.String({
            description: "A very short label for the question, at most 12 characters.",
          }),
          options: Type.Array(
            Type.Object({
              label: Type.String({
                description: "The choice as the user would say it, 1-5 words.",
              }),
              description: Type.Optional(
                Type.String({ description: "What choosing this option means." }),
              ),
            }),
            { minItems: 2, maxItems: 4, description: "The available choices." },
          ),
          multiSelect: Type.Optional(
            Type.Boolean({ description: "Allow more than one option to be chosen." }),
          ),
        }),
        { minItems: 1, maxItems: 4 },
      ),
    }),

    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const value = await ctx.ui.input(REQUEST_TITLE, JSON.stringify({ questions: params.questions }), {
        signal,
      });
      if (value === undefined) {
        throw new Error(
          signal?.aborted ? "The user stopped the turn before answering." : "Raku could not ask the user.",
        );
      }
      const answers = parseAnswers(value);
      const lines = params.questions.map((question, index) => {
        const answer = answers.find((candidate) => candidate.questionId === String(index));
        const text = answer?.answers.filter((item) => item.trim() !== "").join(", ");
        return `"${question.question}"="${text || "(no answer)"}"`;
      });
      return {
        content: [
          {
            type: "text",
            text: `User has answered your questions: ${lines.join(", ")}. You can now continue with the user's answers in mind.`,
          },
        ],
        details: {},
      };
    },
  });
}
