import { describe, expect, test } from 'bun:test';
import { normalizeVersion, publishedTagFetchArgs, sortVersionsNewestFirst } from './versions.mjs';

describe('normalizeVersion', () => {
  test.each([
    ['0.7.5', '0.7.5'],
    ['v0.7.5', '0.7.5'],
  ])('normalizes %s', (input, expected) => {
    expect(normalizeVersion(input)).toBe(expected);
  });

  test('rejects non-release refs', () => {
    expect(() => normalizeVersion('preview-123')).toThrow();
  });
});

describe('publishedTagFetchArgs', () => {
  test('fetches only manifest-bound tags from canonical upstream without force or publication', () => {
    expect(publishedTagFetchArgs([
      { version: '0.8.2', tag: 'v0.8.2', commit: 'a'.repeat(40) },
      { version: '0.8.0', tag: 'v0.8.0', commit: 'b'.repeat(40) },
      { version: '0.7.5', tag: 'v0.7.5' },
    ])).toEqual([
      'fetch', '--atomic', '--no-tags', '--no-recurse-submodules',
      'https://github.com/herdrdev/herdr.git',
      'refs/tags/v0.8.2:refs/tags/v0.8.2',
      'refs/tags/v0.8.0:refs/tags/v0.8.0',
    ]);
  });

  test('does not perform a default fetch when there are no commit-bound tags', () => {
    expect(publishedTagFetchArgs([])).toBeNull();
    expect(publishedTagFetchArgs([{ version: '0.7.5', tag: 'v0.7.5' }])).toBeNull();
  });

  test.each([
    { version: '0.8.2', tag: 'v0.8.0', commit: 'a'.repeat(40) },
    { version: '0.8.2', tag: '+refs/tags/*', commit: 'a'.repeat(40) },
    { version: '0.8.2', tag: 'v0.8.2\n', commit: 'a'.repeat(40) },
    { version: 'preview-123', tag: 'preview-123', commit: 'a'.repeat(40) },
    { version: '0.8.2', tag: 'v0.8.2', commit: 'not-a-commit' },
  ])('rejects invalid provenance before fetching: %j', (entry) => {
    expect(() => publishedTagFetchArgs([entry])).toThrow();
  });
});

describe('sortVersionsNewestFirst', () => {
  test('sorts semantic versions numerically', () => {
    const versions = [
      { version: '0.6.10' },
      { version: '0.7.2' },
      { version: '0.7.10' },
      { version: '0.5.12' },
    ];

    expect(sortVersionsNewestFirst(versions).map(({ version }) => version)).toEqual([
      '0.7.10',
      '0.7.2',
      '0.6.10',
      '0.5.12',
    ]);
  });
});
