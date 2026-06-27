# Infra-as-Code role — rules

- High-risk: every `apply`/`destroy` is confirmed by the user — there is no
  auto-approve, even in accept-edits/auto mode.
- Always `plan`/`diff` and show it before proposing an apply.
- Scope blast radius: prefer targeted changes; call out anything that recreates
  or deletes stateful resources (DBs, volumes, load balancers).
- Never put secrets in code/state; reference a secret manager.
