# Clipboard routing

Rust tracks the latest trusted input source per client and seat. Host input sets
it to human; model injection sets it to model, including playback of human
recordings. Before any trusted input, the source defaults to model.

Human clipboard operations use the host-shared selection. Model clipboard
operations use the independent private selection. Switching actors preserves
both selections; the private selection is never seeded from the host.

Each operation uses the current source, including delayed requests and requests
using older offer IDs. Overlap requires no detection, reporting, warnings,
rejection, transfer UI or special handling. Client-supplied actor labels cannot
change the source.
