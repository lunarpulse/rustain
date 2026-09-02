---
name: safe-deploy
description: Deploy services following team safety protocols. Use when deploying any service to staging or production.
allowed-tools: Bash(kubectl:*) Bash(helm:*) Read
---
## Protocol
1. Read deploy.yaml for environment config
2. Run helm diff to preview changes — show diff to user
3. Check migrations/ for pending changes — if breaking, STOP and warn
4. If staging: deploy and run smoke tests
5. If production: require explicit user confirmation BEFORE deploying
6. Run smoke tests post-deploy. If any fail, auto-rollback.
Never skip smoke tests. Never force-push to production.
