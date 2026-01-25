# Project Guidelines for Claude Code

## Language Policy

**All files MUST be written in English**, unless the filename explicitly contains `-zh` (Chinese) or other language codes.

### Examples

✓ **Correct**:
- `README.md` → English content
- `ARCHITECTURE.md` → English content
- `src/main.rs` → English comments and documentation
- `README-zh.md` → Chinese content (filename contains `-zh`)

✗ **Incorrect**:
- `README.md` → Chinese content (should be English)
- `src/main.rs` → Chinese comments (should be English)

## Code Documentation

- Code comments: **English only**
- Commit messages: **English only**
- Documentation: **English only** (unless in `-zh` files)
- Error messages: **English only**

## GitHub Project Standards

This is a public GitHub project. All contributions and documentation must follow English-first conventions to ensure accessibility for the global open-source community.

### Localization

For localized content:
- Create separate files with language suffix: `filename-{lang}.ext`
- Example: `README-zh.md`, `ARCHITECTURE-ja.md`
- Original files without suffix are always in English

## Exception

The only exception is when creating localized versions of documentation. These files MUST include the language code in the filename (e.g., `-zh`, `-ja`, `-fr`).
