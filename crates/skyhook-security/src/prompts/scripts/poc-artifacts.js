const artifacts = /* the artifacts directory your brief names */, dir = /* your working directory in it */;
const required = [{reference: `${dir}/attack/…`, role: "attack"}, {reference: `${dir}/control/…`, role: "control"} /* , one observation per step */];
const present = (await tool.glob({pattern: "**/*", path: `${artifacts}/${dir}`})).unwrap().paths;
const missing = required.filter(a => !present.some(p => p.endsWith(a.reference)));
return {required: required.length, missing};
