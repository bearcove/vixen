# C Compilation Support Design

## Overview

`vx build` will support multi-language projects via `vx.kdl`:
- **If `vx.kdl` exists**: Use it to define what to build (Rust, C, mixed)
- **If no `vx.kdl`**: Fall back to `Cargo.toml` (current behavior)

This allows:
1. Pure Rust projects (status quo - Cargo.toml)
2. Pure C projects (vx.kdl)
3. Mixed Rust + C projects (vx.kdl references both)

## vx.kdl Format

### Example 1: Pure Rust (equivalent to Cargo.toml)

```kdl
project name="my-app" lang="rust"
bin name="my-app" sources="src/main.rs"
```

### Example 2: Pure C

```kdl
project name="hello" lang="c"
bin name="hello" sources="main.c"
```

### Example 3: Rust + C (future)

```kdl
project name="mixed-app" lang="rust"

// Rust binary that links against C library
bin name="app" lang="rust" sources="src/main.rs" link="mylib"

// C library
lib name="mylib" lang="c" sources="csrc/*.c" headers="csrc/*.h"
```

## Build Flow

### Current (Cargo.toml only)
```
vx build
  → find_cargo_toml()
  → parse Cargo.toml
  → build crate graph
  → build action graph (Rust actions)
  → execute
```

### With vx.kdl
```
vx build
  → check for vx.kdl
    ├─ Found: parse_vx_kdl() → build unified graph
    └─ Not found: find_cargo_toml() → (existing path)
  → build action graph (Rust + C actions)
  → execute
```

## Action Graph Changes

New action types:

```rust
pub enum Action {
    // Existing
    AcquireToolchain { ... },
    CompileRustCrate { ... },

    // New for C
    AcquireZigToolchain {
        version: String,  // e.g., "0.13.0"
    },

    CompileCObject {
        source: Utf8PathBuf,     // src/foo.c
        output: Utf8PathBuf,     // .vx/obj/foo.o
        includes: Vec<Utf8PathBuf>,
        defines: Vec<String>,
        flags: Vec<String>,
    },

    LinkCBinary {
        objects: Vec<Utf8PathBuf>,  // .o files
        output: Utf8PathBuf,         // bin/hello
        libs: Vec<String>,
    },
}
```

## Zig as C Compiler

Why Zig instead of system cc:
- **Hermetic**: Download specific Zig version, store in CAS
- **Cross-compilation**: `zig cc` supports any target without separate toolchains
- **Caching**: Deterministic builds, works with CAS
- **Drop-in**: `zig cc` is clang-compatible

Zig acquisition (similar to Rust):
```
Download from https://ziglang.org/download/
  → Extract to temp
  → Ingest to CAS
  → Store manifest hash
  → Materialize when needed
```

## Implementation Phases

### Phase 1: vx.kdl + Pure C (MVP)
- [ ] Add `knuffel` or `kdl` crate for parsing
- [ ] Create `vx_project` crate with `VxManifest` type
- [ ] Modify `vx build` to check for vx.kdl first
- [ ] Add `AcquireZigToolchain` action
- [ ] Add `CompileCObject` action
- [ ] Add `LinkCBinary` action
- [ ] Test with hello world C program

### Phase 2: Rust via vx.kdl
- [ ] Support Rust projects in vx.kdl (migration path)
- [ ] Unified manifest type that handles both

### Phase 3: Mixed Rust + C
- [ ] Link C objects into Rust binaries
- [ ] Automatic header discovery
- [ ] C dependencies of Rust crates (like cc crate)

### Phase 4: C Dependencies
- [ ] Registry for C libraries (or just git sources?)
- [ ] pkg-config integration?
- [ ] Vendoring

## File Locations

```
project/
├── vx.kdl              # Project manifest (optional, takes precedence)
├── Cargo.toml          # Fallback for Rust-only projects
├── src/
│   ├── main.c          # C sources
│   └── lib.rs          # Rust sources
└── .vx/
    ├── cache/          # CAS for all artifacts
    ├── toolchains/
    │   ├── rust/       # Rust toolchains
    │   └── zig/        # Zig toolchains
    └── obj/            # Intermediate .o files (C compilation)
```

## Example: Hello World in C

**vx.kdl:**
```kdl
project name="hello" lang="c"
bin name="hello" sources="main.c"
```

**main.c:**
```c
#include <stdio.h>

int main() {
    printf("Hello from vx!\n");
    return 0;
}
```

**Action graph generated:**
```
AcquireZigToolchain("0.13.0")
  └─> CompileCObject(main.c → .vx/obj/main.o)
       └─> LinkCBinary([main.o] → bin/hello)
```

**Execution:**
```bash
$ vx build
[1/3] Acquiring Zig 0.13.0 toolchain
[2/3] Compiling main.c
[3/3] Linking hello
Built: bin/hello

$ ./bin/hello
Hello from vx!
```

## Cache Keys for C

C compilation is deterministic when:
- Source file content hash
- Header file content hashes (discovered via -MD flag)
- Compiler version (Zig toolchain hash)
- Flags and defines

Cache key for CompileCObject:
```
blake3(
    source_hash,
    discovered_headers_hash,
    zig_toolchain_hash,
    flags_hash,
)
```

## Open Questions

1. **C standard library**: Should we use Zig's libc or system libc?
   - Zig's: More hermetic, works cross-platform
   - System: Simpler, but less portable

2. **Header discovery**: How to handle #include dependencies?
   - Use `zig cc -MD` to generate .d files
   - Parse .d files to track header dependencies
   - Re-hash when headers change

3. **Optimization levels**: Map to Rust profiles?
   - `--profile debug` → `-O0`
   - `--profile release` → `-O3`

4. **C library linking**: Static or dynamic?
   - Static by default (hermetic)
   - Dynamic opt-in (for system libs)

5. **Windows support**: Zig handles this, but need to test
   - Zig can cross-compile to Windows from Unix
   - May need different link flags

## Migration Path

Existing Rust projects work unchanged:
```bash
$ cd my-rust-project
$ vx build          # Uses Cargo.toml (no vx.kdl)
```

Gradual adoption:
```bash
$ cd my-rust-project
$ cat > vx.kdl <<EOF
# Same project, but now via vx.kdl
project { name "my-app"; lang "rust" }
bin { name "my-app"; sources "src/main.rs" }
EOF
$ vx build          # Uses vx.kdl now
```

## Why KDL?

- **Human-friendly**: Cleaner than TOML for nested structures
- **Comments**: Built-in, unlike JSON
- **Rust support**: `knuffel` or `kdl` crates available
- **Extensible**: Easy to add new node types without breaking existing files
- **No significant whitespace**: Unlike YAML

Alternative considered: Keep TOML
- Pro: Ecosystem familiarity
- Con: vx.toml vs Cargo.toml confusion
- Con: TOML gets verbose for nested config

KDL lets us have a distinct, modern format while being approachable.
