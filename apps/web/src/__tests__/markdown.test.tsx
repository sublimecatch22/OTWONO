import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { Markdown } from '../components/Markdown';

describe('Markdown', () => {
  it('renders headings, lists and code', () => {
    const { container } = render(
      <Markdown
        source={'# Title\n\nSome **bold** text.\n\n- one\n- two\n\n```js\nconst a = 1;\n```'}
      />,
    );

    expect(screen.getByRole('heading', { name: 'Title' })).toBeInTheDocument();
    expect(container.querySelectorAll('li')).toHaveLength(2);
    expect(container.querySelector('pre code')?.textContent).toBe('const a = 1;');
    expect(container.querySelector('strong')?.textContent).toBe('bold');
  });

  it('never renders raw HTML from the model', () => {
    const { container } = render(
      <Markdown source={'<img src=x onerror="alert(1)"> and <script>alert(2)</script>'} />,
    );

    expect(container.querySelector('img')).toBeNull();
    expect(container.querySelector('script')).toBeNull();
    // The markup is shown as text, so the user can see what was produced.
    expect(container.textContent).toContain('<script>alert(2)</script>');
  });

  it('renders http links but refuses other schemes', () => {
    const { container } = render(
      <Markdown
        source={
          '[safe](https://example.com) [unsafe](javascript:alert(1)) [file](file:///etc/passwd)'
        }
      />,
    );

    const links = container.querySelectorAll('a');
    expect(links).toHaveLength(1);
    expect(links[0]?.getAttribute('href')).toBe('https://example.com');
    expect(links[0]?.getAttribute('rel')).toContain('noopener');
    expect(container.textContent).toContain('[unsafe](javascript:alert(1))');
    expect(container.textContent).toContain('[file](file:///etc/passwd)');
  });

  it('keeps blockquotes and ordered lists distinct', () => {
    const { container } = render(<Markdown source={'> quoted\n\n1. first\n2. second'} />);
    expect(container.querySelector('blockquote')?.textContent).toContain('quoted');
    expect(container.querySelector('ol')?.querySelectorAll('li')).toHaveLength(2);
  });

  it('renders plain prose without inventing structure', () => {
    const { container } = render(<Markdown source="Just a sentence." />);
    expect(container.querySelectorAll('p')).toHaveLength(1);
    expect(container.textContent).toBe('Just a sentence.');
  });
});
