// 颜色只在 tokens.css 里定义;别处只能用 var(--…)。这样浅色 / 深色两套取值只有一处,
// 也挡住「顺手写个 #fff」让某块在深色下发白。
export default {
  extends: ['stylelint-config-recommended'],
  rules: {
    // 组件样式按块组织,不同块里的选择器互不相干;按全局特异性排序只会把相关规则拆散。
    'no-descending-specificity': null,
    'color-no-hex': true,
    'color-named': 'never',
    'function-disallowed-list': ['rgb', 'rgba', 'hsl', 'hsla', 'hwb', 'lab', 'lch', 'oklab', 'oklch', 'color'],
  },
  overrides: [
    {
      files: ['src/styles/tokens.css'],
      rules: { 'color-no-hex': null, 'color-named': null, 'function-disallowed-list': null },
    },
  ],
};
