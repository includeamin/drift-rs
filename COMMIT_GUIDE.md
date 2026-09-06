# Commit Message Guide

This project uses [Conventional Commits](https://www.conventionalcommits.org/) to automate versioning and changelog generation.

## Format

```
type(scope): subject

body

footer
```

## Types

| Type | Version Impact | Example |
|------|---|---|
| `feat` | Minor (minor bump) | `feat: add filtering by field name` |
| `fix` | Patch (patch bump) | `fix: handle null values in arrays` |
| `docs` | None | `docs: update API documentation` |
| `style` | None | `style: format code` |
| `refactor` | None | `refactor: simplify pointer logic` |
| `test` | None | `test: add tests for edge cases` |
| `perf` | None | `perf: optimize diff algorithm` |
| `chore` | None | `chore: update dependencies` |
| `ci` | None | `ci: update GitHub Actions` |

## Examples

### Patch Release (0.12.0 → 0.12.1)
```
fix: handle empty array in pointer splitting

Previously, split_pointer("/") would panic instead of
returning a proper error. Now it returns the error
DriftError::Pointer with appropriate message.
```

### Minor Release (0.12.0 → 0.13.0)
```
feat: add max-depth option to paths command

Users can now limit path listing depth with --max-depth option
to avoid traversing deeply nested documents.
```

### Major Release (0.12.0 → 1.0.0)
```
feat: redesign API for better performance

BREAKING CHANGE: The `diff` function signature has changed.
It now takes references instead of owned values.
See MIGRATION.md for migration guide.
```

## Tips

- Be specific and descriptive
- Use imperative mood ("add" not "added")
- Keep subject under 50 characters
- Separate subject from body with blank line
- Wrap body at 72 characters
- Include issue references: `fixes #123` or `Refs #456`

## Checking Your Commit

Before pushing, verify your commit follows the format:
```bash
git log -1 --pretty=%B | head -1
# Should start with: feat:, fix:, docs:, etc.
```

## Automatic Versioning

When you merge a PR to `main`:
- **All conventional commits detected** → Version bump triggered
- **No conventional commits** → No release (skipped)
- **First release ever** → Determines bump from all commits since repo start

The GitHub Actions workflow will automatically:
1. Detect conventional commits
2. Determine new version (semantic versioning)
3. Update `Cargo.toml` and `src/lib.rs`
4. Generate `CHANGELOG.md` entry
5. Create release tag and commit
6. Build all platform binaries
7. Publish GitHub release

## Revert Commits

Use `revert:` for reverting commits:
```
revert: add max-depth option

This reverts commit abc1234def56789.
Reason: Feature introduced a performance regression.
```

## Merge Commits

GitHub's squash-and-merge is recommended. The final commit should follow the convention:
```
feat: implement multiple commits as single feature
```

Rather than a merge commit message.
