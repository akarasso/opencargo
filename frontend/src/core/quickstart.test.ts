import { describe, expect, it } from 'vitest';
import type { RepoFormat } from './types.ts';
import { clientLabel, connectLine, endpointOf, exampleRepo, quickstart } from './quickstart.ts';

const at = endpointOf({ protocol: 'https:', host: 'registry.example.com' });

// Every format a repository can carry must be connectable: a new format that
// forgets its snippet fails to compile, and this list keeps the runtime honest.
const formats: RepoFormat[] = ['npm', 'cargo', 'oci', 'go', 'pypi', 'maven', 'nuget', 'mcp'];

describe('quickstart', () => {
  it('covers every repository format', () => {
    for (const format of formats) {
      expect(connectLine(format, 'team-repo', at)).toContain('team-repo');
      expect(quickstart(format, 'r', at).length).toBeGreaterThan(0);
      expect(exampleRepo(format)).not.toBe('');
    }
  });

  it('names the repository it was given, and labels every step', () => {
    for (const format of formats) {
      const steps = quickstart(format, 'team-repo', at);
      expect(steps.some((s) => s.command.includes('team-repo'))).toBe(true);
      for (const step of steps) expect(step.label).not.toBe('');
    }
  });

  it('builds URLs on the endpoint it was given', () => {
    expect(connectLine('npm', 'npm-all', at)).toBe('registry=https://registry.example.com/npm-all/');
    expect(connectLine('go', 'go-all', at)).toBe('GOPROXY=https://registry.example.com/go-all,direct');
    expect(connectLine('nuget', 'nuget-all', at)).toBe(
      'https://registry.example.com/nuget-all/v3/index.json',
    );
    expect(connectLine('maven', 'maven-all', at)).toBe('https://registry.example.com/maven/maven-all/');
  });

  it('addresses Docker by host, without a scheme', () => {
    expect(connectLine('oci', 'oci-private', at)).toBe('registry.example.com/oci-private/image:tag');
    expect(quickstart('oci', 'oci-private', at)[0].command).toBe('docker login registry.example.com');
  });

  it('uses the checksum-database variable Go actually reads', () => {
    const commands = quickstart('go', 'go-all', at).map((s) => s.command);
    expect(commands.some((c) => c.includes('GONOSUMDB'))).toBe(true);
    expect(commands.some((c) => c.includes('GONOSUMCHECK'))).toBe(false);
  });

  it('gives PyPI the literal __token__ Basic user', () => {
    const commands = quickstart('pypi', 'pypi-all', at).map((s) => s.command);
    expect(commands.some((c) => c.includes('__token__'))).toBe(true);
  });

  it('keeps an endpoint plain HTTP when that is how the server answers', () => {
    const local = endpointOf({ protocol: 'http:', host: 'localhost:6789' });
    expect(connectLine('npm', 'npm-all', local)).toBe('registry=http://localhost:6789/npm-all/');
    // No step may spell a scheme of its own: PyPI's credential URL is the one
    // that is tempting to hardcode as https.
    for (const format of formats) {
      for (const step of quickstart(format, 'r', local)) {
        expect(step.command).not.toContain('https://localhost:6789');
        expect(step.command).not.toContain('https://r');
      }
    }
    const pip = quickstart('pypi', 'pypi-all', local).map((s) => s.command).join('\n');
    expect(pip).toContain('http://__token__:$TOKEN@localhost:6789/pypi-all/simple/');
  });

  it('gives every format a label a user would recognise', () => {
    expect(clientLabel('oci')).toBe('docker');
    expect(clientLabel('npm')).toBe('npm / pnpm');
    for (const format of formats) expect(clientLabel(format)).not.toBe('');
  });
});
