# Reviewed source integration

Use source integration after accepting a verified workflow or direct CLI-agent result. It applies that exact candidate to the assigned source checkout as **unstaged changes**. Review and commit them through the repository's normal Git workflow. A model finish or result acceptance alone never applies changes.

## Prepare and apply

In the desktop, select the accepted parent, enter the operator name, and choose **Prepare source integration**. Review the destination checkout, branch/base and every listed create/update/delete. **Apply to source** submits the exact plan digest and revision. The displayed integration status must reach `applied`; task status remains the historical `accepted` result.

```powershell
dev-conductor integration-prepare TASK --by "Your name"
dev-conductor integration-show TASK
dev-conductor integration-apply TASK --revision INTEGRATION_REVISION --digest PLAN_DIGEST --by "Your name"
```

These commands print JSON. `integration-apply` exits nonzero if the recorded outcome needs attention. Repeating an already-applied plan returns its existing receipt without writing again. One immutable integration journal is retained per accepted task; rolled-back work does not automatically replay.

HTTP uses authenticated `POST /api/tasks/{id}/integration/prepare` with `{by}`, then `POST /api/tasks/{id}/integration` with `{revision,digest,by}`. A paired session requires its CSRF header and supplies the real actor principal. Task detail includes optional `integration` status, file hashes/sizes, destination and event receipts. Existing MCP task reads can observe it; MCP submit/resume/result-review grants do not authorize these source writes.

Preparation/application refuse changed repository identity, branch or base, dirty source/index/untracked files, stale or tampered accepted evidence, foreign Git index locks, unsupported index flags, aliased paths and executable-mode changes. This first slice supports 1-16 regular UTF-8 files up to 64 KiB each and requires existing parent directories. Resolve a conflict in a new reviewed task; no automatic three-way merge or overwrite option exists. Unrelated dirty source is preserved and blocks preparation.

## Restore or recover

**Restore original source** reverses an applied integration only while affected source bytes and the Git index/base still match the journal. **Recover original source** handles `applying`, `rolling_back` or `unknown` records after the owner is confirmed dead. A live/unverifiable owner blocks recovery. No model or agent execution is retried.

```powershell
dev-conductor integration-rollback TASK --revision INTEGRATION_REVISION --digest PLAN_DIGEST --by "Your name"
```

The same HTTP endpoint accepts `rollback:true`. Recovery explicitly releases only this integration's prior reservation and only its exact marked index lock. A lease belonging to another task or a foreign lock is preserved. Ordinary write failures attempt rollback automatically; unknown/conflicting bytes or index/base drift stop restoration and quarantine the source lease. Inspect the preserved source and `integration-show` error/events before resolving those conflicts and requesting recovery again. Never delete an index lock or acknowledge a lease merely to force application through.

Raw original bytes remain in the private SQLite integration journal, including CRLF and deleted-file contents. Recovery leaves unrelated files, the Git index and HEAD intact. A successful rollback reports `rolled_back`; an unresolved one reports `unknown`. Historical task acceptance and candidate evidence remain unchanged. Commit/push/deploy are separate repository actions; the integration engine does none of them.

Global stop blocks new application and is checked between files; restoration remains available. Cooperating operations share Conductor's repository lease and an owned Git index lock. Arbitrary editors and other state roots are not sandboxed; checks are not a defense against hostile concurrent filesystem writes. Process-crash recovery is tested separately from power loss or general repository recovery.
