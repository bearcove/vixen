# Design Question: C/C++ Builds in vx

## Context

We're building vx, a hermetic build system with:
- **CAS (Content-Addressed Storage)**: All artifacts addressed by blake3 hash
- **picante**: Incremental computation runtime with tracked queries and inputs
- **Service architecture**: daemon (orchestration), casd (storage), execd (execution)
- **Graph model**: Builds are DAGs of nodes with explicit inputs/outputs

We've decided to use **zig cc** as our hermetic C/C++ toolchain (avoiding the LLVM+musl bootstrap complexity).

## What We Have So Far

### Toolchain Acquisition

We plan to implement:

```rust
#[picante::input]
pub struct ZigToolchainSpec {
    pub version: String,        // e.g., "0.13.0"
    pub host: String,           // e.g., "x86_64-linux"
}

#[picante::tracked]
pub async fn acquire_zig_toolchain<DB: Db>(
    db: &DB,
    spec: ZigToolchainSpec
) -> PicanteResult<ToolchainId> {
    // Download zig tarball from ziglang.org
    // Verify minisig signature
    // Extract and hash zig binary + lib/ directory
    // Store in CAS
    // Return ToolchainId = blake3(zig_exe + zig_lib)
}
```

### The Question

**How should we model C/C++ compilation in the vx graph?**

Specifically:

## 1. Input Discovery vs Explicit Inputs

For a C compilation like `zig cc -c foo.c -o foo.o`:

**Option A: Explicit header inputs upfront**
```rust
#[picante::tracked]
pub async fn cc_compile<DB: Db>(
    db: &DB,
    toolchain_id: ToolchainId,
    source: SourceFile,          // foo.c
    headers: Vec<SourceFile>,    // ALL headers needed (transitively)
    flags: CompileFlags,
) -> PicanteResult<ObjectFile>
```

Requires scanning all `#include` directives before compilation.

**Option B: Depfile-driven discovery**
```rust
#[picante::tracked]
pub async fn cc_compile<DB: Db>(
    db: &DB,
    toolchain_id: ToolchainId,
    source: SourceFile,          // foo.c
    flags: CompileFlags,
) -> PicanteResult<ObjectFile> {
    // Compile with -MMD -MF foo.d
    // Parse depfile to discover headers
    // On next run, headers are inputs to cache key
}
```

First compilation doesn't know headers, subsequent compilations do.

**Which approach fits vx's model better?** 

- Does picante have a pattern for "discovered inputs" that become tracked on re-execution?
- Should header discovery be a separate query: `discover_headers(source) -> Vec<Header>`?
- How do systems like Bazel/Buck handle this incrementally?

## 2. Compilation Unit Granularity

**Option A: One query per .c file**
```rust
#[picante::tracked]
pub async fn cc_compile<DB: Db>(
    db: &DB,
    source: SourceFile,
    ...
) -> PicanteResult<ObjectFile>
```

Simple, but means N separate queries for N source files.

**Option B: Batch compilation**
```rust
#[picante::tracked]
pub async fn cc_compile_batch<DB: Db>(
    db: &DB,
    sources: Vec<SourceFile>,
    ...
) -> PicanteResult<Vec<ObjectFile>>
```

More efficient for large projects, but loses fine-grained incrementality.

**Option C: Single "compile this target" query**
```rust
#[picante::tracked]
pub async fn cc_compile_target<DB: Db>(
    db: &DB,
    target_spec: CcTarget,  // contains all sources
) -> PicanteResult<Vec<ObjectFile>>
```

picante memoizes the whole batch, but can we get incremental re-compilation of individual files?

**What's the right granularity for the picante graph?**

- Should each .c → .o be a separate tracked query (like rustc invocations)?
- Or should we group by target and rely on picante's internal change detection?

## 3. Linking and Object File Inputs

When linking:

```rust
#[picante::tracked]
pub async fn cc_link<DB: Db>(
    db: &DB,
    toolchain_id: ToolchainId,
    objects: Vec<ObjectFile>,  // .o files from cc_compile
    libs: Vec<Library>,
    flags: LinkFlags,
) -> PicanteResult<Executable>
```

**Questions:**

- Should `objects` be `Vec<Blake3Hash>` (just content addresses) or `Vec<ObjectFile>` (picante inputs with metadata)?
- If we use hashes, how do we ensure the linker invocation is tracked properly?
- Do we need a separate "gather objects" query that materializes them from CAS before linking?

## 4. Cache Key Composition

For `cc_compile`, the cache key should be:

```rust
cache_key = hash(
    toolchain_id +     // ToolchainId from acquire_zig_toolchain
    source_hash +      // blake3 of .c file
    headers_hashes +   // blake3 of each header (from depfile)
    flags +            // compilation flags (sorted, normalized)
    target_triple      // e.g., "x86_64-linux-musl"
)
```

**Questions:**

- Should `toolchain_id` be an input to the `cc_compile` query, or just part of the cache key?
- How do we handle flags like `-I/abs/path/include`? Do we need to canonicalize paths?
- Should `-fdebug-prefix-map=$WORKSPACE=/vx-workspace` be automatic, or explicit in flags?

## 5. Execution Model (execd)

When execd runs a compilation:

```rust
pub struct CcCompileInvocation {
    pub program: String,           // path to zig (from CAS)
    pub args: Vec<String>,         // ["cc", "-c", "foo.c", "-o", "foo.o"]
    pub env: Vec<(String, String)>, // environment variables
    pub cwd: String,               // working directory
    pub expected_outputs: Vec<ExpectedOutput>,
    pub depfile: Option<String>,   // path to .d file (if -MMD)
}
```

**Questions:**

- How does execd materialize inputs (source file, headers) from CAS? 
  - Extract to temp dir? 
  - Use FUSE/overlayfs?
  - Symlink farm?
  
- How does execd handle the zig binary?
  - Extract to temp, run `$TEMP/zig cc`?
  - Or should AcquireZigToolchain produce a "ready to execute" path?
  
- What about the depfile?
  - Parse it in execd and return header paths?
  - Or return raw depfile content and parse in daemon?

## 6. Incremental Builds

**Scenario**: User changes one header file `common.h`.

Expected behavior:
1. picante detects `common.h` changed (hash differs)
2. All `.c` files that include `common.h` (directly or transitively) are recompiled
3. Unaffected `.c` files are NOT recompiled (cache hit)
4. Linking happens with new .o files + cached .o files

**Questions:**

- How do we model the header → source dependency in picante?
  - Is there a "reverse dependency" query like `sources_depending_on(header)`?
  - Or does each `cc_compile` query list headers as inputs, and picante auto-invalidates?
  
- Do we need a separate "header graph" to efficiently find affected sources?
  - Or does picante's tracked query memoization handle this naturally?

## 7. Parallel Compilation

If we have 100 .c files, we want to compile them in parallel (up to CPU core count).

**Questions:**

- Does picante automatically parallelize independent `cc_compile` queries?
- Or do we need to explicitly spawn async tasks?
- Should we batch queries to control parallelism (e.g., "compile 10 at a time")?

## 8. Error Handling

If `foo.c` fails to compile (syntax error):

**Questions:**

- Should `cc_compile` return `Result<ObjectFile, CompileError>`?
- Or should it store the error in the graph and continue with other files?
- How do we surface compilation errors to the user while still compiling other files?

## 9. Example: Building a Simple C Project

Given this project:

```
hello/
├── src/
│   ├── main.c
│   ├── utils.c
│   └── utils.h
└── Cargo.toml (or equivalent vx manifest)
```

**What would the picante query graph look like?**

```rust
// Option A: Fine-grained
let toolchain_id = acquire_zig_toolchain(db, spec).await?;
let main_o = cc_compile(db, toolchain_id, "src/main.c", flags).await?;
let utils_o = cc_compile(db, toolchain_id, "src/utils.c", flags).await?;
let exe = cc_link(db, toolchain_id, vec![main_o, utils_o], flags).await?;
```

```rust
// Option B: Target-oriented
let toolchain_id = acquire_zig_toolchain(db, spec).await?;
let hello_target = CcTarget {
    sources: vec!["src/main.c", "src/utils.c"],
    output: "hello",
};
let exe = cc_build_target(db, toolchain_id, hello_target, flags).await?;
```

**Which matches vx's philosophy better?**

## 10. Integration with Existing vx Concepts

We already have for Rust:

```rust
#[picante::input]
pub struct SourceFile { path: String, content_hash: Blake3Hash }

#[picante::input]
pub struct CargoToml { content_hash: Blake3Hash, name: String, edition: Edition }

#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(
    db: &DB,
    source: SourceFile
) -> PicanteResult<CacheKey>

#[picante::tracked]
pub async fn plan_compile_bin<DB: Db>(
    db: &DB
) -> PicanteResult<RustcInvocation>
```

**Should C compilation follow a similar pattern?**

```rust
#[picante::input]
pub struct CSourceFile { path: String, content_hash: Blake3Hash }

#[picante::tracked]
pub async fn cache_key_cc_compile<DB: Db>(
    db: &DB,
    source: CSourceFile,
    toolchain: ToolchainId,
) -> PicanteResult<CacheKey>

#[picante::tracked]
pub async fn plan_cc_compile<DB: Db>(
    db: &DB,
    source: CSourceFile,
) -> PicanteResult<ZigCcInvocation>
```

Or is there a better way to unify Rust and C builds?

## Summary

**Core design questions:**

1. How to handle header discovery (explicit vs depfile-driven)?
2. What's the right query granularity (per-file vs per-target)?
3. How should object files be passed to the linker (hashes vs inputs)?
4. Cache key composition (what goes in, how to normalize)?
5. Execution model (how execd materializes inputs and toolchain)?
6. Incremental builds (how picante tracks header dependencies)?
7. Parallelism (automatic or explicit)?
8. Error handling (fail fast or continue)?
9. Example graph structure for a simple project?
10. Integration with existing Rust build model?

**What we're looking for:**

- Concrete picante patterns that fit C compilation
- Trade-offs between simplicity and incrementality
- How to avoid reinventing Bazel/Buck mistakes
- The "vx way" to model C builds that scales from hello world to large projects

Thanks for any guidance!
