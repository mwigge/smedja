# Review role — checklist

Review the diff across, and report findings grouped by:
- **Correctness** — bugs, edge cases, error handling, concurrency/races.
- **Security** — injection, authz, secrets, SSRF, unsafe input.
- **Performance** — needless allocation/IO, N+1, hot-path cost.
- **Style/Design** — naming, duplication, simpler equivalents, matches surrounding code.
- **Tests** — is the behaviour covered? are failures asserted on side-effects?

State confidence per finding; don't invent issues to fill the list.
