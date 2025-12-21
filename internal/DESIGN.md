# Project Goal

Build a native Rust build engine that:

- Owns parsing of `Cargo.toml` and `Cargo.lock`
- Constructs its own build graph via picante (incremental queries)
- Executes compiler actions via rapace services
- Stores all artifacts in content-addressed storage from day one
- Implements correct caching for a deliberately tiny subset

The initial goal is not Cargo compatibility.
The initial goal is correctness for a tiny, explicit subset.

---

## v0 Success Definition

v0 is successful if:

- A single-crate Rust project with no dependencies builds correctly
- The build is executed via a picante-driven graph
- All outputs are stored in CAS
- Cache reuse is correct: no-op builds do zero rustc invocations
- Outputs are materialized under a tool-owned directory (not `target/`)
- Unsupported features fail loudly and clearly

**Definition of done for caching:**

- Run `vx build` twice → second run does zero rustc invocations, only materializes from CAS ✓
- Move the repo to a different absolute path → still a cache hit (requires `--remap-path-prefix`) ✓
- Change one byte in `src/main.rs` → cache miss and rebuild ✓
- Change `edition` in `Cargo.toml` → cache miss and rebuild ✓
- Change rustc toolchain → cache miss and rebuild ✓

"Hello world builds deterministically with correct caching" is the bar.

---

## Explicit Non-Goals (v0)

v0 will not support:

- Workspaces
- Dependencies
- Features
- Build scripts (`build.rs`)
- Proc macros
- Tests / benches / examples
- Multiple targets
- Incremental compilation
- Registry resolution

Each unsupported feature must produce a clear diagnostic, not silent fallback.

---

## Architecture: Everything is a Service

From day one, everything is a service boundary. This sets up for:
- Daemonization
- Remote workers
- CI parity
- Sandboxing

### Process Topology

Four separate binaries communicating over rapace:

```
                              ┌──────────┐
vx ──rapace──► vx-daemon ─────┤ vx-casd  │
                    │         └────▲─────┘
                    │              │ rapace
                    │              │
                    └──► vx-execd ─┘
```

- `vx` — CLI binary, connects to daemon
- `vx-daemon` — orchestration binary, connects to CAS and Exec
- `vx-casd` — CAS service binary
- `vx-execd` — execution service binary, also connects to CAS

Rapace handles transport — SHM for local, network for remote.

**Current state:** CLI instantiates services in-process (temporary shortcut).
**Target state:** All four as separate processes over rapace.

---

## Filesystem Access Rules (Staged)

The "service boundaries" principle needs staged enforcement:

**v0 rules:**
- Daemon may read project sources (Cargo.toml, src/main.rs) directly
- Daemon may write materialized outputs to `.vx/build/` directly
- Daemon may execute rustc directly

**Hard rule (all versions):**
- Daemon must not persist artifacts except via CAS
- All durable artifact storage (blobs, manifests, cache mappings) lives in CAS
- Nothing is cached without going through CAS

**v1 rules:**
- Execution moves to vx-execd service
- Materialization moves to a service (or stays daemon-local for perf)
- Source reading may stay daemon-local (it's input, not artifact)

This staged approach avoids awkward "read file over RPC" shims while maintaining the core invariant: **CAS is the only durable artifact store**.

---

## Crate Structure

```
vx/
  crates/
    vx/               # CLI binary - thin client
    vx-daemon/        # Orchestration with picante DB + DaemonService impl
    vx-daemon-proto/  # Daemon RPC protocol (BuildRequest, BuildResult, Daemon trait)
    vx-casd/          # CAS service implementation
    vx-cas-proto/     # CAS RPC protocol (Cas trait, blob/manifest types)
    vx-execd/         # Execution service implementation (stub)
    vx-exec-proto/    # Exec RPC protocol (RustcInvocation, etc.)
    vx-manifest/      # Cargo.toml parsing → typed internal model
```

### Crate Responsibilities

| Crate | Responsibility |
|-------|----------------|
| `vx` | CLI, thin client, instantiates daemon (in-process for v0) |
| `vx-daemon` | Picante DB, orchestration, cache key computation, build execution |
| `vx-daemon-proto` | Daemon service trait + request/response types |
| `vx-casd` | CAS service: blobs, manifests, cache index |
| `vx-cas-proto` | CAS service trait + blob/manifest types |
| `vx-execd` | Execution service (stub, daemon runs rustc directly for v0) |
| `vx-exec-proto` | Exec service trait + invocation types |
| `vx-manifest` | Parse `Cargo.toml` with facet-toml, validate v0 subset |

---

## Protocol Definitions

### Daemon Protocol (`vx-daemon-proto`)

```rust
#[derive(Facet)]
pub struct BuildRequest {
    pub project_path: Utf8PathBuf,
    pub release: bool,
}

#[derive(Facet)]
pub struct BuildResult {
    pub success: bool,
    pub message: String,
    pub cached: bool,
    pub duration_ms: u64,
    pub output_path: Option<Utf8PathBuf>,
    pub error: Option<String>,
}

#[rapace::service]
pub trait Daemon {
    async fn build(&self, request: BuildRequest) -> BuildResult;
}
```

### CAS Protocol (`vx-cas-proto`)

```rust
pub const CACHE_KEY_SCHEMA_VERSION: u32 = 1;

pub type BlobHash = Blake3Hash;
pub type ManifestHash = Blake3Hash;
pub type CacheKey = Blake3Hash;

#[derive(Facet)]
pub struct NodeManifest {
    pub node_id: NodeId,
    pub cache_key: CacheKey,
    pub produced_at: String,
    pub outputs: Vec<OutputEntry>,
}

#[rapace::service]
pub trait Cas {
    // Cache key operations
    async fn lookup(&self, cache_key: CacheKey) -> Option<ManifestHash>;
    async fn publish(&self, cache_key: CacheKey, manifest_hash: ManifestHash) -> PublishResult;

    // Manifest operations
    async fn put_manifest(&self, manifest: NodeManifest) -> ManifestHash;
    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest>;

    // Blob operations (v0: small blobs only)
    async fn put_blob(&self, data: Vec<u8>) -> BlobHash;
    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>>;
    async fn has_blob(&self, hash: BlobHash) -> bool;

    // Chunked blob upload (large files)
    async fn begin_blob(&self) -> BlobUploadId;
    async fn blob_chunk(&self, id: BlobUploadId, chunk: Vec<u8>);
    async fn finish_blob(&self, id: BlobUploadId) -> FinishBlobResult;

    // Future: chunked download for large artifacts
    // async fn read_blob(&self, hash: BlobHash, offset: u64, len: u32) -> Option<Vec<u8>>;
}
```

**Design notes:**
- `materialize` removed from CAS — clients read blobs and write outputs themselves
- v0 uses `get_blob` for small artifacts; large artifacts should use chunked upload
- Future: add symmetric chunked download (`read_blob` with offset/len)

### Publish Semantics

`publish(cache_key, manifest_hash)` has specific semantics:

1. **Validation:** Fails if `manifest_hash` doesn't exist in CAS
2. **Overwrite policy:** First writer wins — if cache key already mapped, publish succeeds but doesn't overwrite
3. **Returns:** `PublishResult { success: bool, error: Option<String> }`

This "first writer wins" policy means:
- Concurrent builds racing to publish the same key are safe
- The first to complete wins; others succeed silently
- No conflict resolution needed

---

## Picante Database (`vx-daemon`)

The daemon uses picante for incremental computation.

**Key principle:** Picante determines *when* the cache key must be recomputed; vx defines *what* the cache key is.

The `CacheKey` is an explicit blake3 hash computed by the `cache_key_compile_bin` query from well-defined inputs. This keeps CacheKey stable across picante internal changes — we don't couple CAS stability to picante's internal hashing/encoding.

### Inputs

```rust
#[picante::input]
pub struct SourceFile {
    #[key]
    pub path: String,
    pub content_hash: Blake3Hash,
}

#[picante::input]
pub struct CargoToml {
    pub content_hash: Blake3Hash,
    pub name: String,
    pub edition: Edition,
    pub bin_path: String,
}

#[picante::input]
pub struct RustcToolchain {
    pub rustc_path: String,      // resolved path to rustc
    pub version_string: String,  // full `rustc -vV` output
}

#[picante::input]
pub struct BuildConfig {
    pub profile: String,
    pub target_triple: String,
    pub workspace_root: String,
}
```

**Toolchain resolution rule:** Honor `RUSTC` env var if set, else find `rustc` in `PATH`. The resolved path and its version output become inputs.

### Tracked Queries

```rust
/// Compute the cache key for compiling a binary crate.
/// This is an EXPLICIT blake3 hash of all inputs — not picante's internal fingerprint.
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(
    db: &DB,
    source: SourceFile,
) -> PicanteResult<CacheKey>;

/// Build a rustc invocation for compiling a binary crate.
#[picante::tracked]
pub async fn plan_compile_bin<DB: Db>(db: &DB) -> PicanteResult<RustcInvocation>;

/// Generate a human-readable node ID.
#[picante::tracked]
pub async fn node_id_compile_bin<DB: Db>(db: &DB) -> PicanteResult<NodeId>;
```

---

## Cache Key Computation

For v0, "proper" caching means: if any input that can affect the produced binary changes, the cache key changes.

**CacheKey = blake3 hash of:**

**Schema version:**
- `CACHE_KEY_SCHEMA_VERSION` (bump when canonicalization changes)

**Toolchain identity:**
- `rustc -vV` full output (includes version, commit hash, LLVM version, host triple)

**Build configuration:**
- Target triple (explicit)
- Profile (`debug` / `release`)
- Crate name
- Edition
- Crate type (`bin`)

**Source inputs:**
- Content hash (blake3) of `src/main.rs`
- Content hash (blake3) of `Cargo.toml`

**What's NOT in the cache key:**
- Workspace root path — `--remap-path-prefix` normalizes path-sensitive outputs
- Absolute paths to source files — we hash content, not location

**Path determinism:**
- Pass `--remap-path-prefix <workspace>=/vx-workspace` when path-sensitive outputs are enabled (debuginfo)
- All temp dirs and output dirs are under `.vx/` for consistency
- Note: some outputs may still embed paths in diagnostics metadata (rare for v0)

---

## Content-Addressed Storage (`vx-casd`)

**Storage Layout:**
```
.vx/
  cas/
    blobs/<hh>/<hash>              # raw bytes (sharded by first 2 hex chars)
    manifests/<hh>/<hash>.json     # structured node output records
    cache/<hh>/<cachekey>          # contains manifest hash (cache index)
    tmp/                           # staging for atomic writes
```

**Design Rules:**
- All writes go through `tmp/` then atomic rename
- Blobs are immutable once written
- Manifests reference blobs by hash
- Cache index maps CacheKey → ManifestHash
- `publish()` validates manifest exists before writing cache mapping
- First writer wins on cache key conflicts

---

## Build Reports vs CAS

Two kinds of truth, different purposes:

**CAS (artifact truth):**
- `NodeManifest`: outputs + hashes + cache key
- Cache index: cache key → manifest mapping
- Blobs: actual artifact bytes
- Stable, minimal, long-lived

**BuildReport (run log):**
- Cache hit/miss
- Rustc invocation used
- Timings (wall clock, rustc duration)
- stdout/stderr (maybe as blobs)
- User-facing, verbose, per-run

The report can be thin because CAS already contains artifact truth. Report adds context about *this run*.

---

## Filesystem Layout

```
.vx/
  cas/
    blobs/<hh>/<hash>
    manifests/<hh>/<hash>.json
    cache/<hh>/<cachekey>
    tmp/
  build/
    <triple>/
      debug/
        <crate-name>           # materialized binary
      release/
        <crate-name>
  runs/
    <run-id>.json              # build reports (future)
```

---

## v0 Build Flow

```
1. vx build instantiates DaemonService (in-process for v0)

2. DaemonService.build():
   - Resolves rustc (RUSTC env or PATH)
   - Parses Cargo.toml (facet-toml via vx-manifest)
   - Creates picante Database
   - Sets inputs (SourceFile, CargoToml, RustcToolchain, BuildConfig)
   - Runs cache_key_compile_bin query

3. Cache check:
   - Calls CAS: lookup(cache_key)

4. If cache hit:
   - Calls CAS: get_manifest(manifest_hash)
   - For each output: get_blob() and write to .vx/build/...
   - Returns BuildResult { cached: true, ... }

5. If cache miss:
   - Runs plan_compile_bin query to get RustcInvocation
   - Executes rustc directly (future: via vx-execd)
   - Reads output binary, calls CAS: put_blob()
   - Creates NodeManifest, calls CAS: put_manifest()
   - Calls CAS: publish(cache_key, manifest_hash)
   - Returns BuildResult { cached: false, duration_ms, ... }

6. CLI prints result
```

---

## CLI Contract (v0)

### `vx build`

1. Instantiate daemon (in-process)
2. Send BuildRequest with working directory + release flag
3. Receive BuildResult (success/failure, cache hit/miss, outputs)
4. Print minimal human summary

### `vx explain`

Not yet implemented.

---

## Error Philosophy

Errors must be:
- **Explicit** — say what happened
- **Actionable** — say what to do
- **Honest** — admit missing features

**Bad:**
```
unsupported
```

**Good:**
```
dependencies are not supported yet (found `foo = "1.0"` in [dependencies])
```

---

## Future-Facing Design Constraints (Do Not Violate)

1. **No implicit inputs** — Every file/env influencing a node must be in the cache key.
2. **No Cargo fallback** — If something isn't supported, fail.
3. **No global mutable state** — Everything needed to explain a build must be in the report.
4. **Stable identifiers** — Node IDs must be deterministic across runs.
5. **Model > execution** — Never "just run a command" without a node.
6. **CAS for all artifacts** — All durable artifact state lives in CAS. Daemon orchestrates via RPC.
7. **Correct cache keys** — If inputs change, the cache key must change.
8. **Explicit cache keys** — CacheKey is a well-defined blake3 hash, not picante internals.

---

## v0 Checklist (Engineering Tasks)

- [x] Create crate structure
- [x] Define rapace service traits (Cas, Daemon)
- [x] Implement vx-casd (blobs, manifests, cache index)
- [x] Implement vx-daemon with picante DB
  - [x] Define inputs (SourceFile, CargoToml, RustcVersion, BuildConfig)
  - [x] Implement cache_key_compile_bin query
  - [x] Implement plan_compile_bin query
  - [x] Implement node_id_compile_bin query
- [x] Implement vx CLI as thin client
- [x] Parse `Cargo.toml` → internal manifest
- [x] Validate supported subset (reject deps, workspace, features, build.rs)
- [x] Verify: second build = cache hit (zero rustc invocations)
- [x] Verify: source change = cache miss
- [x] Verify: profile change (debug/release) = separate cache keys
- [ ] Add RustcToolchain input (resolve RUSTC env / PATH)
- [ ] Implement vx-execd (currently daemon runs rustc directly)
- [ ] Write build reports
- [ ] Implement `vx explain`
- [ ] Verify: different checkout path = cache hit

---

## Closing Principle

We are not building "Cargo, but better".
We are building a build engine that happens to start with Cargo's file format.

Correctness, transparency, and ownership matter more than speed of feature acquisition.
