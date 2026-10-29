# Sweep rules

Record through `research`:

- each **flaw**: a local defect or violated contract, its operation, the guard that fails and
  how, its possible effects, located evidence, and `misreports` when it makes its code report
  a status, error or state for the wrong reason;
- each **guarantee**: an invariant you argue the code enforces, with its kind (bounds,
  progress, status contract, lifetime, authorization, state or other), the function it is
  argued in, the preconditions the argument rests on, and evidence;
- each bound, lifetime, progress or status contract carried across calls that the code does
  not establish, as a contract check (`add_check`, kind `bounds`, `lifetime`, `progress` or
  `status_contract`);
- each region you did not examine, deferred with the reason.

Look for memory bounds and lifetimes, integer and type assumptions, error and status contracts,
state and ordering mistakes, failure paths that skip cleanup or progress, authorization and
injection, resource limits, and any other invariant the code relies on. A dangerous-looking
operation alone is not a flaw, and a check the contract leaves to the caller is a
precondition, not a flaw. Record every possible effect a flaw has, not only the most likely one.

For every status that asks the caller to retry or supply more input, record each site in the
code you cover that returns it, and each that maps a function's false or 0 to it, with
`research` `retry_site`, and classify it: `input_exhausted` only when every way there means the
input ran out, `invalid` when one means malformed input, `other` otherwise. A sweep does not
complete while a site is unclassified.
For a fixed-size buffer or counter carried across attempts, establish the bound per attempt and
that attempts make progress; such a guarantee names the status in `retry_status` and cites every
site of it. Never infer a bound only from the largest valid operation.

When a flaw changes what a status, error or state means, follow that value to every immediate
caller, retry loop and stateful consumer before you finish. Each consumer relying on the usual
meaning is a new flaw, a candidate, or a guarantee or premise the flaw undermines (`research`
`undermine`); one you cannot finish stays with the follow-up the store planned for the flaw.

A guarantee is an argued invariant, not a certificate: later agents will try to break it.
Before recording one, look for a counterexample to each precondition, including error,
malformed-input and retry paths. A path you did not check is an open premise or a deferred
region, not part of the guarantee. When you see a coherent scenario, raise a candidate now.
Record no exposures; whether a path drives a flaw is its path review's decision. When only a
dynamic test would settle something, write that test into its evidence.
