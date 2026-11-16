# Agentic inference and fine-tuning workload study

A benchmark of where VM time goes when one VM serves an AI agent (vLLM
plus CPU tools) and another fine-tunes a model, each with a GPU passed
through. Results, setup and how to run it:
[Performance and benchmarks](../../../docs/pylon/src/developer-guide/benchmarks.md#agentic-inference-and-fine-tuning-on-gpus-measured-2026-11-15).

| File | Runs on | Does |
|---|---|---|
| `mkimage.sh` | host (root) | Builds `img/base.raw`, the guest image |
| `mkmodels.sh` | host | Builds `img/models.raw`, the models and dataset |
| `scenario.sh` | host (root) | Runs one scenario: boots the VMs, collects results |
| `matrix.sh` | host (root) | Runs scenarios R1–R7 |
| `hostsample.py` | host | Samples each VMM thread's CPU time and run delay |
| `analyze.py` | anywhere | Summarises `out/<run>` as JSON |
| `job/run.sh` | guest | Entry point at boot: runs the role in `params.json` |
| `job/agent.py` | guest | vLLM and the agent sessions |
| `job/train.py` | guest | LoRA fine-tuning |
| `job/sampler.py` | guest | Samples CPU, run-queue wait, GPU and processes |
| `job/pandas_tool.py` | guest | The agent's data-analysis tool |
