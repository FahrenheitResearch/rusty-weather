import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const licenseList = require("spdx-license-list/full");
const lockPath = path.resolve("package-lock.json");
const outputPath = path.resolve(process.argv[2] ?? "THIRD_PARTY_LICENSES-node.md");
const lock = JSON.parse(fs.readFileSync(lockPath, "utf8"));
if (lock.lockfileVersion !== 3 || typeof lock.packages !== "object") {
  throw new Error("unsupported or malformed npm lockfile");
}

const operators = new Set(["AND", "OR", "WITH"]);
const components = [];
const usedLicenseIds = new Set();
for (const [packagePath, metadata] of Object.entries(lock.packages)) {
  if (!packagePath.startsWith("node_modules/")) continue;
  const name = packagePath.slice("node_modules/".length);
  if (
    !name ||
    typeof metadata.version !== "string" ||
    typeof metadata.license !== "string" ||
    !metadata.license.trim()
  ) {
    throw new Error(`locked package lacks version/license metadata: ${packagePath}`);
  }
  const tokens = metadata.license.match(/[A-Za-z0-9.+-]+/g) ?? [];
  const ids = tokens.filter((token) => !operators.has(token));
  if (ids.length === 0) {
    throw new Error(`locked package has no SPDX license id: ${packagePath}`);
  }
  for (const id of ids) {
    if (!Object.hasOwn(licenseList, id) || !licenseList[id].licenseText) {
      throw new Error(`no pinned SPDX license text for ${id} (${packagePath})`);
    }
    usedLicenseIds.add(id);
  }
  components.push({ name, version: metadata.version, expression: metadata.license });
}
components.sort((left, right) =>
  `${left.name}@${left.version}`.localeCompare(`${right.name}@${right.version}`, "en"),
);
if (components.length === 0 || components.length > 2_000) {
  throw new Error("npm component inventory is empty or exceeds its bound");
}

const lines = [
  "# Rusty Weather R2 gateway: locked Node build-tool licenses",
  "",
  "This deterministic inventory covers every package entry in the checked-in npm lockfile.",
  "These packages are build, test, and deployment tooling; `node_modules` is never included",
  "in a Rusty Weather service archive or the deployable Worker bundle. License texts are",
  "included conservatively for every SPDX alternative named by the lockfile.",
  "",
  "| Package | Version | License expression |",
  "| --- | --- | --- |",
];
for (const component of components) {
  lines.push(
    `| ${escapeCell(component.name)} | ${escapeCell(component.version)} | ${escapeCell(component.expression)} |`,
  );
}
for (const id of [...usedLicenseIds].sort((left, right) => left.localeCompare(right, "en"))) {
  const license = licenseList[id];
  lines.push("", `## ${id} - ${license.name}`, "", "```text", normalizeText(license.licenseText), "```");
}
lines.push("");
fs.writeFileSync(outputPath, lines.join("\n"), { encoding: "utf8", flag: "w" });

function escapeCell(value) {
  return value.replaceAll("|", "\\|").replaceAll("\n", " ");
}

function normalizeText(value) {
  return value
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    .replace(/[ \t]+$/gm, "")
    .trimEnd();
}
