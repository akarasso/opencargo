import FormatLandingPage from '../components/FormatLandingPage.tsx';

export default function GoModules() {
  const base = () => `${location.protocol}//${location.host}`;

  return (
    <FormatLandingPage
      format="go"
      icon="code"
      title="Go modules"
      subtitle="A GOPROXY endpoint for private modules, with transparent caching of upstream ones."
      reposTitle="Go repositories"
      emptyTitle="No Go repository yet"
      emptyTextAdmin="Create a repository with format “go” to start serving modules."
      emptyTextOther="Ask an administrator to create a repository with format “go”."
      exampleFallback="go-private"
      repoCommand={(name) => `GOPROXY=${base()}/${name},direct`}
      steps={(example) => [
        { label: '1 · Point Go at the registry', command: `export GOPROXY=${base()}/${example},direct` },
        {
          label: '2 · Skip checksum DB for private modules',
          command: 'export GONOSUMCHECK=your.private.domain/*',
        },
        { label: '3 · Install', command: 'go get your.private.domain/module@latest' },
        { label: '4 · Publish (from a Git tag)', command: 'git tag v1.0.0 && git push origin v1.0.0' },
      ]}
      alert={
        <>
          Proxy-type Go repositories cache modules from upstream sources such as{' '}
          <span class="mono">proxy.golang.org</span>; hosted ones serve modules you publish directly.
        </>
      }
    />
  );
}
