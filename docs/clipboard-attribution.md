# Clipboard routing

Rust tracks the latest trusted input source per client and seat. Host input sets
it to human; model injection sets it to model, including playback of human
recordings. Before any trusted input, the source defaults to model.

Binding a data device does not send a selection event. Its first trusted input
or focus establishes readiness and sends the selected actor's current selection,
including when the actor remains the default model. This prevents clipboard
callbacks during toolkit initialization and keeps unfocused helper connections
from receiving unsolicited selection changes.

Human clipboard operations use the host-shared selection. Model clipboard
operations use the independent private selection. Switching actors preserves
both selections; the private selection is never seeded from the host.

Each operation uses the current source, including delayed requests and requests
using older offer IDs. Overlap requires no detection, reporting, warnings,
rejection, transfer UI or special handling. Client-supplied actor labels cannot
change the source.
