# Architecture overview

The initial repository had no application code. The current workspace contains the `everything` host binary and `everything-relay` binary, each with command-line help only. Library modules are introduced through the dependency gates in [status](../status/implementation.md).

The authoritative requirements are GitHub issues #1–#31, especially roadmap #30. The intended dependency direction is clients → host commands → native runtime → providers and broker → storage. This is a design constraint, not a claim that these components already exist.
