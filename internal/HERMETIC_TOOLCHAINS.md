# Hermetic Toolchains in vx

## Philosophy

**Problem**: System toolchains make builds non-reproducible:
- "works on my machine" due to different compiler versions
- Cache misses across machines with different toolchain versions
- Remote execution needs identical toolchains on all workers
- Dependency on host environment breaks hermeticity

**Solution**: Toolchains as first-class CAS artifacts.

A toolchain is a content-addressed bundle containing:
- Compiler (clang)
- Linker (lld)
- Sysroot (musl libc + headers)
- Runtime libraries (compiler-rt)

```
ToolchainId = blake3(ToolchainManifest + all file blobs)
```

Build nodes reference `ToolchainId`, not "whatever is on PATH".

## Graph Integration

### New Node Types

```rust
// Acquire a toolchain (download, verify, store in CAS)
AcquireToolchain {
    spec: ToolchainSpec,      // e.g., "clang-18-musl-x86_64"
} -> ToolchainId

// Compile C/C++ source to object file
CcCompile {
    toolchain_id: ToolchainId,
    source: Blake3Hash,        // .c or .cpp file
    flags: Vec<String>,
    headers: Vec<Blake3Hash>,  // discovered via depfiles
} -> ObjectFile + DepInfo

// Link objects into executable
Link {
    toolchain_id: ToolchainId,
    objects: Vec<Blake3Hash>,
    libraries: Vec<Blake3Hash>,
    flags: Vec<String>,
} -> Executable
```

### Cache Key Benefits

Old (broken):
```
cache_key = hash(source + flags + "clang version 18.0.0")
```
Problem: Different clang 18.0.0 binaries across machines = cache misses

New (correct):
```
cache_key = hash(source + flags + toolchain_id)
```
`toolchain_id` is the hash of every toolchain file. Exact bit-for-bit match required.

## Minimum Viable Hermetic Toolchain

Start with **clang + lld + musl** for a single host target (e.g., x86_64-unknown-linux-musl):

### Components

1. **clang** (compiler driver)
   - Handles C/C++ → object files
   - Can invoke lld for linking

2. **lld** (linker)
   - Fast, deterministic
   - Part of LLVM project (good compatibility with clang)

3. **musl sysroot**
   - `libc.a` (static) or `libc.so` (dynamic)
   - Headers: `<stdio.h>`, `<stdlib.h>`, etc.
   - CRT objects: `crt1.o`, `crti.o`, `crtn.o`

4. **compiler-rt** (runtime library)
   - Builtins (`__divdi3`, etc.)
   - Sanitizer runtimes (optional, for later)

### File Structure

```
toolchain-clang18-musl-x86_64/
├── bin/
│   ├── clang       (compiler driver)
│   └── ld.lld      (linker)
├── lib/
│   ├── clang/18/lib/linux/
│   │   └── libclang_rt.builtins-x86_64.a
│   └── musl/
│       ├── libc.a
│       ├── crt1.o
│       ├── crti.o
│       └── crtn.o
└── include/
    └── [musl headers]
```

### Invocation Contract

**Compile (.c → .o):**
```bash
clang \
  --sysroot=$TOOLCHAIN/sysroot \
  -fdebug-prefix-map=$WORKSPACE=/vx-workspace \
  -fuse-ld=lld \
  -MMD -MF $DEPFILE \
  -c $SOURCE -o $OUTPUT
```

**Link (objects → executable):**
```bash
clang \
  --sysroot=$TOOLCHAIN/sysroot \
  -fuse-ld=lld \
  $OBJECTS -o $OUTPUT
```

Key flags:
- `--sysroot`: Explicit path to libc/headers (no host search paths)
- `-fdebug-prefix-map`: Make debug info path-independent
- `-fuse-ld=lld`: Use hermetic linker
- `-MMD -MF`: Generate depfile for header dependencies

## ToolchainManifest Schema

```rust
pub struct ToolchainManifest {
    /// Unique identifier for this toolchain
    pub id: ToolchainId,
    
    /// Human-readable name
    pub name: String,  // e.g., "clang-18-musl-x86_64"
    
    /// Target triple
    pub target: String,  // e.g., "x86_64-unknown-linux-musl"
    
    /// Compiler version
    pub compiler_version: String,  // e.g., "18.1.2"
    
    /// Libc type and version
    pub libc: LibcInfo,
    
    /// File blobs in CAS
    pub files: ToolchainFiles,
}

pub struct LibcInfo {
    pub kind: LibcKind,  // Musl, Glibc, etc.
    pub version: String,
}

pub enum LibcKind {
    Musl,
    Glibc,
    // Could add others later
}

pub struct ToolchainFiles {
    /// Compiler binary
    pub clang: Blake3Hash,
    
    /// Linker binary
    pub lld: Blake3Hash,
    
    /// Sysroot archive (tar.xz of headers + libs)
    pub sysroot: Blake3Hash,
    
    /// Compiler runtime
    pub compiler_rt: Blake3Hash,
}
```

## Distribution Strategy

### Phase 1: Prebuilt Bundles (MVP)

vx downloads prebuilt toolchain tarballs:

```
https://vx-toolchains.example.com/
├── clang-18-musl-x86_64.tar.xz
├── clang-18-musl-x86_64.tar.xz.sha256
└── manifest.json
```

Process:
1. `AcquireToolchain("clang-18-musl-x86_64")`
2. Download tarball, verify checksum
3. Extract to temp dir
4. Hash all files → `ToolchainId`
5. Store files in CAS
6. Create `ToolchainManifest`, store in CAS
7. Return `ToolchainId`

### Phase 2: Build from Source (Future)

For true "Nix but vx" experience:
- Build LLVM/clang from source
- Build musl from source
- Make these builds hermetic too (bootstrapping!)

This is a whole product. Defer until Phase 1 proves value.

## System Toolchains (Optional Escape Hatch)

Policy decision needed:

**Option A: Hermetic Only (hardline)**
- vx only supports hermetic toolchains
- No "use system clang" mode
- Cleanest, but requires toolchain setup upfront

**Option B: Hermetic Default, System Fallback (pragmatic)**
- Hermetic toolchains for CI/remote execution
- System toolchain adapter for local dev convenience
- System toolchains explicitly non-remote-cacheable

Recommendation: **Start with Option A**, add Option B only if developers complain.

If we do Option B, the adapter:
```rust
// "System toolchain" is just a ToolchainId derived from host files
fn system_toolchain_id() -> ToolchainId {
    let clang_path = which("clang")?;
    let clang_version = run("clang --version")?;
    // Hash clang binary + version + sysroot files
    // Mark as "system" in metadata (no remote cache)
    ToolchainId::from_system(...)
}
```

## Determinism Checklist

- [ ] `--sysroot` points to CAS-backed directory (no `/usr/include`)
- [ ] `-fuse-ld=lld` uses CAS-backed linker (no `/usr/bin/ld`)
- [ ] `-fdebug-prefix-map` makes debug info path-independent
- [ ] Depfiles capture header dependencies (hashed into cache key)
- [ ] Environment variables cleared in execd (only whitelist essentials)
- [ ] Toolchain files are bit-for-bit identical across machines

## Next Steps

1. Define `ToolchainManifest` protobuf schema
2. Implement `AcquireToolchain` node in vx-daemon
3. Create prebuilt clang+musl+lld bundle for x86_64-linux
4. Test: compile "hello world" C program using hermetic toolchain
5. Verify: cache hit when building from different directory
6. Verify: cache hit across different machines with same ToolchainId

## Open Questions

- Do we need GCC support or is clang-only acceptable for v0?
  → Clang-only is fine. GCC has worse determinism story.

- Static linking only, or support dynamic too?
  → Static first (simpler), dynamic later if needed.

- How to handle cross-compilation?
  → Each target gets its own toolchain bundle. Cache key includes target triple.

- What about C++ stdlib (libc++)?
  → Include libc++ headers/libs in sysroot for C++ support.
