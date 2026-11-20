// Mock secrets command for utcp-rs tests. Run as `node secret_source.cjs <config.json> NAME...`
// (no shebang, so it also runs on Windows), the way `[secrets] command` in `settings.toml` runs a
// user's script: utcp appends the variable names it wants after the configured arguments.
//
// <config.json>: {"values": {"NAME": "value", ...}, "exit": <code>, "sleep_ms": <ms>, "bad": true}
//   values    printed as NAME=value for every requested name that has one. UNASKED_SECRET=leak is
//             always printed too, so tests can check utcp drops names it did not ask for.
//   exit      print a value to stderr and exit with this code (utcp must not show stderr).
//   sleep_ms  wait this long before answering (for timeouts).
//   bad       print a line that is not NAME=value.
// Every run appends one line to <config.json>.runs: the requested names, space-separated.
const fs = require("fs");
const [configFile, ...names] = process.argv.slice(2);
const config = JSON.parse(fs.readFileSync(configFile, "utf8"));
fs.appendFileSync(configFile + ".runs", names.join(" ") + "\n");

function answer() {
  if (config.exit !== undefined) {
    process.stderr.write("STDERR-SECRET-VALUE\n");
    process.exit(config.exit);
  }
  const lines = ["# mock secrets", "UNASKED_SECRET=leak"];
  for (const name of names) {
    if (config.values && config.values[name] !== undefined) {
      lines.push(`export ${name}=${config.values[name]}`);
    }
  }
  if (config.bad) {
    lines.push("STDOUT-SECRET-VALUE without a name");
  }
  process.stdout.write(lines.join("\n") + "\n");
}

if (config.sleep_ms) {
  setTimeout(answer, config.sleep_ms);
} else {
  answer();
}
