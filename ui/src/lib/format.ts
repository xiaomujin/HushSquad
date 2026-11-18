/** 头像与名字的确定性映射：同一个人每次渲染都是同一个颜色，不会闪。 */
export function avatarClass(userId: number): string {
  return ['a1', 'a2', 'a3'][userId % 3] ?? 'a1';
}

/**
 * 头像上的那一个字。
 *
 * 中文名取**末字**（小木 → 木、阿岚 → 岚），因为「小/老/阿」是前缀不是名字本身；
 * 拉丁名取首字母（alice → A、老K → K）。这条规则只在两个字以上时才有意义。
 */
export function initial(name: string): string {
  const chars = [...name.trim()];
  if (chars.length === 0) return '?';
  const latin = chars.find((c) => /[a-zA-Z]/.test(c));
  if (latin) return latin.toUpperCase();
  return chars[chars.length - 1]!;
}

/** 0–1 的电平转百分比，带一点下限，免得 0.001 也画一条线。 */
export function levelPct(level: number): number {
  const clamped = Math.max(0, Math.min(1, level));
  return clamped < 0.02 ? 0 : Math.round(clamped * 100);
}

/** dBFS 显示：电平 0 时给「—」，否则给一位小数。 */
export function dbfs(level: number): string {
  if (level <= 0.0005) return '—';
  const db = 20 * Math.log10(level);
  return `${db.toFixed(0)} dBFS`;
}
