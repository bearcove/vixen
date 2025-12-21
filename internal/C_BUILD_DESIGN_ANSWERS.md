# C/C++ Build Design: Consultant Answers

## Summary

The "vx way" for C/C++ builds with zig cc:

| Question | Answer |
|----------|--------|
| Header discovery | Depfile-driven, discovered deps as picante inputs |
| Query granularity | One TU (.c/.cc) per query/action |
| Link inputs | Artifact handles (hash + filename + metadata) |
| Cache key | toolchain_id is a real input, auto debug-prefix-map |
| execd model | Symlink farm sandbox, locally cached toolchain |
| Incremental builds | Forward deps only, per-TU discovered headers |
| Parallelism | Daemon scheduler with semaphore |
| Error handling | Compile all, collect errors, skip link on failure |
| Graph structure | acquire toolchain → compile each TU → link |
| Integration | Same plan/key/exec/CAS as Rust |

---

## 1. Header Discovery: Depfile-Driven with Discovered Deps as Inputs

**Pattern**: "Discovered inputs are an output."

```rust
#[picante::input]
pub struct DiscoveredDeps {
    #[key]
    pub tu_key: TuKey,              // (target_id, source_path, profile, triple)
    pub deps_hash: Blake3Hash,      // hash of canonical deps list
    pub deps: Vec<Utf8PathBuf>,     // workspace-relative paths
}
```

Flow:
1. `cc_compile_key(...)` depends on `DiscoveredDeps(tu_key)` if present
2. On miss (or no deps), compile once, capture depfile, update `DiscoveredDeps`
3. Next run becomes stable no-op

**Why not pre-scan includes?**
- Real closure depends on `-I`, `-isystem`, `-D`, `#if`, compiler builtins
- "Scan includes" = "reimplement the preprocessor"

---

## 2. Granularity: One TU Per Query/Action

`.c → .o` is the unit of compilation and caching.

**Why:**
- Matches the actual expensive thing (a compiler invocation)
- Maximal reuse, minimal rebuild scope
- Composes naturally into linking

If you need target-level API: thin wrapper that enumerates sources and calls per-TU queries.

---

## 3. Link Inputs: Artifact Handles, Not Raw Hashes

```rust
pub struct Artifact {
    pub blob: Blake3Hash,
    pub logical: String,        // "obj", "staticlib", ...
    pub filename: Utf8PathBuf,  // used when materializing
    pub executable: bool,
}
```

`cc_link` takes `Vec<Artifact>`, not `Vec<Blake3Hash>`.

**Why:**
- Linkers care about filenames, extensions, ordering
- Need per-input metadata later (whole-archive, group, search path)
- Want stable, explainable manifests

---

## 4. Cache Key: toolchain_id as Real Input, Auto Path Remapping

**toolchain_id is a query input, not just cache key component:**
```rust
plan_cc_compile(db, toolchain_id, ...) -> Invocation
cache_key_cc_compile(db, toolchain_id, ...) -> CacheKey
```

**Absolute include paths rule:**
1. ✅ Reject absolute paths (strict mode) - best for hermeticity
2. ✅ Allow only if under workspace root, canonicalize to relative
3. ❌ Allow arbitrary - kills portability

**Path remapping is automatic (not user flag):**
```bash
-fdebug-prefix-map=$WORKSPACE=/vx-workspace
-ffile-prefix-map=$WORKSPACE=/vx-workspace  # for gcc compat
```

Injected by planner so "different checkout path" stays cache hit.

---

## 5. execd Model: Symlink Farm + Locally Cached Toolchain

### Input Materialization

Simplest robust approach:
```
workdir = $TMP/vx-execd/<action-id>/
workdir/inputs/...  (symlinks to CAS blobs)
workdir/out/...     (outputs written here)
```

Why symlink farm: fast, simple, good enough until sandboxing needed.

### Zig Binary Handling

**Don't re-extract per action.** Local toolchain materialization cache:
```
~/.vx/toolchains/<toolchain_id>/zig
~/.vx/toolchains/<toolchain_id>/lib/...
```

Perfectly consistent with CAS—just a local materialized view.

### Depfile Handling

- execd returns: depfile bytes (or blob hash) + exit status + stderr/stdout
- daemon parses depfile (owns policy: path normalization, workspace rooting)
- daemon updates `DiscoveredDeps`

---

## 6. Incremental Builds: Forward Deps Only

**No reverse dependency graph needed.**

Each `cc_compile(tu)` depends on `DiscoveredDeps(tu)` which contains header list.
Headers are `SourceFile` inputs with content hashes.

When `common.h` changes:
1. `SourceFile(common.h)` hash changes
2. Every TU whose `DiscoveredDeps` includes `common.h` invalidated by picante
3. Only those TUs recompile
4. Link reruns (cache miss because object set changed)

Forward deps only = cleanest shape.

---

## 7. Parallelism: Daemon Scheduler, Not Batching

Separate "compute graph" from "execute actions":
1. picante computes/returns the plan (nodes/actions)
2. daemon schedules execution with semaphore (N = cores)
3. independent compiles run concurrently
4. link waits on all objects (or fails early if any compile fails)

---

## 8. Errors: Compile All, Collect, Skip Link on Failure

```rust
// Each cc_compile returns:
Result<ObjectArtifact, CompileError>
```

- Daemon runs compiles in parallel
- Collects all errors
- Prints them all (real-world expectation: show all syntax errors)
- Skip link if any compile failed

Also: store stderr/stdout as blobs for `vx explain`.

---

## 9. Example Graph

Given `main.c` and `utils.c` both including `utils.h`:

```rust
let tc = acquire_zig_toolchain(db, spec).await?;
let main_o  = cc_compile(db, tc, tu("src/main.c"),  flags).await?;
let utils_o = cc_compile(db, tc, tu("src/utils.c"), flags).await?;
let exe = cc_link(db, tc, vec![main_o, utils_o], link_flags).await?;
```

Graph (conceptually):
1. `toolchain_id = acquire_zig_toolchain(spec)`
2. For each TU:
   - `deps = discovered_deps(tu_key)` (empty on first run)
   - `key = cache_key_cc_compile(toolchain_id, src_hash, deps_hash, flags, target)`
   - `lookup(key)` in CAS
   - miss → execd compile → outputs: obj, depfile
   - daemon normalizes depfile → updates `DiscoveredDeps(tu_key)`
3. `link_key = cache_key_link(toolchain_id, [obj_hashes ordered], flags, target)`
4. `lookup(link_key)` → hit/miss
5. Materialize exe to `<project>/.vx/build/...`

---

## 10. Integration: Unify at Action Layer

C follows same plan → key → exec → CAS manifest as Rust.

```rust
// Language-specific planners build Invocation
plan_rustc_compile(...) -> Invocation
plan_zig_cc_compile(...) -> Invocation

// Common execution layer
execute_action(invocation) -> NodeManifestHash
```

Difference: C adds "discovered deps feedback loop" for headers.

---

## Data Structures (Proposed)

### TuKey (Translation Unit Key)

```rust
/// Uniquely identifies a translation unit in the build
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct TuKey {
    /// Target this TU belongs to (e.g., "hello")
    pub target: String,
    /// Source file path (workspace-relative)
    pub source: Utf8PathBuf,
    /// Build profile ("debug" or "release")
    pub profile: String,
    /// Target triple (e.g., "x86_64-linux-musl")
    pub triple: String,
}
```

### DiscoveredDeps

```rust
#[picante::input]
pub struct DiscoveredDeps {
    #[key]
    pub tu_key: TuKey,
    /// Hash of the canonical deps list (for cache key stability)
    pub deps_hash: Blake3Hash,
    /// Workspace-relative paths to all headers this TU depends on
    pub deps: Vec<Utf8PathBuf>,
}
```

### Artifact

```rust
/// A build artifact stored in CAS
#[derive(Clone, Debug)]
pub struct Artifact {
    /// Content hash (blob in CAS)
    pub blob: Blake3Hash,
    /// Logical type ("obj", "exe", "staticlib", "depfile")
    pub logical: String,
    /// Filename to use when materializing
    pub filename: Utf8PathBuf,
    /// Whether to set executable bit
    pub executable: bool,
}
```

### CcCompileInvocation

```rust
/// Invocation for compiling a single TU
pub struct CcCompileInvocation {
    /// Path to zig binary (from toolchain cache)
    pub program: Utf8PathBuf,
    /// Arguments: ["cc", "-c", "src/main.c", "-o", "main.o", ...]
    pub args: Vec<String>,
    /// Environment variables (minimal, controlled)
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: Utf8PathBuf,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
    /// Depfile path (relative to cwd)
    pub depfile: Option<Utf8PathBuf>,
}

pub struct ExpectedOutput {
    pub logical: String,      // "obj" or "depfile"
    pub path: Utf8PathBuf,    // where execd should find it
    pub executable: bool,
}
```

### CcLinkInvocation

```rust
/// Invocation for linking objects into executable
pub struct CcLinkInvocation {
    pub program: Utf8PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Utf8PathBuf,
    pub expected_outputs: Vec<ExpectedOutput>,
}
```

### CcCompileResult

```rust
/// Result of compiling a TU
pub enum CcCompileResult {
    Success {
        object: Artifact,
        depfile: Option<Artifact>,
        /// Parsed and normalized header deps
        discovered_deps: Vec<Utf8PathBuf>,
    },
    Failure {
        exit_code: i32,
        stderr: String,
        stdout: String,
    },
}
```

---

## Implementation Order

1. **ZigToolchain acquisition** (already designed)
   - Download, verify, store in CAS
   - Materialize to local cache

2. **DiscoveredDeps input** in picante
   - Store/retrieve per-TU header lists

3. **cc_compile query**
   - Plan invocation
   - Compute cache key (with discovered deps)
   - Execute via execd
   - Parse depfile, update DiscoveredDeps

4. **cc_link query**
   - Plan invocation
   - Compute cache key
   - Execute via execd

5. **Test: hello world**
   - Verify cache hit on rebuild
   - Verify cache hit from different directory
   - Verify incremental: change header → only affected TUs rebuild

6. **Test: real project**
   - Something like sqlite or jq
   - Verify parallelism works
   - Verify error collection works
