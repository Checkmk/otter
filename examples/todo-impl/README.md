# todo-impl

Turn a local `TODO.md` into an implementation queue. otter polls the file, and
for every new top-level task it spins up an isolated git worktree, hands the task
to Claude to implement and commit on its own branch, and (optionally) pushes it.

## Task markers

| Marker     | Meaning                                              |
| ---------- | --------------------------------------------------- |
| *(none)*   | pending — eligible to run                            |
| `(DRAFT)`  | you're still writing it; skipped until you remove it |
| `(LOCK)`   | a run is in progress                                 |
| `(DONE)`   | implemented successfully                             |
| `(FAIL)`   | the run failed or was stopped                        |

## Example `TODO.md`

```markdown
# Add a /healthz endpoint

Add an HTTP endpoint at `/healthz` that returns 200 with body `ok`.

# (DRAFT) Rework the config loader

Not ready to hand off yet.
```
