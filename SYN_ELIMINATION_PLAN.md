# Syn Elimination Plan

## ACCOMPLISHED ✓

1. ✅ **Replaced vx-test-macros** → `tokio-test-lite` from crates.io
2. ✅ **Filed upstream issues**:
   - https://github.com/facet-rs/facet/issues/1458 (tracing defaults)
   - https://github.com/bearcove/rapace/issues/113 (tracing defaults)
3. ✅ **Removed miette-derive** - Manual `Diagnostic` impl in vx-manifest
4. ✅ **Removed thiserror** from:
   - vx-manifest (lib.rs + lockfile.rs)
   - vx-cc/depfile.rs

## CURRENT STATUS

Remaining proc-macros using syn:
- `futures-macro` ← from futures-util (rapace, picante, hyper, etc.)
- `tokio-macros` ← from tokio features
- `thiserror-impl` ← from 13 files still using `#[derive(Error)]`
- `tracing-attributes` ← blocked on upstream (facet, rapace)

## NEXT ACTIONS

### 1. thiserror-impl (IN PROGRESS)
**Files remaining** (13 total):
```
crates/vx-aether/src/error.rs
crates/vx-cass/src/http.rs
crates/vx-cass/src/registry.rs
crates/vx-cc/src/depfile.rs
crates/vx-rhea/src/error.rs
crates/vx-rs/src/build_script.rs
crates/vx-rs/src/crate_graph.rs
crates/vx-rs/src/depfile.rs
crates/vx-rs/src/module_scanner.rs
crates/vx-rs/src/snapshot.rs
crates/vx-tarball/src/lib.rs
crates/vx-toolchain/src/lib.rs
crates/vx-toolchain/src/zig.rs
```

**Approach**: For each file:
1. Remove `#[derive(Error)]`
2. Manually impl `std::fmt::Display`
3. Manually impl `std::error::Error`
4. Handle `#[source]` and `#[from]` attributes manually

### 2. tokio-macros
**Investigation needed**: Find what's pulling in tokio with macros feature
**Solution**: Either disable or manually build runtime everywhere

### 3. futures-macro
**Problem**: Core async utility used throughout
**Options**:
- File issue on rapace/picante to minimize futures-util usage
- Replace specific futures combinators with manual impls
- Accept this one (it's deeply embedded in async ecosystem)

### 4. tracing-attributes
**Status**: BLOCKED - waiting for upstream PRs
**Timeline**: After facet#1458 and rapace#113 are merged

## STRATEGY

We'll tackle these in order of impact:
1. ⏳ **thiserror** (directly fixable, high impact - 13 files)
2. ⏳ **tokio-macros** (directly fixable if unused)
3. ⚠️ **futures-macro** (file upstream issues, may accept)
4. ⏸️ **tracing-attributes** (blocked on upstream)

Once thiserror is eliminated, we'll have removed 2/5 syn dependencies ourselves, with 2 more blocked on upstream fixes we've already filed.
