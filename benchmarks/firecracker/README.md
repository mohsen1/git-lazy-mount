# Firecracker benchmark harness

Runs the 20-repo `git lazy-mount` benchmark with **each repo in its own Firecracker
microVM** — real KVM isolation, a `/dev/fuse` guest, one cold VM per repo.

Needs a **Linux host with `/dev/kvm`** (bare metal, or a nested-virtualization VM —
e.g. GCP `--enable-nested-virtualization`, since git-lazy-mount needs FUSE which the
guest kernel must provide).

## Layout
- `bootstrap.sh` — on the host: installs Firecracker, **builds a guest kernel with
  `CONFIG_FUSE_FS`** (the stock Firecracker CI kernel has no FUSE), and builds the
  guest **rootfs** from `benchmarks/Dockerfile` (git-lazy-mount + git + sgrep).
- `run_vm.sh IDX KEY CLONE PROMPT` — boots one microVM: per-VM rootfs + a results
  drive + a TAP/NAT NIC; the guest runs the bench and writes `metrics.json` to the
  results drive, which the host reads after the VM halts. Networking is set via the
  kernel `ip=` boot arg (no iproute2 in the guest).
- `guest_init.sh` — guest PID 1: brings up `/dev/fuse` for the non-root user
  (`fusermount3` setuid + perms), mounts the results drive, runs the bench, halts.
- `startup.sh` — self-contained orchestrator (also usable as a GCP `startup-script`):
  bakes the clone+mount bench into the rootfs and runs all `repos.tsv` sequentially.
- `bench_lazy_fc.sh` — guest-side wrapper for the full agent benchmark.
- `startup_agent.sh` — host-side orchestrator for the full agent benchmark; it
  runs each repo sequentially in a fresh microVM and writes `run/<key>/metrics.json`.
- `repos.tsv` — the 20 `key<TAB>owner/repo<TAB>prompt` rows.
- `make_charts.py` — renders the disk + time SVG bar charts from the metrics.
- `make_agent_charts.py` — renders full-agent wall-clock and post-task disk charts
  from `bench_repo.sh` metrics.

## Run
```bash
sudo bash bootstrap.sh          # ~25 min: kernel build + rootfs
sudo bash startup.sh            # runs all 20, ~6 min; metrics in run/<key>/metrics.json
python3 make_charts.py chartdata.json .   # -> disk.svg, time.svg
```
The setup run measures clone-vs-mount startup only (no agent).

For the full `sgrep`-driven agent task, provide `ANTHROPIC_API_KEY` in the
environment and run:

```bash
sudo -E bash startup_agent.sh
python3 make_agent_charts.py run ../charts
```

Do not archive `run_*.log` from older harness versions that used shell tracing;
the current runner keeps the API key out of logs and removes the guest `job.env`
before copying result artifacts.
