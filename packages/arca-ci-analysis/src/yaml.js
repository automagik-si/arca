// A reader for the YAML a GitHub Actions workflow is written in: block mappings and sequences, flow `[..]` and
// `{..}`, plain and quoted scalars, block scalars (`|`, `>`), comments. Anchors and tags are read past, an alias is
// null. It is not a full YAML parser: it is enough to find a workflow's triggers, jobs, steps and their inputs, and the
// line each key is on, so that a change can be written into the file as text and leave the rest of it untouched.

/** Each mapping and sequence carries the 1-based line of each of its keys (or items) under this symbol. */
export const LINES = Symbol('lines');

/** Lines live under a symbol no comparison or JSON of the value sees. */
function hide(target, lines) {
  Object.defineProperty(target, LINES, { value: lines, enumerable: false });
}

export function parse(text) {
  const lines = text.replace(/\r\n?/g, '\n').split('\n');
  const parser = new Parser(lines);
  parser.skip();
  if (parser.i >= lines.length) return null;
  return parser.block(parser.indent(parser.i));
}

class Parser {
  constructor(lines) {
    this.lines = lines;
    this.i = 0;
  }

  indent(i) {
    const line = this.lines[i];
    return line.length - line.trimStart().length;
  }

  content(i) {
    return stripComment(this.lines[i]).trimEnd();
  }

  /** Moves past blank lines, comment lines and document markers. */
  skip() {
    while (this.i < this.lines.length) {
      const text = this.content(this.i).trim();
      if (text === '' || text === '---' || text === '...') this.i += 1;
      else break;
    }
  }

  block(ind) {
    const text = this.content(this.i).trimStart();
    if (isItem(text)) return this.sequence(ind);
    return this.mapping(ind);
  }

  sequence(ind) {
    const out = [];
    hide(out, []);
    for (;;) {
      this.skip();
      if (this.i >= this.lines.length) break;
      if (this.indent(this.i) !== ind) break;
      const text = this.content(this.i).trimStart();
      if (!isItem(text)) break;
      const line = this.i;
      out[LINES].push(line + 1);
      const rest = text.slice(1).replace(/^ +/, '');
      if (rest === '') {
        this.i += 1;
        this.skip();
        if (this.i < this.lines.length && this.indent(this.i) > ind) out.push(this.block(this.indent(this.i)));
        else out.push(null);
        continue;
      }
      if (keyOf(rest) !== null && !/^[[{]/.test(rest)) {
        // "- key: value": the mapping starts at the column of its first key.
        const column = this.lines[line].length - this.lines[line].trimStart().length + (text.length - rest.length);
        this.lines[line] = ' '.repeat(column) + this.lines[line].slice(column);
        out.push(this.mapping(column));
        continue;
      }
      out.push(this.value(rest, ind));
    }
    return out;
  }

  mapping(ind) {
    const out = {};
    hide(out, {});
    for (;;) {
      this.skip();
      if (this.i >= this.lines.length) break;
      const here = this.indent(this.i);
      if (here !== ind) break;
      const text = this.content(this.i).trimStart();
      if (isItem(text)) break;
      const split = keyOf(text);
      if (split === null) {
        this.i += 1;
        continue;
      }
      const [key, rest] = split;
      out[LINES][key] = this.i + 1;
      if (rest === '' || /^[&!][^\s]*$/.test(rest)) {
        this.i += 1;
        this.skip();
        if (this.i < this.lines.length) {
          const next = this.indent(this.i);
          const nextText = this.content(this.i).trimStart();
          if (next > ind || (next === ind && isItem(nextText))) {
            out[key] = this.block(next);
            continue;
          }
        }
        out[key] = null;
        continue;
      }
      out[key] = this.value(rest, ind);
    }
    return out;
  }

  /** The value that starts on the current line after a key or a dash; moves past every line it takes. */
  value(rest, ind) {
    rest = rest.replace(/^(?:[&!][^\s]*\s+)+/, '');
    if (/^[|>][-+0-9]*$/.test(rest)) return this.blockScalar(rest, ind);
    if (rest.startsWith('[') || rest.startsWith('{')) {
      let whole = rest;
      this.i += 1;
      while (!balanced(whole) && this.i < this.lines.length) {
        whole += ' ' + this.content(this.i).trim();
        this.i += 1;
      }
      return flow(whole);
    }
    if (rest.startsWith('*')) {
      this.i += 1;
      return null;
    }
    if (rest.startsWith('"') || rest.startsWith("'")) {
      let whole = rest;
      this.i += 1;
      while (!closedQuote(whole) && this.i < this.lines.length) {
        whole += ' ' + this.lines[this.i].trim();
        this.i += 1;
      }
      return scalar(whole);
    }
    // A plain scalar may go on over more indented lines.
    let whole = rest;
    this.i += 1;
    while (this.i < this.lines.length) {
      const text = this.content(this.i).trim();
      if (text === '' || this.indent(this.i) <= ind) break;
      whole += ' ' + text;
      this.i += 1;
    }
    return scalar(whole);
  }

  blockScalar(head, ind) {
    const folded = head.startsWith('>');
    const keep = head.includes('+');
    const strip = head.includes('-');
    this.i += 1;
    const body = [];
    let column = null;
    while (this.i < this.lines.length) {
      const raw = this.lines[this.i];
      if (raw.trim() === '') {
        body.push('');
        this.i += 1;
        continue;
      }
      const here = this.indent(this.i);
      if (here <= ind) break;
      if (column === null) column = here;
      if (here < column) break;
      body.push(raw.slice(column));
      this.i += 1;
    }
    while (body.length && body[body.length - 1] === '' && !keep) body.pop();
    let text = folded ? fold(body) : body.join('\n');
    if (!strip) text += '\n';
    return text;
  }
}

function fold(body) {
  let out = '';
  for (let k = 0; k < body.length; k += 1) {
    const line = body[k];
    if (k === 0) out = line;
    else if (line === '' || /^\s/.test(line) || /^\s/.test(body[k - 1]) || body[k - 1] === '') out += '\n' + line;
    else out += ' ' + line;
  }
  return out;
}

function isItem(text) {
  return text === '-' || text.startsWith('- ');
}

/** `[key, rest]` for a `key: value` line, or null. */
function keyOf(text) {
  let key;
  let at;
  if (text.startsWith('"') || text.startsWith("'")) {
    const end = quoteEnd(text, 0);
    if (end < 0) return null;
    key = scalar(text.slice(0, end + 1));
    at = end + 1;
    if (text[at] !== ':') return null;
  } else {
    const match = /:(?:\s|$)/.exec(text);
    if (!match) return null;
    at = match.index;
    key = text.slice(0, at).trim();
    if (key === '' || key.startsWith('[') || key.startsWith('{')) return null;
  }
  return [String(key), text.slice(at + 1).trim()];
}

function quoteEnd(text, start) {
  const quote = text[start];
  for (let k = start + 1; k < text.length; k += 1) {
    if (quote === "'" && text[k] === "'" && text[k + 1] === "'") {
      k += 1;
      continue;
    }
    if (quote === '"' && text[k] === '\\') {
      k += 1;
      continue;
    }
    if (text[k] === quote) return k;
  }
  return -1;
}

function closedQuote(text) {
  return quoteEnd(text, 0) >= 0;
}

export function stripComment(line) {
  let quote = null;
  for (let k = 0; k < line.length; k += 1) {
    const c = line[k];
    if (quote) {
      if (quote === '"' && c === '\\') k += 1;
      else if (c === quote) quote = null;
      continue;
    }
    if ((c === '"' || c === "'") && (k === 0 || /[\s:[{,-]/.test(line[k - 1]))) quote = c;
    else if (c === '#' && (k === 0 || /\s/.test(line[k - 1]))) return line.slice(0, k);
  }
  return line;
}

function balanced(text) {
  let depth = 0;
  let quote = null;
  for (let k = 0; k < text.length; k += 1) {
    const c = text[k];
    if (quote) {
      if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === '[' || c === '{') depth += 1;
    else if (c === ']' || c === '}') depth -= 1;
  }
  return depth <= 0;
}

/** Splits the inside of a flow collection at its top-level commas. */
function splitFlow(inner) {
  const parts = [];
  let depth = 0;
  let quote = null;
  let start = 0;
  for (let k = 0; k < inner.length; k += 1) {
    const c = inner[k];
    if (quote) {
      if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === '[' || c === '{') depth += 1;
    else if (c === ']' || c === '}') depth -= 1;
    else if (c === ',' && depth === 0) {
      parts.push(inner.slice(start, k));
      start = k + 1;
    }
  }
  parts.push(inner.slice(start));
  return parts.map((part) => part.trim()).filter((part) => part !== '');
}

function flow(text) {
  text = text.trim();
  if (text.startsWith('[')) {
    return splitFlow(text.slice(1, text.lastIndexOf(']'))).map(flow);
  }
  if (text.startsWith('{')) {
    const out = {};
    for (const part of splitFlow(text.slice(1, text.lastIndexOf('}')))) {
      const split = keyOf(part) ?? [part, ''];
      out[split[0]] = split[1] === '' ? null : flow(split[1]);
    }
    return out;
  }
  return scalar(text);
}

export function scalar(text) {
  text = text.trim();
  if (text.startsWith('"')) {
    const end = quoteEnd(text, 0);
    const body = text.slice(1, end < 0 ? undefined : end);
    return body.replace(/\\(.)/g, (_, c) => ({ n: '\n', t: '\t', '"': '"', '\\': '\\' })[c] ?? c);
  }
  if (text.startsWith("'")) {
    const end = quoteEnd(text, 0);
    return text.slice(1, end < 0 ? undefined : end).replace(/''/g, "'");
  }
  if (text === '' || text === '~' || text === 'null' || text === 'Null' || text === 'NULL') return null;
  if (/^(true|True|TRUE)$/.test(text)) return true;
  if (/^(false|False|FALSE)$/.test(text)) return false;
  if (/^[-+]?\d+$/.test(text)) return Number(text);
  if (/^[-+]?(\d+\.\d*|\.\d+)([eE][-+]?\d+)?$/.test(text)) return Number(text);
  return text;
}
