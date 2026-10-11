import React from "react";
import { readFileSync } from "node:fs";
import {
  Arrow,
  CodeBlock,
  Column,
  GraphDiagram,
  Icon,
  Row,
  Scene,
  StatusList,
  WindowFrame,
  getThemeColors,
  type AnimatedScene,
  type ThemeColors,
  type ThemeMode,
} from "vizmatic";

export const width = 1200;
export const height = 780;

// Invented IDs, workspace, code, and responses. No provider stores are read.
const sourceId = "c0decafe-c0de-4000-8000-000000000001";
const steps = ["Code in Codex", "Choose in omni", "Write in Claude"];
const codexLogo = `data:image/svg+xml;base64,${readFileSync(new URL("../public/providers/codex.svg", import.meta.url)).toString("base64")}`;
const claudeLogo = `data:image/svg+xml;base64,${Buffer.from(readFileSync(new URL("../public/providers/claude-code.svg", import.meta.url), "utf8").replace("<path ", '<path fill="#d97757" ')).toString("base64")}`;

type Beat = {
  phase: "prompt" | "code" | "command" | "sessions" | "target" | "transfer" | "resume" | "docs-prompt" | "docs" | "done";
  duration: number;
  reveal?: number;
};

const beats: Beat[] = [
  { phase: "prompt", duration: 2000 },
  { phase: "code", duration: 3000 },
  { phase: "command", reveal: 0, duration: 300 },
  { phase: "command", reveal: 1, duration: 1000 },
  { phase: "sessions", reveal: 0, duration: 1100 },
  { phase: "sessions", reveal: 1, duration: 1700 },
  { phase: "target", reveal: 0, duration: 900 },
  { phase: "target", reveal: 1, duration: 650 },
  { phase: "target", reveal: 2, duration: 1800 },
  { phase: "transfer", reveal: 1, duration: 350 },
  { phase: "transfer", reveal: 2, duration: 350 },
  { phase: "transfer", reveal: 3, duration: 1500 },
  { phase: "resume", duration: 2800 },
  { phase: "docs-prompt", reveal: 0, duration: 120 },
  { phase: "docs-prompt", reveal: 1, duration: 120 },
  { phase: "docs-prompt", duration: 2500 },
  { phase: "docs", reveal: 0, duration: 120 },
  { phase: "docs", reveal: 1, duration: 120 },
  { phase: "docs", duration: 4000 },
  { phase: "done", duration: 3000 },
];

function palette(theme: ThemeMode): ThemeColors {
  const c = getThemeColors(theme);
  return {
    ...c,
    bg: theme === "dark" ? "#080b15" : "#f6f7fb",
    bgCard: theme === "dark" ? "#10162a" : "#ffffff",
    bgSubtle: theme === "dark" ? "#0b1020" : "#f0f2f8",
    bgHover: theme === "dark" ? "#161d35" : "#e8edf5",
    textPrimary: theme === "dark" ? "#eef1f8" : "#0b1020",
    textSecondary: theme === "dark" ? "#c5cde1" : "#34415b",
    textMuted: theme === "dark" ? "#a9b1c7" : "#526078",
    primaryLight: theme === "dark" ? "#ff82ae" : "#b3134f",
    secondaryLight: theme === "dark" ? "#5ee9df" : "#07706a",
    positiveLight: theme === "dark" ? "#74e8b2" : "#0a6e43",
    warningLight: theme === "dark" ? "#ffc55a" : "#8a5300",
    borderSubtle: theme === "dark" ? "#263047" : "#dce2ec",
  };
}

function label(c: ThemeColors, text: string, color = c.textMuted) {
  return <div style={{ display: "flex", color, fontSize: 16, fontWeight: 700, letterSpacing: "0.06em" }}>{text}</div>;
}

function prompt(c: ThemeColors, text: string) {
  return (
    <Column align="stretch" justify="start" gap={8}>
      {label(c, "YOU", c.secondaryLight)}
      <div style={{ display: "flex", fontFamily: c.fontSans, fontSize: 23, lineHeight: 1.4, color: c.textPrimary }}>{text}</div>
    </Column>
  );
}

function pickerRow(c: ThemeColors, text: string, selected: boolean) {
  return (
    <div style={{ display: "flex", padding: "7px 10px", borderRadius: 6, fontSize: 19, color: selected ? c.secondaryLight : c.textSecondary, backgroundColor: selected ? c.bgHover : c.bgSubtle }}>
      {`${selected ? "›" : " "} ${text}`}
    </div>
  );
}

function pickerHint(c: ThemeColors, text: string) {
  return <div style={{ display: "flex", padding: "10px 12px", borderRadius: 8, backgroundColor: c.bgCard, color: c.secondaryLight, fontFamily: c.fontSans, fontSize: 21 }}>{text}</div>;
}

function syntaxLine(c: ThemeColors, ...tokens: Array<string | [string, string]>) {
  return (
    <span style={{ display: "flex", whiteSpace: "pre", color: c.textPrimary }}>
      {tokens.map((token, index) => <span key={index} style={{ color: typeof token === "string" ? c.textPrimary : token[1], whiteSpace: "pre" }}>{typeof token === "string" ? token : token[0]}</span>)}
    </span>
  );
}

function targetRow(c: ThemeColors, name: string, action: string, selected: boolean, claude: boolean) {
  return (
    <Column align="stretch" justify="start" gap={3}>
      <div style={{ display: "flex", padding: "3px 8px", borderRadius: 5, fontSize: 17, color: selected ? c.secondaryLight : c.textPrimary, backgroundColor: selected ? c.bgHover : c.bgSubtle }}>{`${selected ? "›" : " "} ${name}`}</div>
      <div style={{ display: "flex", paddingLeft: 29, color: selected ? c.secondaryLight : c.textMuted, fontSize: 15 }}>{action}</div>
      {selected && <Column align="stretch" justify="start" gap={3}>
        <div style={{ display: "flex", paddingLeft: 29, fontSize: 14 }}>{syntaxLine(c, ["Mode  ", c.textMuted], ["[default]", c.secondaryLight], [` ${claude ? "accept-edits " : ""}auto yolo ←/→`, c.textMuted])}</div>
        <div style={{ display: "flex", paddingLeft: 29, color: c.textMuted, fontSize: 14 }}>no flags · the agent's own settings decide</div>
      </Column>}
    </Column>
  );
}

function originalConversation(c: ThemeColors, pending: boolean) {
  return (
    <Column align="stretch" justify="start" gap={16}>
      {prompt(c, "Build a sliding-window rate limiter.")}
      <Column align="stretch" justify="start" gap={10}>
        {label(c, "ASSISTANT")}
        {pending ? (
          <div style={{ display: "flex", color: c.textSecondary, fontSize: 21 }}>Implementing the API and boundary tests…</div>
        ) : (
          <Column align="stretch" justify="start" gap={10}>
            <div style={{ display: "flex", fontFamily: c.fontSans, color: c.textPrimary, fontSize: 21 }}>Added allow(key), with retry timing.</div>
            <CodeBlock c={c} width="100%" fontSize={18} padding={12} background={c.bgCard} lines={[
              syntaxLine(c, ["const", c.primaryLight], " limiter = ", ["createLimiter", c.secondaryLight], "({"),
              syntaxLine(c, "  limit: ", ["10", c.warningLight], ", windowMs: ", ["60_000", c.warningLight], ","),
              "});",
              "",
              syntaxLine(c, "limiter.", ["allow", c.secondaryLight], "(", ["\"user-42\"", c.positiveLight], ");"),
              { text: "// { allowed: true, retryAfterMs: 0 }", dim: true },
            ]} />
          </Column>
        )}
      </Column>
    </Column>
  );
}

function conversation(c: ThemeColors, beat: Beat) {
  const asked = ["docs-prompt", "docs", "done"].includes(beat.phase);
  const answered = beat.phase === "docs" || beat.phase === "done";
  const scroll = answered ? [215, 275, 340][beat.reveal ?? 2] : asked ? [50, 100, 148][beat.reveal ?? 2] : 0;

  return (
    <div style={{ display: "flex", position: "relative", width: "100%", height: 340, overflow: "hidden" }}>
      <Column align="stretch" justify="start" gap={24} width="100%" style={{ position: "absolute", top: -scroll, left: 0 }}>
        {originalConversation(c, beat.phase === "prompt")}
        {asked && prompt(c, "Write a README with examples and retry behavior.")}
        {answered && <Column align="stretch" justify="start" gap={10}>
          {label(c, "ASSISTANT")}
          <CodeBlock c={c} title="README.md" width="100%" fontSize={18} padding={10} background={c.bgCard} lines={[
            syntaxLine(c, ["# Rate limiter", c.primaryLight]),
            { text: "```ts", dim: true },
            syntaxLine(c, ["const", c.primaryLight], " result = limiter.", ["allow", c.secondaryLight], "(", ["\"user-42\"", c.positiveLight], ");"),
            { text: "```", dim: true },
            "If denied, wait `result.retryAfterMs`.",
          ]} />
        </Column>}
      </Column>
    </div>
  );
}

function terminal(c: ThemeColors, beat: Beat) {
  if (["prompt", "code", "resume", "docs-prompt", "docs", "done"].includes(beat.phase)) {
    return conversation(c, beat);
  }

  if (beat.phase === "sessions") {
    const selected = beat.reveal === 1;
    return (
      <Column align="stretch" justify="start" gap={12}>
        {label(c, "OmniSession  SESSION BROWSER  ·  3", c.textPrimary)}
        <div style={{ display: "flex", color: c.textMuted, fontSize: 16 }}>Scope  current workspace [Tab]   Source  all sources [←/→]</div>
        <div style={{ display: "flex", color: c.textMuted, fontSize: 17 }}>Search › type to filter titles, folders, branches, IDs…</div>
        <Column align="stretch" justify="start" gap={3}>
          {pickerRow(c, "NEW SESSION", false)}
          {pickerRow(c, "claude  API retry guide", !selected)}
          {pickerRow(c, "codex   Sliding-window rate limiter", selected)}
          {pickerRow(c, "pi      Boundary tests", false)}
        </Column>
        <div style={{ display: "flex", color: c.textMuted, fontSize: 17 }}>↑↓ move   Enter continue   Esc quit   ? help</div>
        {pickerHint(c, selected ? "Press Enter to choose the next agent." : "↓ Choose the Codex session.")}
      </Column>
    );
  }

  if (beat.phase === "target") {
    const selected = beat.reveal ?? 0;
    return (
      <Column align="stretch" justify="start" gap={7}>
        {label(c, "OmniSession  Choose target agent", c.textPrimary)}
        <div style={{ display: "flex", color: c.textMuted, fontSize: 14 }}>{`Source: codex:${sourceId}`}</div>
        <Column align="stretch" justify="start" gap={4}>
          <div style={{ display: "flex", color: c.textPrimary, fontSize: 18 }}>Where should this session open?</div>
          <div style={{ display: "flex", color: c.textMuted, fontSize: 14 }}>Filter › type an agent name, such as grok or agy</div>
        </Column>
        <Column align="stretch" justify="start" gap={7}>
          {targetRow(c, "Codex", "Continue original session", selected === 0, false)}
          {targetRow(c, "Codex · fork", "Fork session", selected === 1, false)}
          {targetRow(c, "Claude", "Open continuation in Claude", selected === 2, true)}
        </Column>
        <div style={{ display: "flex", color: c.textMuted, fontSize: 14 }}>↑↓ agent   ←→ mode   Enter open   Esc back</div>
      </Column>
    );
  }

  if (beat.phase === "command" || beat.phase === "transfer") {
    const complete = beat.phase === "transfer" && beat.reveal === 3;
    return (
      <Column align="stretch" justify="start" gap={18}>
        <CodeBlock c={c} width="100%" fontSize={24} padding={14} background={c.bgCard} lines={[{ text: beat.phase === "command" ? (beat.reveal === 0 ? "▌" : "omni▌") : "omni", prefix: "$" }]} />
        {beat.phase === "transfer" ? (
          <Column align="stretch" justify="start" gap={12}>
            <div style={{ display: "flex", color: c.secondaryLight, fontSize: 20 }}>Transfer: codex → claude</div>
            <StatusList c={c} fontSize={19} gap={8} rows={[
              { label: "Provider version + workspace checked", status: (beat.reveal ?? 0) >= 1 ? "check" : "pending", tone: (beat.reveal ?? 0) >= 1 ? "green" : "neutral" },
              { label: "New Claude session written + validated", status: (beat.reveal ?? 0) >= 2 ? "check" : "pending", tone: (beat.reveal ?? 0) >= 2 ? "green" : "neutral" },
              { label: "History read back + verified", status: complete ? "check" : "pending", tone: complete ? "green" : "neutral" },
            ]} />
            {complete && <div style={{ display: "flex", color: c.primaryLight, fontSize: 20 }}>→ Launching Claude Code in this workspace</div>}
          </Column>
        ) : (
          <div style={{ display: "flex", fontFamily: c.fontSans, fontSize: 23, color: c.textSecondary, lineHeight: 1.5 }}>One command. Pick a session. Pick the next agent.</div>
        )}
      </Column>
    );
  }

  return null;
}

function frame(theme: ThemeMode, beat: Beat) {
  const c = palette(theme);
  const step = beat.phase === "prompt" || beat.phase === "code" ? 0 : ["command", "sessions", "target", "transfer"].includes(beat.phase) ? 1 : 2;
  const forked = step === 2 || (beat.phase === "transfer" && beat.reveal === 3);
  const titles = ["Implement the feature.", "Run omni. Pick the session and next agent.", "Same conversation. Continue in Claude."];
  const title = step === 1 ? `${beat.phase === "command" ? "Shell" : "omni"} · ~/src/ratelimit` : (
    <Row gap={8} align="center">
      <img src={step === 0 ? codexLogo : claudeLogo} alt={step === 0 ? "Codex" : "Claude Code"} width={20} height={20} />
      <span>{`${step === 0 ? "Codex" : "Claude Code"} · ~/src/ratelimit`}</span>
    </Row>
  );

  return (
    <Scene c={c} background={c.bg} padding={32} gap={22} align="stretch">
      <Row justify="space-between" align="center" width="100%">
        <Row gap={12} align="center">
          <Icon c={c} name="layers" tone="blue" size={28} />
          <div style={{ display: "flex", fontSize: 25, fontWeight: 700, color: c.textPrimary }}>OmniSession</div>
        </Row>
        <div style={{ display: "flex", padding: "9px 14px", border: `1px solid ${c.borderSubtle}`, backgroundColor: c.bgCard, borderRadius: 99, fontSize: 16, color: c.textSecondary }}>Synthetic demo · illustrative responses</div>
      </Row>

      <Column align="stretch" gap={8}>
        <div style={{ display: "flex", fontSize: 40, fontWeight: 700, letterSpacing: "-0.04em", color: c.textPrimary }}>Code in Codex. Docs in Claude.</div>
        <div style={{ display: "flex", fontSize: 23, color: c.textSecondary }}>{beat.phase === "target" ? "Press ↓ twice to choose Claude, then press Enter." : titles[step]}</div>
      </Column>

      <Row gap={12} width="100%">
        {steps.map((text, index) => (
          <Row key={text} gap={10} align="center" width={370} style={{ padding: "12px 16px", borderRadius: 12, border: `1px solid ${index === step ? c.secondaryLight : c.borderSubtle}`, backgroundColor: c.bgCard }}>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "center", width: 25, height: 25, borderRadius: 99, backgroundColor: index === step ? c.secondaryLight : c.bgHover, color: index === step ? c.bg : c.textMuted, fontSize: 17, fontWeight: 700 }}>{String(index + 1)}</div>
            <div style={{ display: "flex", fontSize: 19, fontWeight: 600, color: index <= step ? c.textPrimary : c.textMuted }}>{text}</div>
          </Row>
        ))}
      </Row>

      <Row gap={22} align="stretch" width="100%">
        <WindowFrame c={c} title={title} variant="terminal" tone="blue" width={790} height={424} padding={22} radius={16} shadow={false} background={c.bgSubtle}>
          {terminal(c, beat)}
        </WindowFrame>
        <Column gap={12} width={324} style={{ padding: "19px 12px", backgroundColor: c.bgCard, borderRadius: 16, border: `1px solid ${c.borderSubtle}` }}>
          <div style={{ display: "flex", justifyContent: "center", fontSize: 17, fontWeight: 700, color: c.textMuted }}>SESSION LINEAGE</div>
          <div style={{ display: "flex", position: "relative", width: 298, height: 284 }}>
            <GraphDiagram c={c} width={298} height={284} layout="manual" sizing="fixed" labelFontSize={21} detailFontSize={16} nodes={[
              { id: "codex", label: "Codex", detail: "Implementation", x: 0.5, y: 0.2, width: 268, height: 92, tone: "warm" },
              { id: "claude", label: "Claude Code", detail: beat.phase === "docs" || beat.phase === "done" ? "Same history + docs" : forked ? "Same history" : "Continue here", x: 0.5, y: 0.8, width: 268, height: 92, tone: "purple", muted: !forked },
            ]} edges={[]} />
            {forked && <div style={{ display: "flex", position: "absolute", left: 142, top: 120 }}><Arrow c={c} direction="down" length={44} color={c.secondaryLight} /></div>}
          </div>
          <div style={{ display: "flex", justifyContent: "center", color: forked ? c.positiveLight : c.textSecondary, fontFamily: c.fontSans, fontSize: 20, fontWeight: 600 }}>Original session stays intact</div>
        </Column>
      </Row>

      <Row width="100%" justify="space-between" align="center">
        <div style={{ display: "flex", fontSize: 18, color: c.textSecondary }}>Visible history carries over. Tools are never replayed.</div>
        <div style={{ display: "flex", fontSize: 18, color: c.secondaryLight, fontWeight: 600 }}>bvolpato.github.io/omnisession</div>
      </Row>
    </Scene>
  );
}

export function create(theme: ThemeMode = "dark") {
  return frame(theme, { phase: "done", duration: 0 });
}

export function createScenes(theme: ThemeMode): AnimatedScene[] {
  return beats.map((beat, index) => ({
    element: frame(theme, beat),
    duration: beat.duration,
    transition: index > 0 && beat.phase !== beats[index - 1].phase && ["code", "command"].includes(beat.phase) ? "fade" : "none",
    transitionDuration: 180,
    label: beat.phase,
  }));
}

export default create();
