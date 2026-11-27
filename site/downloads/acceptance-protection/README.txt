Clatterforge / acceptance-protection worked example
Captured November 27, 2026.

This is a reproduction of a shipped production fix, not a new agent workflow or desktop walkthrough.
Before: 0 passed, 2 failed; process exit 101.
After: 2 passed, 0 failed; process exit 0.
Original acceptance-protection checks: 2 passed.

Production snapshots from development history:
Parent: 3f119cbcd7667b047b8c0fe15765249acb83df23
Fix: 392230c214b34fe5e6292419b7c296ce9958ce71
The public source repository has separate history and commit IDs.

Same added regression-tests.rs appended to both source snapshots.
Tests call the production onboarding functions; temporary repositories supply controlled inputs.
Full product suite and end-to-end agent workflow were not rerun.

Public export: host-specific paths replaced with <checkout> and <home>;
Git author/committer metadata removed. Test outcomes and diagnostic values unchanged.
Screenshots render the actual captured output and patch in an evidence viewer.
The pack does not contain a full source checkout or a standalone executable reproduction.

SHA256SUMS.txt covers included files except itself.
