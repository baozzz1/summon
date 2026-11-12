import { t as translate } from '../i18n';
// token 与费用的文字形态:用量页与会话详情共用。
import type { Spend, TokenCounts } from '../api/types';
import { fmtCount, fmtUsd } from './format';

/** 四项 token 的合计。 */
export function tokenTotal(t: TokenCounts): number {
  return t.input + t.output + t.cache_read + t.cache_write;
}

/** 全部 token 都没能计价。 */
function allUnpriced(s: Spend): boolean {
  return !!s.unpriced && s.unpriced.tokens >= tokenTotal(s.tokens);
}

/** 一笔金额:有未计价的部分时标「起」(不含未计价的部分);全部未计价写「未计价」。 */
function amount(s: Spend, usd: number): string {
  if (allUnpriced(s)) return translate('de9e699');
  return s.unpriced ? translate('d62f86d', [fmtUsd(usd)]) : fmtUsd(usd);
}

/** 费用合计;没报 token 写「—」。 */
export function costText(s: Spend | undefined): string {
  return s ? amount(s, s.cost.total) : '—';
}

/** 四项里的一项的金额,与合计同一口径。 */
export function partText(s: Spend, key: keyof TokenCounts): string {
  return amount(s, s.cost[key]);
}

/** 未计价部分涉及的模型;CLI 没报模型的写「未知模型」。 */
export function unpricedModels(models: string[]): string {
  return models.map((m) => m || translate('d0a9156')).join('、');
}

/** 一轮的 token 与费用,一行写完。 */
export function spendText(s: Spend): string {
  return `${fmtCount(tokenTotal(s.tokens))} token · ${costText(s)}`;
}

/** 四项拆分,悬停时看。 */
export function spendTitle(s: Spend): string {
  const part = (label: string, key: keyof TokenCounts) => `${label} ${partText(s, key)}（${fmtCount(s.tokens[key])} token）`;
  const parts = [part(translate('d8dcc1b'), 'input'), part(translate('d1ded65'), 'cache_read'), part(translate('d6078a6'), 'cache_write'), part(translate('dfb04ad'), 'output')];
  if (s.unpriced) parts.push(translate('dd39e4f', [fmtCount(s.unpriced.tokens), unpricedModels(s.unpriced.models)]));
  return parts.join('\n');
}
