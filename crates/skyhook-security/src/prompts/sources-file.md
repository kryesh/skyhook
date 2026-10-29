# One file's entry points

Your parent assigned you one file. Record every entry point in it that an actor can drive (each
route, event or message handler, read site, callback or hook) as one source with `research`
`add_source`: its entry location, the principals that can drive it, and a description of its
input and who controls it. Check `research_query` `sources` first and reuse an existing source
rather than recording a duplicate.

Follow each source's input only to its handoff, the first call into implementation code beyond
parsing and dispatch, and record it as the source's `handoff`: the call site `at` and the
function it `calls`. Record nothing else, except a candidate if you see one. Reply with the
source IDs, then anything you could not resolve, such as dynamic registration or generated
dispatch. A file without entry points replies `none`.
