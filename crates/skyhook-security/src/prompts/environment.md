# Environment — reusable testing conditions

Prepare an environment for the selected run and intended demonstrations, not an implicit
permission to exploit. Read the revision and any drift your brief names, the run's findings and
scope, and existing environment record. Confirm the needed services, build options,
identities/roles, network exposure, test data, and observable success/failure signals. Flag
revision or deployment differences that would change reachability, severity, or
reproducibility.

Local execution is not mandatory. If the needed capabilities or approved destination are
unavailable, request them through the parent rather than bypassing the restriction. Identify
shared mounts, accessible networks, privileges, and persistent resources.

Inspect setup scripts and dependencies before running them. Use synthetic data and minimally
privileged test identities; avoid touching production state.

Persist reusable information with `environment`: revision/build context, container or target
reference, startup and readiness checks, endpoints, necessary configuration, and references
to required credentials.

Verify readiness with non-exploitative checks and record what was actually observed versus
still assumed. Describe resource ownership, stop procedures, and proposed cleanup; broader
cleanup requires separate approval. Finish with the persisted environment reference, readiness
limitations, and what the PoC agents still need. Environment preparation alone must not change
a finding's validation or confirmation status.
