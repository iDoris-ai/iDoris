# Multi-backend local runtime example

This directory is intentionally **not** part of the default `config/components`
set. It is a copy-and-edit example for running iDoris with process-owned
`mlx_lm.server` and `llama.cpp` backends without changing callers or Rust code.

1. Replace every `/ABSOLUTE/PATH/TO/...` value in `local-runtimes.yaml` with a
   trusted local executable/model path. `memory_gb` is the admission footprint
   for that concrete runtime and must be set from the selected model/runtime
   configuration rather than guessed from free system memory.
2. Start iDoris with both files selected:

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
