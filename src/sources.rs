//! Workspace source membership and content identity shared with collection checks.
//!
//! The walk, glob, path, and byte-hash rules match the collection engine. Collection
//! still has its own private copies until the kernel owner finishes and those helpers
//! can move here without changing check behavior.

use anyhow::{Context, Result, bail};
use blake3::Hasher;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::types::SourceFingerprint;

pub const HASH_READ_CAP: u64 = 1 << 30;
pub const LINE_CAPTURE: usize = 1024 * 1024;
/// Same walk ceiling as the hardened collection engine.
pub const MAX_WALK_STEPS: usize = 50_000;

#[derive(Debug, Clone)]
pub struct MemberList {
    pub paths: Vec<String>,
    /// False when cancel, deadline, or the walk step cap stopped the listing.
    pub complete: bool,
}

#[derive(Debug, Clone)]
pub enum HashedFile {
    Ready {
        fingerprint: String,
        bytes: u64,
    },
    /// Deadline or cancel interrupted the read. Not a content mismatch.
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceHalt {
    None,
    Cancel,
    Timeout,
    /// Global record budget was reached while reading this file.
    RecordBudget,
    /// The file shrank or grew relative to the size captured before the read.
    Unstable,
}

#[derive(Debug, Clone)]
pub struct SourceRecord {
    pub line: usize,
    pub bytes: Vec<u8>,
    pub content_hash: String,
    pub oversized: bool,
}

#[derive(Debug, Clone)]
pub struct ScannedSource {
    pub fingerprint: String,
    pub bytes: u64,
    pub skipped_blank: usize,
    pub records: Vec<SourceRecord>,
    pub halt: SourceHalt,
}

pub fn validate_globs(includes: &[String]) -> Result<()> {
    if includes.len() > 256 {
        bail!("include pattern count exceeds hard cap 256");
    }
    for pattern in includes {
        if pattern.is_empty() {
            bail!("empty include pattern");
        }
        if pattern.len() > 4096 {
            bail!("include pattern exceeds hard cap");
        }
        if pattern.contains('\0') {
            bail!("invalid include pattern");
        }
        if Path::new(pattern).is_absolute() {
            bail!("include pattern must be relative: {pattern}");
        }
        for part in pattern.split(['/', '\\']) {
            if part == ".." {
                bail!("include pattern escapes workspace: {pattern}");
            }
        }
        for comp in Path::new(pattern).components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => bail!("include pattern escapes workspace: {pattern}"),
            }
        }
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid include glob: {pattern}"))?;
    }
    Ok(())
}

pub fn enumerate(root: &Path, includes: &[String]) -> Result<Vec<String>> {
    let listed = enumerate_bounded(root, includes, None, None)?;
    if !listed.complete {
        bail!("source walk stopped before completion");
    }
    Ok(listed.paths)
}

pub fn enumerate_bounded(
    root: &Path,
    includes: &[String],
    deadline: Option<Instant>,
    cancel: Option<&AtomicBool>,
) -> Result<MemberList> {
    if includes.is_empty() {
        return Ok(MemberList {
            paths: Vec::new(),
            complete: true,
        });
    }
    let set = compile_globs(includes)?;
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .parents(false)
        .follow_links(false)
        .filter_entry(|ent| {
            if ent.depth() == 0 {
                return true;
            }
            let name = ent.file_name();
            if name == ".git" || name == ".checkweave" {
                return false;
            }
            !matches!(ent.file_type(), Some(ft) if ft.is_symlink())
        })
        .build();
    let mut out = Vec::new();
    let mut steps = 0usize;
    let mut complete = true;
    for ent in walker {
        steps += 1;
        if steps > MAX_WALK_STEPS
            || cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
            || deadline.is_some_and(|limit| Instant::now() >= limit)
        {
            complete = false;
            break;
        }
        let ent = ent.with_context(|| format!("walk {}", root.display()))?;
        if ent.depth() == 0 {
            continue;
        }
        if ent.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = ent.path();
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            continue;
        }
        let Some(rel) = rel_under(root, path) else {
            continue;
        };
        if set.is_match(rel.as_str()) {
            out.push(rel);
        }
    }
    out.sort();
    out.dedup();
    Ok(MemberList {
        paths: out,
        complete,
    })
}

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\0') {
        bail!("unsafe source path");
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(part) => {
                if part == ".git" || part == ".checkweave" || part == ".." {
                    bail!("unsafe source path");
                }
                out.push(part);
            }
            _ => bail!("unsafe source path"),
        }
    }
    Ok(out)
}

pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    match hash_file_bounded(path, None, None)? {
        HashedFile::Ready { fingerprint, bytes } => Ok((fingerprint, bytes)),
        HashedFile::Incomplete => bail!("hash stopped"),
    }
}

pub fn hash_file_bounded(
    path: &Path,
    deadline: Option<Instant>,
    cancel: Option<&AtomicBool>,
) -> Result<HashedFile> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
            || deadline.is_some_and(|limit| Instant::now() >= limit)
        {
            return Ok(HashedFile::Incomplete);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if total.saturating_add(n as u64) > HASH_READ_CAP {
            return Ok(HashedFile::Ready {
                fingerprint: "overflow".into(),
                bytes: total,
            });
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok(HashedFile::Ready {
        fingerprint: hasher.finalize().to_hex().to_string(),
        bytes: total,
    })
}

pub fn generation_of(sources: &[SourceFingerprint]) -> String {
    let mut hasher = Hasher::new();
    for src in sources {
        hasher.update(src.path.as_bytes());
        hasher.update(&[0]);
        hasher.update(src.fingerprint.as_bytes());
        hasher.update(&[0]);
        hasher.update(&src.bytes.to_le_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// `complete` runs require the same membership. A partial run stays current only
/// while its recorded prefix is still the start of the membership list.
pub fn snapshot_holds(execution: &str, sources: &[SourceFingerprint], members: &[String]) -> bool {
    let recorded: Vec<&str> = sources.iter().map(|s| s.path.as_str()).collect();
    let member_refs: Vec<&str> = members.iter().map(|s| s.as_str()).collect();
    if execution == "complete" {
        recorded == member_refs
    } else {
        recorded.len() <= member_refs.len()
            && recorded
                .iter()
                .copied()
                .eq(member_refs.iter().copied().take(recorded.len()))
    }
}

/// Read one file. `max_records` is the remaining global record budget.
/// Byte hashing continues through the file after that budget so the fingerprint
/// still identifies the whole file. Cancel, timeout, and a size change drop this
/// file's records and return that halt.
pub fn scan_file(
    path: &Path,
    meta_len: u64,
    max_records: usize,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<ScannedSource> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut file_hasher = Hasher::new();
    let mut remaining = meta_len;
    let mut records = Vec::new();
    let mut skipped_blank = 0usize;
    let mut line_no = 0usize;
    let mut stop_records = false;
    let mut halt = SourceHalt::None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(empty_halt(SourceHalt::Cancel));
        }
        if Instant::now() >= deadline {
            return Ok(empty_halt(SourceHalt::Timeout));
        }
        match read_logical_line(&mut reader, &mut file_hasher, &mut remaining)? {
            ReadLine::Eof => break,
            ReadLine::Budget => return Ok(empty_halt(SourceHalt::Unstable)),
            ReadLine::Line(line) => {
                line_no += 1;
                if line.oversized || !line.bytes.iter().all(|b| b.is_ascii_whitespace()) {
                    if stop_records || records.len() >= max_records {
                        stop_records = true;
                        halt = SourceHalt::RecordBudget;
                        continue;
                    }
                    records.push(SourceRecord {
                        line: line_no,
                        bytes: line.bytes,
                        content_hash: line.hash,
                        oversized: line.oversized,
                    });
                } else if !stop_records {
                    skipped_blank += 1;
                }
            }
        }
    }

    Ok(ScannedSource {
        fingerprint: file_hasher.finalize().to_hex().to_string(),
        bytes: meta_len.saturating_sub(remaining),
        skipped_blank,
        records,
        halt,
    })
}

fn empty_halt(halt: SourceHalt) -> ScannedSource {
    ScannedSource {
        fingerprint: String::new(),
        bytes: 0,
        skipped_blank: 0,
        records: Vec::new(),
        halt,
    }
}

fn compile_globs(includes: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in includes {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid include glob: {pattern}"))?;
        builder.add(glob);
    }
    builder.build().context("build include globs")
}

fn rel_under(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part == ".git" || part == ".checkweave" || part == ".." {
                    return None;
                }
                parts.push(part);
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

struct LogicalLine {
    bytes: Vec<u8>,
    hash: String,
    oversized: bool,
}

enum ReadLine {
    Eof,
    Line(LogicalLine),
    Budget,
}

fn read_logical_line(
    reader: &mut BufReader<File>,
    file_hasher: &mut Hasher,
    remaining: &mut u64,
) -> Result<ReadLine> {
    let mut logical = Vec::new();
    let mut hasher = Hasher::new();
    let mut oversized = false;
    let mut pending_cr = false;
    let mut saw = false;
    loop {
        if *remaining == 0 {
            if !saw && !pending_cr {
                let extra = reader.fill_buf()?;
                if extra.is_empty() {
                    return Ok(ReadLine::Eof);
                }
                return Ok(ReadLine::Budget);
            }
            return Ok(ReadLine::Budget);
        }
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            if pending_cr {
                push_byte(&mut logical, &mut hasher, &mut oversized, b'\r');
            }
            if !saw && logical.is_empty() && !oversized {
                return Ok(ReadLine::Eof);
            }
            return Ok(ReadLine::Line(LogicalLine {
                bytes: if oversized { Vec::new() } else { logical },
                hash: hasher.finalize().to_hex().to_string(),
                oversized,
            }));
        }
        let allow = (*remaining).min(avail.len() as u64) as usize;
        let chunk = &avail[..allow];
        let mut consumed = 0usize;
        let mut finished = false;
        for &b in chunk {
            consumed += 1;
            saw = true;
            if pending_cr {
                if b == b'\n' {
                    pending_cr = false;
                    finished = true;
                    break;
                }
                push_byte(&mut logical, &mut hasher, &mut oversized, b'\r');
                pending_cr = false;
            }
            if b == b'\r' {
                pending_cr = true;
                continue;
            }
            if b == b'\n' {
                finished = true;
                break;
            }
            push_byte(&mut logical, &mut hasher, &mut oversized, b);
        }
        file_hasher.update(&chunk[..consumed]);
        *remaining -= consumed as u64;
        reader.consume(consumed);
        if finished {
            return Ok(ReadLine::Line(LogicalLine {
                bytes: if oversized { Vec::new() } else { logical },
                hash: hasher.finalize().to_hex().to_string(),
                oversized,
            }));
        }
    }
}

fn push_byte(logical: &mut Vec<u8>, hasher: &mut Hasher, oversized: &mut bool, b: u8) {
    hasher.update(&[b]);
    if *oversized {
        return;
    }
    if logical.len() >= LINE_CAPTURE {
        *oversized = true;
        logical.clear();
        return;
    }
    logical.push(b);
}
