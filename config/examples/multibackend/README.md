# Multi-backend local runtime example

This directory is intentionally **not** part of the default `config/components`
set. It is a copy-and-edit example for running iDoris with process-owned
`mlx_lm.server` and `llama.cpp` backends without changing callers or Rust code.

1. Replace the executable and model placeholders in `local-runtimes.yaml` with
   trusted absolute local paths. `memory_gb` is the admission footprint for that
   concrete runtime and must be set from the selected model/runtime
   configuration rather than guessed from free system memory.
2. Create a private, persistent state directory owned by the user running
   iDoris:

   ```sh
   state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/idoris"
   umask 077
   mkdir -p "$state_dir/fences"
   chmod 700 "$state_dir" "$state_dir/fences"
   ```

   Replace `/ABSOLUTE/PATH/TO/PRIVATE-IDORIS-STATE` in both `load_fence_path`
   values with the expanded absolute `state_dir` path. Keep the two fence
   filenames stable across restarts so pending-load protection remains durable.
3. Start iDoris with both files selected:

   ```sh
   IDORIS_COMPONENTS_DIR=config/examples/multibackend/components \
   IDORIS_LOCAL_RUNTIME_CONFIG=config/examples/multibackend/local-runtimes.yaml \
   idoris
   ```

The component-card endpoint and runtime `port` must match. iDoris launches both
engines bound to `127.0.0.1`; executable and model paths never live in portable
component cards. A request still uses the same iDoris API surface: backend
selection is a routing/runtime concern, not a caller protocol change.

The sample memory values are illustrative only. They are deliberately explicit
because global capacity admission is only safe when each process-owned runtime
has a trustworthy positive footprint.

## vLLM

`vllm-local-runtime.yaml` is a separate example. It does not replace the
mlx and llama files above. Loading this file does not mean Linux CUDA was
tested. A Mac process test only shows that the adapter can spawn, probe,
chat, and stop.

Replace `engine_version` with the exact `x.y.z` build of the `vllm`
executable you installed. iDoris records that pin and does not accept a
range. It does not run the binary to prove the version.

`model_path` in the sample is an absolute local artifact. vLLM is not asked
to download it, and the argv has no `--revision`. To use a Hugging Face repo
id instead, set `model_path` to `org/name` and set `model_revision` to the
40-character lowercase commit. vLLM downloads that commit on first start
unless the Hugging Face cache already has it. A repo id is not an offline
launch.

`memory_gb` is the GPU reserve for that one process. `gpu_total_gib` is the
size of the device you measured. iDoris passes
`--gpu-memory-utilization` as `memory_gb` divided by `gpu_total_gib`. `memory_gb` is
not a second number that only the admission ledger sees. `memory_gb` must
be greater than 0 and no larger than `gpu_total_gib`.

One process serves one model. Do not add tensor parallel or sleep flags.
If the runtime entry has `auth`, iDoris sends that same bearer on
`/v1/models` and chat, and it passes the secret to vLLM as `--api-key`.
There is no second key field. Do not write the secret into this file.
