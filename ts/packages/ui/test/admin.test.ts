import { describe, expect, it } from 'vitest';
import { adminHeaders, readAdminToken, writeAdminToken } from '../src/lib/adminSession';

describe('admin session', () => {
  it('degrades quietly when the runtime has no session storage', () => {
    // The unit tests run in plain node, which has no `window`. A dashboard
    // must still render there, so every helper falls back instead of throwing.
    expect(readAdminToken()).toBe('');
    expect(adminHeaders()).toEqual({});
    expect(() => writeAdminToken('operator-token')).not.toThrow();
  });
});
