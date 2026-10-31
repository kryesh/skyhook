# One file's entry points

Your parent assigned you one file. Record every entry point in it that an actor can drive (each
route, event or message handler, read site, callback or hook) as one source: its entry location,
its trust, and a body describing its input and who controls it. Reuse an existing source
(`research_query` `list` `{kind: "source"}`) rather than recording a duplicate.

Follow each source's input only to its handoff, the first call into implementation code beyond
parsing and dispatch. When an existing path's flow matches it (`list` `{kind: "path"}`): the
same first step, reached with the same parsing and trust, link the source to that path with
`path_source`. Otherwise record a path of its own for it: its `path_source` and a first
`path_step` at the handoff's call site into the component owning the called function, the note
naming that function. Record nothing else, except a candidate if you see one. Reply with the
source and path IDs, then anything you could not resolve, such as dynamic registration or
generated dispatch. A file without entry points replies `none`.
