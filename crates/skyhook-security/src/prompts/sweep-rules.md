# Sweep rules

Walk the path's steps through the components they enter and record:

- each **flaw** at its location and component, its body the invariant involved, how it fails
  and every possible effect, with its CWE when one fits;
- the **premises** each flaw needs (`flaw_premise`) and those the path's flow needs to carry
  input to it (`path_premise`): the state or logic without which the flaw cannot reach
  unintended state, each one claim at a location;
- **evidence** at the locations that support or contradict them;
- a **candidate** whenever flaws chain along this path to an observable vulnerability.

Look for memory bounds and lifetimes, integer and type assumptions, error and status contracts,
state and ordering mistakes, failure paths that skip cleanup or progress, authorization and
injection, resource limits, and any other invariant the code relies on. A dangerous-looking
operation alone is not a flaw, and a check the contract leaves to the caller is not a flaw in
the callee: a caller that skips it is.

A status that asks its caller to retry or supply more input is a contract: for each site
returning it, and each mapping a false or 0 to it, record as evidence what the status means
there ("returned only when the input ran out"), and for a fixed-size buffer or counter carried
across attempts, the per-attempt bound and the progress each attempt makes; a site where the
status can also mean malformed input is a flaw. Never infer a bound only from the largest valid
operation. When a flaw changes what a status, error or state means, follow that value to every
immediate caller, retry loop and stateful consumer: each one relying on the usual meaning is a
flaw that needs the changed meaning, linked as its premise or chained after it.

Correct the path's steps and the components' files as you find them. Before judging code safe,
look for a counterexample, including error, malformed-input and retry paths.
