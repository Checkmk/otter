# Code style

- Let code speak for itself. Names should convey intent; comments only when they can't.
- Applies to test structure, too: `// GIVEN`, `// WHEN`, `// THEN` labels are often enough.

# Testing

- Write tests for new features.
- For bug fixes, write a test that reproduces the bug, check that it fails, then fix the bug.
- For complex features, integration tests are also encouraged.
- Use GIVEN, WHEN, THEN structure

# Changing user behavior

- Keep @README.md and @USAGE.md up to date.

# Changing schemas

- Keep example workflows up to date in `examples/` and `tests/`.

# Architecture

- Consider @TARGET_ARCHITECTURE.md when planning.
