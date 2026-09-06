# Automated Release Infrastructure

This project now has a fully automated release system that triggers on conventional commits.

## How It Works

### 1. **Automated Release Workflow** (`.github/workflows/auto-release.yml`)
   - Triggers on every push to `main` branch
   - Analyzes commit messages using Conventional Commits specification
   - Automatically determines semantic version bump:
     - `fix:` → patch version (0.12.0 → 0.12.1)
     - `feat:` → minor version (0.12.0 → 0.13.0)
     - `BREAKING CHANGE:` → major version (0.12.0 → 1.0.0)
   - Updates:
     - `Cargo.toml` version field
     - `src/lib.rs` VERSION constant
     - `CHANGELOG.md` (auto-generated via git-cliff)
   - Creates a release commit with tag (e.g., `v0.13.0`)
   - Pushes the tag to trigger the build workflow

### 2. **Binary Build Workflow** (`.github/workflows/release.yml`)
   - Automatically triggered when a tag matching `v*` is pushed
   - Builds binaries for:
     - Linux x86_64
     - Windows x86_64
     - macOS Intel (x86_64)
     - macOS Apple Silicon (aarch64)
   - Generates SHA-256 checksums for each binary
   - Creates GitHub release with all artifacts and auto-generated notes

### 3. **Test & Coverage Workflow** (`.github/workflows/test-coverage.yml`)
   - Runs on every push and PR
   - Executes full test suite (62 tests)
   - Generates code coverage reports
   - Uploads to Codecov for PR comments

### 4. **Code Quality Workflow** (`.github/workflows/quality.yaml`)
   - Linting and code quality checks

## Example Release Flow

```
Developer pushes commits to feature branch
         ↓
Developer creates PR with conventional commits
  - "feat: add new filtering capability"
  - "fix: handle edge case in array operations"
         ↓
PR is reviewed and merged to main
         ↓
auto-release.yml triggered
  - Detects "feat:" commit → Minor version bump
  - Version: 0.12.0 → 0.13.0
  - Updates Cargo.toml, src/lib.rs, CHANGELOG.md
  - Creates commit & tag: v0.13.0
  - Pushes tag
         ↓
release.yml triggered by tag
  - Builds all 4 platform binaries
  - Creates GitHub Release with:
    - All binaries + checksums
    - Auto-generated changelog
    - Release notes
         ↓
Release is published!
```

## Configuration

### Conventional Commits Recognition
The workflow recognizes these commit types:
- `feat:` - New features (minor version bump)
- `fix:` - Bug fixes (patch version bump)
- `feat:` with `BREAKING CHANGE:` - Breaking changes (major version bump)
- `BREAKING CHANGE:` in body - Breaking changes

### Changelog Generation
- Uses `git-cliff` (configuration in `cliff.toml`)
- Automatically groups commits by type (Features, Bug Fixes, etc.)
- Links to GitHub issues with patterns like `#123`

### GitHub Requirements
The workflows require these GitHub permissions (already configured):
- `contents: write` - To create commits, tags, and releases
- Token permissions for git operations

## Files Changed

1. **New Files:**
   - `.github/workflows/auto-release.yml` - Automated version bumping and tag creation
   - `RELEASING.md` - Release documentation for contributors
   - `codecov.yml` - Code coverage configuration

2. **Modified Files:**
   - `Cargo.toml` - Version synced to 0.12.0
   - `cliff.toml` - Repository URL updated to drift-rs
   - `README.md` - Added testing & coverage information

## Manual Release (if needed)

For emergency releases without conventional commits:
```bash
# Manually edit version
sed -i 's/0.12.0/0.13.0/g' Cargo.toml src/lib.rs

# Generate changelog
git-cliff --config cliff.toml --output CHANGELOG.md

# Create release
git add Cargo.toml src/lib.rs CHANGELOG.md
git commit -m "chore(release): v0.13.0"
git tag -a v0.13.0 -m "Release v0.13.0"
git push origin main v0.13.0
```

## Next Steps

1. ✅ Infrastructure is ready
2. Use conventional commits in your PRs:
   - `feat:` for new features
   - `fix:` for bug fixes
   - Add `BREAKING CHANGE:` to commit body for major releases
3. Merge to main with conventional commits
4. Auto-release will trigger automatically
5. Monitor the GitHub Actions workflow
6. Check the Release page for new release

## Troubleshooting

- **Workflow doesn't trigger**: Ensure commits have `feat:`, `fix:`, or `BREAKING CHANGE:` prefix
- **Version not updated**: Check workflow logs in GitHub Actions
- **Build fails**: Review test results in test-coverage.yml workflow
- **Manual override needed**: Use GitHub's `workflow_dispatch` or manually push a tag
