import { providers } from "./providers.generated";

export type Provider = (typeof providers)[number];
export type AgentId = Provider["id"];
export type CapabilityKey = keyof Provider["capabilities"];

export const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

/** CLI name, short label, and line color. Keyed by manifest ID, so a new agent fails the type check. */
const meta: Record<AgentId, { cli: string; short: string; launch: string }> = {
  codex: { cli: "codex", short: "Codex", launch: "Codex" },
  "claude-code": { cli: "claude", short: "Claude Code", launch: "Claude" },
  opencode: { cli: "opencode", short: "OpenCode", launch: "OpenCode" },
  pi: { cli: "pi", short: "Pi", launch: "Pi" },
  omp: { cli: "omp", short: "Oh My Pi", launch: "Oh My Pi" },
  grok: { cli: "grok", short: "Grok", launch: "Grok" },
  "cursor-ide": { cli: "cursor-ide", short: "Cursor IDE", launch: "Cursor IDE" },
  "cursor-agent": { cli: "cursor-cli", short: "Cursor Agent", launch: "Cursor CLI" },
  antigravity: { cli: "antigravity", short: "Antigravity", launch: "Antigravity CLI" },
  hermes: { cli: "hermes", short: "Hermes", launch: "Hermes" },
  "antigravity-desktop": { cli: "antigravity-desktop", short: "Antigravity Desktop", launch: "Antigravity Desktop" },
  "antigravity-ide": { cli: "antigravity-ide", short: "Antigravity IDE", launch: "Antigravity IDE" },
};

export type Agent = Provider & { cli: string; short: string; launch: string };

export const agents: readonly Agent[] = providers.map((provider) => ({ ...provider, ...meta[provider.id] }));

export const platformOrder = [
  { id: "linux", name: "Linux", short: "Linux" },
  { id: "macos", name: "macOS", short: "macOS" },
  { id: "windows", name: "Windows", short: "Win" },
] as const;

export function declares(platforms: readonly string[], platform: string) {
  return platforms.includes(platform);
}

export function platformScope(platforms: readonly string[]) {
  if (platforms.length === 0) return "Not guaranteed";
  return platformOrder.filter(({ id }) => declares(platforms, id)).map(({ name }) => name).join(" + ");
}

export const sources = agents.filter((agent) => agent.capabilities.read_index.length > 0);
export const targets = agents.filter((agent) => agent.capabilities.cross_provider_import.length > 0);
export const readOnlySources = agents.filter(
  (agent) => agent.capabilities.same_provider_resume.length === 0 && agent.capabilities.cross_provider_import.length === 0,
);
/** Paths the conformance matrix runs: every ordered pair of import targets. */
export const conformancePaths = targets.length * (targets.length - 1);

export function agentById(id: AgentId): Agent {
  const agent = agents.find((candidate) => candidate.id === id);
  if (!agent) throw new Error(`unknown agent ${id}`);
  return agent;
}
