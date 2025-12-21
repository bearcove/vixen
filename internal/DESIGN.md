# Overview

This project is a build execution engine focused on deterministic caching, fast no-op builds, and first-class introspection.

It is designed to sit between Cargo and full build systems like buck2:

- Cargo remains the source of truth for dependency resolution and project structure.
- This system owns execution, caching, and explanation.

The primary goal is to make no-op builds reliably no-op, across machines and CI environments, while being able to explain every rebuild.

---

## Non-Goals

This project is explicitly not:

- A Cargo replacement
- A new build language (v1)
- A general-purpose monorepo build system
- A remote execution platform (initially)
- A solution that requires rewriting projects

Correctness, explainability, and bounded scope take priority over maximal generality.

---

## Core Concepts

### Authority Model

**Cargo answers:**

> What should be built?

**This system answers:**

> Should this execution happen again, or can we reuse a previous result?

Rebuild decisions are based on explicit cache keys, not implicit filesystem state.

---

### Unit of Work

The fundamental unit is a single rustc invocation (or equivalent compiler action).

Each unit has:

- Explicit inputs
- Explicit outputs
- A deterministic cache key

No "restore target/ and hope" semantics exist in the system.

---

### Workspace vs Cache

- **Workspace:** always ephemeral, disposable, isolated per build
- **Cache:** persistent, content-addressed, shared across builds

No state leaks across builds except through explicit cached artifacts.

---

## Cache Model

### Content-Addressed Storage (CAS)

Artifacts are stored in a CAS keyed by a cryptographic hash of all relevant inputs.

### Cache Key Inputs (v1)

At minimum:

- Compiler identity
- rustc version + commit hash (`rustc -vV`)
- Target triple
- Build profile (debug/release/etc.)
- Enabled features
- Compiler flags
- Source file content hashes
- Dependency artifact hashes
- Allowlisted environment variables
- Build script outputs (hashed)

Keys are:

- Stable
- Inspectable
- Explainable

---

### Cache Entries

Each cache entry contains:

- Produced artifacts (`.rlib`, `.rmeta`, binaries, dep-info, etc.)
- A manifest describing:
  - Inputs used
  - Outputs produced
  - Dependency relationships
  - Metadata for introspection

---

### Cache Trust Model (CI-safe)

- **Untrusted contexts (PRs):**
  - Read-only access to shared cache
- **Trusted contexts (main, release branches):**
  - Read + write access

Execution always happens in isolated workspaces.

---

## Execution Model

### Planning

Planning is delegated to Cargo:

- Dependency graph
- Feature resolution
- Target configuration

This system consumes Cargo metadata and produces an execution plan.

---

### Execution

For each unit:

1. Compute cache key
2. Check CAS
3. If hit:
   - Materialize artifacts
4. If miss:
   - Execute compiler
   - Store artifacts + manifest in CAS

Execution may be:

- Local
- Persistent (daemon-backed)
- Coordinated via RPC (see below)

---

### Escape Hatches

Some crates are pathological (proc-macros, exotic build scripts).

The system supports:

- Falling back to Cargo-managed execution for specific units
- Mixing managed and unmanaged execution in one build

Correctness always wins.

---

## Introspection (First-Class)

Introspection is not a debug feature — it is a core product feature.

The system must be able to answer:

- Why did this unit rebuild?
- Which input changed?
- Which environment variable differed?
- Which dependency invalidated this cache key?
- Why was a cache entry rejected?

This data comes directly from:

- Cache key diffs
- Stored manifests
- Execution metadata

No log archaeology required.

---

## CLI Surface (Conceptual)

Initial commands are intentionally small:

- `build` — execute build with caching
- `test` — compile tests + run
- `explain` — explain rebuilds or cache misses
- `graph` — inspect dependency/execution graph
- `cache` — inspect / prune cache

The CLI is thin; the engine is the product.

---

## Architecture

### Execution Engine

Responsibilities:

- Key computation
- CAS interaction
- Execution orchestration
- Dependency tracking
- Introspection data production

---

### RPC Layer (rapace)

Used for:

- Persistent daemon mode
- Local/remote parity
- CI integration
- Future distributed execution

This avoids embedding orchestration logic into the CLI.

---

### Graph Model (picante)

Used to model:

- Execution units
- Dependencies
- Invalidation propagation
- Explanation paths

The graph is explicit, queryable, and introspectable.

---

### Introspection UI (facet)

Facet consumes:

- Execution metadata
- Cache manifests
- Graph data

To provide:

- Human-readable explanations
- CI-friendly summaries
- Local inspection tooling

---

## Phased Implementation Plan

### Phase 1 — Observability Tooling

- Surface Cargo rebuild reasons
- Parse fingerprint logs
- Attribute rebuilds to concrete causes

Delivers immediate value on its own.

---

### Phase 2 — CAS + Artifact Mapping

- Implement CAS
- Define manifest format
- Map rustc outputs to cache entries
- Integrate read-only cache restore around Cargo

This replaces GitHub artifact caching entirely.

---

### Phase 3 — Managed Execution

- Own execution for check-like compilation
- Then linking
- Then test compilation

Cargo remains the planner throughout.

---

## Success Criteria

This project is successful if:

- No-op builds are reliably no-op
- CI time is dominated by actual work, not rebuild noise
- Every rebuild can be explained concretely
- The system remains smaller and simpler than buck2
- Cargo projects require minimal or zero modification

---

## Design Constraints (Hard Rules)

- No new build language in v1
- Cargo is an input, not an enemy
- Cache keys must be explainable
- Introspection is mandatory
- Escape hatches are required
- Workspace state must never be trusted
