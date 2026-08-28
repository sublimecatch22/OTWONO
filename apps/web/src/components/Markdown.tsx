/**
 * A small, deliberately limited Markdown renderer.
 *
 * It handles what a model actually produces — headings, lists, code, emphasis,
 * links — and nothing else. It never sets innerHTML, so a model (or a document
 * a model quoted) cannot inject markup into the interface.
 */

import type { ReactNode } from 'react';
import { Fragment } from 'react';

interface Block {
  kind: 'heading' | 'paragraph' | 'code' | 'list' | 'quote';
  level?: number;
  language?: string;
  lines: string[];
  ordered?: boolean;
}

function parseBlocks(source: string): Block[] {
  const lines = source.replace(/\r\n/g, '\n').split('\n');
  const blocks: Block[] = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index] ?? '';

    if (line.trimStart().startsWith('```')) {
      const language = line.trim().slice(3).trim();
      const body: string[] = [];
      index += 1;
      while (index < lines.length && !(lines[index] ?? '').trimStart().startsWith('```')) {
        body.push(lines[index] ?? '');
        index += 1;
      }
      index += 1; // closing fence
      blocks.push({ kind: 'code', language, lines: body });
      continue;
    }

    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading) {
      blocks.push({
        kind: 'heading',
        level: heading[1]!.length,
        lines: [heading[2] ?? ''],
      });
      index += 1;
      continue;
    }

    if (/^\s*>\s?/.test(line)) {
      const body: string[] = [];
      while (index < lines.length && /^\s*>\s?/.test(lines[index] ?? '')) {
        body.push((lines[index] ?? '').replace(/^\s*>\s?/, ''));
        index += 1;
      }
      blocks.push({ kind: 'quote', lines: body });
      continue;
    }

    const bullet = /^\s*([-*+]|\d+[.)])\s+/.exec(line);
    if (bullet) {
      const ordered = /\d/.test(bullet[1] ?? '');
      const body: string[] = [];
      while (index < lines.length && /^\s*([-*+]|\d+[.)])\s+/.test(lines[index] ?? '')) {
        body.push((lines[index] ?? '').replace(/^\s*([-*+]|\d+[.)])\s+/, ''));
        index += 1;
      }
      blocks.push({ kind: 'list', ordered, lines: body });
      continue;
    }

    if (line.trim() === '') {
      index += 1;
      continue;
    }

    const body: string[] = [];
    while (index < lines.length && (lines[index] ?? '').trim() !== '') {
      const next = lines[index] ?? '';
      if (
        next.trimStart().startsWith('```') ||
        /^(#{1,4})\s+/.test(next) ||
        /^\s*([-*+]|\d+[.)])\s+/.test(next) ||
        /^\s*>\s?/.test(next)
      ) {
        break;
      }
      body.push(next);
      index += 1;
    }
    blocks.push({ kind: 'paragraph', lines: body });
  }

  return blocks;
}

/** Inline emphasis, code and links, produced as React nodes rather than HTML. */
function renderInline(text: string, keyPrefix: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  const pattern = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*]+\*)|(\[[^\]]+\]\([^)\s]+\))/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  let counter = 0;

  while ((match = pattern.exec(text)) !== null) {
    if (match.index > lastIndex) {
      nodes.push(text.slice(lastIndex, match.index));
    }
    const token = match[0];
    const key = `${keyPrefix}-${counter++}`;

    if (token.startsWith('`')) {
      nodes.push(<code key={key}>{token.slice(1, -1)}</code>);
    } else if (token.startsWith('**')) {
      nodes.push(<strong key={key}>{token.slice(2, -2)}</strong>);
    } else if (token.startsWith('*')) {
      nodes.push(<em key={key}>{token.slice(1, -1)}</em>);
    } else {
      const link = /^\[([^\]]+)\]\(([^)\s]+)\)$/.exec(token);
      const label = link?.[1] ?? token;
      const href = link?.[2] ?? '';
      // Only http(s) links are rendered as links. Anything else — javascript:,
      // data:, file: — is shown as plain text.
      if (/^https?:\/\//i.test(href)) {
        nodes.push(
          <a key={key} href={href} target="_blank" rel="noreferrer noopener">
            {label}
          </a>,
        );
      } else {
        nodes.push(<span key={key}>{token}</span>);
      }
    }
    lastIndex = match.index + token.length;
  }

  if (lastIndex < text.length) nodes.push(text.slice(lastIndex));
  return nodes;
}

export function Markdown({ source }: { source: string }) {
  const blocks = parseBlocks(source);

  return (
    <div className="markdown">
      {blocks.map((block, index) => {
        const key = `block-${index}`;
        switch (block.kind) {
          case 'heading': {
            const level = Math.min(block.level ?? 2, 4);
            const Tag = (['h2', 'h3', 'h4', 'h5'][level - 1] ?? 'h3') as 'h2' | 'h3' | 'h4' | 'h5';
            return <Tag key={key}>{renderInline(block.lines[0] ?? '', key)}</Tag>;
          }
          case 'code':
            return (
              <pre key={key} data-language={block.language || undefined}>
                <code>{block.lines.join('\n')}</code>
              </pre>
            );
          case 'quote':
            return (
              <blockquote key={key}>
                {block.lines.map((line, lineIndex) => (
                  <p key={`${key}-${lineIndex}`}>{renderInline(line, `${key}-${lineIndex}`)}</p>
                ))}
              </blockquote>
            );
          case 'list': {
            const Tag = block.ordered ? 'ol' : 'ul';
            return (
              <Tag key={key}>
                {block.lines.map((line, lineIndex) => (
                  <li key={`${key}-${lineIndex}`}>{renderInline(line, `${key}-${lineIndex}`)}</li>
                ))}
              </Tag>
            );
          }
          default:
            return (
              <p key={key}>
                {block.lines.map((line, lineIndex) => (
                  <Fragment key={`${key}-${lineIndex}`}>
                    {renderInline(line, `${key}-${lineIndex}`)}
                    {lineIndex < block.lines.length - 1 && <br />}
                  </Fragment>
                ))}
              </p>
            );
        }
      })}
    </div>
  );
}
