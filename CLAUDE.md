# Architecture

When planning any meaningful changes, consider @TARGET_ARCHITECTURE.md

# Code style

- Let code speak for itself. Names should convey intent; comments only when they can't.
- This applies to test structure too: `// GIVEN`, `// WHEN`, `// THEN` labels are often enough — don't always elaborate.

# Schema changes

- Update all example workflows in `examples/` and `tests/`.

# Bug Fixing

- When possible, add a test that reproduces the bug before fixing it.

# Documentation

- Always keep @README.md and @CONFIGURATION_REFERENCE.md up to date
