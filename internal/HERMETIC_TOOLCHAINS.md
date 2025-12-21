# Hermetic Toolchains in vx

## Philosophy

**Problem**: System toolchains make builds non-reproducible:
- "works on my machine" due to different compiler versions
- Cache misses across machines with different toolchain versions
- Remote execution needs identical toolchains on all workers
- Dependency on host environment breaks hermeticity

**Solution**: Toolchains as first-class CAS artifacts.

```
ToolchainId = blake3(ToolchainManifest + all file blobs)
```

Build nodes reference `ToolchainId`, not "whatever is on PATH".

## The Zig Shortcut (v0 Implementation)

Instead of assembling clang + lld + musl + sysroot pieces manually, **use `zig cc`** as a hermetic C/C++ toolchain.

### Why Zig?

**Single artifact approach:**
- One `zig` binary + its `lib/` directory = complete toolchain
- No LLVM/clang/musl bootstrap needed
- Already bundles sysroots for cross-compilation

**Built-in cross-compilation:**
- `zig cc -target x86_64-linux-musl` just works
- `zig cc -target aarch64-macos` just works
- No sysroot scavenger hunt

**ToolchainId simplicity:**
```rust
ToolchainId = hash(zig_binary + zig_lib_directory)
```

That's it. Everything needed to compile is in those two pieces.

### The Catches

1. **Not "exactly clang"**: `zig cc` accepts most clang flags, but it's a wrapper with opinions
2. **Libc choice matters**: Musl is the clean lane; glibc works but needs care
3. **C++ complexity**: `zig c++` works, but C++ ABI/stdlib selection can get hairy
4. **Path determinism**: Still need `-fdebug-prefix-map` and controlled env
5. **Remote execution**: Zig helps a lot, but still need consistent temp dirs/env

### When to Use Zig

✅ **Good fit:**
- Want hermetic + cross quickly
- Choosing musl-first for reproducibility
- Need remote execution sooner than "bootstrap LLVM" allows
- Building new projects with clean build systems

❌ **Bad fit:**
- Must match existing distro toolchain outputs exactly
- Deep GCC quirks or weird linker scripts
- Legacy build systems that assume system compiler behavior

**Decision: Use Zig for v0**, treat vanilla clang+musl as future option.

## Graph Integration

### Node Types

```rust
// Acquire a Zig toolchain (download, verify, store in CAS)
AcquireZigToolchain {
    version: String,           // e.g., "0.13.0"
    targets: Vec<String>,      // e.g., ["x86_64-linux-musl", "aarch64-linux-musl"]
} -> ToolchainId

// Compile C/C++ source to object file
CcCompile {
    toolchain_id: ToolchainId,
    source: Blake3Hash,        // .c or .cpp file
    target: String,            // e.g., "x86_64-linux-musl"
    flags: Vec<String>,
    headers: Vec<Blake3Hash>,  // discovered via depfiles
} -> ObjectFile + DepInfo

// Link objects into executable
Link {
    toolchain_id: ToolchainId,
    target: String,
    objects: Vec<Blake3Hash>,
    libraries: Vec<Blake3Hash>,
    flags: Vec<String>,
} -> Executable
```

### Cache Key

```
cache_key = hash(
    toolchain_id +        // hash(zig binary + lib/)
    source +              // hash of .c file
    headers +             // hashes from depfile
    target +              // "x86_64-linux-musl"
    flags +               // compilation flags
    profile               // debug/release
)
```

Exact bit-for-bit zig binary = cache hit across machines.

## ZigToolchainManifest Schema

```rust
pub struct ZigToolchainManifest {
    /// Unique identifier (hash of zig binary + lib/)
    pub id: ToolchainId,
    
    /// Zig version
    pub version: String,  // e.g., "0.13.0"
    
    /// Host triple (what platform zig itself runs on)
    pub host: String,     // e.g., "x86_64-linux"
    
    /// Supported targets (what platforms it can compile for)
    pub targets: Vec<String>,
    
    /// File blobs in CAS
    pub files: ZigToolchainFiles,
}

pub struct ZigToolchainFiles {
    /// Zig compiler binary
    pub zig_exe: Blake3Hash,
    
    /// Zig lib directory (tarball of lib/)
    pub zig_lib: Blake3Hash,
}
```

## Invocation Contract

### Compile (.c → .o)

```bash
zig cc \
  -target x86_64-linux-musl \
  -fdebug-prefix-map=$WORKSPACE=/vx-workspace \
  -MMD -MF $DEPFILE \
  -c $SOURCE -o $OUTPUT \
  $FLAGS
```

### Link (objects → executable)

```bash
zig cc \
  -target x86_64-linux-musl \
  $OBJECTS -o $OUTPUT \
  $FLAGS
```

### Key Flags

- `-target`: Explicit target triple (musl by default for reproducibility)
- `-fdebug-prefix-map`: Make debug info path-independent
- `-MMD -MF`: Generate depfile for header dependencies
- No `--sysroot` needed - Zig bundles everything

### Environment Control

Execute in clean environment:
```rust
env::clear();
env::set("PATH", "/dev/null");  // Zig doesn't need PATH
env::set("PWD", workspace_root);
// That's it
```

## Distribution Strategy

### Download from Zig Releases

```
https://ziglang.org/download/0.13.0/
├── zig-linux-x86_64-0.13.0.tar.xz
├── zig-linux-x86_64-0.13.0.tar.xz.minisig
├── zig-macos-aarch64-0.13.0.tar.xz
├── zig-macos-aarch64-0.13.0.tar.xz.minisig
└── ...
```

Process:
1. `AcquireZigToolchain(version = "0.13.0", host = "x86_64-linux")`
2. Download `zig-linux-x86_64-0.13.0.tar.xz`
3. Verify minisig signature
4. Extract tarball
5. Hash `zig` binary → `zig_exe_hash`
6. Tar up `lib/` directory, hash → `zig_lib_hash`
7. `ToolchainId = hash(zig_exe_hash + zig_lib_hash)`
8. Store both in CAS
9. Create `ZigToolchainManifest`, store in CAS
10. Return `ToolchainId`

### CAS Storage Layout

```
~/.vx/cas/blobs/
├── ab/cd/ef...  [zig executable]
├── 12/34/56...  [lib/ directory tar]
└── ...
```

At execution time:
1. Extract `zig` binary to temp location
2. Extract `lib/` to temp location
3. Run: `$TEMP/zig cc ...`
4. Clean up temp files

## Determinism Checklist

- [x] Zig binary is exact same bytes across machines (from official release)
- [x] Zig lib/ directory is exact same bytes
- [x] `-target` explicitly specified (no host inference)
- [x] `-fdebug-prefix-map` for path-independent debug info
- [x] Depfiles capture header dependencies
- [x] Environment cleared (Zig is self-contained)
- [x] Temp dir paths not leaked into output

## Targets to Support (v0)

Start with **musl targets only** for clean reproducibility:

1. `x86_64-linux-musl` (most common)
2. `aarch64-linux-musl` (ARM servers)

Later:
3. `x86_64-macos` (Apple Silicon via Rosetta or cross)
4. `aarch64-macos` (Apple Silicon native)
5. `x86_64-windows-gnu` (Windows via MinGW)

Musl-first strategy avoids glibc version hell.

## C++ Support

`zig c++` works for most code:

```bash
zig c++ \
  -target x86_64-linux-musl \
  -std=c++20 \
  -fdebug-prefix-map=$WORKSPACE=/vx-workspace \
  -c $SOURCE -o $OUTPUT
```

Uses bundled libc++ (LLVM's C++ stdlib).

Limitations:
- Some complex template metaprogramming may hit edge cases
- ABI compatibility with system C++ libs can be tricky
- Sanitizers work but need explicit flags

**Recommendation**: C is fully hermetic day 1, C++ is "best effort" initially.

## Implementation Plan

### Phase 1: Basic Hermetic C (Week 1)

1. Implement `AcquireZigToolchain` node
   - Download from ziglang.org
   - Verify signature
   - Store in CAS
   
2. Implement `CcCompile` node
   - Extract zig to temp
   - Run `zig cc -c`
   - Parse depfile
   - Hash output

3. Implement `Link` node
   - Run `zig cc` (linker mode)
   - Hash output

4. Test: Build "hello world"
   - Verify cache hit on re-build
   - Verify cache hit from different directory
   - Verify cache hit on different machine (with same ToolchainId)

### Phase 2: Real Project (Week 2)

5. Build a small real project (e.g., `jq`, `sqlite`)
   - Test depfile parsing works
   - Test incremental builds (change one file)
   - Verify cache efficiency

### Phase 3: Cross-Compilation (Week 3)

6. Add `aarch64-linux-musl` target
   - Same ToolchainId, different `-target` flag
   - Verify cross-compiled binary works

### Phase 4: Remote Execution (Week 4)

7. Send CcCompile to remote execd
   - Send ToolchainId + source + flags
   - Remote fetches toolchain from CAS
   - Remote compiles, returns object file
   - Verify cache hit locally

## Alternative: Vanilla Clang+Musl (Future)

If Zig proves limiting (unlikely), fall back to:

```
ToolchainManifest {
    clang: Blake3Hash,      // clang binary
    lld: Blake3Hash,        // ld.lld binary
    sysroot: Blake3Hash,    // musl headers + libs tarball
    compiler_rt: Blake3Hash // compiler builtins
}
```

Invocation:
```bash
clang \
  --sysroot=$EXTRACTED_SYSROOT \
  -fuse-ld=lld \
  -fdebug-prefix-map=$WORKSPACE=/vx-workspace \
  -c $SOURCE -o $OUTPUT
```

More work to assemble, but gives 100% vanilla LLVM behavior.

**Decision: Start with Zig, only do vanilla if we hit blockers.**

## Open Questions

- Zig version pinning strategy?
  → Pin to specific release in vx repo (e.g., "0.13.0"), update manually
  
- How to handle Zig itself updating?
  → New Zig version = new ToolchainId = rebuild everything (expected)
  
- Static vs dynamic linking?
  → Static only for v0 (simpler, more reproducible)
  
- How to verify Zig signatures?
  → Use minisign crate to verify .minisig files from ziglang.org
  
- What if user needs glibc specifically?
  → Defer to Phase 2, or tell them "musl or bust" for v0

## Success Criteria

vx can build a C project hermetically when:

1. ✅ Same source + same ToolchainId = cache hit (even different dir)
2. ✅ Different machines with same ToolchainId = cache hit
3. ✅ Change one .c file = only recompile that file
4. ✅ Change one header = recompile only affected .c files (depfile-driven)
5. ✅ Remote execution produces bit-for-bit identical output
6. ✅ Cross-compile to aarch64 works without installing cross-tools

If Zig enables all 6, it's the right choice for v0.
