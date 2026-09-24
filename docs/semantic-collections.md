# Semantic collections

Use a semantic check when a rule needs to interpret text: for example, “Does
each bug report describe steps to reproduce the problem?” Supply records as
JSON Lines, choose the text field and question, and Checkweave returns model
judgments with references to the records. For an exact rule such as a missing
field or a numeric limit, use the [model-free check command](usage.md#check).

The [semantic command example](usage.md#semantic-collection) shows the smallest
request. The rest of this page describes result fields and caching.

A semantic collection check reads the same workspace membership as a deterministic collection check, extracts one string from each JSON Lines record, and judges that string with the managed model provider. Counting, limits, and freshness stay in Rust. The model does not decide which files exist or which records were skipped.

The local checkpoint is whatever the provider is configured to run. The current local pin is a provisional open-weight candidate, not a release-qualified default. Hosted Jev is used only when `checkweave.toml` selects it. This adapter does not choose or download a different model on its own.

## Request

`SemanticCheckRequest` fields:

- `globs`: workspace-relative glob list, same rules as collection `include`
- `text_pointer`: JSON Pointer to a string. `""` is the whole JSON value
- `questions`: typed `ModelQuestion` values (`choice`, `ordinal`, or `predicate`)
- `limits`: `Limits` for files, bytes, records, and returned decisions. Omitted `timeout_ms`, including when `limits` itself is omitted or the object leaves `timeout_ms` out, is `180000`. An explicit `timeout_ms` is used as written, including `30000`. `0` is rejected. The hard cap stays `600000`. Deterministic collection checks still default to `30000`.
- `batch_size`: model states per call, from 1 to 32

`max_results` bounds how many decisions are copied into the report. It does not stop judgment of the remaining rows. `max_files`, `max_bytes`, `max_records`, and `timeout_ms` bound the full scan, directory walk, rehash, cache load, model calls, and publication. Record and result caps are global across files. A walk or rehash that hits the deadline finishes as partial or `freshness: unknown`, not as a validated complete scan.

## Report

`SemanticReport` uses `basis = "model_judgment"`.

`execution` is `complete`, `partial`, or `cancelled`. Cancellation is never reported as complete. `freshness` is `validated`, `stale`, or `unknown` for the recorded source snapshot. After the run, and again when evidence is loaded, membership is listed again and each recorded file is hashed. A file that changes during the run makes the report `stale`.

`coverage` counts records and pair outcomes separately from whether a label is the preferred one. Blank lines increment `skipped`. Invalid JSON and a missing pointer are `unresolved`. Non-text values, oversized lines, and text longer than 1 MiB of UTF-8 are `unsupported`. That byte cap is a serialized resource limit. Token limits are enforced by the model worker on the full prompt, not by a bytes-per-token guess, so long text with few tokens is still sent. None of those outcomes are omitted. `cache_hits` are reused observations. `cache_misses` and `fresh_model_calls` are new model work.

Each returned decision keeps the source path, line, and file fingerprint, the state id sent to the model, and the question id. Duplicate input text still gets distinct state ids. The original record and the extracted text are included when each fits the 32 KiB evidence budget. Larger values set `omissions` and are not silently shortened.

## Cache

Item identity is the blake3 of the operator version, the extracted text, the canonical question, and the full model provenance except usage. That payload includes provider, model, revision, device, precision, adapter, runtime versions (null values kept), score semantics, input policy, backend, GGUF hash, code revision, device fallback, tokenizer fields carried on the provenance object, settings fingerprint, config source fingerprint, serialization (`json-pointer-utf8-v1`), and a hash of model-related environment variables. Environment names outside the tracked set are listed under `limitations` and included in that hash.

Hits are accepted only after `ModelProvider::readiness` returns the current ready-frame provenance. A stored auto-device latch is not enough. If that call is not offered, one row is sent first and later rows may be reused only under the provenance that call returned. If a later batch reports different provenance, that batch is recomputed once and left unresolved if the identity still does not match. An alias such as `latest` is not a cache key.

User requests may contain up to 64 questions. Each worker call is split to at most 16 questions, 32 states, and 64 state-question pairs. Every valid pair is judged once. `batch_size` still cannot exceed 32.

Unchanged text is reused only under that live identity. A changed question, settings fingerprint, or live device/runtime invalidates the affected rows.

SQLite lives at `.checkweave/semantic.sqlite` in the workspace. It is a separate file from the deterministic collection cache. Items, reports, and identity rows are deleted in batches of 256 and the WAL is checkpointed. The logical payload cap is 64 MiB, and at most 64 identity rows are kept. A database is replaced only when SQLite reports corruption. The full source fingerprint list is stored for freshness even when decision text is omitted to keep the published JSON under 7 MiB plus protocol overhead. Coverage counts stay exact when details are truncated.

## Evidence

`evidence(root, id)` returns `Ok(None)` when the id is absent. A stored row that cannot be decoded returns an error. A found report is rechecked against the workspace and may change from `validated` to `stale`.

## API entry points

```rust
pub async fn check(
    root: &Path,
    request: &SemanticCheckRequest,
    provider: &mut crate::models::ModelProvider,
    cancel: Arc<AtomicBool>,
) -> Result<SemanticReport>;

pub fn evidence(root: &Path, id: &str) -> Result<Option<SemanticReport>>;
```

Source listing and hashing used by this check are public in `crate::sources` (`enumerate`, `hash_file`, `safe_join`, `scan_file`, `snapshot_holds`, `generation_of`). The collection engine still has its own private copies until that owner is finished.
