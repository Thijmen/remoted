import { describe, expect, it } from 'vitest';
import { RateMeter, formatDuration, formatSize } from './format';
import { baseName, invalidName, joinPath, keepBothName, splitExt } from './paths';

describe('paths', () => {
  it('joins without doubling the separator', () => {
    expect(joinPath('/', 'a', '/')).toBe('/a');
    expect(joinPath('/Users/me', 'a.txt', '/')).toBe('/Users/me/a.txt');
    expect(joinPath('C:\\', 'Users', '\\')).toBe('C:\\Users');
    expect(joinPath('C:\\Users', 'me', '\\')).toBe('C:\\Users\\me');
  });

  it('takes the last component', () => {
    expect(baseName('/Users/me/a.txt', '/')).toBe('a.txt');
    expect(baseName('/Users/me/', '/')).toBe('me');
    expect(baseName('/', '/')).toBe('/');
    expect(baseName('C:\\Users\\me', '\\')).toBe('me');
    expect(baseName('C:\\', '\\')).toBe('C:\\');
  });

  it('splits extensions like Finder and Explorer', () => {
    expect(splitExt('report.pdf')).toEqual(['report', '.pdf']);
    expect(splitExt('archive.tar.gz')).toEqual(['archive.tar', '.gz']);
    expect(splitExt('.bashrc')).toEqual(['.bashrc', '']);
    expect(splitExt('v1.2', true)).toEqual(['v1.2', '']);
  });

  it('finds a free "keep both" name', () => {
    expect(keepBothName('a.txt', ['b.txt'])).toBe('a.txt');
    expect(keepBothName('a.txt', ['a.txt'])).toBe('a (2).txt');
    expect(keepBothName('a.txt', ['A.TXT', 'a (2).txt'])).toBe('a (3).txt');
    expect(keepBothName('a.txt', ['A.TXT'], false, true)).toBe('a.txt');
    expect(keepBothName('Photos', ['Photos'], true)).toBe('Photos (2)');
    expect(keepBothName('v1.2', ['v1.2'], true)).toBe('v1.2 (2)');
  });

  it('checks names for the remote OS', () => {
    expect(invalidName('ok.txt', 'macos')).toBeNull();
    expect(invalidName('', 'macos')).not.toBeNull();
    expect(invalidName('..', 'macos')).not.toBeNull();
    expect(invalidName('a/b', 'macos')).not.toBeNull();
    expect(invalidName('a:b', 'macos')).toBeNull();
    expect(invalidName('a:b', 'windows')).not.toBeNull();
    expect(invalidName('name.', 'windows')).not.toBeNull();
    expect(invalidName('CON.txt', 'windows')).not.toBeNull();
    expect(invalidName('console.txt', 'windows')).toBeNull();
  });
});

describe('format', () => {
  it('formats sizes', () => {
    expect(formatSize(0)).toBe('0 B');
    expect(formatSize(999)).toBe('999 B');
    expect(formatSize(1000)).toBe('1.00 KB');
    expect(formatSize(12_345)).toBe('12.3 KB');
    expect(formatSize(999_999)).toBe('1.00 MB');
    expect(formatSize(4_560_000_000)).toBe('4.56 GB');
  });

  it('formats durations', () => {
    expect(formatDuration(4.4)).toBe('4 s');
    expect(formatDuration(150)).toBe('3 min');
    expect(formatDuration(3600)).toBe('1 h');
    expect(formatDuration(4800)).toBe('1 h 20 min');
  });

  it('measures a rate and drops to zero when stalled', () => {
    const r = new RateMeter(3000);
    r.add(0, 0);
    r.add(1_000_000, 1000);
    r.add(2_000_000, 2000);
    expect(r.rate(2000)).toBeCloseTo(1_000_000);
    expect(r.rate(6000)).toBe(0);
  });
});
