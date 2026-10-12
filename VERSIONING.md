# Version management

## Authorities and release units

Existing version values are retained. A value in a manifest is development/package metadata, not evidence that a release was published. Each unit is selected explicitly; matching numbers do not couple unrelated units.

- `desktop`: `0.1.0` from `package.json`; 5 precisely bound mirror fields.
- `server`: `0.1.0` from `server/Cargo.toml`; 1 precisely bound mirror fields.

## Safe local workflow

Requires Python 3.11+ (standard library only). From the repository root:

```sh
python scripts/version_manager.py show
python scripts/version_manager.py check
python scripts/test_versioning.py
python tests/test_version_manager.py
```

Preview one unit first; `bump`/`set` without `--apply` never write files:

```sh
python scripts/version_manager.py bump patch --unit desktop
python scripts/version_manager.py bump minor --unit desktop
```

For an intentional update, append `--expect CURRENT_VERSION --apply` to the reviewed command. Use `set NEW_VERSION --unit UNIT` for an explicit version or `bump prerelease --preid rc --unit UNIT` where the native format supports prereleases. `set` to the current version is a no-op. The helper refuses missing, ambiguous, mismatched, unsafe, or stale sources before writing. It does not invoke package managers, lifecycle hooks, Git, tags, publishing, deployment, signing or network access.

Only allowlisted own-version string tokens change. Dependency/SDK versions, lock resolution/integrity data, generated outputs and schemas are never rewritten. A mismatch should be investigated against the canonical authority before a reviewed correction; automatic guessing is intentionally refused. Review the complete diff, update the Unreleased changelog, run unit-specific native tests/builds and installer validation, then separately follow the project release process. Native installer/platform restrictions still apply; a metadata check is not a full application build.

## Preserved boundaries

The server and desktop are separate units. Storage protocol and API schema version identifiers are not application versions.

## Adoption evidence and validation

Adoption reviewed the current default `main` at `e5816d02a7aae26226c5c24701a99ab9afad196a` and used `fix/macos-runtime-storage` at `6302c2be20231956110558f5f931e5981f00cda4` as the parent. Repair bases include the current default; earlier refs remain untouched.

GitHub tags/releases and the existing central R-Box-Releases catalog were inspected on 2026-10-12. No verified central application release mapping was found for this repository; no assumption about unpublished or external releases is made. Existing source versions are authoritative for this change. No version bump or release is part of adoption.

Offline tests cover check, CLI dry-run, no-op, mismatch refusal, stale expected versions, unit isolation and exact unrelated-byte preservation. Native/full-application builds are separate and were not run as part of this metadata-only adoption. The new CI workflow is read-only (`contents: read`) and cannot publish a release.
