// Pele emits CSS variables for browsers. Resolve paint/font attributes before
// handing the SVG to the native rasterizer; text nodes stay untouched.
function resolveCss(value, variables, depth = 0) {
  if (depth > 16) return 'inherit';
  let result = '';
  let cursor = 0;
  while (cursor < value.length) {
    const start = value.indexOf('var(', cursor);
    if (start < 0) return result + value.slice(cursor);
    result += value.slice(cursor, start);
    let nesting = 1;
    let end = start + 4;
    let comma = -1;
    for (; end < value.length && nesting; end++) {
      const ch = value[end];
      if (ch === '(') nesting++;
      if (ch === ')') nesting--;
      if (ch === ',' && nesting === 1 && comma < 0) comma = end;
    }
    if (nesting) return result + value.slice(start);
    const name = value.slice(start + 4, comma < 0 ? end - 1 : comma).trim();
    const fallback = comma < 0 ? 'inherit' : value.slice(comma + 1, end - 1).trim();
    result += resolveCss(variables[name] ?? fallback, variables, depth + 1);
    cursor = end;
  }
  return result;
}

function rakuRenderMermaid(source, colors) {
  const variables = {
    '--pele-bg': colors.background,
    '--pele-fg': colors.text,
    '--pele-muted': colors.muted,
    '--pele-line': colors.text,
    '--pele-surface': colors.surface,
    '--pele-surface-alt': colors.surfaceAlt,
    '--pele-border': colors.border,
    '--pele-accent': colors.accent,
    '--pele-critical': colors.danger,
    '--pele-error': colors.danger,
    '--pele-font': 'IBM Plex Sans, sans-serif',
    '--pele-font-mono': 'Lilex, monospace',
    '--pele-radius': '4px',
    '--pele-title-weight': 'bold',
    '--pele-heading-weight': 'bold',
    '--pele-bold-weight': 'bold',
    '--_r': '4px',
    '--_fm': 'Lilex, monospace',
    '--_tw': 'bold',
    '--_hw': 'bold',
    '--_w': 'bold',
    '--_bg': colors.background,
    '--_fg': colors.text,
    '--_m': colors.muted,
    '--_l': colors.text,
    '--_s': colors.surface,
    '--_a': colors.surfaceAlt,
    '--_b': colors.border,
    '--_c': colors.accent,
  };
  const result = pele.render(source, {
    fontFamily: 'IBM Plex Sans, sans-serif',
    fontFamilyMono: 'Lilex, monospace',
    fontSize: 16,
    responsive: false,
    autoDirection: false,
    links: false,
    images: false,
    limit: 50000,
    outputLimit: 2000000,
    maxEdges: 2000,
    idPrefix: 'raku-mermaid',
  });
  result.svg = result.svg.replace(
    /\s(?:style|fill|stroke|color|font-family|font-weight|rx|ry|stroke-width)="[^"]*"/g,
    (attribute) => resolveCss(attribute, variables),
  );
  return JSON.stringify({ svg: result.svg, width: result.width, height: result.height });
}
