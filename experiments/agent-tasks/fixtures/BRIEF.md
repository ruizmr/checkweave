# Three debugging tasks

Complete all three tasks. The deliverable is findings and evidence. Do not add product features, patches, or a rewritten program unless a task asks for a separate repro script.

Use the tools available in this environment. A correct result is what matters. There is no required tool sequence.

Write these files, relative to the workspace root (the parent of `TASK/`):

- `submission/answers.json`
- `submission/repro.py`

Do not modify anything under `TASK/`.

## 1. Policy count

Files: `TASK/policy/before.jsonl` and `TASK/policy/after.jsonl`.

Each line is one JSON object with `id`, `action`, `risk`, and `region`.

A record changed between the two files when its `id` is present on only one side, or when `action` differs, or when `risk` differs. A difference in `region` alone is not a change.

An after-file record violates policy when either of these is true:

- `action` is `allow` and `risk` is greater than or equal to 70
- `action` is `deny` and `risk` is less than 30

Records that exist only in the before file are changes. They are not after-file violations.

In `submission/answers.json`, set `policy` to:

```json
{
  "changed_count": 0,
  "changed_ids": ["sorted ids"],
  "changes": [
    {
      "id": "id",
      "before": {"action": "allow", "risk": 1},
      "after": {"action": "deny", "risk": 1}
    }
  ],
  "after_violation_count": 0,
  "after_violation_ids": ["sorted ids"]
}
```

Use `null` for `before` or `after` when that id is absent on that side. `changes` must be sorted by `id` and must contain every changed id. `before` and `after` objects, when present, must copy `action` and `risk` from the files.

## 2. Behavior difference

Files: `TASK/diff/billing_before.py` and `TASK/diff/billing_after.py`. Both define `invoice_cents(lines)`.

`submission/repro.py` must run with `python3` and print exactly one line on stdout:

```text
DIFF <json> <before_total> <after_total>
```

`<json>` is a JSON array of objects with `qty` and `cents`. The two totals are integers. They must be the values those functions return for that array, and the totals must differ. Keep the JSON under 400 characters. Do not import a network, and do not modify the billing modules.

## 3. Exception origin

Run `python3 TASK/incident/main.py` from the workspace root, or run `main.py` with that directory on `sys.path`. The process raises.

The outer error is a wrapper. Report the originating exception (the cause), not the wrapper.

In `submission/answers.json`, set `exception` to:

```json
{
  "exception_type": "ExceptionClassName",
  "origin_file": "basename.py",
  "origin_line": 1,
  "source_evidence": "contiguous substring of that source line, at least 10 characters",
  "cause": "one sentence naming the key that was missing"
}
```

`origin_line` is 1-based in `origin_file`. `source_evidence` must appear in that source line.

## answers.json shape

```json
{
  "policy": {},
  "exception": {}
}
```

The behavior-difference deliverable is `submission/repro.py`, not a field in the JSON.
