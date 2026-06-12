# Security Notes for `rustain ask`

## User-provided file context trust boundary

Files attached via the CLI `--file` flag or piped on `stdin` are treated as
**user-provided** context (`FileContextProvenance::UserProvided`). They are read
using the OS filesystem permissions of the rustain process and injected into the
user message sent to the model.

Because the user explicitly named these paths, they intentionally bypass the
workspace-boundary check that gates model-suggested file paths (e.g. `Read`
tool arguments). The blocklist and path-traversal checks still apply.

This distinction is enforced in the `SecurityPort::check_workspace_access_with_provenance`
default and in `SecurityAdapter::validate_path`. Call sites that build
`ResolvedFileContext` from user input must tag the provenance as
`FileContextProvenance::UserProvided`; paths produced by model tool calls must
remain `FileContextProvenance::ModelSuggested` so the workspace boundary
continues to gate them.
