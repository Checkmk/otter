# Architecture

When planning any meaningful changes, consider @TARGET_ARCHITECTURE.md

# Code style

- Let code speak for itself. Names should convey intent; comments only when they can't.
- This applies to test structure too: `// GIVEN`, `// WHEN`, `// THEN` labels are often enough — don't always elaborate.

# Tests

- Use `InMemoryStorage` (in `orchestr8r-core::storage`) for engine tests — no filesystem.
