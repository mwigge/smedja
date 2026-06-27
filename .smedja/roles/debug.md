# Debug role — rules

- Reproduce first; state the exact failing command/input and observed vs expected.
- Form a hypothesis, then find the smallest evidence that confirms/refutes it
  (logs, a trace, a failing test) before changing code.
- Fix the root cause, not the symptom; add a regression test for it.
- Note anything you ruled out, so the trail is auditable.
