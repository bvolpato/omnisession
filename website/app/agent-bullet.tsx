import { basePath, type Agent } from "./agents";

/** Round line marker with the agent's logo. Decorative: always pair it with the agent's name. */
export function AgentBullet({ agent, size = 28 }: { agent: Agent; size?: number }) {
  return (
    <span aria-hidden="true" className={`bullet line-${agent.id}`} style={{ width: size, height: size }}>
      <img alt="" className={agent.logo === "codex" ? "bullet-full" : "bullet-mark"} src={`${basePath}/providers/${agent.logo}.svg`} />
    </span>
  );
}
