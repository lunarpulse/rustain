# rustain Product Requirements Document

## Platform Support Matrix

| Platform | Tier | Coverage | Daemon Supervision |
|---|---|---|---|
| Linux (ubuntu-latest) | Tier 1 | TUI + daemon, CI-proven | systemd — CI-proven via `nightly-daemon.yml` |
| macOS (macos-latest) | Tier 2 | Builds + tests proven by `ci.yml` macos-unit job | launchd — ships as-is, documented UNVERIFIED (descoped if runner blocks bootstrap); re-scope trigger: first macOS daemon user report or CI runner that permits LaunchAgent bootstrap |

> NFR50: Daemon auto-restarts on crash via systemd with crash state persisted — CI-proven on Linux. macOS launchd supervision is out of scope: unit files ship as-is, documented UNVERIFIED, no auto-restart claim. Re-scope trigger: first macOS daemon user report, or a CI runner environment that permits launchd user-agent bootstrap.
