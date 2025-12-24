# Syn Elimination Plan for Vixen

## Executive Summary

The goal is to remove `syn` entirely from the dependency tree. `syn` is a heavy proc-macro dependency that significantly impacts compile times. After analysis, **all syn dependencies come from proc-macros that can be replaced with manual implementations or feature flag changes**.

## Current Syn Sources (from `cargo tree -i syn`)

| Proc-Macro | Brought By | Status | Fix |
|------------|-----------|--------|-----|
| `async-stream-impl` | rapace-core | Active | Replace with manual async stream |
| `enum_dispatch` | rapace-core | Active | Replace with manual match dispatch |
| `futures-macro` | futures-util (via futures-timeout) | Active | Use futures-util without macros + tokio timeout |
| `miette-derive` | facet-reflect[miette] | Active | Make derive optional in facet |
| `pin-project-internal` | pin-project (via futures-timeout) | Active | Replace futures-timeout with tokio::time |
| `test-log-macros` | vx (dev-dep) | Active | Write simple test helper |
| `thiserror-impl` | clonetree | Active | Clonetree being removed (VFS route) |
| `tokio-macros` | tokio[full] | Active | Use tokio without macros feature |
| `tracing-attributes` | tracing | Active | Use tracing without attributes feature |

---

## Phase 1: Vixen-Local Changes (Easy Wins)

### 1.1 Remove `tokio-macros` dependency

**Location:** `vixen/Cargo.toml` workspace dependencies

**Current:**
```toml
tokio = { version = "1", features = ["full"] }
```

**Fix:** Replace `"full"` with explicit features minus `macros`:
```toml
tokio = { version = "1", features = ["rt-multi-thread", "io-util", "net", "time", "fs", "process", "sync", "signal"] }
```

**Code changes required:**
- `crates/vx/src/main.rs`: Replace `#[tokio::main]` with manual runtime creation
- `crates/vx-aether/src/main.rs`: Same
- `crates/vx-rhea/src/main.rs`: Same
- `crates/vx-cass/src/main.rs`: Same
- `crates/vx-aether/src/tests.rs`: Replace `#[tokio::test]` with manual runtime
- `crates/vx-cass/examples/test_client.rs`: Same

**Pattern for main:**
```rust
fn main() -> eyre::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main())
}

async fn async_main() -> eyre::Result<()> {
    // original main body
}
```

**Pattern for tests:**
```rust
#[test]
fn test_name() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // test body
    });
}
```

### 1.2 Remove `tracing-attributes` dependency

**Location:** Implicit via `tracing` crate

**Fix:** Add `default-features = false` to tracing:
```toml
tracing = { version = "0.1", default-features = false, features = ["std"] }
```

**Code changes required:**
- `crates/vx-cass/src/service.rs` - No `#[instrument]` found in vixen!
- `crates/vx-cass/src/toolchain.rs` - No `#[instrument]` found in vixen!

Actually, grepping shows NO `#[instrument]` usage in vixen. The dependency comes from facet-args and rapace which use tracing. Those crates need to disable the feature.

### 1.3 Remove `test-log` dependency

**Location:** `crates/vx/Cargo.toml`

**Current:**
```toml
[dev-dependencies]
test-log = { version = "0.2", features = ["trace"] }
```

**Fix:** Create a simple helper function instead:
```rust
// In a test module or test harness file
fn init_test_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();
}
```

Then each test calls `init_test_tracing();` at the start. The `try_init()` is idempotent.

---

## Phase 2: Rapace Changes

### 2.1 Remove `async-stream`

**Location:** `rapace/crates/rapace-core/Cargo.toml`

**Usage:** `rapace-macros/src/lib.rs` uses `try_stream!` macro in generated code.

**Fix:** The `try_stream!` macro generates an async stream. Replace with a custom implementation using `futures_core::Stream` + `Pin` + manual state machine, or use `async-stream-lite` if it exists, or inline the pattern.

The generated code looks like:
```rust
let stream = try_stream! {
    // yields
};
```

This can be replaced with a manual `Stream` implementation or using `futures::stream::unfold`.

### 2.2 Remove `enum_dispatch`

**Location:** `rapace/crates/rapace-core/src/transport.rs`

**Current usage:**
```rust
#[enum_dispatch]
pub(crate) trait TransportBackend { ... }

#[enum_dispatch(TransportBackend)]
pub enum Transport {
    Mem(MemTransport),
    Stream(StreamTransport),
    // ...
}
```

**Fix:** Manual implementation using a `match`:
```rust
impl Transport {
    pub async fn send_frame(&self, frame: Frame) -> Result<(), TransportError> {
        match self {
            Transport::Mem(t) => t.send_frame(frame).await,
            Transport::Stream(t) => t.send_frame(frame).await,
            // ...
        }
    }
    // etc
}
```

Or use generics with `dyn TransportBackend` if the enum isn't needed.

### 2.3 Remove `futures-timeout`

**Location:** `rapace/crates/rapace-core/src/session.rs`

**Current usage:**
```rust
use futures_timeout::TimeoutExt;
rx.timeout(Duration::from_millis(timeout_ms)).await
```

**Fix:** Replace with `tokio::time::timeout`:
```rust
use tokio::time::timeout;
match timeout(Duration::from_millis(timeout_ms), rx).await {
    Ok(Ok(frame)) => frame,
    Ok(Err(_)) => { /* channel closed */ }
    Err(_elapsed) => { /* timeout */ }
}
```

This also removes `pin-project` dependency since futures-timeout uses it.

**Note:** futures-timeout is runtime-agnostic (works on WASM). If WASM support is needed, need conditional compilation:
```rust
#[cfg(target_arch = "wasm32")]
// use gloo_timers or similar
#[cfg(not(target_arch = "wasm32"))]
// use tokio::time::timeout
```

---

## Phase 3: Facet Changes

### 3.1 Remove `miette-derive` from facet-reflect

**Location:** `facet/facet-reflect/Cargo.toml`

**Current:**
```toml
miette = { workspace = true, optional = true, features = ["derive", "fancy-no-backtrace"] }
```

**Fix:** Remove `"derive"` from features:
```toml
miette = { workspace = true, optional = true, features = ["fancy-no-backtrace"] }
```

**Code changes required:** Check if `facet-reflect` uses `#[derive(Diagnostic)]` anywhere. If so, implement the trait manually.

### 3.2 Disable tracing attributes in facet-args

**Location:** `facet/facet-args/Cargo.toml`

If facet-args uses `#[instrument]`, it needs tracing with attributes. Check and either:
- Remove `#[instrument]` usage
- Or disable default features on tracing

---

## Phase 4: Picante Changes

Check if picante uses any syn-dependent crates and update accordingly.

---

## Dependency Order

Changes should be made in this order (bottom-up):

1. **Facet** (upstream) - miette-derive removal, tracing attributes
2. **Rapace** (upstream) - async-stream, enum_dispatch, futures-timeout
3. **Picante** (upstream) - check dependencies
4. **Vixen** (this repo) - tokio-macros, test-log, tracing

---

## Verification

After all changes:
```bash
cargo tree -i syn
```

Should return nothing (or error "syn not found").

---

## Compile Time Impact

Rough estimates based on typical syn compile times:
- `syn` itself: ~15-20 seconds
- Each proc-macro depending on syn: adds parsing overhead

Expected improvement: **30-60 seconds off clean build** depending on parallelism.

---

## Risk Assessment

| Change | Risk | Mitigation |
|--------|------|------------|
| tokio without macros | Low | Well-documented pattern |
| tracing without attributes | Low | Just removes convenience |
| async-stream removal | Medium | Need careful Stream impl |
| enum_dispatch removal | Low | Mechanical replacement |
| futures-timeout → tokio | Low | Unless WASM needed |
| miette derive removal | Low-Medium | Need to check Diagnostic usage |
| test-log removal | Low | Simple replacement |
