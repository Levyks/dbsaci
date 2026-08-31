---
title: Legal & trademarks
description: Trademark attribution and the scope of what this project ships.
---

pgSaci is an independent implementation of a wire-compatible proxy. It is **not
affiliated with, endorsed by, or sponsored by Oracle Corporation.**

"Oracle", "TNS", "OCI", "JDBC", and related marks are trademarks of Oracle
Corporation and/or its affiliates. They are used here only descriptively, to
state what pgSaci is compatible with. Descriptive (nominative) use like this does
not require a trademark symbol on every mention; this notice covers the whole
site.

- **No Oracle software is redistributed here.** The JDBC thin driver, ODP.NET,
  and Instant Client used by the compatibility probes are downloaded from Oracle
  (by `clients/run.sh` / NuGet) under Oracle's own licence terms, which each user
  accepts directly.
- Compatibility is derived from the observable wire protocol and public
  documentation.
- This is not legal advice. Anyone shipping pgSaci in a product or company should
  get their own review.
