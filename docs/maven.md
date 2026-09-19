# Maven repositories

Hosted, proxy and group repositories speak the Maven 2 layout under one mount.

```
GET    /maven/{repo}/{group/as/path}/{artifact}/{version}/{file}     Also HEAD
GET    /maven/{repo}/{group/as/path}/{artifact}/maven-metadata.xml
GET    /maven/{repo}/{...}/{file}.{md5|sha1|sha256|sha512}
PUT    /maven/{repo}/{...}                                           Deposit, hosted only
```

A checksum is served beside every file and computed by the server, never taken
from the client's word: a `.sha1` a client deposits is a *declaration*, checked
against the bytes when they arrive and refused as a mismatch when they disagree.

## Depositing

`mvn deploy` sends a file per request, in no guaranteed order, and a version is
therefore assembled. A version becomes visible when its POM has arrived and no
declaration is still waiting for its file; until then it is a pending unit that
belongs to its depositor:

- a visible file never changes — a second deploy of different bytes under a
  published name is refused (`the file is published and immutable`);
- the same bytes again are accepted and change nothing;
- a pending version takes files from its depositor only; another principal's
  different bytes mark it contested rather than overwrite it;
- a file larger than 1 GiB is refused.

`SNAPSHOT` versions are stamped with a `yyyyMMdd.HHmmss-N` build on deposit and
the version's `maven-metadata.xml` names the newest build.

## Metadata

`maven-metadata.xml` is rendered by the server at three levels — a group's
plugin prefixes, an artifact's versions, a snapshot version's builds. For an
artifact, `versions` is sorted in Maven's own comparison order, `release` is the
newest non-snapshot and `latest` the newest of all.

In a `group`, the members' documents are merged: versions united and
`latest`/`release`/`lastUpdated` recomputed, the newest snapshot build taken
whole from the single member announcing it, plugin prefixes united. A member
whose document does not parse is left out of the merge with a warning rather
than failing the read. The merged `ETag` digests the members' own.

## Proxying

A `proxy` takes a repository base as `upstream`, e.g. `https://repo1.maven.org/maven2`.
`maven-metadata.xml` is revalidated every five minutes; a release file and a
timestamped snapshot build are immutable and cached forever. A file is verified
against the upstream's own `.sha1`, fetched beside it, before it is served; that
sidecar is never served as is. A file over 2 GiB is refused.

## Client configuration

```xml
<!-- ~/.m2/settings.xml -->
<settings>
  <servers>
    <server><id>oc</id><username>dev1</username><password>trg_...</password></server>
  </servers>
</settings>
```

```xml
<!-- pom.xml -->
<repositories>
  <repository><id>oc</id><url>https://registry.example.com/maven/maven-all/</url>
    <releases><enabled>true</enabled></releases>
    <snapshots><enabled>true</enabled></snapshots>
  </repository>
</repositories>
<distributionManagement>
  <repository><id>oc</id><url>https://registry.example.com/maven/maven-releases/</url></repository>
  <snapshotRepository><id>oc</id><url>https://registry.example.com/maven/maven-snapshots/</url></snapshotRepository>
</distributionManagement>
```

Gradle takes the same URLs:

```kotlin
repositories {
    maven {
        url = uri("https://registry.example.com/maven/maven-all/")
        credentials { username = "dev1"; password = "trg_..." }
    }
}
```
