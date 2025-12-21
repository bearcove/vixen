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
- A structured build report is produced
- Unsupported features fail loudly and clearly

**Definition of done for caching:**

- Run `vx build` twice → second run does zero rustc invocations, only materializes from CAS
- Move the repo to a different absolute path → still a cache hit (requires `--remap-path-prefix`)
- Change one byte in `src/main.rs` → cache miss and rebuild
- Change `edition` in `Cargo.toml` → cache miss and rebuild
- Change rustc toolchain → cache miss and rebuild

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

```
vx              ─── CLI / thin client
    │
    └──► vx-daemon  ─── orchestration + graph state (picante DB)
              │
              ├──► vx-casd   ─── CAS service (blobs + manifests + cache index)
              │
              └──► vx-execd  ─── execution service (run rustc, stream to CAS)
```

All communication over rapace transport (SHM locally by default).

**Key principle:** `vx-daemon` never reads/writes artifacts directly. It asks CAS and exec over RPC.

---

## Crate Structure

```
vx/
  crates/
    vx/              # CLI binary - thin client, talks to daemon
    vx-daemon/       # Orchestration daemon with picante DB
    vx-casd/         # CAS service daemon
    vx-execd/        # Execution service daemon
    vx-services/     # Shared rapace service trait definitions
    vx-manifest/     # Cargo.toml parsing → typed internal model
    vx-types/        # Shared types (NodeId, CacheKey, Edition, etc.)
```

### Crate Responsibilities

| Crate | Responsibility |
|-------|----------------|
| `vx` | CLI, thin client, starts/connects to daemon |
| `vx-daemon` | Picante DB, orchestration, scheduling, cache key computation |
| `vx-casd` | CAS service: blobs, manifests, cache index, materialization |
| `vx-execd` | Execution service: runs rustc, captures output, streams to CAS |
| `vx-services` | Rapace service trait definitions (`Cas`, `Exec`) |
| `vx-manifest` | Parse `Cargo.toml` with facet-toml, validate v0 subset |
| `vx-types` | Shared types used across crates |

---

## Service Definitions (`vx-services`)

### CAS Service

```rust
#[rapace::service]
pub trait Cas {
    /// Look up a cache key, returns manifest hash if found
    async fn lookup(&self, cache_key: CacheKey) -> Option<ManifestHash>;
    
    /// Store a cache key → manifest hash mapping
    async fn store_cache(&self, cache_key: CacheKey, manifest_hash: ManifestHash);
    
    /// Store a manifest, returns its hash
    async fn put_manifest(&self, manifest: NodeManifest) -> ManifestHash;
    
    /// Get a manifest by hash
    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest>;
    
    /// Store a blob, returns its hash
    async fn put_blob(&self, data: Vec<u8>) -> BlobHash;
    
    /// Get a blob by hash (streaming for large files)
    async fn get_blob(&self, hash: BlobHash) -> Streaming<Vec<u8>>;
    
    /// Materialize outputs to a destination directory (avoids shipping blobs to client)
    async fn materialize(&self, manifest_hash: ManifestHash, dest_dir: String) -> Result<(), String>;
}
```

### Exec Service

```rust
#[rapace::service]
pub trait Exec {
    /// Execute a rustc invocation
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult;
}

/// Structured rustc command (not a shell string!)
struct RustcInvocation {
    program: String,           // "rustc"
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
    expected_outputs: Vec<ExpectedOutput>,
}

struct ExpectedOutput {
    logical: String,           // "bin"
    path: String,              // relative path where output will be written
    executable: bool,
}

struct ExecuteResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    outputs: Vec<ProducedOutput>,
    /// If exec pushed to CAS directly, this is set
    manifest_hash: Option<ManifestHash>,
}

struct ProducedOutput {
    logical: String,
    path: String,
    blob_hash: BlobHash,       // exec can push to CAS and return hash
    executable: bool,
}
```

---

## Picante Database (`vx-daemon`)

The daemon uses picante for incremental computation.

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
    pub manifest: Manifest,  // parsed
}

#[picante::input]
pub struct RustcVersion {
    pub version_string: String,  // full `rustc -vV` output
}

#[picante::input]
pub struct BuildConfig {
    pub profile: Profile,
    pub target_triple: String,
}
```

### Tracked Queries

```rust
/// Compute the cache key for a compile-bin node
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(
    db: &DB,
    cargo_toml: CargoToml,
    source: SourceFile,
    rustc: RustcVersion,
    config: BuildConfig,
) -> PicanteResult<CacheKey> {
    // Hash all inputs deterministically
    // ...
}

/// Build a rustc invocation for a compile-bin node
#[picante::tracked]
pub async fn plan_compile_bin<DB: Db>(
    db: &DB,
    cargo_toml: CargoToml,
    config: BuildConfig,
) -> PicanteResult<RustcInvocation> {
    let manifest = cargo_toml.manifest(db)?;
    // Build the invocation
    // ...
}

/// Execute the build (checks cache, runs on miss)
#[picante::tracked]
pub async fn build_compile_bin<DB: Db>(
    db: &DB,
    cargo_toml: CargoToml,
    source: SourceFile,
    rustc: RustcVersion,
    config: BuildConfig,
    cas: &CasClient,
    exec: &ExecClient,
) -> PicanteResult<ManifestHash> {
    let cache_key = cache_key_compile_bin(db, cargo_toml, source, rustc, config).await?;
    
    // Check cache
    if let Some(manifest_hash) = cas.lookup(cache_key).await? {
        return Ok(manifest_hash);
    }
    
    // Cache miss - execute
    let invocation = plan_compile_bin(db, cargo_toml, config).await?;
    let result = exec.execute_rustc(invocation).await?;
    
    // Result includes manifest_hash if exec pushed to CAS
    Ok(result.manifest_hash.unwrap())
}
```

---

## Cache Key Computation

For v0, "proper" caching means: if any input that can affect the produced binary changes, the cache key changes.

**CacheKey = blake3 hash of:**

**Toolchain identity:**
- `rustc -vV` full output (includes version, commit hash, LLVM version, host triple)

**Build configuration:**
- Target triple (explicit)
- Profile (`debug` / `release`)
- Crate name
- Edition
- Crate type (`bin`)

**Compiler flags:**
- The exact rustc flags passed (including `-C` settings)
- `--remap-path-prefix` for path-independent outputs

**Source inputs:**
- Content hash (blake3) of `src/main.rs`
- Content hash (blake3) of `Cargo.toml`

**Environment:**
- Empty for v0. Do not read `RUSTFLAGS` or `CARGO_*` env vars.

**Path determinism:**
- Pass `--remap-path-prefix <workspace>=/vx-workspace` so absolute paths don't leak into debuginfo

---

## Content-Addressed Storage (`vx-casd`)

**Storage Layout:**
```
.vx/
  cas/
    blobs/
      blake3/<hh>/<hash>           # raw bytes (sharded by first 2 hex chars)
    manifests/
      blake3/<hh>/<hash>.json      # structured node output records
    cache/
      blake3/<hh>/<cachekey>       # contains manifest hash (cache index)
    tmp/                           # atomic writes stage here
```

**Design Rules:**
- All writes go through `tmp/` then atomic rename
- Blobs are immutable once written
- Manifests reference blobs by hash
- Cache index maps CacheKey → ManifestHash
- Materialization is done in-daemon (avoids shipping blobs over RPC for local case)

---

## Filesystem Layout

```
.vx/
  cas/
    blobs/blake3/<hh>/<hash>
    manifests/blake3/<hh>/<hash>.json
    cache/blake3/<hh>/<cachekey>
    tmp/
  build/
    <triple>/
      debug/
        <crate-name>           # materialized binary
      release/
        <crate-name>
  runs/
    <run-id>.json              # build reports
    latest -> <run-id>.json    # symlink to most recent
  daemon.sock                  # rapace SHM socket
```

---

## Execution Service (`vx-execd`)

Exec is intentionally dumb. It:
- Accepts a structured `RustcInvocation`
- Runs it with a clean environment
- Captures stdout/stderr/status/timing
- Streams produced outputs directly to CAS
- Returns the manifest hash

**rustc invocation (v0):**
```
rustc
  --crate-name <name>
  --crate-type bin
  --edition <edition>
  --remap-path-prefix <workspace>=/vx-workspace
  src/main.rs
  -o <output-path>
```

**Notes:**
- No `--extern`
- No incremental
- No linker args unless required by host
- Explicit flags only
- `--remap-path-prefix` for portable outputs
- Clean environment (only PATH for linker)

---

## v0 Build Flow

```
1. vx build connects to vx-daemon (or starts it)

2. daemon:
   - Parses Cargo.toml (facet-toml)
   - Sets picante inputs (SourceFile, CargoToml, RustcVersion, BuildConfig)
   - Runs build_compile_bin query

3. build_compile_bin:
   - Computes cache_key via cache_key_compile_bin query
   - Calls CAS service: lookup(cache_key)

4. If cache hit:
   - Calls CAS service: materialize(manifest_hash, .vx/build/...)
   - Returns manifest_hash

5. If cache miss:
   - Computes RustcInvocation via plan_compile_bin query
   - Calls Exec service: execute_rustc(invocation)
   - Exec runs rustc, streams outputs to CAS
   - Exec returns ExecuteResult with manifest_hash
   - Calls CAS service: store_cache(cache_key, manifest_hash)
   - Calls CAS service: materialize(manifest_hash, .vx/build/...)
   - Returns manifest_hash

6. daemon writes build report to .vx/runs/<id>.json

7. vx prints summary to user
```

---

## CLI Contract (v0)

### `vx build`

1. Connect to daemon (start if needed)
2. Send build request with working directory + profile
3. Receive build result (success/failure, cache hit/miss, outputs)
4. Print minimal human summary

### `vx explain`

1. Read latest build report (or specified run)
2. Print:
   - What ran vs what was cached
   - Cache keys and why they matched/didn't
   - How long execution took
   - What failed (if any)
   - Where outputs are

No flags beyond `--release` (optional).

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
6. **CAS for all artifacts** — Nothing is materialized without going through CAS first.
7. **Correct cache keys** — If inputs change, the cache key must change.
8. **Service boundaries** — Daemon never touches files directly; always via CAS/Exec RPC.

---

## Why Services Even When Local?

Because it forces:
- Transport-agnostic orchestration
- Replayable commands
- Centralized auditing
- Easy future remote workers
- Consistent CI topology (GitHub runner just runs daemons)

And local is still fast:
- SHM transport
- Materialize on-host (CAS hardlinks/copies locally)
- Avoid blob round-trips

Rapace's SHM transport + service model was made for this "local but separated" architecture.

---

## v0 Checklist (Engineering Tasks)

- [ ] Create crate structure (vx, vx-daemon, vx-casd, vx-execd, vx-services, vx-manifest, vx-types)
- [ ] Define rapace service traits (Cas, Exec)
- [ ] Implement vx-casd (blobs, manifests, cache index, materialize)
- [ ] Implement vx-execd (run rustc, stream to CAS)
- [ ] Implement vx-daemon with picante DB
  - [ ] Define inputs (SourceFile, CargoToml, RustcVersion, BuildConfig)
  - [ ] Implement cache_key_compile_bin query
  - [ ] Implement plan_compile_bin query
  - [ ] Implement build_compile_bin query
- [ ] Implement vx CLI (connect to daemon, send build request)
- [ ] Parse `Cargo.toml` → internal manifest
- [ ] Validate supported subset (reject deps, workspace, features, build.rs)
- [ ] Write build reports
- [ ] Implement `vx explain`
- [ ] Add golden "hello world" test repo
- [ ] Verify: second build = zero rustc invocations
- [ ] Verify: different checkout path = cache hit
- [ ] Verify: source change = cache miss

---

## Closing Principle

We are not building "Cargo, but better".
We are building a build engine that happens to start with Cargo's file format.

Correctness, transparency, and ownership matter more than speed of feature acquisition.
