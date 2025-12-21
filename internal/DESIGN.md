# vertex

vertex is a native Rust build engine. The CLI binary is named `vx`.

## Project Goal

Build a native Rust build engine that:

- Owns parsing of `Cargo.toml` and `Cargo.lock`
- Constructs its own build graph via picante (incremental queries)
- Executes compiler actions via rapace services
- Stores all artifacts in content-addressed storage from day one
- Implements correct caching for a deliberately tiny subset

The initial goal is not full Cargo compatibility.
The initial goal is correctness for a tiny, explicit subset.

---

## v0 Scope

v0 targets single-crate Rust projects with no dependencies.

**Success criteria:**

- Build is executed via a picante-driven graph
- All outputs are stored in CAS
- Cache reuse is correct: no-op builds do zero rustc invocations
- Outputs are materialized under `.vx/build/` (not `target/`)
- Unsupported features fail loudly and clearly

**Caching correctness means:**

- Second build = cache hit, zero rustc invocations
- Different checkout path = still a cache hit (`--remap-path-prefix`)
- Change one byte in source = cache miss
- Change edition = cache miss
- Change rustc toolchain = cache miss

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

## Architecture: Everything Durable Crosses a Service Boundary

From day one, all durable storage and execution crosses a service boundary. This sets up for:
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

**Service endpoints are configured (precedence: CLI flags > env vars > defaults):**
- CLI knows daemon endpoint (`--daemon` / `$VX_DAEMON` / default SHM socket)
- Daemon knows CAS endpoint (`$VX_CAS` / default SHM socket)
- Daemon knows Exec endpoint (`$VX_EXEC` / default SHM socket)
- Execd knows CAS endpoint (`$VX_CAS` / default SHM socket)

**v0 deployment:** All four run as separate processes on localhost using SHM transport.

**Process lifecycle:**
- `vx build` auto-spawns daemon if not running; daemon auto-spawns casd and execd
- `vx kill` sends shutdown to daemon, which cascades to execd (casd keeps running for other projects)
- Daemon is the supervisor for execd; casd is independent (shared resource)

---

## Filesystem Access Rules

The "service boundaries" principle needs staged enforcement:

**Hard rule (all versions):**
- All durable artifact storage (blobs, manifests, cache mappings) lives in CAS
- Nothing is cached without going through CAS
- Daemon never writes to `.vx/cas/` directly

**Allowed direct filesystem access:**
- Daemon may read project sources (Cargo.toml, src/main.rs)
- Daemon may write materialized outputs to `.vx/build/`
- vx-execd may read sources and write temp files during compilation

This avoids awkward "read file over RPC" shims while maintaining the core invariant: **CAS is the only durable artifact store**.

**Staged capability for execd:**
- v0: execd reads sources directly from the local workspace (simplest)
- Later: execd can fetch source blobs from CAS, enabling remote execution

**v0 execd contract:**
- Execd runs on the same host as the workspace path provided by daemon
- Workspace path is passed to execd; execd treats it as read-only
- This assumption is explicitly scoped to v0; remote execution will use CAS-fetched sources

---

## Crate Structure

```
vx/
  crates/
    vx/               # CLI binary - thin client
    vx-daemon/        # Orchestration with picante DB
    vx-daemon-proto/  # Daemon RPC protocol
    vx-casd/          # CAS service binary
    vx-cas-proto/     # CAS RPC protocol
    vx-execd/         # Execution service binary
    vx-exec-proto/    # Exec RPC protocol
    vx-manifest/      # Cargo.toml parsing
```

See [COMPONENTS.md](COMPONENTS.md) for detailed crate responsibilities.

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
    async fn shutdown(&self);
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

    // Blob operations
    async fn put_blob(&self, data: Vec<u8>) -> BlobHash;
    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>>;
    async fn has_blob(&self, hash: BlobHash) -> bool;

    // Chunked blob upload (large files)
    async fn begin_blob(&self) -> BlobUploadId;
    async fn blob_chunk(&self, id: BlobUploadId, chunk: Vec<u8>);
    async fn finish_blob(&self, id: BlobUploadId) -> FinishBlobResult;
}
```

### Publish Semantics

`publish(cache_key, manifest_hash)`:

1. **Validation:** Fails if `manifest_hash` doesn't exist in CAS
2. **Idempotent:** If cache key already maps to `manifest_hash`, returns `AlreadyExists`
3. **Conflict detection:** If cache key maps to a *different* manifest hash, returns `Conflict { existing: ManifestHash }`
4. **Returns:** `PublishResult { status: PublishStatus, error: Option<String> }`

```rust
pub enum PublishStatus {
    Published,        // New mapping created
    AlreadyExists,    // Same mapping already existed (idempotent success)
    Conflict,         // Different mapping exists (bug: same key, different output)
}
```

Conflicts should never happen with correct cache keys. If they do, it indicates a bug in cache key computation — the error makes it visible rather than silently using a stale artifact.

---

## picante Database

The daemon uses picante for incremental computation.

**Key principle:** picante determines *when* the cache key must be recomputed; vertex defines *what* the cache key is.

The `CacheKey` is an explicit blake3 hash computed by the `cache_key_compile_bin` query. This keeps CacheKey stable across picante internal changes.

### Persistence

picante's incremental database is persisted to `~/.vx/picante.cache` (or `$VX_HOME/picante.cache`):
- **On startup:** `load_cache()` restores memoized query results from disk
- **After each build:** `save_cache()` persists the database

This means:
- Query results survive across vx invocations
- Second build with unchanged inputs = zero query recomputation
- Corrupt cache files are automatically deleted and rebuilt

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

**Toolchain resolution:** Honor `RUSTC` env var if set, else find `rustc` in `PATH`.

### Tracked Queries

```rust
/// Compute the cache key for compiling a binary crate.
/// This is an EXPLICIT blake3 hash of all inputs — not picante's internal fingerprint.
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(db: &DB, source: SourceFile) -> PicanteResult<CacheKey>;

/// Build a rustc invocation for compiling a binary crate.
#[picante::tracked]
pub async fn plan_compile_bin<DB: Db>(db: &DB) -> PicanteResult<RustcInvocation>;

/// Generate a human-readable node ID.
#[picante::tracked]
pub async fn node_id_compile_bin<DB: Db>(db: &DB) -> PicanteResult<NodeId>;
```

---

## Cache Key Computation

**CacheKey = blake3 hash of:**

- `CACHE_KEY_SCHEMA_VERSION` (bump when canonicalization changes)
- `rustc -vV` full output
- Target triple
- Profile (`debug` / `release`)
- Crate name
- Edition
- Crate type (`bin`)
- Content hash of all source files in the crate
- Content hash of `Cargo.toml`

**v0 source enumeration rule:**
v0 supports only single-file crates (`src/main.rs` only). `mod` declarations that reference other files are rejected with a clear error. This keeps source discovery trivial while maintaining correctness.

Later versions will implement proper module discovery by parsing `mod` declarations and walking the source closure.

**What's NOT in the cache key:**
- Workspace root path — `--remap-path-prefix` normalizes path-sensitive outputs
- Absolute paths to source files — we hash content, not location

**Path determinism:**
- Pass `--remap-path-prefix <workspace>=/vx-workspace` for debuginfo
- All temp dirs and output dirs are under `.vx/`

---

## Content-Addressed Storage

CAS is global, shared across all projects. This enables cross-project cache reuse.

**Cache entries are not namespaced by project, only by CacheKey.** This is intentional — two projects with identical inputs produce identical cache keys and share artifacts. Don't add "project id" to the key; that would break cross-project reuse.

**CAS Location:** `~/.vx/` (or `$VX_HOME` if set)

**Storage Layout:**
```
~/.vx/
  blobs/<hh>/<hash>              # raw bytes
  manifests/<hh>/<hash>.json     # structured node output records
  cache/<hh>/<cachekey>          # contains manifest hash
  picante.cache                  # picante incremental database
  tmp/                           # staging for atomic writes
```

**Design Rules:**
- All writes go through `tmp/` then atomic rename
- Blobs are immutable once written
- Manifests reference blobs by hash
- Cache index maps CacheKey → ManifestHash
- Conflicts detected and reported (see Publish Semantics)

---

## Build Reports vs CAS

**CAS (artifact truth):**
- `NodeManifest`: outputs + hashes + cache key
- Cache index: cache key → manifest mapping
- Blobs: actual artifact bytes
- Stable, minimal, long-lived

**BuildReport (run log):**
- Cache hit/miss
- Rustc invocation used
- Timings
- stdout/stderr
- User-facing, verbose, per-run

---

## Filesystem Layout

**Global (shared CAS + picante):**
```
~/.vx/
  blobs/<hh>/<hash>
  manifests/<hh>/<hash>.json
  cache/<hh>/<cachekey>
  picante.cache
  tmp/
```

**Project-local:**
```
<project>/.vx/
  build/
    <triple>/
      debug/<crate-name>
      release/<crate-name>
  runs/
    <run-id>.json
```

---

## CLI Commands

### `vx build [--release]`

Build the project. Connects to daemon, sends build request, prints result.

### `vx kill`

Stop the daemon process.

### `vx clean`

Stop the daemon and remove the entire `.vx/` directory.

**Note:** Project-local `.vx/` contains build outputs and run logs. The CAS is shared globally at `~/.vx/`. `vx clean` removes only the project-local `.vx/` directory, not the global CAS.

### `vx explain`

Print details about the last build (cache hits, timings, invocations).

### `vx cache prune` (future)

Garbage-collect the global CAS. Out of scope for v0.

---

## Error Philosophy

Errors must be:
- **Explicit** — say what happened
- **Actionable** — say what to do
- **Honest** — admit missing features

**Bad:** `unsupported`

**Good:** `dependencies are not supported yet (found "foo = 1.0" in [dependencies])`

---

## Design Constraints

These are non-negotiable:

1. **No implicit inputs** — Every file/env influencing a node must be in the cache key.
2. **No Cargo fallback** — If something isn't supported, fail.
3. **No global mutable state** — Everything needed to explain a build must be in the report.
4. **Stable identifiers** — Node IDs must be deterministic across runs.
5. **Model > execution** — Never "just run a command" without a node.
6. **CAS for all artifacts** — All durable artifact state lives in CAS.
7. **Correct cache keys** — If inputs change, the cache key must change.
8. **Explicit cache keys** — CacheKey is a well-defined blake3 hash, not picante internals.

---

## Closing Principle

vertex is a replacement for `cargo build`, not a wrapper around it.

It targets the same problem space, but with different priorities:
determinism, correct caching, explicit inputs, and service-oriented execution.

Compatibility is intentionally incomplete.
Correctness within the supported subset is non-negotiable.
