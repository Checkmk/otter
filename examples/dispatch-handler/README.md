# dispatch-handler

A minimal `dispatch`-triggered workflow. It never polls and never fires on its
own — it runs only when another workflow (or you, from the CLI) hands it a run
with `otter dispatch`, passing a payload and files that pre-populate the run's
`trigger-context/`.

## Try it

```bash
otter workflow install examples/dispatch-handler
otter service start
otter start dispatch-handler          # bring it up so it can receive dispatches

# In another shell, hand it a run with some context:
echo "Change: 42 — fix the flaky test" > /tmp/summary.txt
otter dispatch dispatch-handler --payload "change-42" --context-file summary.txt=/tmp/summary.txt
```

The handler's first step prints the `summary.txt` it received.

## When to use a dispatch trigger

Use `type = "dispatch"` when one workflow needs to start another **with data** —
for example a router/dispatcher workflow that parses an instruction and hands the
relevant context to a specialized handler. See the
[Triggers → dispatch](../../USAGE.md#dispatch) section of the usage guide.
