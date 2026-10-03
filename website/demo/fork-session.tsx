import React from "react";
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
const command = ["omni fork \\", `  codex:${sourceId} \\`, "  --in claude"];
const steps = ["Code in Codex", "Fork with omni", "Write in Claude"];

type Beat = {
  phase: "prompt" | "code" | "command" | "transfer" | "docs-prompt" | "docs" | "done";
  duration: number;
  reveal?: number;
};

const beats: Beat[] = [
  { phase: "prompt", duration: 2000 },
  { phase: "code", duration: 3300 },
  { phase: "command", reveal: 0, duration: 350 },
  { phase: "command", reveal: 1, duration: 350 },
  { phase: "command", reveal: 2, duration: 350 },
  { phase: "command", reveal: 3, duration: 1400 },
  { phase: "transfer", reveal: 0, duration: 500 },
  { phase: "transfer", reveal: 1, duration: 500 },
  { phase: "transfer", reveal: 2, duration: 500 },
  { phase: "transfer", reveal: 3, duration: 2200 },
  { phase: "docs-prompt", duration: 2800 },
  { phase: "docs", duration: 3700 },
  { phase: "done", duration: 3400 },
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

function terminal(c: ThemeColors, beat: Beat) {
  if (beat.phase === "prompt" || beat.phase === "code") {
    return (
      <Column align="stretch" justify="start" gap={20}>
        {prompt(c, "Build a sliding-window rate limiter.")}
        <Column align="stretch" justify="start" gap={10}>
          {label(c, "CODEX", c.warningLight)}
          {beat.phase === "prompt" ? (
            <div style={{ display: "flex", color: c.textSecondary, fontSize: 21 }}>Implementing the API and boundary tests…</div>
          ) : (
            <Column align="stretch" justify="start" gap={10}>
              <div style={{ display: "flex", fontFamily: c.fontSans, color: c.textPrimary, fontSize: 21 }}>Added allow(key), with retry timing.</div>
              <CodeBlock c={c} width="100%" fontSize={19} padding={14} background={c.bgCard} lines={[
                "const limiter = createLimiter({",
                "  limit: 10, windowMs: 60_000,",
                "});",
                "",
                "limiter.allow(\"user-42\");",
                "// { allowed: true, retryAfterMs: 0 }",
              ]} />
            </Column>
          )}
        </Column>
      </Column>
    );
  }

  if (beat.phase === "command" || beat.phase === "transfer") {
    const revealed = beat.phase === "command" ? (beat.reveal ?? 0) : command.length;
    const complete = beat.phase === "transfer" && beat.reveal === 3;
    return (
      <Column align="stretch" justify="start" gap={18}>
        <CodeBlock c={c} width="100%" fontSize={19} padding={10} background={c.bgCard} lines={[
          ...command.slice(0, revealed).map((text, index) => ({ text, prefix: index === 0 ? "$" : " " })),
          ...(beat.phase === "command" ? [{ text: "▌", prefix: revealed === 0 ? "$" : " " }] : []),
        ]} />
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
          <div style={{ display: "flex", fontFamily: c.fontSans, fontSize: 23, color: c.textSecondary, lineHeight: 1.5 }}>Fork the conversation into a new agent session.</div>
        )}
      </Column>
    );
  }

  if (beat.phase === "done") {
    return (
      <Column align="stretch" justify="start" gap={22}>
        {label(c, "CLAUDE CODE", c.primaryLight)}
        <div style={{ display: "flex", fontSize: 30, fontFamily: c.fontSans, fontWeight: 700, color: c.textPrimary }}>README.md is ready.</div>
        <StatusList c={c} fontSize={23} gap={16} rows={[
          { label: "Setup and usage examples", status: "check", tone: "green" },
          { label: "Window and retry behavior explained", status: "check", tone: "green" },
          { label: "Based on the Codex conversation", status: "check", tone: "green" },
        ]} />
        <div style={{ display: "flex", padding: 18, borderRadius: 12, backgroundColor: c.bgCard, color: c.secondaryLight, fontFamily: c.fontSans, fontSize: 23, lineHeight: 1.4 }}>Keep coding in the original. Keep writing in the fork.</div>
      </Column>
    );
  }

  return (
    <Column align="stretch" justify="start" gap={20}>
      {prompt(c, "Write a README with examples and retry behavior.")}
      <Column align="stretch" justify="start" gap={10}>
        {label(c, "CLAUDE CODE", c.primaryLight)}
        {beat.phase === "docs-prompt" ? (
          <div style={{ display: "flex", fontFamily: c.fontSans, color: c.textSecondary, fontSize: 22, lineHeight: 1.5 }}>I have the limiter API from this conversation. I’ll use it in the guide.</div>
        ) : (
          <CodeBlock c={c} width="100%" fontSize={19} padding={14} background={c.bgCard} lines={[
            "# Rate limiter",
            "",
            "## Usage",
            "const result = limiter.allow(\"user-42\");",
            "",
            "If denied, wait result.retryAfterMs.",
            "windowMs sets the rolling time window.",
          ]} />
        )}
      </Column>
    </Column>
  );
}

function frame(theme: ThemeMode, beat: Beat) {
  const c = palette(theme);
  const step = beat.phase === "prompt" || beat.phase === "code" ? 0 : beat.phase === "command" || beat.phase === "transfer" ? 1 : 2;
  const forked = step === 2 || (beat.phase === "transfer" && beat.reveal === 3);
  const titles = ["Implement the feature.", "Fork the session. Keep the context.", "Write the guide with the same context."];

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
        <div style={{ display: "flex", fontSize: 23, color: c.textSecondary }}>{titles[step]}</div>
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
        <WindowFrame c={c} title={`${step === 0 ? "Codex" : step === 1 ? "omni" : "Claude Code"} · ~/src/ratelimit`} variant="terminal" tone={step === 0 ? "warm" : step === 1 ? "blue" : "purple"} width={790} height={424} padding={22} radius={16} shadow={false} background={c.bgSubtle}>
          {terminal(c, beat)}
        </WindowFrame>
        <Column gap={12} width={324} style={{ padding: "19px 12px", backgroundColor: c.bgCard, borderRadius: 16, border: `1px solid ${c.borderSubtle}` }}>
          <div style={{ display: "flex", justifyContent: "center", fontSize: 17, fontWeight: 700, color: c.textMuted }}>SESSION LINEAGE</div>
          <div style={{ display: "flex", position: "relative", width: 298, height: 284 }}>
            <GraphDiagram c={c} width={298} height={284} layout="manual" sizing="fixed" labelFontSize={21} detailFontSize={16} nodes={[
              { id: "codex", label: "Codex", detail: "Implementation", x: 0.5, y: 0.2, width: 268, height: 92, tone: "warm" },
              { id: "claude", label: "Claude Code", detail: forked ? "Documentation fork" : "New session", x: 0.5, y: 0.8, width: 268, height: 92, tone: "purple", muted: !forked },
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
    transition: index > 0 && beat.phase !== beats[index - 1].phase ? "fade" : "none",
    transitionDuration: 180,
    label: beat.phase,
  }));
}

export default create();
