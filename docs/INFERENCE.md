# Local inference

Inference profiles describe an explicit local endpoint, model identity, context/output limits, capabilities and resource ID. Edit the [examples](../examples/inference-config.json) for your deployment; their model/hardware values are illustrative and do not establish installed capability. Conductor does not install models or start a runtime.

```powershell
cargo +1.90.0 build --locked
target/debug/clatterforge.exe capabilities
target/debug/clatterforge.exe inference-create --help
target/debug/clatterforge.exe inference-run --help
```

Preparation stores an immutable request/config snapshot without dispatch. Running explicitly performs bounded inference. Native Ollama discovery checks model identity and rejects reported cloud-backed models. Artifact pins fail closed on mismatch or absence. Compatible local APIs have their own capability limits.

Default endpoints are loopback HTTP. The explicit `endpoint_access: private_ipv4` opt-in additionally permits RFC1918 and carrier-grade private IPv4 literals. Address classification does not authenticate or encrypt the server. Public addresses, non-loopback DNS, credentials, redirects and query/fragment URLs remain rejected.

The 4-MiB serialized request bound is separate from the configured model context window. The host uses a conservative byte-based context estimate, not native token counting, and does not silently trim requests. Native Ollama receives the configured `num_ctx`; choose and benchmark limits for your hardware. Output and generation deadlines are separately bounded.

Once dispatched, generation stays pinned. Partial output, cancellation, timeout and uncertain server abort do not trigger another model automatically. Clients sharing a physical resource must share its admission database/resource identity. Retain quarantine until actual completion or explicit investigated acknowledgement. Model residency alone does not prove idleness.

Direct inference is text plus an optional application-owned pure arithmetic tool. It is distinct from [workflow routing](runbook/workflows.md) and direct scoped coding. No arbitrary shell, file or network tool is granted to a generated tool call.
