// Mock stdio MCP server for utcp-rs tests. Run as `node mock_mcp_server.cjs` (no shebang, so it
// also runs on Windows). One JSON-RPC message per line on stdin/stdout. Used by the transport
// tests in `utcp-core` and the CLI/daemon tests in `utcp-cli`.
//
// Options:
//   --wrapper   start the real server as a child sharing our stdio and wait for it, the way
//               `npx`, `uvx` or `docker run` do; killing only this process leaves the server.
//   --stubborn  keep running after stdin closes, so only an explicit kill stops the server.
//   --ignore-term  keep running after SIGTERM too (like a container's PID 1 without a handler),
//               so only SIGKILL stops the server.
//
// Env: MOCK_MCP_PID_FILE   append the server's pid (one line per start) to this file.
//      MOCK_MCP_TERM_FILE  append the server's pid to this file when it receives SIGTERM.
//      MOCK_MCP_EXTRA_TOOLS_FILE  if this file exists, `tools/list` also lists the
//                          comma-separated tool names in it (they answer like `echo`).
//
// Tools:
//   echo   answers after a random 0-30 ms delay with {tag, pid}, so concurrent answers come back
//          out of order. Before each answer it prints a non-JSON line, a blank line, a
//          notification and a `ping` request, which the client must skip or answer.
//   stats  reports {pingsSent, pingsAnswered, pid}. Listed with `annotations: {readOnlyHint: true}`.
//   silent never answers.
//   crash  exits the process.
const flags = process.argv.slice(2);
if (flags.includes("--wrapper")) {
  const rest = flags.filter((f) => f !== "--wrapper");
  const child = require("child_process").spawn(process.execPath, [__filename, ...rest], {
    stdio: "inherit",
  });
  child.on("exit", (code) => process.exit(code ?? 1));
  return;
}
if (flags.includes("--stubborn")) setInterval(() => {}, 1000);
if (process.env.MOCK_MCP_PID_FILE) {
  require("fs").appendFileSync(process.env.MOCK_MCP_PID_FILE, process.pid + "\n");
}
process.on("SIGTERM", () => {
  if (process.env.MOCK_MCP_TERM_FILE) {
    require("fs").appendFileSync(process.env.MOCK_MCP_TERM_FILE, process.pid + "\n");
  }
  if (!flags.includes("--ignore-term")) process.exit(0);
});

const readline = require("readline");

const send = (msg) => process.stdout.write(JSON.stringify(msg) + "\n");
let pingsSent = 0;
let pingsAnswered = 0;

process.stdout.write("mock MCP server starting (not JSON)\n");

readline.createInterface({ input: process.stdin }).on("line", (line) => {
  if (!line.trim()) return;
  const msg = JSON.parse(line);
  if (!msg.method) {
    if (String(msg.id).startsWith("srv-ping-") && msg.result) pingsAnswered++;
    return;
  }
  if (msg.id === undefined) return; // notification
  const reply = (result) => send({ jsonrpc: "2.0", id: msg.id, result });

  if (msg.method === "initialize") {
    return reply({
      protocolVersion: "2024-11-05",
      capabilities: { tools: {} },
      serverInfo: { name: "node-mock", version: "1.0.0" },
    });
  }
  if (msg.method === "tools/list") {
    const tool = (name) => ({
      name,
      description: name + " tool",
      inputSchema: { type: "object", properties: { tag: { type: "integer" } } },
      ...(name === "stats" ? { annotations: { readOnlyHint: true } } : {}),
    });
    const extraFile = process.env.MOCK_MCP_EXTRA_TOOLS_FILE;
    const fs = require("fs");
    const extra =
      extraFile && fs.existsSync(extraFile)
        ? fs.readFileSync(extraFile, "utf8").split(",").map((s) => s.trim()).filter(Boolean)
        : [];
    return reply({ tools: ["echo", "stats", "silent", "crash", ...extra].map(tool) });
  }
  if (msg.method !== "tools/call") {
    return send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "no such method" } });
  }

  const name = msg.params.name;
  const args = msg.params.arguments || {};
  if (name === "silent") return;
  if (name === "crash") process.exit(3);
  if (name === "stats") return reply({ pingsSent, pingsAnswered, pid: process.pid });

  setTimeout(() => {
    process.stdout.write("working on " + args.tag + " (not JSON)\n\n");
    send({ jsonrpc: "2.0", method: "notifications/progress", params: {} });
    send({ jsonrpc: "2.0", id: "srv-ping-" + ++pingsSent, method: "ping" });
    reply({ content: [{ type: "text", text: String(args.tag) }], tag: args.tag, pid: process.pid });
  }, Math.floor(Math.random() * 30));
});
