# Release Process

This project uses automated releases triggered by conventional commits. When you merge a PR to `main`, the automated release workflow will:

1. Analyze commit messages for conventional commit patterns
2. Determine semantic version bump (major, minor, patch)
3. Update version in `Cargo.toml` and `src/lib.rs`
4. Generate/update `CHANGELOG.md`
5. Create a release commit and tag
6. Build binaries for all platforms
7. Create a GitHub release with changelog

## Conventional Commits

Commit messages should follow the [Conventional Commits](https://www.conventionalcommits.org/) specification:

### Triggering a Patch Release (e.g., 0.12.0 → 0.12.1)
```
fix: description of the bug fix
```

### Triggering a Minor Release (e.g., 0.12.0 → 0.13.0)
```
feat: description of new feature
```

### Triggering a Major Release (e.g., 0.12.0 → 1.0.0)
```
feat: description of breaking feature

BREAKING CHANGE: description of what changed
```

## How Releases Work

1. **Push to main**: Merging a PR with conventional commits to `main` automatically triggers the `auto-release.yml` workflow
2. **Version bump**: The workflow calculates the next semantic version based on commit types
3. **Files updated**: 
   - `Cargo.toml` - package version
   - `src/lib.rs` - VERSION constant
   - `CHANGELOG.md` - generated with git-cliff
4. **Tag created**: A tag like `v0.13.0` is created and pushed
5. **Build triggered**: The `release.yml` workflow builds binaries for all platforms
6. **Release published**: GitHub release is created with:
   - All platform binaries
   - SHA-256 checksums
   - Auto-generated changelog

## Example Workflow

### Making a feature release:
```bash
git checkout -b feature/new-functionality
# ... make changes ...
git commit -m "feat: add new filtering capability"
git push
# Create PR and merge to main
# ↓
# auto-release.yml runs automatically
# ↓ 
# Version updated to 0.13.0
# Binaries built
# Release published on GitHub
```

## Skipping Release

If a commit to `main` should not trigger a release:
- Commits to `CHANGELOG.md`, `Cargo.toml`, or `src/lib.rs` that are from the release workflow are automatically skipped
- Use `chore:` prefix for maintenance commits that aren't releases

## Manual Release (if needed)

You can manually trigger a release:
```bash
# Edit Cargo.toml and src/lib.rs with new version
# Update CHANGELOG.md
# Commit and push
git tag -a v0.13.0 -m "Release v0.13.0"
git push origin v0.13.0
# release.yml workflow will be triggered automatically
```
