# SigNoz for Smedja observability

Self-contained SigNoz all-in-one for inspecting what smdjad (the Smedja
daemon) emits over OpenTelemetry: traces, metrics, and trace-correlated logs.

## Quickstart

```sh
cd docker/signoz
docker compose up -d
# wait for healthy, then open the UI and create the first account
# (that account becomes the root/admin user — nothing is pre-provisioned)
xdg-open http://localhost:8080
```

Ports used: `8080` (UI + API), `4317` (OTLP gRPC), `4318` (OTLP HTTP).

## Point smdjad at it

The daemon activates its OTLP pipelines when `SMEDJA_OTLP_ENDPOINT` is set.
With the systemd user service, use a drop-in:

```ini
# ~/.config/systemd/user/smdjad.service.d/otel.conf
[Service]
Environment=SMEDJA_OTLP_ENDPOINT=http://localhost:4318
```

```sh
systemctl --user daemon-reload
systemctl --user restart smdjad
```

Unset the variable (or remove the drop-in) to disable export — smdjad then
runs with no-op providers and only its structured logs.

## Dashboards

Three pre-built dashboards live in `dashboards/`:

| File | Contents |
|---|---|
| `overview.json` | RPC throughput by method, health checks, turns by tier, turn latency p50/p95/p99, log volume by severity |
| `runners-models.json` | Turns by model/runner, token usage (`gen_ai.*`), tool executions, LLM calls and TTFT |
| `loops-logs-traces.json` | Loop tokens/slices/escalations, compression savings, live WARN/ERROR tail, recent turns |

Import them (after creating your account in the UI):

```sh
./import-dashboards.sh you@example.com yourpassword
# if the build asks for an orgID:
./import-dashboards.sh you@example.com yourpassword http://localhost:8080 <org-id>
```

Dashboard JSONs are plain SigNoz dashboard models — no users, tokens, or
instance ids. They can equally be imported via the UI
(Dashboards → New Dashboard → Import JSON).

## Notes

- `gen_ai.client.token.usage`, `smedja.llm.chat` spans and TTFT are only
  emitted by the anthropic/openai **HTTP adapters**. CLI runners
  (claude-cli, codex-cli, …) emit `smedja.agent.invoke` turn spans and
  `smedja.tool.execute` spans instead.
- Data lands in named docker volumes (`smedja-signoz_clickhouse`,
  `smedja-signoz_sqlite`, `smedja-signoz_zookeeper-1`). `docker compose down -v`
  wipes everything including your account.
- `SIGNOZ_JWT_SECRET` defaults to a local-dev value; set it for anything
  reachable by others. The UI has no authentication until you create the
  first account.
