# Recipes

Three small tasks you can run with Checkweave. From the Checkweave source
checkout or extracted release archive (both contain `examples/recipes`), save
its location once:

```sh
CHECKWEAVE_SOURCE="$PWD"
```

Each recipe creates its own temporary workspace. The Python recipes need
`python3`; none needs a model. For the assistant version, open the temporary
workspace in Cursor, run `checkweave init --agent cursor`, and use the prompt
shown below. The CLI commands work independently.

Command details: [check](usage.md#check), [compare and replay](usage.md#compare-and-replay),
[trace](usage.md#trace).

## Data quality

`examples/recipes/data-quality/exports.jsonl` is a config export. Every row
should have an `owner`.

Ask:

> The config export in exports.jsonl should have an owner on every row. Which rows fail, and is any row unreadable?

```sh
CHECKWEAVE_RECIPE=$(mktemp -d)
cp -R "$CHECKWEAVE_SOURCE/examples/recipes/data-quality/." "$CHECKWEAVE_RECIPE/"
cd "$CHECKWEAVE_RECIPE"
checkweave init --agent none
checkweave check \
  --include 'exports.jsonl' \
  --predicate '{"op":"exists","path":"/owner"}'
```

Expect `execution` `complete`, `freshness` `validated`, coverage `matched` 2,
`unmatched` 1 (`metrics`, no owner), and `unresolved` 1 (the last line is not
JSON). Next: fix those two lines, then run the same check. `unmatched` and
`unresolved` should both be 0.

## Refactor comparison

`examples/recipes/refactor/before.py` and `after.py` each read one JSON value
on stdin and write one JSON document on stdout. `after.py` drops negative
numbers.

Ask:

> I refactored the total so negatives are ignored. Did behavior change for values 1, -2, and 3?

```sh
CHECKWEAVE_RECIPE=$(mktemp -d)
cp -R "$CHECKWEAVE_SOURCE/examples/recipes/refactor/." "$CHECKWEAVE_RECIPE/"
cd "$CHECKWEAVE_RECIPE"
checkweave init --agent none
checkweave compare --request-file compare.json
```

Expect `outcome` `observed_difference`. The retained case is
`{"values":[1,-2,3]}`. Before total is 2. After total is 4. That is an
observation of this input, not proof the refactor is wrong in general, and a
later no-difference result would not prove the two programs are equivalent.
Next: if dropping negatives was unintended, restore the sum and compare again.
Compare replay re-runs this case against the current files.

## Debugging a wrong value

`examples/recipes/debug/discount.py` reads one JSON order and prints `charged`.
For price 80 and percent 20 the charge should be 64 (20% off). The script
returns 60.

Ask:

> discount.py returns the wrong charge for price 80 and percent 20. Where does the value go wrong?

```sh
CHECKWEAVE_RECIPE=$(mktemp -d)
cp -R "$CHECKWEAVE_SOURCE/examples/recipes/debug/." "$CHECKWEAVE_RECIPE/"
cd "$CHECKWEAVE_RECIPE"
checkweave init --agent none
checkweave trace --request-file trace.json
```

Expect `execution` `complete` and a `line` or `return` event in
`apply_discount` whose scalar local `charged` is 60. Next: change that line to
`price * (100 - percent) / 100`, then run a fresh trace. Trace replay would
rerun the saved snapshot of the old script. The new trace should show
`charged` as 64.0.

When finished with a recipe, run `checkweave shutdown` in that temporary
workspace. Keep its path if you want to inspect or replay the saved evidence.
