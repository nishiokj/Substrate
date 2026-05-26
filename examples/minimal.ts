import { Environment } from "@substrate/sdk";

const env = await Environment.create({
  workspace: { kind: "new" },
  policy: { process: { allowExec: true, allowedCommands: ["ls"] } },
});
const session = await env.createSession();

try {
  await session.write("notes.txt", "hello");
  console.log(await session.read("notes.txt"));
  console.log(await session.bash("ls /workspace"));
} finally {
  await env.close();
}
