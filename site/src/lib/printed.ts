// What runnerctl printed against the dashboard's demo daemon (crates/web/examples/demo), kept whole in
// assets/terminal by scripts/capture/capture.sh. A page cuts out the lines it shows and changes none: from the first
// line that starts with `from` up to the one that starts with `to`, or to the end.
const files = import.meta.glob<string>('../assets/terminal/*.txt', { eager: true, query: '?raw', import: 'default' });

export function printed(name: string, from?: string, to?: string): string {
  const text = files[`../assets/terminal/${name}.txt`];
  if (text === undefined) throw new Error(`no capture ${name}`);
  const lines = text.trimEnd().split('\n');
  const at = (head: string) => {
    const found = lines.findIndex((line) => line.startsWith(head));
    if (found < 0) throw new Error(`no "${head}" in ${name}`);
    return found;
  };
  return lines.slice(from === undefined ? 0 : at(from), to === undefined ? undefined : at(to)).join('\n').trimEnd();
}
