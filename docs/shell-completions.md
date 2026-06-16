# Shell Completions

`rustain completions <shell>` generates a completion script and prints it to stdout.
Pipe the output into your shell's completions directory for tab-completion of
subcommands, flags, and arguments.

Supported shells: **bash**, **zsh**, **fish**, **powershell**.

## Per-shell install instructions

### Bash

```sh
rustain completions bash > ~/.local/share/bash-completion/completions/rustain
```

Or for system-wide:

```sh
rustain completions bash | sudo tee /etc/bash_completion.d/rustain > /dev/null
```

### Zsh

```sh
rustain completions zsh > ~/.zfunc/_rustain
```

Ensure `~/.zfunc` is in your `fpath` (add `fpath=(~/.zfunc $fpath)` **before**
`compinit` in `~/.zshrc`):

```zsh
# ~/.zshrc
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```

### Fish

```sh
rustain completions fish > ~/.config/fish/completions/rustain.fish
```

### PowerShell

```powershell
# Avoid duplicate registration by checking first:
$registerLine = 'rustain completions powershell | Out-String | Invoke-Expression'
if (-not (Test-Path $PROFILE) -or (Get-Content $PROFILE -Raw) -notmatch [regex]::Escape($registerLine)) {
    Add-Content -Path $PROFILE -Value $registerLine
}
```
Or, more robustly, write to a separate file sourced from your profile:

```powershell
rustain completions powershell > "$HOME\Documents\PowerShell\Completions\rustain.ps1"
# Then add to $PROFILE: . "$HOME\Documents\PowerShell\Completions\rustain.ps1"
```
## Staleness

Completions reflect the subcommands compiled into **your installed binary**.
After upgrading rustain (or changing cargo features), re-run
`rustain completions <shell>` to regenerate the script. There is no
auto-update mechanism — the script is a point-in-time snapshot.

## `--bin-name` override

If your distribution or wrapper invokes rustain under a different name, pass
`--bin-name` so the completion script matches the invoked command name:

```sh
rustain completions bash --bin-name my-rustain > ~/.local/share/bash-completion/completions/my-rustain
```

Completion scripts install into a path keyed by the invoked command name; a
mismatched name silently fails to trigger.

## Notes

- The command performs no network call, no config load, no filesystem write.
- Output is clean: no prompts, no ANSI escapes, no log lines on stdout.
- Piping through `head` or other early-closing readers exits cleanly (no panic).
