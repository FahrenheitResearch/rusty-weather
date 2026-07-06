# rusty-fire-weather

**Read `docs/AGENT_GUIDE.md` first** — it is the single entry point for any
agent: project map, hard rules, deploy runbook, gotcha ledger, current state.
For server work, `docs/HETZNER_OPS.md` is the operational runbook.

Non-negotiables (full list + rationale in the guide):
- Do NOT touch the Hetzner server without Drew's explicit approval
  (read-only inspection is fine).
- `.rws`/rw-store is the canonical backend — never WxStore. Never fake fuel
  data. Never commit `outputs/`.
- Card SVGs: plain `<text>` only — no `<tspan>`/`xml:space` (breaks browser
  Copy Image).
- Users never see `cafire.wxsection.com` — products say cafire.org/weather.
- Commit, then PUSH (`origin`), then deploy via `git archive` — never scp
  working files to the server.
- Verify with tests + real proof renders you look at + the public URL.
  `cargo test`: two pre-existing rw-ingest `size_estimate` failures are
  known and not yours.
