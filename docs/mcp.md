# Using Checkweave through MCP

MCP (Model Context Protocol) lets your coding assistant call Checkweave tools. You ask a question in ordinary language. The assistant chooses a tool, Checkweave runs that check on your machine, and returns evidence. The assistant then explains the result or edits code with its own tools. Checkweave does not rewrite your files.

Your editor starts Checkweave and manages the connection. You do not type a handshake or start a separate service first.

## Cursor

From the project you want to check, with `checkweave` on `PATH`:

```sh
checkweave init
```

That prepares the project and writes a Checkweave entry in `.cursor/mcp.json`,
plus a managed rule at `.cursor/rules/checkweave.mdc`. The rule tells the
assistant when these tools are useful. Other MCP servers in that file are
left alone. Open the project in Cursor and check that its MCP tools list includes
`checkweave_check`; reload the connection if it has not picked up the new config.

`checkweave init --agent none` only prepares project state and skips the editor files. `checkweave deinit` removes the managed entry and rule. On WSL, run `init` in the Linux project directory and open that directory from WSL.

You can also add the server yourself. The example below uses Cursor’s config
format. Other clients that support local MCP servers use the same executable
and arguments, with their own configuration format. Use absolute paths:

```json
{
  "mcpServers": {
    "checkweave": {
      "command": "/absolute/path/to/checkweave",
      "args": ["--workspace", "/absolute/path/to/project", "mcp"]
    }
  }
}
```

## What you can ask

These are the tools for everyday checks. Say what you want in normal language, and name Checkweave so the assistant calls it.

| You can say | Tool |
| --- | --- |
| Which records in `notes.jsonl` are missing an owner? Call Checkweave. | `checkweave_check` |
| Compare `before.py` and `after.py` on this JSON input. Call Checkweave. | `checkweave_compare` |
| Trace `app.py` with this input and show the values. Call Checkweave. | `checkweave_trace` |
| Replay that comparison. Call Checkweave. | `checkweave_replay` |
| Judge the text in this JSONL file. Call Checkweave. | `checkweave_semantic` |

Supporting tools let the assistant read stored evidence, check worker status,
page through trace events, or start and cancel longer work. Their exact names
and arguments are in the [command reference](usage.md#runs-and-mcp).
The example below needs no model setup.

## Example: a missing owner

You ask:

> Which records in `notes.jsonl` are missing an owner? Call Checkweave.

The assistant should call `checkweave_check`. The arguments are the file and the rule themselves, not wrapped in another object:

```json
{
  "include": ["notes.jsonl"],
  "predicate": {
    "op": "not",
    "predicate": { "op": "exists", "path": "/owner" }
  }
}
```

For the two-record file in [Getting started](getting-started.md), expect one match and one non-match. The record `{"id":2}` matches because it has no owner. The record `{"id":1,"owner":"ada"}` does not. The assistant should say that in words. If you add an owner to the second record and ask for the same check, the match count should drop to zero.

Asking “which rows are missing an owner?” without “call Checkweave” is easy for the assistant to answer by reading the file itself. Naming Checkweave makes the tool call the action you wanted.

## Example: a differing input

You ask:

> Compare `before.py` and `after.py` on the input `{"values": [1, -2, 3]}`. Call Checkweave.

The assistant should call `checkweave_compare` with the two commands and that input as direct arguments:

```json
{
  "before": { "argv": ["python3", "before.py"], "sources": ["before.py"] },
  "after": { "argv": ["python3", "after.py"], "sources": ["after.py"] },
  "inputs": [{ "values": [1, -2, 3] }]
}
```

`before.py` drops negative numbers and totals 4. `after.py` adds every number and totals 2. Checkweave should return that input as a stable difference. That case is a useful regression test. The assistant can explain the two totals. It should not rewrite the programs unless you asked it to.

A Python trace works the same way. You name the script and the input, the assistant calls `checkweave_trace`, and the result shows the values and source lines from that run.

## After the result

Read the assistant’s explanation against the evidence it was given. If you change the file, ask it to call Checkweave again. To rerun a comparison it already kept, ask it to call `checkweave_replay` with the id from that report. You do not need to assemble the id yourself with a script. Copy it from the report the assistant showed you.

Check whether a report is complete before drawing a conclusion. Unresolved
records need investigation; a partial scan did not cover everything requested.
If stored evidence is marked stale, rerun the check against the current files.
Model judgments should be reviewed separately from exact counts.

## What runs when

Checkweave updates its collection bookkeeping on its own as files change. Executing a program happens only when you or the assistant call a tool such as compare or trace. Saving a file does not launch those programs.

Compare replay runs the retained input against the current programs, or against a revision you named. Trace replay runs the snapshot saved with the original trace, so a fix needs a new trace.

## Privacy

Checkweave runs locally and keeps its evidence in the project’s ignored
`.checkweave/` folder. Optional semantic checks use local inference by default
and may download model files on first use. Configuring hosted Jev sends semantic
inputs to that provider; local failures never trigger that switch. The assistant
can still send returned evidence to its own AI provider.

Comparisons, traces, and replays execute the programs you name with your normal
local permissions. Their inputs and saved outputs make findings inspectable;
they do not isolate the program from the rest of your machine.

## What has been tested

Integration tests exercise the MCP protocol and the Cursor configuration that `init` writes. They do not open a real Cursor agent session. Real agent use is not established yet.
