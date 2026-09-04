# DeepSWE benchmark

This project owns the reproducible DeepSWE evaluation workflow for Zork. It is
kept outside `scripts/` because it has its own runtime dependencies, CLI,
resources, agent adapters, result comparison, and tests.

Run commands from the repository root:

```bash
uv sync --project benchmarks/deep-swe
uv run --project benchmarks/deep-swe zork-deep-swe --help
uv run --project benchmarks/deep-swe zork-deep-swe run <task-id>
uv run --project benchmarks/deep-swe python -m unittest discover \
  -s benchmarks/deep-swe/tests
```

`run` validates the fixed seed-0 task subset, builds a fresh `zork-agent` Linux
binary for the native host architecture by default (override with `--platform`)
unless an explicit binary is supplied, checks out the pinned
DeepSWE dataset revision, pulls the selected task images, and starts Pier. Its
defaults can be inspected with `zork-deep-swe run --help`; command-line options
replace the former `ZORK_DEEPSWE_*` shell environment contract.

Containers stay running after completion, failure, or interruption for inspection;
the `run` command uses `zork_deepswe.environment:RetainedDockerEnvironment` and
never automatically stops or deletes them. Stop them explicitly when finished.

Other subcommands expose the reusable stages independently:

- `profile`: build a private benchmark profile from local credentials.
- `prepare`: pull the unique Docker images needed by a task sample.
- `compare`: compare Pier results with an official trial artifact.
- `responses-probe`: forward a Responses stream while recording protocol-only
  metadata.

Pier agent import paths are:

- `zork_deepswe.agents.zork:ZorkDeepSweAgent`
- `zork_deepswe.agents.pi:PiDeepSweAgent`
- `zork_deepswe.agents.pi_qwen:PiQwenDeepSweAgent`
- `zork_deepswe.agents.codex:CodexSubscriptionDeepSweAgent`

Generated datasets stay in `.data/benchmarks/deep-swe`, binaries in
`target/deepswe`, and job results in `artifacts/deepswe`; none belongs to this
project's source tree.
