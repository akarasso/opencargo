import type { RepoFormat } from './types.ts';

export interface QuickstartStep {
  label: string;
  command: string;
}

/** Where this server answers: `base` carries the scheme, `host` does not. */
export interface Endpoint {
  base: string;
  host: string;
  scheme: string;
}

export function endpointOf(location: { protocol: string; host: string }): Endpoint {
  return {
    base: `${location.protocol}//${location.host}`,
    host: location.host,
    scheme: location.protocol,
  };
}

interface Client {
  /** What a user calls the toolchain, which is not always the format's name. */
  label: string;
  /** Repository name used when none exists yet. */
  example: string;
  /** The one line that connects a client to this repository. */
  connect: (repo: string, at: Endpoint) => string;
  steps: (repo: string, at: Endpoint) => QuickstartStep[];
}

const clients: Record<RepoFormat, Client> = {
  npm: {
    label: 'npm / pnpm',
    example: 'npm-all',
    connect: (repo, at) => `registry=${at.base}/${repo}/`,
    steps: (repo, at) => [
      { label: '1 · Point npm at the registry', command: `npm config set registry ${at.base}/${repo}/` },
      { label: '2 · Sign in', command: `npm login --registry ${at.base}/${repo}/` },
      { label: '3 · Publish', command: `npm publish --registry ${at.base}/${repo}/` },
      { label: '4 · Install', command: 'npm install @your-scope/package' },
    ],
  },
  cargo: {
    label: 'cargo',
    example: 'cargo-all',
    connect: (repo, at) => `index = "sparse+${at.base}/${repo}/index/"`,
    steps: (repo, at) => [
      {
        label: '1 · Declare the registry (.cargo/config.toml)',
        command: `[registries.oc]\nindex = "sparse+${at.base}/${repo}/index/"`,
      },
      { label: '2 · Sign in', command: 'cargo login --registry oc' },
      { label: '3 · Publish', command: 'cargo publish --registry oc' },
      { label: '4 · Depend on a crate', command: 'my-crate = { version = "0.1", registry = "oc" }' },
    ],
  },
  oci: {
    label: 'docker',
    example: 'oci-private',
    connect: (repo, at) => `${at.host}/${repo}/image:tag`,
    steps: (repo, at) => [
      { label: '1 · Sign in', command: `docker login ${at.host}` },
      { label: '2 · Tag', command: `docker tag myapp:latest ${at.host}/${repo}/myapp:latest` },
      { label: '3 · Push', command: `docker push ${at.host}/${repo}/myapp:latest` },
    ],
  },
  go: {
    label: 'go',
    example: 'go-all',
    connect: (repo, at) => `GOPROXY=${at.base}/${repo},direct`,
    steps: (repo, at) => [
      { label: '1 · Point Go at the registry', command: `export GOPROXY=${at.base}/${repo},direct` },
      { label: '2 · Skip the checksum database for private modules', command: 'export GONOSUMDB=your.private.domain/*' },
      { label: '3 · Install', command: 'go get your.private.domain/module@latest' },
      { label: '4 · Publish (from a Git tag)', command: 'git tag v1.0.0 && git push origin v1.0.0' },
    ],
  },
  pypi: {
    label: 'pip / twine',
    example: 'pypi-all',
    connect: (repo, at) => `--index-url ${at.base}/${repo}/simple/`,
    steps: (repo, at) => [
      {
        label: '1 · Point pip at the index',
        command: `pip config set global.index-url ${at.base}/${repo}/simple/`,
      },
      {
        label: '2 · Install (the Basic user is the literal __token__)',
        command: `pip install --index-url "${at.scheme}//__token__:$TOKEN@${at.host}/${repo}/simple/" demo`,
      },
      {
        label: '3 · Publish',
        command: `twine upload --repository-url ${at.base}/${repo}/legacy/ -u __token__ -p "$TOKEN" dist/*`,
      },
    ],
  },
  maven: {
    label: 'maven / gradle',
    example: 'maven-all',
    connect: (repo, at) => `${at.base}/maven/${repo}/`,
    steps: (repo, at) => [
      {
        label: '1 · Declare the server (~/.m2/settings.xml)',
        command: '<server><id>oc</id><username>you</username><password>$TOKEN</password></server>',
      },
      {
        label: '2 · Read from it (pom.xml)',
        command: `<repository><id>oc</id><url>${at.base}/maven/${repo}/</url></repository>`,
      },
      {
        label: '3 · Deploy to it (pom.xml)',
        command: `<distributionManagement><repository><id>oc</id><url>${at.base}/maven/${repo}/</url></repository></distributionManagement>`,
      },
      { label: '4 · Publish', command: 'mvn deploy' },
    ],
  },
  nuget: {
    label: 'dotnet',
    example: 'nuget-all',
    connect: (repo, at) => `${at.base}/${repo}/v3/index.json`,
    steps: (repo, at) => [
      {
        label: '1 · Add the source',
        command: `dotnet nuget add source ${at.base}/${repo}/v3/index.json -n oc -u you -p $TOKEN --store-password-in-clear-text`,
      },
      { label: '2 · Publish', command: 'dotnet nuget push MyLib.1.0.0.nupkg --source oc --api-key $TOKEN' },
      { label: '3 · Restore', command: 'dotnet restore' },
    ],
  },
  raw: {
    label: 'curl',
    example: 'raw-private',
    connect: (repo, at) => `${at.base}/raw/${repo}/<path>`,
    steps: (repo, at) => [
      {
        label: '1 · Store a file at a path',
        command: `curl -u you:$TOKEN -T ./tool.tar.gz ${at.base}/raw/${repo}/dist/tool.tar.gz`,
      },
      { label: '2 · Read it back', command: `curl -O ${at.base}/raw/${repo}/dist/tool.tar.gz` },
      {
        label: '3 · List what a prefix holds',
        command: `curl -u you:$TOKEN "${at.base}/api/v1/raw/${repo}/files?prefix=dist"`,
      },
    ],
  },
  mcp: {
    label: 'mcp',
    example: 'mcp-mirror',
    connect: (repo, at) => `${at.base}/${repo}/v0.1/servers`,
    steps: (repo, at) => [
      { label: '1 · Browse the approved servers', command: `curl ${at.base}/${repo}/v0.1/servers` },
      {
        label: '2 · Generate a client configuration',
        command: `curl -o .mcp.json ${at.base}/${repo}/clients/claude-code/config.json`,
      },
      {
        label: '3 · Point VS Code at the gallery',
        command: `"chat.mcp.gallery.serviceUrl": "${at.base}/${repo}/v0.1/servers"`,
      },
    ],
  },
};

export function exampleRepo(format: RepoFormat): string {
  return clients[format].example;
}

export function clientLabel(format: RepoFormat): string {
  return clients[format].label;
}

/** The single line that connects a client, for a compact list. */
export function connectLine(format: RepoFormat, repo: string, at: Endpoint): string {
  return clients[format].connect(repo, at);
}

export function quickstart(format: RepoFormat, repo: string, at: Endpoint): QuickstartStep[] {
  return clients[format].steps(repo, at);
}
