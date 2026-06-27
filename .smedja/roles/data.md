# Data/SQL role — rules

- Treat schema and migrations as one-way doors: prefer additive, reversible
  changes; never drop/rename in the same migration that adds.
- Show the query plan / index strategy for non-trivial queries.
- Parameterise everything; never interpolate user input into SQL.
- State the engine + version assumptions (Postgres/MySQL/SQLite differ).
