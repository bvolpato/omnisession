import { Fragment } from "react";

/** Keeps a flag and its value on one line, so a command wraps only between arguments. */
function parts(text: string) {
  const result: string[] = [];
  for (const word of text.split(" ")) {
    const last = result.at(-1);
    if (last && /^--?[a-z-]+$/.test(last) && !word.startsWith("-")) result[result.length - 1] = `${last} ${word}`;
    else result.push(word);
  }
  return result;
}

export function Command({ text, muted = "" }: { text: string; muted?: string }) {
  return (
    <code className="command">
      {muted ? <span className="command-muted">{muted} </span> : null}
      {parts(text).map((part, index) => <Fragment key={index}>{index > 0 ? " " : ""}<span className="nowrap">{part}</span></Fragment>)}
    </code>
  );
}
