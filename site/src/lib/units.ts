// Sizes as runnerd's ByteSize prints them (crates/core/src/config.rs), for figures the site computes itself.

const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

/** Bytes the way runnerd's ByteSize prints them: the largest unit, two decimals, trailing zeros dropped. */
export function size(bytes: number): string {
  const units: [string, number][] = [
    ['T', 1024 ** 4],
    ['G', GIB],
    ['M', MIB],
    ['K', 1024],
  ];
  const unit = units.find(([, factor]) => bytes >= factor);
  if (!unit) return `${Math.round(bytes)}B`;
  return `${(bytes / unit[1]).toFixed(2).replace(/0+$/, '').replace(/\.$/, '')}${unit[0]}`;
}
