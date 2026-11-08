"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const html = fs.readFileSync(require("node:path").join(__dirname, "../src/web/index.html"), "utf8");
const handler = html.match(/\$\("jit-app"\)\.onchange = \(\) => \{[^\n]+\};/)[0];
const fields = {"jit-app": {value: ""}, "jit-enable": {disabled: false}};
const ctx = {$: id => fields[id], state: null};
vm.createContext(ctx); vm.runInContext(handler, ctx);
fields["jit-app"].onchange(); assert.equal(fields["jit-enable"].disabled, true);
fields["jit-app"].value = "42";
fields["jit-app"].onchange(); assert.equal(fields["jit-enable"].disabled, false);
ctx.state = {busy: true};
fields["jit-app"].onchange(); assert.equal(fields["jit-enable"].disabled, true);
for (const text of ["Enable JIT", "Refresh running apps", "Running app", "Select an app"]) {
  assert(html.includes(JSON.stringify(text) + ":"), "Korean JIT translation missing: " + text);
}
console.log("JIT app selection, busy guard and Korean UI labels OK");
