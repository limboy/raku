/**
 * Waku delegation tools for Pi and Oh My Pi.
 *
 * The parent agent runs inside Pi, which cannot inherit Waku's MCP server the
 * way Codex does. This extension bridges that gap: it spawns
 * `waku-daemon mcp-agent-tools` once and re-exports its two tools, so the agent
 * can delegate review subtasks and read their conclusions.
 *
 * The daemon chooses the executable and injects the session registration path,
 * so the tools always talk to the daemon that owns this task — never a second
 * one, and never with a model-supplied session id.
 */
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

type JsonObject = Record<string, unknown>;

type EssentialToolDefinition = Parameters<ExtensionAPI["registerTool"]>[0] & {
  /**
   * Marking delegation tools `"essential"` keeps their full schemas top-level on turn 1.
   */
  loadMode: "essential";
};

function registerEssentialTool(
  pi: ExtensionAPI,
  definition: EssentialToolDefinition,
) {
  pi.registerTool(definition);
}

class WakuDelegationClient {
  private child: ChildProcessWithoutNullStreams | undefined;
  private starting: Promise<void> | undefined;
  private nextId = 0;
  private pending = new Map<
    number,
    { resolve: (value: unknown) => void; reject: (error: Error) => void }
  >();

  private async ensureStarted(): Promise<void> {
    if (this.child) return;
    if (this.starting) return this.starting;
    this.starting = this.start();
    try {
      await this.starting;
    } finally {
      this.starting = undefined;
    }
  }

  private async start(): Promise<void> {
    const executable = process.env.WAKU_AGENT_TOOLS_SERVER;
    if (!executable) {
      throw new Error("WAKU_AGENT_TOOLS_SERVER is not configured");
    }
    const child = spawn(executable, ["mcp-agent-tools"], {
      env: process.env,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child = child;
    const lines = createInterface({ input: child.stdout });
    lines.on("line", (line) => this.handleLine(line));
    child.stderr.on("data", (chunk) => {
      const message = String(chunk).trim();
      if (message) console.error(`Waku delegation bridge: ${message}`);
    });
    child.once("error", (error) => this.handleExit(error));
    child.once("exit", (code, signal) => {
      this.handleExit(
        new Error(
          `Waku delegation bridge exited${code === null ? "" : ` with ${code}`}${
            signal ? ` (${signal})` : ""
          }`,
        ),
      );
    });
    await this.requestWithoutStart("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "waku-pi-delegation", version: "1" },
    });
    this.notify("notifications/initialized", {});
  }

  private handleLine(line: string): void {
    let message: JsonObject;
    try {
      message = JSON.parse(line) as JsonObject;
    } catch {
      return;
    }
    const id = typeof message.id === "number" ? message.id : undefined;
    if (id === undefined) return;
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    if (message.error && typeof message.error === "object") {
      const detail = (message.error as JsonObject).message;
      pending.reject(
        new Error(typeof detail === "string" ? detail : "MCP request failed"),
      );
    } else {
      pending.resolve(message.result);
    }
  }

  private handleExit(error: Error): void {
    this.child = undefined;
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }

  private notify(method: string, params: JsonObject): void {
    this.child?.stdin.write(
      `${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`,
    );
  }

  private requestWithoutStart(
    method: string,
    params: JsonObject,
  ): Promise<unknown> {
    const child = this.child;
    if (!child) {
      return Promise.reject(new Error("Waku delegation bridge is not running"));
    }
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      child.stdin.write(
        `${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`,
        (error) => {
          if (!error) return;
          this.pending.delete(id);
          reject(error);
        },
      );
    });
  }

  async call(name: string, args: JsonObject): Promise<JsonObject> {
    await this.ensureStarted();
    return (await this.requestWithoutStart("tools/call", {
      name,
      arguments: args,
    })) as JsonObject;
  }

  async close(): Promise<void> {
    const child = this.child;
    if (!child) return;
    try {
      await this.requestWithoutStart("shutdown", {});
    } catch {
      // The child may already be closing with Pi.
    }
    child.stdin.end();
    if (this.child === child) child.kill("SIGTERM");
  }
}

function toolResult(result: JsonObject) {
  const content = Array.isArray(result.content) ? result.content : [];
  if (result.isError === true) {
    const message = content
      .filter(
        (item): item is { type: "text"; text: string } =>
          !!item &&
          typeof item === "object" &&
          (item as JsonObject).type === "text" &&
          typeof (item as JsonObject).text === "string",
      )
      .map((item) => item.text)
      .join("\n");
    throw new Error(message || "Waku delegation request failed");
  }
  return { content, details: result._meta ?? {} };
}

export default function wakuDelegation(pi: ExtensionAPI) {
  const client = new WakuDelegationClient();

  registerEssentialTool(pi, {
    name: "delegate_batch",
    label: "Delegate",
    loadMode: "essential",
    description:
      "Run independent subtasks on the models the user marked with ^, in parallel. Pass one task per target; each task's `target_ref` is a target id from the delegation block of the user's message. Returns a group id for `wait_delegates`.",
    parameters: Type.Object(
      {
        request_key: Type.Optional(
          Type.String({
            description:
              "Optional stable key for this batch. Repeating it with the same tasks reuses the existing group instead of running them again. When omitted, Waku derives one from the tasks, so an identical retry is still deduplicated.",
          }),
        ),
        tasks: Type.Array(
          Type.Object(
            {
              task_key: Type.String({
                minLength: 1,
                description:
                  "Nonempty key unique across the entire batch, even when the same instructions go to different targets. For three models, use distinct keys such as books-target-1, books-target-2, and books-target-3. The returned task_ids map uses these exact keys.",
              }),
              target_ref: Type.String({
                description:
                  "Target id exactly as listed in the delegation block of the user's message.",
              }),
              instructions: Type.String({
                description:
                  "What this one subtask must judge. It is independent of the other tasks and cannot see their output.",
              }),
            },
            { additionalProperties: false },
          ),
          { minItems: 1, maxItems: 4 },
        ),
      },
      { additionalProperties: false },
    ),
    executionMode: "sequential",
    async execute(_id, params) {
      return toolResult(await client.call("delegate_batch", params));
    },
  });

  registerEssentialTool(pi, {
    name: "wait_delegates",
    label: "Wait for delegates",
    loadMode: "essential",
    description:
      "Wait for a delegated group. Returns as soon as every subtask in the group has settled, or with their current progress when the timeout elapses — call it again to keep waiting. Concluded subtasks carry their full conclusion.",
    parameters: Type.Object(
      {
        group_id: Type.String({
          description: "The group id returned by `delegate_batch`.",
        }),
        timeout_ms: Type.Optional(
          Type.Integer({
            minimum: 0,
            description:
              "How long to wait before returning progress. Defaults to 25000 ms; larger values are capped to 25000 ms by the server. Use 0 to return current progress immediately, or call again to keep waiting.",
          }),
        ),
      },
      { additionalProperties: false },
    ),
    executionMode: "sequential",
    async execute(_id, params) {
      return toolResult(await client.call("wait_delegates", params));
    },
  });

  pi.on("session_shutdown", async () => {
    await client.close();
  });
}
