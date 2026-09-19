import FormatLandingPage from '../components/FormatLandingPage.tsx';
import { connectLine, endpointOf, exampleRepo, quickstart } from '../core/quickstart.ts';

export default function GoModules() {
  const at = () => endpointOf(location);

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
      exampleFallback={exampleRepo('go')}
      repoCommand={(name) => connectLine('go', name, at())}
      steps={(example) => quickstart('go', example, at())}
      alert={
        <>
          Proxy-type Go repositories cache modules from upstream sources such as{' '}
          <span class="mono">proxy.golang.org</span>; hosted ones serve modules you publish directly.
        </>
      }
    />
  );
}
