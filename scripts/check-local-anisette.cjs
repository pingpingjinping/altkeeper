"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const html = fs.readFileSync(require("node:path").join(__dirname, "../src/web/index.html"), "utf8");
const source = html.match(/function selectedAnisetteUrl\(\) \{[\s\S]*?\n\}/)[0];
const fields = {"anisette-mode": {value: "external"}, "anisette-url": {value: "https://custom.example/v3"}};
const context = { $: id => fields[id] };
vm.createContext(context); vm.runInContext(source, context);
assert.equal(context.selectedAnisetteUrl(), "https://ani.jkcoxson.com");
fields["anisette-mode"].value = "local";
assert.equal(context.selectedAnisetteUrl(), "http://127.0.0.1:8788");
fields["anisette-mode"].value = "custom";
assert.equal(context.selectedAnisetteUrl(), "https://custom.example/v3");
for (const label of ["External Anisette server", "AltKeeper local Anisette server", "Custom URL", "Local server: "]) {
  assert(html.includes(JSON.stringify(label) + ":"), "Korean translation missing: " + label);
}
assert(html.includes('id="anisette-local-status"'));
console.log("Local/external/custom Anisette URL selection and Korean status UI OK");
