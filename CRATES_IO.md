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

## Decisions locked

- **Materialization happens in execd** (not daemon).
- **Global cache at `~/.vx/registry/...` + project-local staging** via `clonetree`.
- **Lockfile is authoritative**; no resolver in v1.

---

## Summary of the design

1) Parse `Cargo.lock` and treat it as authoritative.
2) Compute reachable registry packages from the root crate (see algorithm below).
3) For each reachable registry crate:
   - CAS fetches `.crate` tarball + verifies checksum.
   - Execd materializes into global cache `~/.vx/registry/<name>/<version>/<checksum>/`.
   - Execd then uses `clonetree` to copy into project-local `.vx/registry/...`.
4) CrateGraph uses `.vx/registry/...` paths, keeping all source paths workspace-relative.
5) Cache keys include source closure hashes and dependency rlib hashes.

---

## Cargo.lock parsing and reachability

### Parsing requirements

Extract for each `[[package]]`:
- `name`
- `version`
- `source`
- `checksum`
- `dependencies` (list of dep specs)

Enforce:
- `source` must be `registry+https://github.com/rust-lang/crates.io-index`
- `checksum` required for registry packages

### Reachability algorithm

1) Identify the root package (matching `[package].name` in the root `Cargo.toml`).
2) BFS/DFS through `dependencies` field in `Cargo.lock` packages.
3) Mark all reachable packages; only these are materialized and built.
4) Fail if any reachable package has unsupported `source`.

Note: dependency specs in `Cargo.lock` may be `"name version"` or `"name"`. Use
Cargo.lock semantics to map them to the correct `[[package]]` entry by name+version.

### Supported lockfile versions

Support Cargo.lock v3 and v4 formats. Fail with a clear error on unrecognized
versions. Both formats use `[[package]]` arrays; v4 uses a different checksum
encoding but the same logical structure.

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

If a versioned dep exists but Cargo.lock is missing or doesn’t contain a matching package:
- fail with clear error: "registry dependencies require Cargo.lock".

---

## New protocol types (new proto crate or extend vx-cas-proto)

### RegistrySpec

- `registry_url: String` (v1 must equal `https://github.com/rust-lang/crates.io-index`)
- `name: String`
- `version: String`
- `checksum: String` (SHA256 from Cargo.lock)

### RegistryCrateManifest

- `schema_version: u32` (e.g. `REGISTRY_MANIFEST_SCHEMA_VERSION = 1`)
- `spec: RegistrySpec` (includes checksum for provenance)
- `crate_tarball_blob: Blake3Hash` (tarball bytes in CAS)
- `created_at: String` (RFC3339)

### EnsureRegistryCrateResult

- `spec_key: Option<Blake3Hash>`
- `manifest_hash: Option<Blake3Hash>`
- `status: EnsureStatus { Hit | Downloaded | Failed }`
- `error: Option<String>`

### CasRegistry trait (on casd)

- `ensure_registry_crate(spec: RegistrySpec) -> EnsureRegistryCrateResult`
- `get_registry_manifest(manifest_hash: Blake3Hash) -> Option<RegistryCrateManifest>`

### Execd RPC

- `materialize_registry_crate(manifest_hash: Blake3Hash, workspace_root: String) -> Result<RegistryMaterializationResult>`

`RegistryMaterializationResult`:
- `workspace_rel_path: String` (e.g. `.vx/registry/serde/1.0.197`)
- `global_cache_path: String` (optional, for debugging/reporting)

---

## CAS acquisition (casd)

- Download URL: `https://crates.io/api/v1/crates/<name>/<version>/download`.
- Verify SHA256 matches `Cargo.lock` checksum.
- Store tarball bytes as CAS blob.
- Store `RegistryCrateManifest` via `put_manifest`.
- Publish `spec_key -> manifest_hash` mapping (atomic, first-writer-wins).
- Retry with exponential backoff on transient failures (429, 5xx, network errors).

### Tarball validation

Before accepting the tarball:
- Ensure exactly one top-level directory.
- Reject absolute paths and `..` components.
- Reject symlink escapes (symlinks pointing to `/` or containing `..`).
- **Reject all symlinks entirely** (not just escaping ones).

**Rationale for rejecting all symlinks**: While non-escaping symlinks are technically safe,
preserving them through the CAS→execd pipeline adds complexity (manifest format changes,
platform-specific extraction logic). In practice, crates.io crates rarely use symlinks—
cargo's own packaging strips them. If we encounter a crate that legitimately needs symlinks,
we can revisit this decision. For now, fail-fast with a clear error is better than silent
breakage (extracting symlinks as empty/broken files).

This validation belongs in casd (like toolchain validation) to keep execd simple.

---

## Execd materialization

For each needed registry crate:

1. Fetch manifest from casd.
2. Ensure global cache directory exists:
   `~/.vx/registry/<name>/<version>/<checksum>/`.
3. If not present, extract tarball blob into global cache.
   - Use a lock file: `~/.vx/registry/<name>/<version>/<checksum>/.lock`.
   - Write `.materialized` marker on success.
4. Use `clonetree` to copy from global cache to project-local staging:
   `<workspace>/.vx/registry/<name>/<version>/`.
   - This should be a best-effort reflink/clone; fallback to copy if unsupported.
   - Write `.vx/registry/<name>/<version>/.checksum` with the expected checksum.
   - If `.checksum` exists and does not match, delete the workspace-local copy and re-clone.
5. Return workspace-relative path for graph construction.

Notes:
- Lock granularity is `(name, version, checksum)` to allow parallel extraction
  of different versions of the same crate.
- The workspace-local copy should be overwritten safely if already present.

---

## Crate identity and graph changes (vx-rs)

### New crate identity

Replace `CrateId::new(workspace_rel, package)` with source-aware identity:

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

- Path deps: same as today.
- Registry deps:
  - Resolve from Cargo.lock to `RegistrySpec`.
  - Ensure materialization (execd) and use the returned workspace-relative root.
  - Parse that crate’s `Cargo.toml` and validate v0 subset.

### Validation timing

Before building:
- Ensure all reachable registry crates are materialized.
- Parse their manifests and validate restrictions.
- Fail fast if any registry crate violates v0 limits.

Important: The lockfile provides the full transitive closure. We do not
discover additional dependencies by parsing registry `Cargo.toml`; it is
used only for target info and v0 validation.

---

## Cache key inputs

No new cache key scheme required, but inputs must include:
- Source closure hash for registry crates (computed from `.vx/registry/...`).
- Dependency rlib hashes as already done for path deps.

Because registry sources are materialized into workspace-relative paths,
existing source-closure hashing continues to work without absolute paths.

---

## Input tracking changes (vx-rs)

- Treat `.vx/registry/**` as workspace files (still under workspace root).
- Ensure snapshot generation includes registry sources once materialized.
- If execd materializes during build, daemon must wait before computing
  source closures and cache keys.

---

## Error messages (examples)

- Missing Cargo.lock:
  "registry dependencies require Cargo.lock (found 'serde = \"1.0\"' in [dependencies])"

- Unsupported registry source:
  "only crates.io is supported, found source = '{source}' for package '{name}'"

- build.rs in registry crate:
  "registry crate '{name} {version}' has build.rs, which is not supported yet"

- proc-macro in registry crate:
  "registry crate '{name} {version}' is a proc-macro, which is not supported yet"

- checksum mismatch:
  "checksum mismatch for {name} {version}: expected {expected}, got {actual}"

---

## Implementation steps

1. Add registry proto types and CAS RPCs.
2. Implement Cargo.lock parser + reachability algorithm.
3. Implement casd registry acquisition and manifest storage.
4. Implement execd materialization (global cache + workspace copy via clonetree),
   with `.checksum` verification for workspace-local copies.
5. Extend vx-manifest dependency parsing to allow versioned deps.
6. Update CrateGraph for registry sources and new CrateId.
7. Update input tracking for `.vx/registry`.
8. Add tests and docs.

---

## Tests

### Unit tests

- Cargo.lock parser: happy path + invalid source + missing checksum.
- Registry manifest: checksum verification + spec_key stability.
- Tarball validation: reject `..`, absolute paths, symlink escapes.

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
