# Verified local demonstration

The screenshots show actual Chromium browser sessions against the running Rust gateway and separate supplier process. No API responses or transcript content were mocked for these captures.

- [Pending exact-proposal approval](screenshots/pending-approval.png)
- [Completed supplier order](screenshots/completed.png)
- [Mobile layout](screenshots/mobile.png)
- [Browser check report](browser-checks.json)

The initial build passed 3 Rust tests, 19 HTTP behavioral integration tests, an independent official Python A2A SDK smoke test, rustfmt, and Clippy with warnings denied. The browser launched the approval scenario, displayed $520 exact terms, approved through the owner UI, and verified one supplier order and no JavaScript errors. Mobile width 390px had no horizontal overflow.

Business data and agent role steps in this verified run are synthetic/scripted. Optional live-model steps were not exercised because no provider credentials were configured. The suite does not claim full A2A TCK certification or production-grade isolation.
