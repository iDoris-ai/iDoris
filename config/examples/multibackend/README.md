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
