# Crates.io Support Plan (v1)

This document specifies the first implementation slice for crates.io dependencies in vertex.
It is designed for engineer1 to implement directly. Scope is lockfile-only registry deps,
with existing v0 restrictions (no features, build.rs, proc-macros, workspaces, etc.).

## Goals

- Support crates.io dependencies using `Cargo.lock` as the source of truth.
- Preserve vertex invariants: CAS is the only durable store, explicit inputs, deterministic
  cache keys, workspace-relative paths, no Cargo fallback.
- Keep v0 restriction set (fail loudly on unsupported features/build.rs/proc-macro/etc.).
- Enable reuse of registry sources via global cache under `~/.vx/registry`, while building
  from project-local `.vx/registry` for workspace-relative paths.

## Non-goals (v1)

- Version solving (PubGrub/Cargo resolver). Use `Cargo.lock` only.
- Multiple registries, git deps, or non-crates.io sources.
- Features, build scripts, proc-macros, dev/build deps, workspaces, multiple targets.

---

## Summary of the design

1) Parse `Cargo.lock` and treat it as authoritative.
2) For each registry crate in the reachable dependency closure:
   - CAS fetches `.crate` tarball + verifies checksum.
   - Execd materializes into global cache `~/.vx/registry/<name>/<version>/<checksum>/`.
   - Execd then uses `clonetree` to copy into project-local `.vx/registry/...`.
3) CrateGraph uses `.vx/registry/...` paths, keeping all source paths workspace-relative.
4) Cache keys include source closure hashes and dependency rlib hashes.

---

## Decisions locked

- **Materialization happens in execd** (not daemon).
- **Global cache at `~/.vx/registry/...` + project-local staging** via `clonetree`.
- **Lockfile is authoritative**; no resolver in v1.

---

## New protocol types (add a new proto crate or extend vx-cas-proto)

### RegistrySpec

- `registry_url: String` (v1 must equal `https://github.com/rust-lang/crates.io-index`)
- `name: String`
- `version: String`
- `checksum: String` (from Cargo.lock)

### RegistryCrateManifest

- `schema_version: u32`
- `spec: RegistrySpec`
- `crate_tarball_blob: Blake3Hash` (the `.crate` tarball bytes)
- `checksum: String` (repeated for provenance)
- `created_at: String` (RFC3339)

### EnsureRegistryCrateResult

- `spec_key: Option<Blake3Hash>`
- `manifest_hash: Option<Blake3Hash>`
- `status: EnsureStatus { Hit | Downloaded | Failed }`
- `error: Option<String>`

### CasRegistry trait (on casd)

- `ensure_registry_crate(spec: RegistrySpec) -> EnsureRegistryCrateResult`
- `get_registry_manifest(manifest_hash: Blake3Hash) -> Option<RegistryCrateManifest>`
- Optional: `get_registry_materialization_plan(manifest_hash) -> RegistryMaterializationPlan`

### Execd RPC

- `materialize_registry_crate(manifest_hash: Blake3Hash, workspace_root: String) -> Result<RegistryMaterializationResult>`

`RegistryMaterializationResult`:
- `workspace_rel_path: String` (e.g. `.vx/registry/serde/1.0.197`)
- `global_cache_path: String` (optional, for debugging/reporting)

---

## Materialization flow

### 1) CAS acquisition (casd)

- Given `RegistrySpec`, casd downloads `https://crates.io/api/v1/crates/<name>/<version>/download`.
- Verify checksum matches Cargo.lock.
- Store tarball bytes as a blob.
- Store `RegistryCrateManifest` via `put_manifest` (not as a blob).
- Publish `spec_key -> manifest_hash` mapping (atomic, first-writer-wins).

### 2) Execd materialization

For each needed registry crate:

1. Fetch manifest from casd.
2. Ensure global cache directory exists:
   `~/.vx/registry/<name>/<version>/<checksum>/`.
3. If not present, extract tarball blob into global cache.
   - Validate tar entries: no absolute paths, no `..`, no symlink escapes.
   - Write `.materialized` marker.
4. Use `clonetree` to copy from global cache to project-local staging:
   `<workspace>/.vx/registry/<name>/<version>/`.
5. Return workspace-relative path for graph construction.

### Why two-step?

- Keeps workspace-relative paths for build inputs.
- Allows dedupe in global cache while avoiding absolute paths in cache keys.

---

## Manifest parsing changes (vx-manifest)

Extend dependency parsing to accept versioned deps while still rejecting:
- features
- optional deps
- build/dev deps
- git/registry overrides
- package rename

Accepted forms:
- `dep = "1.2"`
- `dep = { version = "1.2" }`

Still reject path-only or registry-only for v1 if inconsistent with lockfile?
- Use Cargo.lock as authoritative; if dependency in Cargo.toml is versioned but
  missing in Cargo.lock -> error: "Cargo.lock required for registry deps".

---

## Cargo.lock parsing

Implement a strict parser that extracts:
- `name`, `version`, `source`, `checksum`, `dependencies` (list of name + optional
  rename/extra)

Enforce:
- `source` must be `registry+https://github.com/rust-lang/crates.io-index`
- `checksum` required for registry packages
- ignore packages not reachable from root crate

---

## Crate identity and graph changes (vx-rs)

### New crate identity

Replace `CrateId::new(path, package)` with source-aware identity:

- `Path { workspace_rel, package_name }`
- `Registry { name, version, checksum }`

CrateId hash input becomes:
- Prefix string + source-specific fields (all normalized)

### Graph nodes

`CrateNode` gains:
- `source: CrateSource` (Path|Registry)
- `source_root_rel: Utf8PathBuf` (path under workspace root)

Path nodes keep behavior; registry nodes use materialized `.vx/registry/...` root.

### Dependency resolution

- For each path dep: same as today.
- For each registry dep:
  - Resolve from Cargo.lock to `RegistrySpec`.
  - Ensure materialization (execd) and use the returned workspace-relative root.
  - Parse that crate’s `Cargo.toml` and validate v0 subset.

---

## Cache key changes

No new cache key scheme required, but inputs must include:
- Source closure hash for registry crates (based on `.vx/registry/...` files).
- Dependency rlib hashes as already done for path deps.

Because registry sources are materialized into workspace-relative paths,
existing source-closure hashing continues to work without absolute paths.

---

## Input tracking changes (vx-rs)

- Treat `.vx/registry/**` as workspace files (still under workspace root).
- Ensure snapshot generation includes registry sources once materialized.
- If execd materializes during build, ensure daemon waits for materialization
  before computing source closures.

---

## Error handling

Fail loudly with actionable diagnostics:

- Missing Cargo.lock when registry deps present.
- Registry source not crates.io.
- Build scripts / proc-macro / features / optional deps in registry crate.
- Checksum mismatch for downloaded crate.
- Tarball path traversal or symlink escape.

---

## Implementation steps

1. Add registry proto types and CAS RPCs.
2. Implement casd registry acquisition and manifest storage.
3. Implement execd materialization + clonetree copy into `.vx/registry`.
4. Add Cargo.lock parsing and validation in daemon.
5. Extend vx-manifest dependency parsing to allow versioned deps.
6. Update CrateGraph to support registry crates and new CrateId.
7. Update input tracking as needed for `.vx/registry`.
8. Add tests and docs.

---

## Tests

### Unit tests

- Cargo.lock parser: happy path + invalid source + missing checksum.
- Registry manifest: checksum verification + spec_key stability.
- Tarball extraction safety: reject `..`, absolute paths, symlink escapes.

### Integration tests

- Minimal project with one registry dep (no build.rs/proc-macro) builds with lockfile.
- Missing Cargo.lock yields explicit error.
- Registry crate with build.rs or proc-macro yields explicit error.

---

## Documentation updates

- `README.md`: update limitations and usage notes (lockfile required for registry deps).
- `internal/DESIGN.md`: add registry dependency model + invariants.

---

## Risks / edge cases

- Many popular crates require features/build.rs/proc-macros, which v1 will reject.
- Global cache introduces a new path class; must avoid leaking absolute paths into cache keys.
- Concurrency: execd must lock per registry crate extraction to avoid races.

