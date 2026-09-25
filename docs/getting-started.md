# Getting started

This tutorial installs Checkweave from the checkout you already have, then runs one small check in a temporary folder. It does not change your project.

You need **Rust 1.88 or newer**. **Python 3** is optional and is used only in the comparison near the end. The data check does not need Python, an account, or a model.

On Windows, use a WSL shell and keep the checkout on the Linux filesystem. Native Windows is not supported. Machine details are in [platforms](platforms.md).

## 1. Install from this checkout

From the Checkweave directory (the one that contains `Cargo.toml`):

```sh
cargo install --path . --locked
export PATH="$HOME/.cargo/bin:$PATH"
command -v checkweave
```

`cargo install` places the program in `~/.cargo/bin`. The `export` line is for this shell. Add that directory to your shell startup file if you want it in later sessions. `command -v` should print a path ending in `checkweave`.

There is no published download yet. If you are inspecting an archive from the
release workflow, extract it and add the extracted directory to `PATH` instead
of compiling. The binary runs directly; the included `docs` and `examples/recipes`
provide this tutorial and the [recipes](recipes.md). Python 3 is needed for the
Python recipes. Rust is needed only for the source installation above.

## 2. Create a temporary folder

```sh
CHECKWEAVE_DEMO=$(mktemp -d)
cd "$CHECKWEAVE_DEMO"
checkweave --workspace "$CHECKWEAVE_DEMO" init --agent none
```

`mktemp` creates a new directory outside your project. `init --agent none` prepares that folder for Checkweave and does not write editor settings. Later commands use `$CHECKWEAVE_DEMO`, so keep this shell open.

## 3. Check for a missing owner

JSONL means one JSON record per line. This file has two records. The second has no owner.

```sh
printf '%s\n' '{"id":1,"owner":"ada"}' '{"id":2}' > "$CHECKWEAVE_DEMO/notes.jsonl"
checkweave --workspace "$CHECKWEAVE_DEMO" check \
  --include notes.jsonl \
  --predicate '{"op":"not","predicate":{"op":"exists","path":"/owner"}}'
```

The command prints a report. Expect `records` 2, `matched` 1, and `unmatched` 1. The second record matches the rule “owner is missing.” The first record has an owner, so it does not match. The command still succeeds: an unmatched record is a normal answer.

Run the same check again:

```sh
checkweave --workspace "$CHECKWEAVE_DEMO" check \
  --include notes.jsonl \
  --predicate '{"op":"not","predicate":{"op":"exists","path":"/owner"}}'
```

Nothing in the file changed, so both records are reused. Expect `cache_hits` 2.

Give the second record an owner, then run the same check again:

```sh
printf '%s\n' '{"id":1,"owner":"ada"}' '{"id":2,"owner":"grace"}' > "$CHECKWEAVE_DEMO/notes.jsonl"
checkweave --workspace "$CHECKWEAVE_DEMO" check \
  --include notes.jsonl \
  --predicate '{"op":"not","predicate":{"op":"exists","path":"/owner"}}'
```

Both records now have an owner, so expect `matched` 0. The edited line is new work and the first line is unchanged: expect `cache_hits` 1 and `cache_misses` 1.

A line that is not valid JSON is **unresolved**. A check that stops early because of a limit is **partial**. Those words, and the rest of the report, are in [usage](usage.md).

## 4. Optional: compare two Python programs

Skip this section if you do not have `python3`.

`before.py` drops negative numbers. `after.py` adds every number. For the input `[1, -2, 3]`, those totals are 4 and 2.

```sh
cat > "$CHECKWEAVE_DEMO/before.py" <<'PY'
import json, sys
value = json.load(sys.stdin)
print(json.dumps({"total": sum(n for n in value["values"] if n >= 0)}))
PY
cat > "$CHECKWEAVE_DEMO/after.py" <<'PY'
import json, sys
value = json.load(sys.stdin)
print(json.dumps({"total": sum(value["values"])}))
PY
cat > "$CHECKWEAVE_DEMO/compare.json" <<'EOF'
{
  "before": {"argv": ["python3", "before.py"], "sources": ["before.py"]},
  "after": {"argv": ["python3", "after.py"], "sources": ["after.py"]},
  "inputs": [{"values": [1, -2, 3]}]
}
EOF
checkweave --workspace "$CHECKWEAVE_DEMO" compare --request-file "$CHECKWEAVE_DEMO/compare.json"
```

Expect `outcome` to be `observed_difference`, with totals of 4 before and 2
after. This gives you a concrete case to turn into a regression test: if the
requirement is to include negative numbers, the expected total is 2. Checkweave
reports the difference; your intended behavior determines which version is right.

The programs run with your normal local permissions. This example only reads
its input and prints a result.

To run that retained case again, copy the id from the report and replace the placeholder:

```sh
checkweave --workspace "$CHECKWEAVE_DEMO" replay --kind compare --id ID_FROM_REPORT
```

How long results are kept, and how replay decides that a case still matches, is in [behavior comparison](behavior.md). A new trace checks a fix in the file you just edited. Trace replay runs the old snapshot saved with the original trace. That difference is spelled out in [execution evidence](execution-evidence.md).

## 5. Stop

```sh
checkweave --workspace "$CHECKWEAVE_DEMO" shutdown
echo "$CHECKWEAVE_DEMO"
```

`shutdown` stops Checkweave for this folder. You can delete `$CHECKWEAVE_DEMO` when you are finished. To use Checkweave in a real project, open that project and run `checkweave init`. The [MCP guide](mcp.md) explains what that connects.
