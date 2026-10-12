// Marks each output folder with its module format so Node resolves it correctly.
import { writeFileSync } from "node:fs";

writeFileSync(new URL("../dist/cjs/package.json", import.meta.url), '{\n  "type": "commonjs"\n}\n');
writeFileSync(new URL("../dist/esm/package.json", import.meta.url), '{\n  "type": "module"\n}\n');
