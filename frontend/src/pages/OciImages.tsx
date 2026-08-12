import FormatLandingPage from '../components/FormatLandingPage.tsx';

export default function OciImages() {
  const host = () => location.host;

  return (
    <FormatLandingPage
      format="oci"
      icon="container"
      title="Containers"
      subtitle="Push and pull OCI images with the standard Docker toolchain — same accounts, same permissions as the rest of the registry."
      reposTitle="OCI repositories"
      emptyTitle="No OCI repository yet"
      emptyTextAdmin="Create a repository with format “oci” to start pushing images."
      emptyTextOther="Ask an administrator to create a repository with format “oci”."
      exampleFallback="oci-private"
      repoCommand={(name) => `${host()}/${name}/image:tag`}
      steps={(example) => [
        { label: '1 · Sign in', command: `docker login ${host()}` },
        { label: '2 · Tag', command: `docker tag myapp:latest ${host()}/${example}/myapp:latest` },
        { label: '3 · Push', command: `docker push ${host()}/${example}/myapp:latest` },
      ]}
      alert={
        <>
          Serving over plain HTTP? Add <span class="mono">"insecure-registries": ["{host()}"]</span>{' '}
          to Docker's <span class="mono">daemon.json</span> — or put the registry behind TLS.
        </>
      }
    />
  );
}
