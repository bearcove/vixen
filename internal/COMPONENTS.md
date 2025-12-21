# Component Architecture

This document describes how vx crates relate to each other, their responsibilities, assumptions, and filesystem isolation.

---

## Process Topology

Four separate binaries communicating over rapace:

```
                              ┌──────────┐
vx ──rapace──► vx-daemon ─────┤ vx-casd  │
                    │         └────▲─────┘
                    │              │ rapace
                    │              │
                    └──► vx-execd ─┘
```

## Crate Dependency Graph

```
┌─────────────────────────────────────────────────────────────────┐
│                         BINARIES                                 │
├─────────────┬─────────────┬─────────────┬───────────────────────┤
│     vx      │  vx-daemon  │  vx-casd    │      vx-execd         │
│  (CLI)      │ (orchestr.) │  (storage)  │    (execution)        │
└──────┬──────┴──────┬──────┴──────┬──────┴───────────┬───────────┘
       │             │             │                   │
       │             │             │                   │
       ▼             ▼             ▼                   ▼
┌─────────────────────────────────────────────────────────────────┐
│                      PROTOCOL CRATES                             │
├────────────────┬────────────────┬────────────────────────────────┤
│ vx-daemon-proto│  vx-cas-proto  │         vx-exec-proto          │
│ (Daemon trait) │ (Cas trait,    │ (Exec trait, RustcInvocation)  │
│                │  Blake3Hash)   │                                │
└────────────────┴────────────────┴────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────┐
│                      SHARED CRATES                               │
├─────────────────────────────────────────────────────────────────┤
│  vx-manifest (Cargo.toml parsing)                                │
└─────────────────────────────────────────────────────────────────┘
```

**Protocol crates** (`-proto` suffix) define traits and types.
**Binary crates** (`-d` suffix or plain name) implement services or CLI.

---

## Crate Details

### `vx` — CLI Binary

**Responsibility:** Parse CLI arguments, connect to daemon, print results.

**What it does:**
- Parses `vx build [--release]` using facet-args
- Creates a `BuildRequest` with project path and release flag
- Connects to `vx-daemon` over rapace
- Calls `daemon.build(request)` and prints the result

**What it does NOT do:**
- Parse Cargo.toml
- Compute cache keys
- Read/write any files in `.vx/`
- Execute rustc

**Filesystem access:** Only reads current working directory path.

**Dependencies:**
- `vx-daemon-proto` — for `BuildRequest`, `BuildResult`, `Daemon` trait
- `rapace` — for RPC connection

---

### `vx-daemon-proto` — Daemon Protocol

**Responsibility:** Define the RPC contract between CLI and daemon.

**Types:**
```rust
pub struct BuildRequest {
    pub project_path: Utf8PathBuf,
    pub release: bool,
}

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

**Filesystem access:** None.

**Dependencies:** `facet`, `rapace`, `camino`

---

### `vx-daemon` — Orchestration Daemon

**Responsibility:** Own the picante database, orchestrate builds, compute cache keys.

**What it does:**
1. Creates picante `Database` with inputs:
   - `SourceFile` (keyed by path, stores content hash)
   - `CargoToml` (singleton: content hash, name, edition, bin path)
   - `RustcToolchain` (singleton: rustc path, version string)
   - `BuildConfig` (singleton: profile, target triple, workspace root)

2. Runs tracked queries:
   - `cache_key_compile_bin` — computes cache key from all inputs
   - `plan_compile_bin` — builds `RustcInvocation`
   - `node_id_compile_bin` — generates human-readable node ID

3. Interacts with CAS (over rapace):
   - `lookup(cache_key)` — check cache
   - `get_manifest`, `get_blob` — materialize on cache hit
   - `put_blob`, `put_manifest`, `publish` — store on cache miss

4. Dispatches execution to vx-execd (over rapace)

**Filesystem access:**
- Reads: `Cargo.toml`, `src/main.rs` (to compute hashes)
- Writes: `.vx/build/<triple>/<profile>/<name>` (materialized output)

**Key principle:** All CAS operations go through rapace RPC. Daemon never writes to `.vx/cas/` directly.

**Dependencies:**
- `vx-daemon-proto` — its own trait
- `vx-cas-proto` — CAS trait and types
- `vx-exec-proto` — Exec trait and invocation types
- `vx-manifest` — Cargo.toml parsing
- `picante` — incremental queries
- `rapace` — RPC to CAS and Exec



---

### `vx-cas-proto` — CAS Protocol

**Responsibility:** Define content-addressed storage types and trait.

**Key types:**
```rust
pub const CACHE_KEY_SCHEMA_VERSION: u32 = 1;

pub struct Blake3Hash(pub [u8; 32]);
pub type BlobHash = Blake3Hash;
pub type ManifestHash = Blake3Hash;
pub type CacheKey = Blake3Hash;

pub struct NodeManifest {
    pub node_id: NodeId,
    pub cache_key: CacheKey,
    pub produced_at: String,
    pub outputs: Vec<OutputEntry>,
}

pub struct OutputEntry {
    pub logical: String,      // "bin"
    pub filename: String,     // "hello"
    pub blob: BlobHash,
    pub executable: bool,
}
```

**Trait:**
```rust
#[rapace::service]
pub trait Cas {
    async fn lookup(&self, cache_key: CacheKey) -> Option<ManifestHash>;
    async fn publish(&self, cache_key: CacheKey, manifest_hash: ManifestHash) -> PublishResult;
    async fn put_manifest(&self, manifest: NodeManifest) -> ManifestHash;
    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest>;
    async fn put_blob(&self, data: Vec<u8>) -> BlobHash;
    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>>;
    async fn has_blob(&self, hash: BlobHash) -> bool;
    // Chunked upload for large files
    async fn begin_blob(&self) -> BlobUploadId;
    async fn blob_chunk(&self, id: BlobUploadId, chunk: Vec<u8>);
    async fn finish_blob(&self, id: BlobUploadId) -> FinishBlobResult;
}
```

**Design decisions:**
- No `materialize()` — clients read blobs and write output themselves
- `publish()` validates manifest exists before writing cache mapping
- Schema versioning via `CACHE_KEY_SCHEMA_VERSION`

**Filesystem access:** None.

---

### `vx-casd` — CAS Service Binary

**Responsibility:** Store and retrieve blobs, manifests, and cache mappings. Runs as a separate process, accepts connections over rapace.

**Storage layout:**
```
.vx/cas/
  blobs/<hh>/<hash>           # raw bytes
  manifests/<hh>/<hash>.json  # JSON-encoded NodeManifest
  cache/<hh>/<cachekey>       # contains manifest hash hex
  tmp/                        # staging for atomic writes
```

**Invariants:**
- All writes are atomic (write to `tmp/`, then rename)
- Blobs are immutable once written
- `publish()` fails if manifest doesn't exist
- First writer wins on cache key conflicts
- Sharding by first 2 hex chars of hash

**Filesystem access:** Full control of `.vx/cas/` directory. Only process that writes here.

**Dependencies:**
- `vx-cas-proto` — trait it implements
- `rapace` — for serving RPC

---

### `vx-exec-proto` — Execution Protocol

**Responsibility:** Define structured rustc invocation types and Exec service trait.

**Types:**
```rust
pub struct RustcInvocation {
    pub program: String,           // "rustc"
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub expected_outputs: Vec<ExpectedOutput>,
}

pub struct ExpectedOutput {
    pub logical: String,    // "bin"
    pub path: String,       // relative path
    pub executable: bool,
}

pub struct ExecuteResult {
    pub success: bool,
    pub manifest_hash: Option<ManifestHash>,  // if outputs pushed to CAS
    pub error: Option<String>,
}

#[rapace::service]
pub trait Exec {
    async fn execute(&self, invocation: RustcInvocation, cas_endpoint: String) -> ExecuteResult;
}
```

**Filesystem access:** None (just types).

---

### `vx-execd` — Execution Service Binary

**Responsibility:** Execute rustc invocations, push outputs to CAS. Runs as a separate process, accepts connections over rapace.

**What it does:**
1. Receives `RustcInvocation` + CAS endpoint from daemon
2. Fetches any needed inputs from CAS (future: for deps)
3. Runs rustc with clean environment
4. Pushes output blobs to CAS
5. Creates and pushes manifest to CAS
6. Returns manifest hash to daemon

**Filesystem access:**
- Reads: source files (fetched from CAS or local)
- Writes: temp directory for rustc output (cleaned after push to CAS)

**Dependencies:**
- `vx-exec-proto` — trait it implements
- `vx-cas-proto` — for CAS client
- `rapace` — for serving RPC and CAS client



---

### `vx-manifest` — Cargo.toml Parsing

**Responsibility:** Parse Cargo.toml into typed model, reject unsupported features.

**Output type:**
```rust
pub struct Manifest {
    pub name: String,
    pub edition: Edition,
    pub bin: BinTarget,
}

pub struct BinTarget {
    pub name: String,
    pub path: Utf8PathBuf,
}

pub enum Edition { E2015, E2018, E2021, E2024 }
```

**Validation (fails loudly):**
- `[dependencies]` → error
- `[dev-dependencies]` → error
- `[build-dependencies]` → error
- `[workspace]` → error
- `[features]` → error
- `build = "..."` → error

**Filesystem access:** Reads `Cargo.toml` only.

**Dependencies:** `facet-toml` for parsing.

---

## Filesystem Isolation Summary

| Crate | Reads | Writes |
|-------|-------|--------|
| `vx` | cwd path only | none |
| `vx-daemon` | `Cargo.toml`, `src/main.rs` | `<project>/.vx/build/` |
| `vx-casd` | `~/.vx/` | `~/.vx/` |
| `vx-manifest` | `Cargo.toml` | none |
| `*-proto` | none | none |

**Key isolation rule:** `vx-daemon` never touches `~/.vx/` directly — all CAS operations go through the `Cas` trait (rapace RPC).

---

## Data Flow

### Cache Miss

```
CLI                     Daemon                      CAS
 │                         │                         │
 │ BuildRequest            │                         │
 ├────────────────────────►│                         │
 │                         │ read Cargo.toml         │
 │                         │ read src/main.rs        │
 │                         │                         │
 │                         │ set picante inputs      │
 │                         │ run cache_key query     │
 │                         │                         │
 │                         │ lookup(cache_key)       │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ None
 │                         │                         │
 │                         │ run plan_compile_bin    │
 │                         │ execute rustc           │
 │                         │                         │
 │                         │ put_blob(binary)        │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ BlobHash
 │                         │                         │
 │                         │ put_manifest(...)       │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ ManifestHash
 │                         │                         │
 │                         │ publish(key, manifest)  │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ Ok
 │                         │                         │
 │◄────────────────────────┤ BuildResult             │
 │                         │                         │
```

### Cache Hit

```
CLI                     Daemon                      CAS
 │                         │                         │
 │ BuildRequest            │                         │
 ├────────────────────────►│                         │
 │                         │ (same setup...)         │
 │                         │                         │
 │                         │ lookup(cache_key)       │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ Some(ManifestHash)
 │                         │                         │
 │                         │ get_manifest(hash)      │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ NodeManifest
 │                         │                         │
 │                         │ get_blob(blob_hash)     │
 │                         ├────────────────────────►│
 │                         │◄────────────────────────┤ Vec<u8>
 │                         │                         │
 │                         │ write to .vx/build/     │
 │                         │                         │
 │◄────────────────────────┤ BuildResult(cached=true)│
 │                         │                         │
```

---

## Process Topology

All components are separate binaries communicating over rapace:

```
                              ┌──────────┐
vx ──rapace──► vx-daemon ─────┤ vx-casd  │
                    │         └────▲─────┘
                    │              │
                    └──► vx-execd ─┘
```

- `vx` — CLI binary, connects to daemon
- `vx-daemon` — orchestration binary, connects to CAS and Exec
- `vx-casd` — CAS service binary
- `vx-execd` — execution service binary, **also connects to CAS**

Execd talks to CAS directly:
1. Daemon tells execd: "run this invocation, CAS is at X, inputs are blobs Y/Z"
2. Execd fetches inputs from CAS (or has them cached locally)
3. Execd runs rustc
4. Execd pushes outputs to CAS
5. Execd returns manifest hash to daemon

This design means execd can be remote — it just needs access to a CAS (local cache that syncs, or direct remote access).

Rapace handles transport — SHM for local, network for remote. No HTTP layer needed.

### Local Development

All four binaries run on localhost, rapace uses SHM transport for speed. Execd and daemon share the same local CAS.

### Remote Workers

```
                                    ┌─────────────────┐
vx ──rapace──► vx-daemon ───────────┤ vx-casd (local) │
                    │               └────────▲────────┘
                    │                        │ sync
                    │               ┌────────▼────────┐
                    └──rapace(net)─►│ remote vx-execd │
                                    │   + local cache │
                                    └─────────────────┘
```

Remote execd has its own local cache that syncs with the central CAS. It fetches inputs on demand, pushes outputs when done. CI runners just start the service binaries.
