import { describe, expect, it } from 'vitest';
import { mediaType } from './media';

describe('mediaType', () => {
  it('knows images, audio and video by extension, in any case', () => {
    expect(mediaType('photo.JPG')).toEqual({ kind: 'image', mime: 'image/jpeg' });
    expect(mediaType('song.mp3')).toEqual({ kind: 'audio', mime: 'audio/mpeg' });
    expect(mediaType('clip.final.MOV')).toEqual({ kind: 'video', mime: 'video/quicktime' });
    expect(mediaType('drawing.svg')?.kind).toBe('image');
  });

  it('refuses other files, names without an extension and dot files', () => {
    expect(mediaType('notes.txt')).toBeNull();
    expect(mediaType('Makefile')).toBeNull();
    expect(mediaType('.png')).toBeNull();
    expect(mediaType('trailing.')).toBeNull();
  });
});
